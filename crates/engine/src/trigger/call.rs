//! トリガ関数の呼び出し口（TABLE-45・Issue #1723・#1724）。
//!
//! 発火側（`trigger::fire`）が NEW 行などを [`TriggerCall`] に詰めて [`TriggerFunctionInvoker`] を
//! 呼び、[`TriggerOutcome`] を受け取る。トリガ関数の実体（PL/pgSQL 関数・Issue #2228）はこの
//! トレイトを実装して差し替える。本番の既定は `PlpgsqlTriggerInvoker`（Issue #3185）。
//! テスト専用の [`UnwiredInvoker`] は固定文言の `0A000` を返し、発火側の意味論を invoker から切り離して固定する。
//! 未結線の経路は黙ってトリガを飛ばさない（fail-closed。実行モデルの D8）。

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
    /// 束縛済みのトリガ関数ホスト（`TriggerRuntime::bind`。Issue #3184）。本番の
    /// [`PlpgsqlTriggerInvoker`] だけが読む。未束縛（`None`）の経路では本番の invoker は `0A000`。
    pub(crate) host: Option<&'a dyn TriggerHost>,
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
/// 発火側（本番の invoker `PlpgsqlTriggerInvoker`。Issue #3184）が呼ぶ。`table_name`・`table_schema` は利用者向けの名前（内部キー
/// `#<id>.<name>` を `TG_TABLE_NAME` に出さないため、呼び出し側が解決して渡す）。NEW／OLD は行トリガ
/// の該当する側だけ値を持ち、それ以外（INSERT の OLD・DELETE の NEW・文トリガ）は未代入にする。
/// 値の変換ができない列型（表現できない型）は固定文言の `0A000`（fail-closed）。
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

    /// 行トリガの `WHEN` 条件を `call` の NEW／OLD で評価する（Issue #3190）。`true` のときだけ発火する
    /// （FALSE・NULL は不発火）。既定は固定文言の `0A000`（評価を結線していない invoker は黙って発火しない）。
    fn eval_when(&self, _call: &TriggerCall<'_>, _when_sql: &str) -> Result<bool, SqlSurfaceError> {
        Err(SqlSurfaceError::feature_not_supported(
            "trigger WHEN condition is not supported",
        ))
    }
}

/// セッション・Tx を持たない発火文脈でトリガ関数の本体を実行する実体（Issue #3184）。`EngineCore` が
/// 実装し、`TriggerRuntime::bind` が文ごとに束縛する。`Sync` を要求するのは、未束縛の通過証
/// （`TriggerRuntime<'static>`）を持つ `EngineCore` の `Send`／`Sync` を保つため。
pub(crate) trait TriggerHost: Sync {
    /// `call` のトリガ関数を、`call.writes` が借用する外側の write txn・呼び出しテナントの文脈の上で
    /// 1 回実行する。エラーは [`SqlSurfaceError`] のまま返し、文の取り消しは発火側が担う。
    fn invoke_trigger(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError>;

    /// 行トリガの `WHEN` 条件（保存済みの正規化テキスト）を `call` の NEW／OLD で評価する（Issue #3190）。
    /// TRUE のときだけ `true`。書き込みはできない読み取り専用の評価で、エラーは [`SqlSurfaceError`] のまま返す。
    fn eval_trigger_when(
        &self,
        call: &TriggerCall<'_>,
        when_sql: &str,
    ) -> Result<bool, SqlSurfaceError>;
}

/// 本番の呼び出し口（Issue #3184）。`call.host`（`EngineCore`）へ委譲するだけで状態を持たない。
/// `EngineCore` の既定（Issue #3185。`TriggerRuntime::production`）。ホスト未束縛の経路は固定文言の `0A000`（fail-closed）。
///
/// 共有 `public` 表のトリガは定義時に拒否する（Issue #3361）。それ以前にカタログへ残った定義への保険として、
/// 対象が共有 `public` 表なら（内部キーの解析失敗も含め）ホストへ委譲せず `0A000` を返す。判定は書き込み
/// テナント自身の scope の定義にだけ作用し、他テナントの定義の有無に依存しない。
pub(crate) struct PlpgsqlTriggerInvoker;

impl TriggerFunctionInvoker for PlpgsqlTriggerInvoker {
    fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        if crate::catalog::trigger::is_shared_public_table_key(call.table) {
            return Err(SqlSurfaceError::feature_not_supported(
                "triggers on shared public tables are not supported",
            ));
        }
        match call.host {
            Some(host) => host.invoke_trigger(call),
            None => Err(SqlSurfaceError::FeatureNotSupported {
                detail: "trigger function execution is not supported".to_string(),
            }),
        }
    }

    fn eval_when(&self, call: &TriggerCall<'_>, when_sql: &str) -> Result<bool, SqlSurfaceError> {
        if crate::catalog::trigger::is_shared_public_table_key(call.table) {
            return Err(SqlSurfaceError::feature_not_supported(
                "triggers on shared public tables are not supported",
            ));
        }
        match call.host {
            Some(host) => host.eval_trigger_when(call, when_sql),
            None => Err(SqlSurfaceError::FeatureNotSupported {
                detail: "trigger function execution is not supported".to_string(),
            }),
        }
    }
}

/// テスト専用の未結線の呼び出し口（常に固定文言の `0A000`）。
#[cfg(test)]
pub(crate) struct UnwiredInvoker;

#[cfg(test)]
impl TriggerFunctionInvoker for UnwiredInvoker {
    fn invoke(&self, _call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        Err(SqlSurfaceError::FeatureNotSupported {
            detail: "trigger function execution is not supported".to_string(),
        })
    }
}
