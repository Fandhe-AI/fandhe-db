//! BEFORE・AFTER ROW UPDATE／DELETE トリガ付きの行書き込み（TABLE-45・Issue #1725）。
//!
//! `tenant.rs` の述語形・結合形・id 指定形の UPDATE／DELETE が、スキーマ取得・台帳照合・候補の列挙・
//! 上限判定までを従来どおり済ませた後、**該当イベントのトリガがある表に限って**適用段をこのモジュールへ
//! 差し替える。トリガの無い表は従来の適用段のままでバイト不変。従来の適用段は候補ループの間ずっと
//! 行ストアの可変ハンドルを開いているため、BEFORE を「redb の表ハンドルを 1 つも開かずに」呼ぶ契約
//! （実行モデルの D5）を満たせず、別の行ごとループへ差し替える形を採る。
//!
//! 行ごとの順序（PostgreSQL と同じ: BEFORE → 書き込み。制約検査は全行の後）:
//! 1. 割り込み（`57014`）を確認し、行ストアを読んで OLD を得る（ハンドルを閉じる）
//! 2. UPDATE は `SET` を適用した NEW の候補を作る。表ハンドルを開かずに BEFORE を名前のバイト順に連鎖し、
//!    `NULL` 返却ならその行は書かない（影響行数にも AFTER にも数えない）。DELETE は `NULL` で削除をやめる
//! 3. UPDATE は最終 NEW を検査・encode して書く（可視性・行キーは既存行のまま）。DELETE は行を消す
//! 4. 全行の後に制約検査・FK（参照元側・参照先側）を 1 回ずつ行い、そのあと AFTER ROW を
//!    変更の順 × トリガ名のバイト順で発火する（戻り値は無視。実行モデルの D3）
//!
//! BEFORE が主キー・UNIQUE 列を書き換えうるため、BEFORE UPDATE トリガがある表では参照先側の FK 検査を
//! 全列を更新済みとして行う（fail-closed）。AFTER の失敗は文全体の失敗で、呼び出し元の txn が commit
//! されないため行・トリガの書いた行・台帳・索引はいずれも残らない。トリガが書いた表の遅延 FK は
//! [`super::trigger_insert`] と同じ規則で扱う。エラーは `TenantWriteError`（トリガ由来は `TriggerFailed`）。

use redb::ReadableTable;

use super::trigger_insert::settle_trigger_written_tables;
use super::{
    captured_values_from_parts, encode_row, merge_row_for_update_set, resolve_update_set_values,
    row_key, statement_writes, user_rows_table_def, user_rows_table_name, RowId, TenantWriteError,
    WriteTarget,
};
use crate::catalog::{map_row_table_error, CatalogError, ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::storage::RowInput;
use crate::trigger::call::TriggerOp;
use crate::trigger::plan::RowTriggerPlan;
use crate::trigger::write_handle::TriggerWriteHandle;

/// トリガが返した行を行 codec が受理しなかったときの固定文言（値・列名を含めない）。
const ROW_ENCODE_MISMATCH: &str = "trigger function returned a row that does not match the table";

/// 適用段の結果。
pub(super) struct TriggeredDml {
    /// 実際に変更した行数（BEFORE の `NULL` 返却でスキップした行は含まない）。
    pub(super) applied: usize,
    /// トリガ本体が別表へ書き込んだか。全行スキップでも別表への書き込みがあれば txn を commit 対象にする。
    pub(super) trigger_wrote: bool,
}

/// BEFORE STATEMENT（段 2。Issue #1726）を発火する。対象の列挙より前・redb の表ハンドルを開く前に、
/// 文につき 1 回、対象が 0 件でも呼ぶ（呼び出し元が台帳の記録の後に呼ぶ。再送ではトリガを呼ばない）。
pub(super) fn fire_before_statement(
    plan: &RowTriggerPlan<'_>,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    op: TriggerOp,
) -> Result<(), TenantWriteError> {
    crate::trigger::fire::fire_statement(
        plan.runtime,
        writes,
        table,
        schema,
        &plan.before_stmt,
        op,
        crate::catalog::trigger::TriggerTiming::Before,
    )
    .map_err(TenantWriteError::TriggerFailed)
}

/// UPDATE の `SET` 入力（述語形・結合形・id 指定形が共有する）。
pub(super) struct UpdateSpec<'a> {
    pub(super) assignments: &'a [(usize, Value)],
    pub(super) set_exprs: &'a [(usize, crate::sql::set_expr::BoundSetExpr)],
    /// 結合形 `UPDATE ... FROM` の行ごとの SET 値（`candidate_ids` と同じ並び・同じ長さ）。
    pub(super) row_overrides: Option<&'a [Vec<(usize, Value)>]>,
}

