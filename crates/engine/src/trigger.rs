//! トリガの発火側（TABLE-45・Issue #1723）。`catalog::trigger`（定義の保存・Issue #1722）が持つ
//! TRIGGER 定義を、行の書き込み経路から呼び出す責務を持つ。
//!
//! - [`call`]: 呼び出し口。トリガ関数の内部 API は Issue #2228 が `core::plpgsql_host` の
//!   `invoke_trigger_function_in_txn` と [`call::trigger_vars_from_call`] で用意した。発火時に
//!   セッション・Tx を持たない本番の [`call::TriggerFunctionInvoker`] 実装（`PlpgsqlTriggerInvoker`）と、
//!   それが委譲するホストの口（[`call::TriggerHost`]。実装は `core::plpgsql_trigger_host`。Issue #3184）を
//!   置く。入出力の型は二重に定義しない
//! - [`runtime`]: 呼び出し口と深さ上限を保持する通過証（`EngineCore` が 1 つ持つ。結線済み経路だけが、文ごとに
//!   ホストを束縛〔`bind`〕して渡す）
//! - `subject`: 発火元セッションの主体の写しを張る区間（トリガ本体を発火元と同じロールで実行するため）
//! - [`depth`]: トリガ呼び出しの入れ子深さの計数（上限超過は `54001`。Issue #1727）
//! - [`plan`]: 書き込み txn 内でカタログを 1 回読み、発火する経路か・未結線で拒否する経路かを決める
//! - [`fire`]: BEFORE ROW トリガの連鎖実行（NEW の受け渡し・NULL 返却による行のスキップ）と、
//!   文の終わりの AFTER ROW トリガの実行（戻り値は無視。Issue #1724）、文トリガ（STATEMENT。TRUNCATE を
//!   含む）の文につき 1 回の実行（対象 0 行でも発火。Issue #1726）
//! - [`write_handle`]: トリガ本体が同じ write txn・呼び出しテナントの文脈で他表へ書く狭い窓口
//!
//! 呼び出し元は `tenant`（書き込み経路。`tenant/trigger_insert.rs`）と `core`（通過証の保持）。
//! 設計記録は `docs/design/trigger-before-insert.md`・`docs/design/trigger-after-insert.md`・
//! `docs/design/trigger-statement-truncate.md`。

pub(crate) mod call;
pub(crate) mod depth;
pub(crate) mod fire;
pub(crate) mod plan;
pub(crate) mod runtime;
pub(crate) mod subject;
pub(crate) mod write_handle;

pub(crate) use runtime::TriggerRuntime;

#[cfg(test)]
mod tests;
