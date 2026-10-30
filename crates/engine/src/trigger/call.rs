//! トリガ関数の呼び出し口（TABLE-45・Issue #1723）。
//!
//! 発火側（`trigger::fire`）が NEW 行などを [`TriggerCall`] に詰めて [`TriggerFunctionInvoker`] を
//! 呼び、[`TriggerOutcome`] を受け取る。トリガ関数の実体（PL/pgSQL 関数・Issue #2228）はこの
//! トレイトを実装して差し替える。既定の [`UnwiredInvoker`] は固定文言の `0A000` を返し、実体が
//! 無い間にトリガを黙って飛ばさない（fail-closed。実行モデルの D8）。

use crate::catalog::TableSchema;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;

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
    /// `NEW`（`schema.columns` と同じ幅・順。DEFAULT・SEQUENCE の値は確定済み）。
    pub(crate) new_row: &'a [Value],
    /// `TG_ARGV`（`TG_NARGS` はその長さ）。値として渡し、SQL テキストへ連結しない。
    pub(crate) args: &'a [String],
}

#[allow(dead_code)] // 実体（#2228）が参照する。
impl TriggerCall<'_> {
    /// `TG_OP`（本 Issue の範囲は INSERT のみ）。
    pub(crate) const TG_OP: &'static str = "INSERT";
    /// `TG_WHEN`。
    pub(crate) const TG_WHEN: &'static str = "BEFORE";
    /// `TG_LEVEL`。
    pub(crate) const TG_LEVEL: &'static str = "ROW";
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
