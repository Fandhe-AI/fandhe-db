//! 参照アクションが書き換える子行ごとのトリガ発火（TABLE-45・Issue #3284）。
//!
//! 役割: 親の UPDATE／DELETE のトリガ経路（`tenant/trigger_dml.rs`）が
//! [`super::enforce_referencing_rows_with_triggers_in_txn`] へ渡した [`super::ChildTriggers`] を使い、
//! `propagate_referential_actions` の適用段（`apply_referential_action`）を、子表に該当イベント（CASCADE の
//! ON DELETE は DELETE、それ以外は UPDATE）の ROW トリガがあるときだけ、子行ごとのループへ差し替える。
//! 子表にトリガが無い連鎖・トリガ経路以外の呼び出し元は従来の適用段のままでバイト不変。従来の適用段は
//! 行ストアの可変ハンドルを開いたまま回るため、BEFORE を「redb の表ハンドルを 1 つも開かずに」呼ぶ契約
//! （実行モデルの D5）を満たせず、行ごとに開閉する別ループを持つ。
//!
//! 行ごとの順序（`trigger_dml.rs` と同じ）: 割り込み確認 → 行を読んで OLD（ハンドルを閉じる）→ NEW の候補を
//! 作る → BEFORE ROW（`NULL` 返却はその子行を書き換え・削除しない）→ 書き込み（行ごとに表を開閉）。AFTER ROW は
//! 待ち行列へ積み、連鎖と事後検証がすべて済んだ後に [`flush_after`] が適用順で発火する。`NULL` 返却で
//! 旧キーのまま残った子行は、既存の事後検証（NO ACTION 判定・バックストップ）が `23503` で文全体を取り消す
//! （fail-closed。専用の `wire_code` は追加しない）。
//!
//! 連鎖の走査・書き込みはすべてサーバー側導出の `tenant_id` の物理キー範囲に閉じ、トリガ定義も同じテナントの
//! scope だけから引く（RLS-10）。トリガ本体は発火元の文の主体・`PolicyContext` のまま実行される。

use super::{
    build_action_assignments, child_fk_indices, finish_cascade_delete, internal, row_key,
    ActionState, ChildTriggers, ForeignKeyDef, PropagatedChange, ReferentialAction, RowId,
    TableSchema, TenantWriteError, UpdatedKeyPreImages,
};
use crate::catalog::trigger::{TriggerDef, TriggerEvents};
use crate::catalog::{map_row_table_error, CatalogError};
use crate::row_codec::Value;
use crate::storage::change_log::CaptureWriteTxn as _;
use crate::tenant::trigger_row_loop::RowLoopScope;
use crate::trigger::call::TriggerOp;
use crate::trigger::plan::{plan_child_row_triggers, RowTriggerPlan, UpdateTarget};
use crate::trigger::TriggerRuntime;
use redb::ReadableTable as _;
use std::collections::HashMap;

/// 子表の AFTER ROW の待ち行列の 1 件（連鎖・事後検証の後に発火する。トリガ定義は複製して持つ）。
pub(super) struct QueuedAfter {
    table: String,
    schema: TableSchema,
    triggers: Vec<TriggerDef>,
    op: TriggerOp,
    old: Option<Vec<Value>>,
    new: Vec<Value>,
    /// 連鎖の段の深さ（最外段が 0。BEFORE が発火時に見る入れ子の深さと同じ）。発火時に同じ深さへ揃える。
    depth: usize,
}

/// 子表 `child_schema` の、この参照アクションが書き換えるイベントの ROW トリガを分類する。
/// トリガが無ければ `None`（呼び出し元は従来の適用段へ進む）。
pub(super) fn plan_for_child<'r>(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    action: ReferentialAction,
    is_delete: bool,
    runtime: &'r TriggerRuntime<'r>,
) -> Result<Option<RowTriggerPlan<'r>>, TenantWriteError> {
    let (event, set_columns) = if is_delete && matches!(action, ReferentialAction::Cascade) {
        (TriggerEvents::DELETE, Vec::new())
    } else {
        (TriggerEvents::UPDATE, child_fk_indices(child_schema, fk)?)
    };
    plan_child_row_triggers(
        write_txn,
        tenant_id,
        &child_schema.name,
        event,
        Some(&UpdateTarget {
            schema: child_schema,
            set_columns: &set_columns,
        }),
        runtime,
    )
}

