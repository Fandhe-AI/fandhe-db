//! トリガの発火側（TABLE-45・Issue #1723）。`catalog::trigger`（定義の保存・Issue #1722）が持つ
//! TRIGGER 定義を、行の書き込み経路から呼び出す責務を持つ。
//!
//! - [`call`]: 呼び出し口。トリガ関数の実体（`RETURNS trigger` の PL/pgSQL 関数・Issue #2228）は
//!   本クレートの別担当のため、ここでは入出力の型とトレイトだけを定義し、実体への結線は #2228 が
//!   [`call::TriggerFunctionInvoker`] を実装して差し替える（型を二重に定義しない）
//! - [`runtime`]: 呼び出し口を保持する通過証（`EngineCore` が 1 つ持つ。結線済み経路だけが渡す）
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
pub(crate) mod fire;
pub(crate) mod plan;
pub(crate) mod runtime;
pub(crate) mod write_handle;

pub(crate) use runtime::TriggerRuntime;

#[cfg(test)]
mod tests;
