//! `sql::allowlist::validate_create_table_tokens`／`validate_alter_table_tokens`／
//! `validate_drop_table_tokens` の crate 外部公開 API（NOSQL-13・TASK-207、
//! Issue #910）を検証する。
//!
//! これらは NoSQL 表層（`wire-server` の `http/query/ddl.rs`）が JSON の DDL
//! 要求をトークン列へ写像したうえで直接呼ぶ入口（`Self::parse_sql_prepared`
//! が `$n` を実値トークンへ置換した列をパーサーへ渡す前例と同じ設計）。
//! 本テストは、SQL テキストを [`engine::sql::lexer::tokenize`] で字句解析した
//! トークン列を渡した場合と、SQL 表層の `parse_sql` が返す [`ParsedSql`] が
//! 同一の構造情報を持つことを固定する（token 入口が SQL テキスト経由の
//! 通常経路と同一の構造検証を通ることの回帰検証）。

use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::sql::allowlist::{
    validate_alter_table_tokens, validate_create_table_tokens, validate_drop_table_tokens,
};
use engine::sql::lexer::tokenize;
use engine::storage::Storage;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

#[test]
fn create_table_token_entry_matches_text_entry() {
    let (core, path) = new_core("sql_ddl_tokens_public_api_create");
    let _cleanup = CleanupGuard(path);
    let sql = "CREATE TABLE t1 (name TEXT, body VECTOR(3))";
    let tokens = tokenize(sql).expect("tokenize");
    let via_tokens = validate_create_table_tokens(&tokens).expect("token entry");
    let via_text = core.parse_sql(sql).expect("text entry");
    match via_text {
        ParsedSql::CreateTable(stmt) => assert_eq!(stmt, via_tokens),
        other => panic!("expected ParsedSql::CreateTable, got {other:?}"),
    }
}

#[test]
fn alter_table_add_column_token_entry_matches_text_entry() {
    let (core, path) = new_core("sql_ddl_tokens_public_api_alter");
    let _cleanup = CleanupGuard(path);
    let sql = "ALTER TABLE t1 ADD COLUMN score INTEGER";
    let tokens = tokenize(sql).expect("tokenize");
    let via_tokens = validate_alter_table_tokens(&tokens).expect("token entry");
    let via_text = core.parse_sql(sql).expect("text entry");
    match via_text {
        ParsedSql::AlterTable(stmt) => assert_eq!(stmt, via_tokens),
        other => panic!("expected ParsedSql::AlterTable, got {other:?}"),
    }
}

#[test]
fn drop_table_token_entry_matches_text_entry() {
    let (core, path) = new_core("sql_ddl_tokens_public_api_drop");
    let _cleanup = CleanupGuard(path);
    let sql = "DROP TABLE t1";
    let tokens = tokenize(sql).expect("tokenize");
    let via_tokens = validate_drop_table_tokens(&tokens).expect("token entry");
    let via_text = core.parse_sql(sql).expect("text entry");
    match via_text {
        ParsedSql::DropTable(stmt) => assert_eq!(stmt, via_tokens),
        other => panic!("expected ParsedSql::DropTable, got {other:?}"),
    }
}