fn reserve_err() -> TenantWriteError {
    TenantWriteError::Storage(crate::storage::StorageError::Codec(
        "failed to reserve trigger dml batch".to_string(),
    ))
}

fn internal(msg: &str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(msg.to_string()))
}

/// 行ストアから 1 行を読み、全列値（`schema.columns` の幅・順。VECTOR は埋め込みで置換）と既存行を返す。
/// 表ハンドルは戻る前に閉じる（BEFORE を呼ぶ前の不変条件）。
fn read_row(
    write_txn: &redb::WriteTransaction,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
) -> Result<crate::storage::Row, TenantWriteError> {
    let row_table_name = user_rows_table_name(table);
    let row_table = write_txn
        .open_table(user_rows_table_def(&row_table_name))
        .map_err(map_row_table_error)?;
    let key = row_key(ctx.tenant_id(), RowId::from_legacy_id(id));
    let guard = row_table
        .get(&key)
        .map_err(CatalogError::from)?
        .ok_or_else(|| internal("internal: candidate row disappeared before apply"))?;
    let row = crate::storage::decode_row_for_key(
        ctx.tenant_id(),
        RowId::from_legacy_id(id),
        guard.value(),
    )?;
    Ok(row)
}

/// BEFORE が返した最終 NEW を検査して `(embedding, metadata)` へ encode する（NOT NULL・VECTOR 次元・
/// 行 codec）。
fn encode_final_row(
    schema: &TableSchema,
    final_row: &[Value],
) -> Result<(Vec<f32>, Vec<u8>), TenantWriteError> {
    for (col, value) in schema.columns.iter().zip(final_row.iter()) {
        if !col.nullable && matches!(value, Value::Null) {
            return Err(TenantWriteError::Catalog(
                CatalogError::NotNullConstraintViolation(col.name.clone()),
            ));
        }
    }
    let vector_idx = schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, ColumnType::Vector(_)));
    let embedding: Vec<f32> = match vector_idx {
        Some(idx) => match final_row.get(idx) {
            Some(Value::Vector(v)) => v.clone(),
            // NULL の VECTOR 列は埋め込みなし（行ストアの既存契約）。NOT NULL は上で拒否済み。
            Some(Value::Null) => Vec::new(),
            _ => {
                return Err(internal("VECTOR column value missing or not a Vector"));
            }
        },
        None => Vec::new(),
    };
    if !embedding.is_empty() {
        schema.validate_row_embedding_dim(embedding.len())?;
    }
    let metadata = crate::row_codec::encode_scalar_columns(schema, final_row).map_err(|_| {
        TenantWriteError::TriggerFailed(SqlSurfaceError::DatatypeMismatch {
            detail: ROW_ENCODE_MISMATCH.to_string(),
        })
    })?;
    Ok((embedding, metadata))
}

