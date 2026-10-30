//! BEFORE・AFTER ROW INSERT トリガ付きの行書き込み（TABLE-45・Issue #1723・#1724）。
//!
//! `tenant::insert_typed_row_impl`／`insert_typed_rows_unchecked` が、台帳照合（再送判定は書き換え前の値の
//! ハッシュで行う）・SEQUENCE／`id`／rowid の払い出しまでを従来どおり済ませた後、トリガのある表に限って
//! 行を書くループだけをこのモジュールへ差し替える。トリガの無い表は従来経路のままでバイト不変。
//!
//! 行ごとの順序（PostgreSQL と同じ: BEFORE → 制約検査）:
//! 1. redb の表ハンドルを 1 つも開かずにトリガを連鎖実行する（トリガ本体が同じ write txn へ再入しても
//!    排他を壊さない。実行モデルの D5）。`NULL` 返却ならその行は書かず次の行へ
//! 2. 最終 NEW の NOT NULL 検査（`23502`。BEFORE が補えるよう、従来の事前検査〔省略列の
//!    `MissingNotNull`〕はトリガ経路では遅らせてここで行う）
//! 3. VECTOR 列の次元検査 → 行 codec の encode → 行ストアへ 1 行書く
//! 4. その 1 行の制約検査（索引同期・CHECK・式索引・部分索引・UNIQUE・GIN・ビュー。FK は除く）
//!
//! 全行の後に、書いた行すべての FK を 1 回検査する（同じ文の後の行を参照する行を誤って `23503` にしない）。
//!
//! その後（実行モデルの D4 段 6）に、書いた行（最終 NEW）ごと・トリガ名のバイト順で AFTER ROW を
//! 発火する（Issue #1724）。この時点で redb の表ハンドルは 1 つも開いておらず（行ストアのハンドルは
//! 各行のブロックで閉じ済み）、トリガ本体は [`TriggerWriteHandle`] 経由で同じ write txn へ再入して
//! 書ける。AFTER の失敗は文全体の失敗で、呼び出し元の txn が commit されないため行・トリガの書いた行・
//! 台帳・索引はいずれも残らない。トリガが書いた表の遅延 FK は、`Autocommit` では文の終わりに検査し、
//! `InTxn` では COMMIT 時の検査対象にする（[`super::trigger_writes`]）。
//! 呼び出し元は `tenant.rs` のみ。エラーは `TenantWriteError`（トリガ由来は `TriggerFailed`）。

