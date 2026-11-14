//! BEFORE・AFTER ROW／STATEMENT トリガ付きの UPSERT（`INSERT ... ON CONFLICT`）の行書き込み
//! （TABLE-45・Issue #3191。実行モデルの D4・D7・D8・D10）。
//!
//! `tenant::upsert_typed_rows_impl` が、スキーマ取得・台帳照合・BEFORE STATEMENT・rowid の払い出しまでを
//! 従来どおり済ませた後、INSERT または UPDATE のトリガがある表に限って、行を書くループだけをこの
//! モジュールへ差し替える。トリガの無い表は従来の行ループのままでバイト不変。従来のループは行ストアの
//! 可変ハンドルをループ全体で保持するため、BEFORE を「redb の表ハンドルを 1 つも開かずに」呼ぶ契約
//! （実行モデルの D5）を満たせず、行ごとにハンドルを開いて閉じる別ループが要る。
//!
//! 行ごとの順序（PostgreSQL と同じ）:
//! 1. BEFORE ROW INSERT を連鎖する（`NULL` 返却ならその行は終わり。衝突判定も AFTER も数えない）。
//!    最終 NEW を検査・encode し、INSERT 側の利用者定義ポリシーを評価する
//! 2. 衝突判定（所有 ∩ 可視の 3 状態。テナント境界は従来経路と同じ規則）。判定に使う鍵は BEFORE 適用後の NEW
//! 3. 衝突なし: 挿入。`DO NOTHING` の衝突: 何もしない（AFTER も呼ばない）。`DO UPDATE` の衝突: 既存行ポリシーの
//!    USING → `WHERE` → SET 適用（従来経路と共有する `apply_upsert_assignments`。`EXCLUDED` は BEFORE 適用後の
//!    NEW）→ BEFORE ROW UPDATE（OLD は既存行・NEW は SET 後。`NULL` ならその行は更新しない）→ 書き込み
//!
//! 全行の後に制約検査・FK 検査を行い、AFTER ROW を変更の順 × トリガ名のバイト順で発火し、AFTER STATEMENT
//! を UPDATE → INSERT の順に発火する。AFTER の失敗は文全体の失敗で、呼び出し元の txn が commit されない
//! ため行・トリガの書いた行・台帳・索引はいずれも残らない。台帳照合は呼び出し元が全トリガ呼び出しより前に
//! 済ませる（再送ではトリガを 1 回も呼ばない。実行モデルの D10）。
//!
//! PostgreSQL との差分（fail-closed 側）: 同じ文の中で先に書いた行と衝突する場合、`DO UPDATE` は
//! `22000`（PG は `21000`）、`DO NOTHING` の BEFORE による鍵の書き換え衝突は一意制約違反（`23505`）で
//! 文全体を取り消す。呼び出し元は `tenant.rs` のみ。エラーは `TenantWriteError`（トリガ由来は `TriggerFailed`）。

use std::collections::HashSet;

use redb::ReadableTable;

use super::trigger_dml::encode_final_row;
use super::trigger_insert::settle_trigger_written_tables;
use super::trigger_row_loop::RowLoopScope;
use super::{
    apply_upsert_assignments, captured_values_from_parts, encode_row, insert_unique_row, row_key,
    user_rows_table_def, user_rows_table_name, ConflictState, RowId, TenantWriteError,
    UpsertAction, UpsertOutcome, UpsertTarget, WriteTarget,
};
use crate::catalog::{map_row_table_error, CatalogError, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::storage::change_log::CaptureWriteTxn as _;
use crate::storage::{RowInput, Visibility};
use crate::trigger::call::TriggerOp;
use crate::trigger::plan::RowTriggerPlan;
use crate::trigger::write_handle::TriggerWriteHandle;

/// AFTER ROW に渡す変更（書いた順）。
enum AfterEvent {
    /// 挿入した行の最終 NEW。
    Insert(Vec<Value>),
    /// 更新した行の（OLD, 最終 NEW）。
    Update(Vec<Value>, Vec<Value>),
}

fn reserve_err() -> TenantWriteError {
    TenantWriteError::Storage(crate::storage::StorageError::Codec(
        "failed to reserve trigger upsert batch".to_string(),
    ))
}

fn internal(msg: &str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(msg.to_string()))
}