/// `candidate_ids`（列挙済みの対象行）を、BEFORE UPDATE トリガを通して更新し、全行の後に制約検査・FK
/// 検査・AFTER UPDATE トリガを行う。書き込み後の世代 bump・commit は呼び出し元の責務。
#[allow(clippy::too_many_arguments)]
pub(super) fn update_rows_with_triggers(
    write_txn: &redb::WriteTransaction,
    target: &WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    schema: &TableSchema,
    candidate_ids: &[u64],
    spec: &UpdateSpec<'_>,
    plan: &RowTriggerPlan<'_>,
    writes: &TriggerWriteHandle<'_>,
) -> Result<TriggeredDml, TenantWriteError> {
    // 対象行が 0 件でも `row_security = off` の拒否を文の開始時点で適用する（RLS-13・Issue #2278）。
    crate::sql::policy_eval::reject_row_security_off_in_txn(write_txn, ctx.tenant_id(), table)?;
    if spec
        .row_overrides
        .is_some_and(|o| o.len() != candidate_ids.len())
    {
        return Err(internal(
            "internal: per-row SET values do not match the candidate rows",
        ));
    }
    let track_pre_images =
        schema.primary_key().is_some() || !schema.unique_constraints().is_empty();
    let mut pre_images = crate::constraint::UpdatedKeyPreImages::new();
    let mut written: Vec<RowId> = Vec::new();
    written
        .try_reserve_exact(candidate_ids.len())
        .map_err(|_| reserve_err())?;
    let mut after_queue: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    if !plan.after.is_empty() {
        after_queue
            .try_reserve_exact(candidate_ids.len())
            .map_err(|_| reserve_err())?;
    }
    for (pos, id) in candidate_ids.iter().enumerate() {
        crate::interrupt::check().map_err(TenantWriteError::TriggerFailed)?;
        let row_extra: &[(usize, Value)] = match spec.row_overrides {
            Some(o) => o.get(pos).map(Vec::as_slice).ok_or_else(|| {
                internal("internal: per-row SET values missing for a candidate row")
            })?,
            None => &[],
        };
        let existing = read_row(write_txn, table, ctx, *id)?;
        let visibility = existing.visibility;
        let old_values =
            captured_values_from_parts(schema, existing.embedding.clone(), &existing.metadata)?;
        // BEFORE トリガがあるときは、NEW 候補を値の列として作るだけで制約検査・encode をしない
        // （BEFORE が NULL 等を補正・Skip できるよう、検査は BEFORE の連鎖後の最終行に対して行う）。
        let (embedding, metadata, final_row) = if plan.before.is_empty() {
            let (merged_embedding, merged_metadata) = merge_row_for_update_set(
                schema,
                existing,
                *id,
                spec.assignments,
                spec.set_exprs,
                row_extra,
            )?;
            let new_candidate =
                captured_values_from_parts(schema, merged_embedding.clone(), &merged_metadata)?;
            (merged_embedding, merged_metadata, new_candidate)
        } else {
            let resolved = resolve_update_set_values(
                schema,
                &existing,
                *id,
                spec.assignments,
                spec.set_exprs,
                row_extra,
                true,
            )?;
            let mut new_candidate = old_values.clone();
            let mut seen: Vec<usize> = Vec::with_capacity(resolved.len());
            for (idx, value) in resolved {
                // 重複指定は先勝ち（merge_encode_scalar_columns と同じ意味論）。
                if seen.contains(&idx) {
                    continue;
                }
                seen.push(idx);
                let slot = new_candidate
                    .get_mut(idx)
                    .ok_or_else(|| internal("internal: SET column index out of range"))?;
                *slot = value;
            }
            // 表ハンドルを開く前に発火する（再入の不変条件）。
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
            let (embedding, metadata) = encode_final_row(schema, &final_row)?;
            (embedding, metadata, final_row)
        };
        let row = RowInput {
            tenant_id: ctx.tenant_id(),
            // 既存行の可視性を保持する（SET で触れない列と同じ扱い）。
            visibility,
            embedding: &embedding,
            metadata: &metadata,
        };
        let encoded = encode_row(&row)?;
        {
            let row_table_name = user_rows_table_name(table);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            let key = row_key(ctx.tenant_id(), RowId::from_legacy_id(*id));
            row_table
                .insert(key, encoded.as_slice())
                .map_err(CatalogError::from)?;
            // commit 記録の行の足跡（SQL-38・Issue #2404）。可視性は更新前後で不変（上の
            // `visibility`）のため、更新後の行 1 つで前後を表す。
            crate::storage::commit_log::note_row_bytes(write_txn, table, encoded.as_slice());
        }
        statement_writes::record(table, *id);
        if track_pre_images {
            pre_images.record(RowId::from_legacy_id(*id), old_values.clone());
        }
        written.push(RowId::from_legacy_id(*id));
        if !plan.after.is_empty() {
            after_queue.push((old_values, final_row));
        }
    }
    if !written.is_empty() {
        let delta = crate::constraint::enforce_row_constraints_in_txn(
            write_txn,
            table,
            schema,
            ctx.tenant_id(),
            &written,
            target.fk_check_mode(),
            crate::constraint::RowPolicyWrite::Update,
        )?;
        // BEFORE ROW が SET に無い主キー・UNIQUE 列を書き換えうるため、BEFORE UPDATE の行トリガが
        // あれば全列を更新済みとして扱う（fail-closed）。文トリガは NEW を書き換えられないので、
        // 無ければ SET の列だけ。
        let updated_columns: Vec<usize> = if plan.before.is_empty() {
            let mut cols = update_set_columns(spec);
            cols.sort_unstable();
            cols.dedup();
            cols
        } else {
            (0..schema.columns.len()).collect()
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
            target.fk_check_mode(),
        )?;
    }
    // AFTER ROW（段 6）。変更の順 × トリガ名のバイト順。
    for (old, new) in &after_queue {
        crate::trigger::fire::fire_after_row(
            plan.runtime,
            writes,
            table,
            schema,
            &plan.after,
            TriggerOp::Update,
            Some(old),
            new,
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    // AFTER STATEMENT（段 7。Issue #1726）。対象 0 件・全行スキップでも 1 回発火する。
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
    settle_trigger_written_tables(write_txn, target, ctx, writes)?;
    Ok(TriggeredDml {
        applied: written.len(),
        trigger_wrote: !writes.written_tables().is_empty(),
    })
}

/// `SET` の対象列 index（リテラル項・式項・行ごとの上書き値。`UPDATE OF` の判定と FK 検査の入力）。
pub(super) fn update_set_columns(spec: &UpdateSpec<'_>) -> Vec<usize> {
    spec.assignments
        .iter()
        .map(|(idx, _)| *idx)
        .chain(spec.set_exprs.iter().map(|(idx, _)| *idx))
        .chain(
            spec.row_overrides
                .into_iter()
                .flatten()
                .flat_map(|row| row.iter().map(|(idx, _)| *idx)),
        )
        .collect()
}

/// `candidate_ids`（列挙済みの対象行）を、BEFORE DELETE トリガを通して削除し、全行の後に索引の後片付け・
/// 参照先 FK 検査・AFTER DELETE トリガを行う。世代 bump・commit は呼び出し元の責務。
#[allow(clippy::too_many_arguments)]
pub(super) fn delete_rows_with_triggers(
    write_txn: &redb::WriteTransaction,
    target: &WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    schema: &TableSchema,
    candidate_ids: &[u64],
    plan: &RowTriggerPlan<'_>,
    writes: &TriggerWriteHandle<'_>,
) -> Result<TriggeredDml, TenantWriteError> {
    // `delete_row_impl` と同じ判定条件（`id` 参照 FK を含む）で、参照アクションの起点の旧値を残す。
    let needs_fk_removed_pre_image = schema.primary_key().is_some()
        || !schema.unique_constraints().is_empty()
        || crate::catalog::referencing_foreign_keys_in_txn(write_txn, table)?
            .iter()
            .any(|(_, fk)| fk.references_parent_id());
    let mut removed_pre_images = crate::constraint::UpdatedKeyPreImages::new();
    let mut removed: Vec<RowId> = Vec::new();
    removed
        .try_reserve_exact(candidate_ids.len())
        .map_err(|_| reserve_err())?;
    let mut after_queue: Vec<Vec<Value>> = Vec::new();
    if !plan.after.is_empty() {
        after_queue
            .try_reserve_exact(candidate_ids.len())
            .map_err(|_| reserve_err())?;
    }
    for id in candidate_ids {
        crate::interrupt::check().map_err(TenantWriteError::TriggerFailed)?;
        let existing = read_row(write_txn, table, ctx, *id)?;
        let old_values =
            captured_values_from_parts(schema, existing.embedding, &existing.metadata)?;
        if !plan.before.is_empty() {
            let proceed = crate::trigger::fire::fire_before_row_delete(
                plan.runtime,
                writes,
                table,
                schema,
                &plan.before,
                &old_values,
            )
            .map_err(TenantWriteError::TriggerFailed)?;
            if !proceed {
                continue;
            }
        }
        {
            let row_table_name = user_rows_table_name(table);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            let key = row_key(ctx.tenant_id(), RowId::from_legacy_id(*id));
            // commit 記録の行の足跡: 削除される前の行（SQL-38・Issue #2404）。
            let removed = row_table.remove(&key).map_err(CatalogError::from)?;
            if let Some(removed) = removed.as_ref() {
                crate::storage::commit_log::note_row_bytes(write_txn, table, removed.value());
            }
        }
        statement_writes::record(table, *id);
        if needs_fk_removed_pre_image {
            removed_pre_images.record(RowId::from_legacy_id(*id), old_values.clone());
        }
        removed.push(RowId::from_legacy_id(*id));
        if !plan.after.is_empty() {
            after_queue.push(old_values);
        }
    }
    if !removed.is_empty() {
        crate::constraint::forget_unique_index_rows_in_txn(
            write_txn,
            table,
            schema,
            ctx.tenant_id(),
            &removed,
        )?;
        crate::constraint::forget_gin_index_rows_in_txn(
            write_txn,
            table,
            ctx.tenant_id(),
            &removed,
        )?;
        crate::constraint::enforce_referencing_rows_in_txn(
            write_txn,
            table,
            schema,
            ctx.tenant_id(),
            crate::constraint::ReferencedRowsChange::Removed { ids: &removed },
            needs_fk_removed_pre_image.then_some(&removed_pre_images),
            target.fk_check_mode(),
        )?;
    }
    for old in &after_queue {
        crate::trigger::fire::fire_after_row(
            plan.runtime,
            writes,
            table,
            schema,
            &plan.after,
            TriggerOp::Delete,
            Some(old),
            &[],
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    // AFTER STATEMENT（段 7。Issue #1726）。対象 0 件・全行スキップでも 1 回発火する。
    crate::trigger::fire::fire_statement(
        plan.runtime,
        writes,
        table,
        schema,
        &plan.after_stmt,
        TriggerOp::Delete,
        crate::catalog::trigger::TriggerTiming::After,
    )
    .map_err(TenantWriteError::TriggerFailed)?;
    settle_trigger_written_tables(write_txn, target, ctx, writes)?;
    Ok(TriggeredDml {
        applied: removed.len(),
        trigger_wrote: !writes.written_tables().is_empty(),
    })
}

/// id 指定形（`WHERE id = <n>`）の対象判定。所有かつ可視（`owned_visible`）または所有（そうでなければ）で、
/// 同じ文の先の副文が書いた行でないときだけ `true`。内容（本体）には触れない（ヘッダのみ。RLS-9・RLS-10:
/// 不存在・他テナント・不可視を区別しない）。
///
/// `policy`（RLS-13・Issue #2276）が `Some` なら、所有・可視と確認できた行に限り本体を復号して利用者
/// ポリシーの USING を評価し、通らない行は対象にしない（0 件・エラーなし）。
pub(super) fn id_is_target(
    write_txn: &redb::WriteTransaction,
    table: &str,
    ctx: &PolicyContext,
    id: u64,
    owned_visible: bool,
    schema: &crate::catalog::TableSchema,
    policy: Option<&crate::sql::row_policy::BoundRowPolicy>,
) -> Result<bool, TenantWriteError> {
    let row_table_name = user_rows_table_name(table);
    let row_table = write_txn
        .open_table(user_rows_table_def(&row_table_name))
        .map_err(map_row_table_error)?;
    let key = row_key(ctx.tenant_id(), RowId::from_legacy_id(id));
    let found = match row_table.get(&key).map_err(CatalogError::from)? {
        Some(_) if statement_writes::is_prior(table, id) => false,
        Some(guard) => match crate::storage::decode_row_tenant_and_visibility(guard.value()) {
            Ok((tenant_id, visibility)) => {
                let base = ctx.is_owner(tenant_id)
                    && (!owned_visible || ctx.is_visible(tenant_id, visibility));
                match (base, policy) {
                    (true, Some(policy)) => {
                        let row = crate::storage::decode_row_for_key(
                            ctx.tenant_id(),
                            RowId::from_legacy_id(id),
                            guard.value(),
                        )
                        .map_err(TenantWriteError::from)?;
                        super::row_passes_policy(policy, schema, &row)?
                    }
                    (base, _) => base,
                }
            }
            // ヘッダのデコード失敗は不可視と同じ扱い（本体には触れない）。
            Err(_) => false,
        },
        None => false,
    };
    Ok(found)
}
