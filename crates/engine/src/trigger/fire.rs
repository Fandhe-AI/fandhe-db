//! BEFORE ROW トリガの連鎖実行（TABLE-45・Issue #1723）。
//!
//! `tenant/trigger_insert.rs` が行ごとに呼ぶ。カタログ順（名前のバイト順）に各トリガの関数を
//! [`TriggerFunctionInvoker`](super::call::TriggerFunctionInvoker) 経由で呼び、前のトリガが返した行を
//! 次のトリガの NEW とする。`NULL` 返却（[`TriggerOutcome::Skip`]）はその行の残りのトリガを呼ばずに終える。
//! 呼び出し元は redb の表ハンドルを 1 つも開かずに呼ぶこと（トリガ本体が同じ write txn へ再入しても
//! 排他を壊さない。実行モデルの D5）。

use super::call::{TriggerCall, TriggerOutcome};
use super::runtime::TriggerRuntime;
use crate::catalog::trigger::TriggerDef;
use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;

/// 返却行の構造違いの固定文言（値・列名・行を含めない）。
const RETURNED_ROW_MISMATCH: &str = "trigger function returned a row that does not match the table";

/// 発火時にも `WHEN` 条件を実行できないことの固定文言（定義時に拒否済みだが fail-closed に二重化する）。
const WHEN_UNSUPPORTED: &str = "trigger WHEN condition is not supported";

fn returned_row_mismatch() -> SqlSurfaceError {
    SqlSurfaceError::DatatypeMismatch {
        detail: RETURNED_ROW_MISMATCH.to_string(),
    }
}

/// トリガ関数が返した行の構造（列数・VECTOR 列の形）を検査する。型の細部（範囲・長さ）は
/// 書き込み時の行 codec が検査する。
pub(crate) fn check_returned_row(
    schema: &TableSchema,
    row: &[Value],
) -> Result<(), SqlSurfaceError> {
    if row.len() != schema.columns.len() {
        return Err(returned_row_mismatch());
    }
    for (col, value) in schema.columns.iter().zip(row) {
        match (&col.ty, value) {
            (ColumnType::Vector(dim), Value::Vector(v)) => {
                let expected = usize::try_from(*dim).map_err(|_| returned_row_mismatch())?;
                if v.len() != expected {
                    return Err(returned_row_mismatch());
                }
            }
            (ColumnType::Vector(_), Value::Null) => {}
            (ColumnType::Vector(_), _) | (_, Value::Vector(_)) => {
                return Err(returned_row_mismatch());
            }
            _ => {}
        }
    }
    Ok(())
}

/// `triggers`（INSERT・BEFORE・ROW のみ。カタログ順）を `new_row` に対して順に発火する。
/// `Ok(None)` はその行の挿入をやめる（`NULL` 返却）、`Ok(Some(row))` は書き込む最終行。
pub(crate) fn fire_before_row_insert(
    runtime: &TriggerRuntime,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    new_row: Vec<Value>,
) -> Result<Option<Vec<Value>>, SqlSurfaceError> {
    let mut row = new_row;
    for trigger in triggers {
        // 行ごと・呼び出しごとに割り込み（`57014`）を確認する。
        crate::interrupt::check()?;
        if trigger.when_sql.is_some() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: WHEN_UNSUPPORTED.to_string(),
            });
        }
        let outcome = runtime.invoker().invoke(&TriggerCall {
            trigger_name: &trigger.name,
            function_key: &trigger.function_key,
            table,
            schema,
            new_row: &row,
            args: &trigger.tg_args,
        })?;
        match outcome {
            TriggerOutcome::Skip => return Ok(None),
            TriggerOutcome::Proceed(next) => {
                check_returned_row(schema, &next)?;
                row = next;
            }
        }
    }
    Ok(Some(row))
}