/// UPSERT の行書き込みの結果。
pub(super) struct TriggeredUpsert {
    /// 挿入・更新した行数。
    pub(super) outcome: UpsertOutcome,
}

/// `rows`（`VALUES` の行。DEFAULT 確定後）を、BEFORE ROW トリガを通して書き、全行の後に制約検査・
/// AFTER ROW・AFTER STATEMENT を行う。`insert_plan`・`update_plan` のどちらかは `Some`（呼び出し元が
/// 保証する）。`update_plan` は `DO UPDATE` のときだけ `Some` になり得る。
#[allow(clippy::too_many_arguments)]
pub(super) fn upsert_rows_with_triggers(
    write_txn: &redb::WriteTransaction,
    write_target: &WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    schema: &TableSchema,
    insert_visibility: Visibility,
    rows: &[(u64, &[Value])],
    new_row_ids: Option<&[RowId]>,
    target: &UpsertTarget<'_>,
    action: &UpsertAction<'_>,
    insert_plan: Option<&RowTriggerPlan<'_>>,
    update_plan: Option<&RowTriggerPlan<'_>>,
    writes: &TriggerWriteHandle<'_>,
) -> Result<TriggeredUpsert, TenantWriteError> {
    // 行数が 0 でも `row_security = off` の拒否を開始時点で適用する（RLS-13・Issue #2278）。
    crate::sql::policy_eval::reject_row_security_off_in_txn(write_txn, ctx.tenant_id(), table)?;
    let vector_idx = schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, crate::catalog::ColumnType::Vector(_)));
    let insert_before = insert_plan.filter(|p| !p.before.is_empty());
    let update_before = update_plan.filter(|p| !p.before.is_empty());
    let insert_after = insert_plan.filter(|p| !p.after.is_empty());
    let update_after = update_plan.filter(|p| !p.after.is_empty());

    // 利用者定義ポリシー（RLS-13・Issue #2277）。1 文につき 1 回コンパイルする。
    let policy_proposed = crate::sql::policy_eval::compile_in_txn(
        write_txn,
        ctx.tenant_id(),
        table,
        schema,
        crate::sql::policy_eval::PolicyCheckKind::InsertCheck,
    )?;
    let policy_conflict_update = crate::sql::policy_eval::compile_in_txn(
        write_txn,
        ctx.tenant_id(),
        table,
        schema,
        crate::sql::policy_eval::PolicyCheckKind::ConflictUpdateUsing,
    )?;
    let policy_conflict_select = crate::sql::policy_eval::compile_in_txn(
        write_txn,
        ctx.tenant_id(),
        table,
        schema,
        crate::sql::policy_eval::PolicyCheckKind::ConflictSelectUsing,
    )?;

    // UNIQUE 対象は、BEFORE ROW が呼ばれる前（行ループの前）に既存行を 1 回だけ走査して対象キー →
    // 既存行 id の対応表を作る。表ハンドルは走査の間だけ開く（従来経路と同じ母集合・同じ復元手順）。
    let unique_existing: Option<std::collections::HashMap<Vec<u8>, RowId>> = match target {
        UpsertTarget::RowId | UpsertTarget::AnyUnique => None,
        UpsertTarget::Unique(indices) => {
            let row_table_name = user_rows_table_name(table);
            let (mut existing, corrupt_ids) = {
                let row_table = write_txn
                    .open_table_logged(user_rows_table_def(&row_table_name))
                    .map_err(map_row_table_error)?;
                crate::constraint::scan_tenant_rows_by_unique_key(&row_table, schema, ctx, indices)?
            };
            crate::constraint::recover_unique_keys_of_corrupt_rows(
                write_txn,
                table,
                schema,
                ctx.tenant_id(),
                indices,
                &corrupt_ids,
                &mut existing,
            )?;
            Some(existing)
        }
    };

    // 対象省略（`DO NOTHING` 専用）は、行ごとの制約検査で永続一意索引を同期しながら進める
    // （判定器は行ごとに作り直すため、以降の行から先行行の鍵が見える）。
    let per_row_constraints = matches!(target, UpsertTarget::AnyUnique);

    let mut written_ids: Vec<u64> = Vec::new();
    written_ids
        .try_reserve_exact(rows.len())
        .map_err(|_| reserve_err())?;
    let mut updated_write_ids: Vec<u64> = Vec::new();
    // 同じ文の中で書いた行（挿入・更新）の id。`DO UPDATE` が同じ行を 2 度変更するのを拒否する。
    let mut touched: HashSet<u64> = HashSet::new();
    touched.try_reserve(rows.len()).map_err(|_| reserve_err())?;
    let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
    let mut after_queue: Vec<AfterEvent> = Vec::new();
    if insert_after.is_some() || update_after.is_some() {
        after_queue
            .try_reserve_exact(rows.len())
            .map_err(|_| reserve_err())?;
    }
    let mut inserted: u64 = 0;
    let mut updated: u64 = 0;

    // 行ループの間は、トリガ本体が同じ表の行を消せない（`trigger::row_loop`。D6 の暫定ガード）。
    let row_loop_guard = crate::trigger::row_loop::RowLoopGuard::enter(ctx.tenant_id(), table);
    // BEFORE UPDATE の本体が同じ表を書き換えると、BEFORE 前に読んだ OLD から作る最終行が黙って上書きする
    // （失われた更新）。UPDATE 側の BEFORE があるときだけ、ループの間の同表 `UPDATE` を拒否させる。
    // BEFORE INSERT 側も同様に拒否する。`unique_existing` は BEFORE ROW INSERT より前に作るため、本体が
    // 既存行の UNIQUE キーを書き換えると古い対応表で衝突先を誤選択する（fail-closed）。
    let row_loop =
        (update_before.is_some() || insert_before.is_some()).then(|| RowLoopScope::enter(table));

    for (row_idx, (id, values)) in rows.iter().enumerate() {
        super::check_interrupt()?;
        let insert_id: u64 = match new_row_ids {
            Some(ids) => ids
                .get(row_idx)
                .map(|r| r.legacy_id())
                .ok_or_else(|| internal("row id allocation count mismatch"))?,
            None => *id,
        };
        // BEFORE ROW INSERT（表ハンドルを開く前に発火する。再入の不変条件）。
        let new_values: Vec<Value> = match insert_before {
            Some(plan) => {
                let fired = crate::trigger::fire::fire_before_row_insert(
                    plan.runtime,
                    writes,
                    table,
                    schema,
                    &plan.before,
                    values.to_vec(),
                )
                .map_err(TenantWriteError::TriggerFailed)?;
                match fired {
                    Some(v) => v,
                    None => continue,
                }
            }
            None => values.to_vec(),
        };
        let (embedding, metadata) = encode_final_row(schema, &new_values)?;
        // 提案行（BEFORE 適用後）への INSERT 側ポリシー検査。衝突判定より前（PostgreSQL と同順）。
        if let Some(checks) = policy_proposed.as_ref() {
            checks.enforce(schema, insert_id, &embedding, &metadata)?;
        }
        let proposed = RowInput {
            tenant_id: ctx.tenant_id(),
            visibility: insert_visibility,
            embedding: &embedding,
            metadata: &metadata,
        };
        let encoded_new = encode_row(&proposed)?;

        // 衝突判定。表ハンドルはこのブロックの中だけで開き、閉じてからトリガを呼ぶ。
        let (conflict_state, conflict_id) = {
            let row_table_name = user_rows_table_name(table);
            let row_table = write_txn
                .open_table_logged(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            if matches!(target, UpsertTarget::AnyUnique) {
                let mut probe = crate::constraint::AnyUniqueProbe::prepare(
                    write_txn,
                    table,
                    schema,
                    ctx.tenant_id(),
                    &row_table,
                )?;
                let state = if probe.conflicts_or_reserve(
                    &row_table,
                    schema,
                    ctx,
                    insert_id,
                    encoded_new.as_slice(),
                )? {
                    ConflictState::Skipped
                } else {
                    ConflictState::Absent
                };
                (state, None)
            } else {
                let conflict_id: Option<u64> = match target {
                    UpsertTarget::RowId => Some(*id),
                    UpsertTarget::AnyUnique => None,
                    UpsertTarget::Unique(indices) => {
                        crate::constraint::unique_key_from_values(indices, &new_values)
                            .map_err(|m| {
                                TenantWriteError::Catalog(CatalogError::Invalid(m.to_string()))
                            })?
                            .and_then(|key| {
                                unique_existing
                                    .as_ref()
                                    .and_then(|m| m.get(&key).copied())
                                    .map(RowId::legacy_id)
                            })
                    }
                };
                let state = match conflict_id {
                    Some(cid) => match row_table
                        .get(&row_key(ctx.tenant_id(), RowId::from_legacy_id(cid)))
                        .map_err(CatalogError::from)?
                    {
                        Some(guard) => {
                            let raw = guard.value();
                            match crate::storage::decode_row_tenant_and_visibility(raw) {
                                Ok((row_tenant, row_visibility))
                                    if ctx.is_owner(row_tenant)
                                        && ctx.is_visible(row_tenant, row_visibility) =>
                                {
                                    match action {
                                        UpsertAction::DoUpdate(..) => ConflictState::Visible(Some(
                                            crate::storage::decode_row_for_key(
                                                ctx.tenant_id(),
                                                RowId::from_legacy_id(cid),
                                                raw,
                                            )
                                            .map_err(TenantWriteError::Storage)?,
                                        )),
                                        UpsertAction::DoNothing => ConflictState::Visible(None),
                                    }
                                }
                                _ => ConflictState::Invisible,
                            }
                        }
                        None => ConflictState::Absent,
                    },
                    None => ConflictState::Absent,
                };
                (state, conflict_id)
            }
        };

        match conflict_state {
            // 対象省略で一意性に違反した行。件数・AFTER にも数えない。
            ConflictState::Skipped => {}
            ConflictState::Invisible => match action {
                UpsertAction::DoNothing => {}
                // 書き込み意図を黙って捨てず文全体を拒否する（従来経路と同じ。行・台帳とも痕跡ゼロ）。
                UpsertAction::DoUpdate(..) => {
                    return Err(TenantWriteError::ConflictTargetNotVisible);
                }
            },
            ConflictState::Visible(existing_opt) => {
                let UpsertAction::DoUpdate(assignments, condition) = action else {
                    // `DO NOTHING` の可視な衝突: BEFORE INSERT だけが呼ばれ、AFTER は呼ばない。
                    continue;
                };
                let write_id = conflict_id
                    .ok_or_else(|| internal("internal: visible conflict without a conflict id"))?;
                let existing = existing_opt.ok_or_else(|| {
                    internal("internal: visible conflict without an existing row")
                })?;
                // 同じ文の中で既に書いた行へ当たったら、2 度目の変更を拒否する（fail-closed）。
                if touched.contains(&write_id) {
                    return Err(internal(
                        "duplicate ON CONFLICT target key within the same UPSERT statement",
                    ));
                }
                // 既存行への利用者定義ポリシーの USING（所有 ∩ 可視の後、WHERE の前。従来経路と同順）。
                for checks in [
                    policy_conflict_update.as_ref(),
                    policy_conflict_select.as_ref(),
                ]
                .into_iter()
                .flatten()
                {
                    checks.enforce(schema, write_id, &existing.embedding, &existing.metadata)?;
                }
                // `DO UPDATE ... WHERE`: 偽・NULL の行は UPDATE トリガも呼ばず何も残さない。
                if let Some(condition) = condition {
                    let existing_view =
                        crate::row_codec::scan_scalar_columns(schema, &existing.metadata)
                            .map_err(|e| CatalogError::Invalid(e.to_string()))?;
                    let take = (condition.eval)(&existing_view, &new_values, write_id)
                        .map_err(TenantWriteError::SetExpressionEvaluationFailed)?;
                    if !take {
                        continue;
                    }
                }
                // SET 適用（従来経路と共有）。`EXCLUDED` は BEFORE INSERT 適用後の NEW。
                let (pre_image_values, merged_values, merged_embedding) = apply_upsert_assignments(
                    schema,
                    vector_idx,
                    assignments,
                    &new_values,
                    &existing,
                    write_id,
                )?;
                schema.validate_row_embedding_dim(merged_embedding.len())?;
                if schema.primary_key().is_some() || !schema.unique_constraints().is_empty() {
                    pre_images.record(RowId::from_legacy_id(write_id), pre_image_values);
                }
                let merged_metadata =
                    crate::row_codec::encode_scalar_columns(schema, &merged_values)
                        .map_err(|e| CatalogError::Invalid(e.to_string()))?;
                let old_values = if update_before.is_some() || update_after.is_some() {
                    captured_values_from_parts(
                        schema,
                        existing.embedding.clone(),
                        &existing.metadata,
                    )?
                } else {
                    Vec::new()
                };
                let mut new_candidate: Vec<Value> = Vec::new();
                if update_before.is_some() || update_after.is_some() {
                    new_candidate = captured_values_from_parts(
                        schema,
                        merged_embedding.clone(),
                        &merged_metadata,
                    )?;
                }
                // BEFORE ROW UPDATE（OLD = 既存行、NEW = SET 適用後。表ハンドルを開く前に発火する）。
                let (final_embedding, final_metadata, final_row) = match update_before {
                    Some(plan) => {
                        let fired = crate::trigger::fire::fire_before_row_update(
                            plan.runtime,
                            writes,
                            table,
                            schema,
                            &plan.before,
                            &old_values,
                            new_candidate,
                        )
                        .map_err(TenantWriteError::TriggerFailed)?;
                        let Some(final_row) = fired else {
                            continue;
                        };
                        let (e, m) = encode_final_row(schema, &final_row)?;
                        (e, m, final_row)
                    }
                    None => (merged_embedding, merged_metadata, new_candidate),
                };
                let row = RowInput {
                    tenant_id: ctx.tenant_id(),
                    // 既存行の可視性を保持する（SET で触れない列と同じ扱い）。
                    visibility: existing.visibility,
                    embedding: &final_embedding,
                    metadata: &final_metadata,
                };
                let encoded = encode_row(&row)?;
                {
                    let row_table_name = user_rows_table_name(table);
                    let mut row_table = write_txn
                        .open_table_logged(user_rows_table_def(&row_table_name))
                        .map_err(map_row_table_error)?;
                    row_table
                        .insert(
                            row_key(ctx.tenant_id(), RowId::from_legacy_id(write_id)),
                            encoded.as_slice(),
                        )
                        .map_err(CatalogError::from)?;
                    // commit 記録の行の足跡（SQL-38・Issue #2404）。可視性は更新前後で不変。
                    crate::storage::commit_log::note_row_bytes(
                        write_txn,
                        table,
                        encoded.as_slice(),
                    );
                }
                updated = updated
                    .checked_add(1)
                    .ok_or_else(|| internal("upsert updated row counter overflow"))?;
                written_ids.push(write_id);
                updated_write_ids.push(write_id);
                touched.insert(write_id);
                if update_after.is_some() {
                    after_queue.push(AfterEvent::Update(old_values, final_row));
                }
            }
            ConflictState::Absent => {
                {
                    let row_table_name = user_rows_table_name(table);
                    let mut row_table = write_txn
                        .open_table_logged(user_rows_table_def(&row_table_name))
                        .map_err(map_row_table_error)?;
                    insert_unique_row(
                        write_txn,
                        table,
                        &mut row_table,
                        row_key(ctx.tenant_id(), RowId::from_legacy_id(insert_id)),
                        encoded_new.as_slice(),
                    )?;
                }
                if per_row_constraints {
                    // 対象省略: 永続一意索引を行ごとに同期する（FK は全行の後に 1 回）。
                    crate::constraint::enforce_row_constraints_in_txn(
                        write_txn,
                        table,
                        schema,
                        ctx.tenant_id(),
                        &[RowId::from_legacy_id(insert_id)],
                        crate::constraint::FkCheckMode::Skip,
                        crate::constraint::RowPolicyWrite::Insert,
                    )?;
                }
                inserted = inserted
                    .checked_add(1)
                    .ok_or_else(|| internal("upsert inserted row counter overflow"))?;
                written_ids.push(insert_id);
                touched.insert(insert_id);
                if insert_after.is_some() {
                    after_queue.push(AfterEvent::Insert(new_values));
                }
            }
        }
    }
    drop(row_loop_guard);
    // 外側の書き込みが済んだため、以降（制約検査・AFTER）のトリガ本体の同表 `UPDATE` は許す。
    drop(row_loop);

    if !written_ids.is_empty() {
        let written_rowids: Vec<RowId> = written_ids
            .iter()
            .map(|&i| RowId::from_legacy_id(i))
            .collect();
        if per_row_constraints {
            crate::constraint::enforce_foreign_keys_for_rows_in_txn(
                write_txn,
                table,
                schema,
                ctx.tenant_id(),
                &written_rowids,
                write_target.fk_check_mode(),
            )?;
        } else {
            let updated_rowids: Vec<RowId> = updated_write_ids
                .iter()
                .map(|&i| RowId::from_legacy_id(i))
                .collect();
            let delta = crate::constraint::enforce_row_constraints_in_txn(
                write_txn,
                table,
                schema,
                ctx.tenant_id(),
                &written_rowids,
                write_target.fk_check_mode(),
                crate::constraint::RowPolicyWrite::Upsert {
                    updated_ids: &updated_rowids,
                },
            )?;
            if updated > 0 {
                if let UpsertAction::DoUpdate(assignments, _) = action {
                    // BEFORE ROW UPDATE が SET に無い主キー・UNIQUE 列を書き換えうるため、UPDATE 側の
                    // BEFORE 行トリガがあれば全列を更新済みとして扱う（fail-closed）。
                    let updated_columns: Vec<usize> = if update_before.is_some() {
                        (0..schema.columns.len()).collect()
                    } else {
                        assignments.iter().map(|(idx, _)| *idx).collect()
                    };
                    crate::constraint::enforce_referencing_rows_in_txn(
                        write_txn,
                        table,
                        schema,
                        ctx.tenant_id(),
                        crate::constraint::ReferencedRowsChange::ColumnsUpdated {
                            indices: &updated_columns,
                            delta: &delta,
                        },
                        Some(&pre_images),
                        write_target.fk_check_mode(),
                    )?;
                }
            }
        }
    }

    // AFTER ROW（段 6）。変更の順 × トリガ名のバイト順。
    for event in &after_queue {
        match (event, insert_after, update_after) {
            (AfterEvent::Insert(new_row), Some(plan), _) => {
                crate::trigger::fire::fire_after_row_insert(
                    plan.runtime,
                    writes,
                    table,
                    schema,
                    &plan.after,
                    new_row,
                )
                .map_err(TenantWriteError::TriggerFailed)?;
            }
            (AfterEvent::Update(old, new_row), _, Some(plan)) => {
                crate::trigger::fire::fire_after_row(
                    plan.runtime,
                    writes,
                    table,
                    schema,
                    &plan.after,
                    TriggerOp::Update,
                    Some(old),
                    new_row,
                )
                .map_err(TenantWriteError::TriggerFailed)?;
            }
            _ => {}
        }
    }
    // AFTER STATEMENT（段 7）。対象 0 件・全行スキップでも 1 回ずつ。`DO UPDATE` は UPDATE → INSERT の順。
    if let Some(plan) = update_plan {
        crate::trigger::fire::fire_statement(
            plan.runtime,
            writes,
            table,
            schema,
            &plan.after_stmt,
            TriggerOp::Update,
            crate::catalog::trigger::TriggerTiming::After,
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    if let Some(plan) = insert_plan {
        crate::trigger::fire::fire_statement(
            plan.runtime,
            writes,
            table,
            schema,
            &plan.after_stmt,
            TriggerOp::Insert,
            crate::catalog::trigger::TriggerTiming::After,
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    settle_trigger_written_tables(write_txn, write_target, ctx, writes)?;
    Ok(TriggeredUpsert {
        outcome: UpsertOutcome { inserted, updated },
    })
}
