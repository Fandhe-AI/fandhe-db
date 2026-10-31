//! `substring(... FROM ... FOR ...)` 特殊構文と `position(... IN ...)` 構文の結合テスト
//! （Issue #1903、対象ビヘイビア: SQL-65・ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! `substring` は構文段で `substr` と同じ束縛・評価経路へ脱糖される（`allowlist::Parser::
//! parse_substring_expr`）。正規表現形は SQL-55 の管轄のため `0A000` で fail-closed に拒否する。
//! `position` は Issue #919 で実装済みの構文を SQL-65 の観点で回帰固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id=1..=3` に `label = "Hello", "world", NULL` を投入する固定コーパス
/// （多バイト文字のケースは別途 `multibyte_functions_are_char_indexed` で扱う）。
fn seed(storage: &Storage, tenant: &str) {
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for(tenant);
    let rows: [(u64, Option<&str>); 3] = [(1, Some("Hello")), (2, Some("world")), (3, None)];
    for (id, label) in rows {
        let value = match label {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.1, 0.2]), value],
            &op_id,
        )
        .expect("insert row");
    }
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed(&storage, "tenant-a");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn cell_for(result: &engine::sql::exec::QueryResult, id: u64, col: usize) -> Cell {
    result
        .rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("row id={id} not found in result"))
        .cells
        .get(col)
        .cloned()
        .unwrap_or_else(|| panic!("column {col} missing for id={id}"))
}

fn one(core: &EngineCore, sql: &str) -> Cell {
    let ctx = ctx_for("tenant-a");
    let result = core
        .execute_sql(&ctx, sql)
        .unwrap_or_else(|e| panic!("{sql} should succeed: {e}"));
    cell_for(&result, 1, 1)
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn code_of(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    core.execute_sql(&ctx, sql)
        .expect_err(&format!("{sql} should fail"))
        .wire_code()
        .to_string()
}

/// SQL-65: 4 つの順序形とカンマ形が PostgreSQL と同じ結果を返す（キーワードは大小無視）。
#[test]
fn substring_from_for_forms_match_postgres() {
    let path = unique_db_path("sql65-substring-forms");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let q = |expr: &str| {
        one(
            &core,
            &format!("SELECT id, {expr} FROM docs WHERE id = 1 LIMIT 1"),
        )
    };
    assert_eq!(q("substring('hello' FROM 2)"), text("ello"));
    assert_eq!(q("substring('hello' FROM 2 FOR 3)"), text("ell"));
    assert_eq!(q("substring('hello' FOR 2)"), text("he"));
    assert_eq!(q("substring('hello' FOR 3 FROM 2)"), text("ell"));
    assert_eq!(q("substring('hello', 2)"), text("ello"));
    assert_eq!(q("substring('hello', 2, 3)"), text("ell"));
    assert_eq!(q("Substring('hello' from 2 For 3)"), text("ell"));
    assert_eq!(q("substring('hello' FROM -1 FOR 4)"), text("he"));
    assert_eq!(q("substring('hello' FROM 9)"), text(""));
    assert_eq!(q("substring(label FROM 2 FOR 3)"), text("ell"));
}

/// SQL-65: 文字単位で切り出す（マルチバイト）。
#[test]
fn substring_is_char_indexed() {
    let path = unique_db_path("sql65-substring-mb");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let sql = "SELECT id, substring('あいうえお' FROM 2 FOR 2) FROM docs WHERE id = 1 LIMIT 1";
    assert_eq!(one(&core, sql), text("いう"));
}

/// SQL-65: NULL 入力は NULL（strict）。WHERE・ORDER BY の式位置でも使える。
#[test]
fn substring_null_propagation_and_positions() {
    let path = unique_db_path("sql65-substring-pos");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, substring(label FROM 1 FOR 2) FROM docs LIMIT 10",
        )
        .expect("projection");
    assert_eq!(cell_for(&r, 1, 1), text("He"));
    assert_eq!(cell_for(&r, 3, 1), Cell::Null);
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, label FROM docs WHERE substring(label FROM 1 FOR 1) = 'w' LIMIT 10",
        )
        .expect("where");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].id, 2);
    core.execute_sql(
        &ctx,
        "SELECT id, label FROM docs ORDER BY substring(label FROM 2) LIMIT 10",
    )
    .expect("order by");
}

/// SQL-65: 正規表現形は `0A000`、個数の誤りは `42883`、構文の誤りは `42601`。
#[test]
fn substring_rejects_regex_form_and_bad_shapes() {
    let path = unique_db_path("sql65-substring-err");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let c = |expr: &str| code_of(&core, &format!("SELECT id, {expr} FROM docs LIMIT 1"));
    assert_eq!(c("substring(label FROM 'a.c')"), "0A000");
    assert_eq!(c("substring(label, 'x')"), "0A000");
    assert_eq!(c("substring(label SIMILAR 'a%' ESCAPE '#')"), "0A000");
    assert_eq!(c("substring(label)"), "42883");
    assert_eq!(c("substring(label, 1, 2, 3)"), "42883");
    // カンマ形の引数上限は通常の関数呼び出しと同じ 100（54023）。上限以内は個数不一致 42883。
    let many = |n: usize| format!("substring(label{})", ", 1".repeat(n - 1));
    assert_eq!(c(&many(33)), "42883");
    assert_eq!(c(&many(100)), "42883");
    assert_eq!(c(&many(101)), "54023");
    assert_eq!(c("substring(label FROM)"), "42601");
    assert_eq!(c("substring(label FOR)"), "42601");
}

/// SQL-65: `substring` は組み込み関数名として予約される。
#[test]
fn substring_is_reserved_against_udf_definition() {
    let path = unique_db_path("sql65-substring-reserved");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION substring(x) AS x")
        .expect_err("reserved");
    assert_eq!(err.wire_code(), "42723");
}

/// SQL-65: `position(needle IN haystack)` の構文（Issue #919 実装済み）の回帰固定。
#[test]
fn position_in_syntax_matches_postgres() {
    let path = unique_db_path("sql65-position");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let q = |expr: &str| {
        one(
            &core,
            &format!("SELECT id, {expr} FROM docs WHERE id = 1 LIMIT 1"),
        )
    };
    assert_eq!(q("position('l' IN 'hello')"), Cell::Float(3.0));
    assert_eq!(q("position('z' IN 'hello')"), Cell::Float(0.0));
    assert_eq!(q("position('' IN 'hello')"), Cell::Float(1.0));
    assert_eq!(q("position('う' IN 'あいうえお')"), Cell::Float(3.0));
    assert_eq!(
        code_of(&core, "SELECT id, position('l', 'hello') FROM docs LIMIT 1"),
        "42601"
    );
}

/// SQL-65: 負の長さは `22011`（substring_error）。`substr` も PostgreSQL に合わせて同じコード。
#[test]
fn substring_negative_length_is_22011() {
    let path = unique_db_path("sql65-substring-neg");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let c = |expr: &str| code_of(&core, &format!("SELECT id, {expr} FROM docs LIMIT 1"));
    assert_eq!(c("substring('hello' FROM 1 FOR -1)"), "22011");
    assert_eq!(c("substr('hello', 1, -1)"), "22011");
    assert_eq!(c("substring(label FOR -2)"), "22011");
}
