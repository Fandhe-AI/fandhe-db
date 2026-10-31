//! ドル引用文字列（SQL-67 (1)・Issue #2216）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-67 (1)・WIRE-12。
//! `$$...$$`・`$tag$...$tag$` が `'...'` と同じ 1 個の文字列リテラルとして字句化され、
//! 通常の値・`DO`・複数文メッセージで同じ結果になることを production 経路で固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, "CREATE TABLE d (s TEXT)")
        .expect("create table");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
}

fn texts(core: &EngineCore) -> Vec<String> {
    let r = core
        .execute_sql(&ctx(), "SELECT s FROM d ORDER BY s")
        .expect("select");
    r.rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(t) => t.clone(),
            o => panic!("unexpected {o:?}"),
        })
        .collect()
}

#[test]
fn dollar_quoted_value_equals_single_quoted_value() {
    let (core, _g) = setup("sql67_value");
    run(&core, "INSERT INTO d (s) VALUES ($$it's; ok$$)").expect("insert dollar");
    run(&core, "INSERT INTO d (s) VALUES ($t$a$$b$t$)").expect("insert tagged");
    assert_eq!(
        texts(&core),
        vec!["a$$b".to_string(), "it's; ok".to_string()]
    );
    let hit = core
        .execute_sql(&ctx(), "SELECT s FROM d WHERE s = $q$it's; ok$q$")
        .expect("where");
    let same = core
        .execute_sql(&ctx(), "SELECT s FROM d WHERE s = 'it''s; ok'")
        .expect("where");
    assert_eq!(hit.rows.len(), 1);
    assert_eq!(same.rows.len(), 1);
}

#[test]
fn do_block_accepts_dollar_quoted_body() {
    let (core, _g) = setup("sql67_do");
    run(&core, "DO $$ BEGIN NULL; END $$").expect("do");
}

#[test]
fn malformed_dollar_quotes_fail_with_42601_without_echoing_body() {
    let (core, _g) = setup("sql67_bad");
    for sql in [
        "INSERT INTO d (s) VALUES ($$secret-body)",
        "INSERT INTO d (s) VALUES (a$$secret-body$$)",
    ] {
        let e = run(&core, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42601", "{sql}");
        assert!(!format!("{e:?}").contains("secret-body"), "{sql}");
    }
    assert!(texts(&core).is_empty());
}

#[test]
fn long_dollar_quoted_body_within_limit_is_accepted() {
    let (core, _g) = setup("sql67_long");
    let body = "x".repeat(200_000);
    run(&core, &format!("INSERT INTO d (s) VALUES ($${body}$$)")).expect("insert");
    assert_eq!(texts(&core).len(), 1);
}