/// 行ストアから子行 1 本を読む。表ハンドルは戻る前に閉じる（BEFORE を呼ぶ前の不変条件）。
/// 既に消えている行（多段連鎖の交差）は `None`。
fn read_child_row(
    write_txn: &redb::WriteTransaction,
    child: &str,
    tenant_id: &str,
    id: RowId,
) -> Result<Option<crate::storage::Row>, TenantWriteError> {
    let name = crate::catalog::user_rows_table_name(child);
    let table = write_txn
        .open_table_logged(crate::catalog::user_rows_table_def(&name))
        .map_err(map_row_table_error)?;
    let key = row_key(tenant_id, id);
    let Some(guard) = table.get(&key).map_err(CatalogError::from)? else {
        return Ok(None);
    };
    let row = crate::storage::decode_row_for_key(tenant_id, id, guard.value())
        .map_err(TenantWriteError::Storage)?;
    Ok(Some(row))
}

/// `affected`（Pass 1 が確定した子行）へ参照アクションを、子行ごとにトリガを発火しながら適用する。
/// 戻り値は [`super::apply_referential_action`] と同じ契約（孫段への変更内容と pre-image。スキップした
/// 子行は含めない）。`plan` は [`plan_for_child`] の結果（行トリガが 1 件以上ある）。
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_with_triggers(
    write_txn: &redb::WriteTransaction,
    child_schema: &TableSchema,
    fk: &ForeignKeyDef,
    action: ReferentialAction,
    affected: &super::ActionTargets,
    original_snapshot: &HashMap<RowId, Vec<Value>>,
    tenant_id: &str,
    is_delete: bool,
    plan: &RowTriggerPlan<'_>,
    ctx: &ChildTriggers<'_, '_>,
    state: &mut ActionState<'_, '_>,
) -> Result<(PropagatedChange, Option<UpdatedKeyPreImages>), TenantWriteError> {
    // 子表の行ごとループの間は、トリガ本体が同じ表の行を書き換えられない（`trigger::row_loop`。D6 の暫定
    // ガード）。BEFORE があれば、`UPDATE` の失われた更新も `RowLoopScope` で拒否する。
    let _row_loop_guard =
        crate::trigger::row_loop::RowLoopGuard::enter(tenant_id, &child_schema.name);
    let _row_loop = (!plan.before.is_empty()).then(|| RowLoopScope::enter(&child_schema.name));
    let cascade_delete = is_delete && matches!(action, ReferentialAction::Cascade);
    let fk_indices = child_fk_indices(child_schema, fk)?;
    let mut pre_images = UpdatedKeyPreImages::new();
    let mut deleted_ids: Vec<RowId> = Vec::new();

    for (id, new_key_values) in affected {
        // 協調キャンセルポイント（Issue #1953・`57014`）。
        crate::tenant::check_interrupt()?;
        crate::interrupt::check().map_err(TenantWriteError::TriggerFailed)?;
        let Some(existing) = read_child_row(write_txn, &child_schema.name, tenant_id, *id)? else {
            // 同一トランザクション内で既に削除された等（多段連鎖の交差）。
            continue;
        };
        let Some(original_values) = original_snapshot.get(id) else {
            return Err(internal(
                "referential action pre-image snapshot missing for a triggered row",
            ));
        };
        let visibility = existing.visibility;
        let old_values = crate::tenant::captured_values_from_parts(
            child_schema,
            existing.embedding.clone(),
            &existing.metadata,
        )?;

        if cascade_delete {
            if !plan.before.is_empty() {
                // 表ハンドルを開かずに発火する（再入の不変条件）。
                let proceed = crate::trigger::fire::fire_before_row_delete(
                    ctx.runtime,
                    ctx.writes,
                    &child_schema.name,
                    child_schema,
                    &plan.before,
                    &old_values,
                )
                .map_err(TenantWriteError::TriggerFailed)?;
                if !proceed {
                    continue;
                }
            }
            let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
            {
                let mut row_table = write_txn
                    .open_table_logged(crate::catalog::user_rows_table_def(&row_table_name))
                    .map_err(map_row_table_error)?;
                let removed = row_table
                    .remove(row_key(tenant_id, *id))
                    .map_err(CatalogError::from)?;
                if let Some(removed) = removed.as_ref() {
                    // commit 記録の行の足跡: 連鎖で消える前の子行（SQL-38・Issue #2404）。
                    crate::storage::commit_log::note_row_bytes(
                        write_txn,
                        &child_schema.name,
                        removed.value(),
                    );
                }
            }
            pre_images.record(*id, original_values.clone());
            deleted_ids.push(*id);
            if !plan.after.is_empty() {
                state.after_queue.push(QueuedAfter {
                    table: child_schema.name.clone(),
                    schema: child_schema.clone(),
                    triggers: plan.after.clone(),
                    op: TriggerOp::Delete,
                    old: Some(old_values),
                    new: Vec::new(),
                    depth: state.depth_guards.len().saturating_sub(1),
                });
            }
            continue;
        }

        // `SET NULL`／`SET DEFAULT`／`ON UPDATE CASCADE`: NEW の候補を作る。
        let assignments =
            build_action_assignments(child_schema, &fk_indices, action, new_key_values)?;
        let (embedding, metadata, final_row) = if plan.before.is_empty() {
            let (embedding, metadata) =
                crate::tenant::merge_row_for_update(child_schema, existing, &assignments)
                    .map_err(TenantWriteError::Catalog)?;
            let new_row = crate::tenant::captured_values_from_parts(
                child_schema,
                embedding.clone(),
                &metadata,
            )?;
            (embedding, metadata, new_row)
        } else {
            let mut candidate = old_values.clone();
            for (idx, value) in &assignments {
                let slot = candidate
                    .get_mut(*idx)
                    .ok_or_else(|| internal("internal: SET column index out of range"))?;
                *slot = value.clone();
            }
            let fired = crate::trigger::fire::fire_before_row_update(
                ctx.runtime,
                ctx.writes,
                &child_schema.name,
                child_schema,
                &plan.before,
                &old_values,
                candidate,
            )
            .map_err(TenantWriteError::TriggerFailed)?;
            let Some(final_row) = fired else {
                continue;
            };
            let (embedding, metadata) =
                crate::tenant::trigger_dml::encode_final_row(child_schema, &final_row)?;
            (embedding, metadata, final_row)
        };
        // `visibility` は既存行のものを維持する（SET で触れない列と同じ扱い）。
        let row_input = crate::storage::RowInput {
            tenant_id,
            visibility,
            embedding: &embedding,
            metadata: &metadata,
        };
        let encoded = crate::storage::encode_row(&row_input).map_err(TenantWriteError::Storage)?;
        {
            let row_table_name = crate::catalog::user_rows_table_name(&child_schema.name);
            let mut row_table = write_txn
                .open_table_logged(crate::catalog::user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            row_table
                .insert(row_key(tenant_id, *id), encoded.as_slice())
                .map_err(CatalogError::from)?;
            // commit 記録の行の足跡: 連鎖で書き換わった子行（可視性は不変のため更新後の行で前後を表す）。
            crate::storage::commit_log::note_row_bytes(
                write_txn,
                &child_schema.name,
                encoded.as_slice(),
            );
        }
        pre_images.record(*id, original_values.clone());
        if !plan.after.is_empty() {
            state.after_queue.push(QueuedAfter {
                table: child_schema.name.clone(),
                schema: child_schema.clone(),
                triggers: plan.after.clone(),
                op: TriggerOp::Update,
                old: Some(old_values),
                new: final_row,
                depth: state.depth_guards.len().saturating_sub(1),
            });
        }
    }

    if cascade_delete {
        finish_cascade_delete(write_txn, child_schema, tenant_id, &deleted_ids)?;
        return Ok((PropagatedChange::Removed, Some(pre_images)));
    }
    // BEFORE ROW が主キー・UNIQUE 列を書き換えうるため、BEFORE のある子表は孫段へ全列を更新済みとして
    // 渡す（fail-closed。`trigger_dml.rs` と同じ）。
    let changed = if plan.before.is_empty() {
        fk_indices
    } else {
        (0..child_schema.columns.len()).collect()
    };
    Ok((PropagatedChange::ColumnsUpdated(changed), Some(pre_images)))
}

/// 待ち行列の子表 AFTER ROW を、積んだ順（適用順）× トリガ名のバイト順で発火する。戻り値は無視する
/// （実行モデルの D3）。連鎖・事後検証がすべて済んだ後に呼ぶ。エラーは文全体の失敗（呼び出し元の txn が
/// commit されない）。
pub(super) fn flush_after(state: &mut ActionState<'_, '_>) -> Result<(), TenantWriteError> {
    if state.after_queue.is_empty() {
        return Ok(());
    }
    let Some(ctx) = state.child else {
        return Err(internal(
            "internal: queued child triggers without a firing context",
        ));
    };
    let queue = std::mem::take(&mut state.after_queue);
    for q in &queue {
        // 連鎖の段の深さを再現する。`invoke` が自前で 1 加算するため、段の深さ分だけ先に積めば BEFORE と
        // 同じく「段の深さ + 1」が上限を超えたとき `54001` になり、AFTER 本体の再入も同じ深さから始まる。
        let mut _depth_guards = Vec::with_capacity(q.depth.min(64));
        for _ in 0..q.depth {
            _depth_guards.push(
                crate::trigger::depth::enter(ctx.runtime.limits().max_trigger_depth())
                    .map_err(TenantWriteError::TriggerFailed)?,
            );
        }
        crate::trigger::fire::fire_after_row(
            ctx.runtime,
            ctx.writes,
            &q.table,
            &q.schema,
            &q.triggers,
            q.op,
            q.old.as_deref(),
            &q.new,
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    Ok(())
}
