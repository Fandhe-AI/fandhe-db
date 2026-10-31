//! トリガ関数の呼び出し口（TABLE-45・Issue #1723・#1724）。
//!
//! 発火側（`trigger::fire`）が NEW 行などを [`TriggerCall`] に詰めて [`TriggerFunctionInvoker`] を
//! 呼び、[`TriggerOutcome`] を受け取る。トリガ関数の実体（PL/pgSQL 関数・Issue #2228）はこの
//! トレイトを実装して差し替える。既定の [`UnwiredInvoker`] は固定文言の `0A000` を返し、実体が
//! 無い間にトリガを黙って飛ばさない（fail-closed。実行モデルの D8）。

use super::write_handle::TriggerWriteHandle;
use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
use crate::catalog::TableSchema;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;

/// 発火した操作（`TG_OP`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriggerOp {
    Insert,
    Update,
    Delete,
    /// 文トリガのみ（TRUNCATE は文単位だけ。Issue #1726）。
    Truncate,
}

/// 1 回のトリガ呼び出しの入力。NEW に入るのは利用者の列（`schema.columns` 宣言順）だけで、テナントの
/// キー・rowid・可視性は含めない（トリガ関数からは見えず、書き換えもできない。実行モデルの D2）。
// 実体（#2228）が読むまでは、本クレート内で読む者がいない（呼び出し口の契約として先に定義する）。
#[allow(dead_code)]
pub(crate) struct TriggerCall<'a> {
    /// `TG_NAME`。
    pub(crate) trigger_name: &'a str,
    /// トリガ関数の正規化キー（`catalog::function` の関数キー）。
    pub(crate) function_key: &'a str,
    /// `TG_TABLE_NAME`。
    pub(crate) table: &'a str,
    /// 対象表のスキーマ（列名・列型の一覧。NEW の列対応づけに使う）。
    pub(crate) schema: &'a TableSchema,
    /// `TG_OP`（INSERT・UPDATE・DELETE・TRUNCATE）。
    pub(crate) op: TriggerOp,
    /// `TG_LEVEL`（ROW・STATEMENT）。文トリガ（Issue #1726）は `old_row: None`・`new_row` は空
    /// （実行モデルの D1: NEW／OLD は NULL）。
    pub(crate) level: TriggerLevel,
    /// `OLD`（UPDATE・DELETE の更新前・削除前の行。`schema.columns` と同じ幅・順。INSERT は `None`）。
    pub(crate) old_row: Option<&'a [Value]>,
    /// `NEW`（`schema.columns` と同じ幅・順。DEFAULT・SEQUENCE の値は確定済み）。DELETE は NEW を
    /// 持たないため空スライス（`op` が `Delete` のとき読んではならない）。
    pub(crate) new_row: &'a [Value],
    /// `TG_ARGV`（`TG_NARGS` はその長さ）。値として渡し、SQL テキストへ連結しない。
    pub(crate) args: &'a [String],
    /// `TG_WHEN` の元（BEFORE・AFTER）。
    pub(crate) timing: TriggerTiming,
    /// 他表への書き込みの窓口（外側の文と同じ write txn・呼び出しテナントの文脈。Issue #1724）。
    pub(crate) writes: &'a TriggerWriteHandle<'a>,
}

#[allow(dead_code)] // 実体（#2228）が参照する。
impl TriggerCall<'_> {
    /// `TG_OP`（呼び出しごと）。
    pub(crate) fn tg_op(&self) -> &'static str {
        match self.op {
            TriggerOp::Insert => "INSERT",
            TriggerOp::Update => "UPDATE",
            TriggerOp::Delete => "DELETE",
            TriggerOp::Truncate => "TRUNCATE",
        }
    }
    /// `TG_LEVEL`（呼び出しごと）。
    pub(crate) fn tg_level(&self) -> &'static str {
        match self.level {
            TriggerLevel::Row => "ROW",
            TriggerLevel::Statement => "STATEMENT",
        }
    }

    /// `TG_WHEN`（呼び出しごと）。
    pub(crate) fn tg_when(&self) -> &'static str {
        match self.timing {
            TriggerTiming::Before => "BEFORE",
            TriggerTiming::After => "AFTER",
        }
    }
}

