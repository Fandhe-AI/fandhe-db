//! 従来方式（暗黙の `id` 疑似列を持つ）の表を SQL テキストから作るテスト専用ヘルパー
//! （TABLE-23・Issue #2199）。
//!
//! 背景: SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る。一方、疑似列 `id`
//! （物理キーの直接指定・FK の `id` 参照・`id` 昇順の同点順序・ファイル形 INSERT の行 id 導出）の契約は
//! 既存の表（従来方式）に対して引き続き成立するため、その契約を検証する結合テストは、NoSQL 表層の
//! 表作成と同じ入口（従来方式の構造検証 `validate_create_table_tokens_with_implicit_id` →
//! `EngineCore::execute_parsed_in_session`）で従来方式の表を作る。
//!
//! 呼び出し文脈: 結合テスト（`tests/*.rs`）が `#[path = "../src/test_util/legacy_table.rs"] mod
//! legacy_table;` で取り込む（`temp_db.rs` と同じ方式）。`engine` クレートの公開 API だけに依存する。
#![allow(dead_code)]

use engine::core::{EngineCore, ParsedSql};
use engine::policy::PolicyContext;
use engine::sql::allowlist::{validate_create_table_tokens_with_implicit_id, SqlSurfaceError};
use engine::sql::lexer::tokenize;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;

/// `sql` が永続表の `CREATE TABLE`（`TEMP` なし）かを返す。
pub fn is_plain_create_table(sql: &str) -> bool {
    let mut words = sql.split_whitespace();
    let first = words.next().unwrap_or("");
    let second = words.next().unwrap_or("");
    first.eq_ignore_ascii_case("CREATE") && second.eq_ignore_ascii_case("TABLE")
}

/// `CREATE TABLE` 文 `sql` を従来方式の表として実行する（DDL 権限を許可したセッションで実行する）。
pub fn create_legacy_table(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    create_legacy_table_in(core, ctx, &mut session, sql)
}

/// [`create_legacy_table`] の、呼び出し元のセッションで実行する版。
pub fn create_legacy_table_in(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let tokens = tokenize(sql)?;
    let stmt = validate_create_table_tokens_with_implicit_id(&tokens)?;
    core.execute_parsed_in_session(ctx, session, &ParsedSql::CreateTable(stmt))
}

/// `EngineCore` に、`execute_sql_in_session` と同じ形で `CREATE TABLE` だけを従来方式の表作成へ
/// 振り向ける実行入口を足す拡張 trait。疑似列 `id` の契約を検証する結合テストが、SQL 実行の呼び出しを
/// `.execute_sql_in_session_legacy(` へ置き換えるだけで、`CREATE TABLE` を従来方式の表にできる
/// （その他の文は `execute_sql_in_session` と同一）。
pub trait LegacyExec {
    fn execute_sql_in_session_legacy(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        sql: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError>;
}

impl LegacyExec for EngineCore {
    fn execute_sql_in_session_legacy(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        sql: &str,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        if is_plain_create_table(sql) {
            create_legacy_table_in(self, ctx, session, sql)
        } else {
            self.execute_sql_in_session(ctx, session, sql)
        }
    }
}