use super::{
    encode_row, insert_unique_row, row_key, user_rows_table_def, user_rows_table_name, RowId,
    TenantWriteError, WriteTarget,
};
use crate::catalog::{map_row_table_error, CatalogError, ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::storage::{RowInput, Visibility};
use crate::trigger::plan::InsertTriggerPlan;
use crate::trigger::write_handle::TriggerWriteHandle;

/// トリガが返した行を行 codec が受理しなかったときの固定文言（値・列名を含めない）。
const ROW_ENCODE_MISMATCH: &str = "trigger function returned a row that does not match the table";

/// `rows`（払い出し済みの行キーと、DEFAULT・SEQUENCE 確定後の NEW）を、BEFORE ROW トリガを通して
/// 書き、全行の後に AFTER ROW トリガを発火する。書いた行の行キーを返す（`NULL` 返却でスキップした行は
/// 含まない）。あわせて、トリガ本体が別表へ書き込んだか（BEFORE・AFTER を問わない）を返す。
/// 全行スキップでも別表への書き込みがあれば、呼び出し元は txn を commit 対象にする。
#[allow(clippy::too_many_arguments)]
pub(super) fn write_rows_with_triggers(
    write_txn: &redb::WriteTransaction,
    target: &WriteTarget<'_>,
    table: &str,
    ctx: &PolicyContext,
    schema: &TableSchema,
    visibility: Visibility,
    rows: &[(RowId, &[Value])],
    plan: &InsertTriggerPlan<'_>,
) -> Result<(Vec<RowId>, bool), TenantWriteError> {
    let vector_idx = schema
        .columns
        .iter()
        .position(|c| matches!(c.ty, ColumnType::Vector(_)));
    let mut written: Vec<RowId> = Vec::new();
    written.try_reserve_exact(rows.len()).map_err(|_| {
        TenantWriteError::Storage(crate::storage::StorageError::Codec(
            "failed to reserve trigger insert batch".to_string(),
        ))
    })?;
    // AFTER ROW に渡す最終 NEW（書いた順）。確保は行数（呼び出し元がバッチ上限で検証済み）に抑える。
    let mut after_queue: Vec<Vec<Value>> = Vec::new();
    if !plan.after.is_empty() {
        after_queue.try_reserve_exact(rows.len()).map_err(|_| {
            TenantWriteError::Storage(crate::storage::StorageError::Codec(
                "failed to reserve trigger insert batch".to_string(),
            ))
        })?;
    }
    let writes = TriggerWriteHandle::new(write_txn, ctx);
    for (row_id, values) in rows {
        crate::interrupt::check().map_err(TenantWriteError::TriggerFailed)?;
        // 表ハンドルを開く前に発火する（再入の不変条件）。
        let fired = crate::trigger::fire::fire_before_row_insert(
            plan.runtime,
            &writes,
            table,
            schema,
            &plan.before,
            values.to_vec(),
        )
        .map_err(TenantWriteError::TriggerFailed)?;
        let Some(final_row) = fired else {
            continue;
        };
        for (col, value) in schema.columns.iter().zip(final_row.iter()) {
            if !col.nullable && matches!(value, Value::Null) {
                return Err(TenantWriteError::Catalog(
                    CatalogError::NotNullConstraintViolation(col.name.clone()),
                ));
            }
        }
        let embedding: &[f32] = match vector_idx {
            Some(idx) => match final_row.get(idx) {
                Some(Value::Vector(v)) => v.as_slice(),
                _ => {
                    return Err(TenantWriteError::Catalog(CatalogError::Invalid(
                        "VECTOR column value missing or not a Vector".to_string(),
                    )))
                }
            },
            None => &[],
        };
        schema.validate_row_embedding_dim(embedding.len())?;
        let metadata =
            crate::row_codec::encode_scalar_columns(schema, &final_row).map_err(|_| {
                TenantWriteError::TriggerFailed(SqlSurfaceError::DatatypeMismatch {
                    detail: ROW_ENCODE_MISMATCH.to_string(),
                })
            })?;
        let row = RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding,
            metadata: &metadata,
        };
        let key = row_key(ctx.tenant_id(), *row_id);
        let encoded = encode_row(&row)?;
        {
            let row_table_name = user_rows_table_name(table);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            insert_unique_row(table, &mut row_table, key, encoded.as_slice())?;
        }
        // この 1 行の制約検査（FK は全行の後に 1 回）。
        crate::constraint::enforce_row_constraints_in_txn(
            write_txn,
            table,
            schema,
            ctx.tenant_id(),
            &[*row_id],
            crate::constraint::FkCheckMode::Skip,
        )?;
        written.push(*row_id);
        if !plan.after.is_empty() {
            after_queue.push(final_row);
        }
    }
    if !written.is_empty() {
        crate::constraint::enforce_foreign_keys_for_rows_in_txn(
            write_txn,
            table,
            schema,
            ctx.tenant_id(),
            &written,
            target.fk_check_mode(),
        )?;
    }
    // AFTER ROW（段 6）。変更の順（書いた順）× トリガ名のバイト順。
    for new_row in &after_queue {
        crate::trigger::fire::fire_after_row_insert(
            plan.runtime,
            &writes,
            table,
            schema,
            &plan.after,
            new_row,
        )
        .map_err(TenantWriteError::TriggerFailed)?;
    }
    settle_trigger_written_tables(write_txn, target, ctx, &writes)?;
    let trigger_wrote = !writes.written_tables().is_empty();
    Ok((written, trigger_wrote))
}

/// トリガが書いた表の遅延 FK の扱いを決める（fail-open の防止）。トリガ内の書き込みは `InTxn` 経由の
/// ため文単位検査から遅延 FK が外れる。`Autocommit` は COMMIT 時検査が無いのでここで検査し、`InTxn` は
/// 記録区間があれば `mark_written` 経由で COMMIT 時検査へ載せ、区間が無ければここで即時検査する。
pub(super) fn settle_trigger_written_tables(
    write_txn: &redb::WriteTransaction,
    target: &WriteTarget<'_>,
    ctx: &PolicyContext,
    writes: &TriggerWriteHandle<'_>,
) -> Result<(), TenantWriteError> {
    let tables = writes.written_tables();
    if tables.is_empty() {
        return Ok(());
    }
    if matches!(target, WriteTarget::InTxn(_)) && super::trigger_writes::record(&tables) {
        return Ok(());
    }
    for t in &tables {
        crate::constraint::enforce_deferred_foreign_keys_in_txn(write_txn, ctx.tenant_id(), t)?;
    }
    Ok(())
}