/// トリガ関数の戻り値。
#[derive(Debug, Clone, PartialEq)]
// `Proceed`／`Skip` を作るのは実体（#2228。本 Issue ではテスト）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TriggerOutcome {
    /// 行を（書き換えて）続行する。中身は次のトリガの NEW・最終的に保存する行。
    Proceed(Vec<Value>),
    /// `NULL` を返した。その行の挿入をやめ、後続のトリガも呼ばない。
    Skip,
}

/// [`TriggerCall`] からトリガ関数へ渡す変数（`NEW`・`OLD`・`TG_*`）を作る純粋関数（Issue #2228）。
///
/// 発火側（#1720 の本番 invoker）が呼ぶ。`table_name`・`table_schema` は利用者向けの名前（内部キー
/// `#<id>.<name>` を `TG_TABLE_NAME` に出さないため、呼び出し側が解決して渡す）。NEW／OLD は行トリガ
/// の該当する側だけ値を持ち、それ以外（INSERT の OLD・DELETE の NEW・文トリガ）は未代入にする。
/// 値の変換ができない列型（表現できない型）は固定文言の `0A000`（fail-closed）。
// 本番の invoker（#1720）が結線するまでは、結合テストだけが呼ぶ。
#[allow(dead_code)]
pub(crate) fn trigger_vars_from_call(
    call: &TriggerCall<'_>,
    table_name: &str,
    table_schema: &str,
) -> Result<crate::plpgsql::interp::TriggerVars, SqlSurfaceError> {
    use crate::plpgsql::host::PlScalar;
    use crate::sql::matview::value_to_cell;

    let unsupported = |_: SqlSurfaceError| {
        SqlSurfaceError::feature_not_supported(
            "trigger row has a column type unsupported in PL/pgSQL",
        )
    };
    let to_scalars = |row: &[Value]| -> Result<Vec<PlScalar>, SqlSurfaceError> {
        if row.len() != call.schema.columns.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "trigger row width mismatch".to_string(),
            });
        }
        call.schema
            .columns
            .iter()
            .zip(row)
            .map(|(col, v)| {
                let cell = value_to_cell(v.clone(), &col.ty).map_err(unsupported)?;
                Ok(PlScalar::new(cell, col.ty.clone()))
            })
            .collect()
    };
    let is_row = call.level == TriggerLevel::Row;
    let new = if is_row && matches!(call.op, TriggerOp::Insert | TriggerOp::Update) {
        Some(to_scalars(call.new_row)?)
    } else {
        None
    };
    let old = match (is_row, call.old_row) {
        (true, Some(row)) if matches!(call.op, TriggerOp::Update | TriggerOp::Delete) => {
            Some(to_scalars(row)?)
        }
        _ => None,
    };
    Ok(crate::plpgsql::interp::TriggerVars {
        name: call.trigger_name.to_string(),
        when: call.tg_when().to_string(),
        level: call.tg_level().to_string(),
        op: call.tg_op().to_string(),
        table_name: table_name.to_string(),
        table_schema: table_schema.to_string(),
        nargs: i32::try_from(call.args.len()).map_err(|_| SqlSurfaceError::Internal {
            detail: "too many trigger arguments".to_string(),
        })?,
        columns: call
            .schema
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.ty.clone()))
            .collect(),
        new,
        old,
    })
}

/// トリガ関数を実行する実体の差し込み口。実装は #2228（PL/pgSQL 結線）が与える。
pub(crate) trait TriggerFunctionInvoker: Send + Sync {
    /// 関数を 1 回実行する。エラーは [`SqlSurfaceError`] のまま呼び出し元へ透過する。
    fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError>;
}

/// 実体が結線されていない間の既定の呼び出し口（常に固定文言の `0A000`）。
pub(crate) struct UnwiredInvoker;

impl TriggerFunctionInvoker for UnwiredInvoker {
    fn invoke(&self, _call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        Err(SqlSurfaceError::FeatureNotSupported {
            detail: "trigger function execution is not supported".to_string(),
        })
    }
}
