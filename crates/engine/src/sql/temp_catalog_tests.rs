//! セッション単位の一時カタログと temp 優先の名前解決のテスト（Issue #1681、対象ビヘイビア:
//! TABLE-44）。
//!
//! 役割: (1) `SessionTempCatalog` の登録・上限・識別子検証、(2) `SessionRelationLookup` が一時表で
//! 同名の永続表・ビューを隠すこと、(3) `EngineCore` の production 経路（`execute_sql_in_session`・
//! `execute_sql_in_txn`・`execute_parsed_*`）で、一時表を引く文が同名の永続表へ作用せず
//! `0A000` で拒否されること、他セッション・他テナントへ漏れないこと、ROLLBACK で一時カタログが
//! 巻き戻ることを検証する。

use std::cell::Cell;

use crate::catalog::{ColumnDef, ColumnType, TableSchema, ViewDef};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{SqlSurfaceError, TableLookup};
use crate::sql::mode::SessionState;
use crate::sql::temp_catalog::{
    tokens_reference_temp, ResolvedRelation, SessionRelationLookup, SessionTempCatalog,
    DEFAULT_MAX_TEMP_TABLES_PER_SESSION,
};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str, cols: &[&str]) -> TableSchema {
    TableSchema::new(
        name,
        cols.iter()
            .map(|c| ColumnDef::new(*c, ColumnType::Text, true))
            .collect(),
    )
}

fn code(e: &SqlSurfaceError) -> &'static str {
    e.wire_code()
}

/// 永続側を模す固定 lookup（テーブル `t(a,b)`・ビュー `v`）。
struct Inner;

impl TableLookup for Inner {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == "t")
    }
    fn view_definition(&self, name: &str) -> Result<Option<ViewDef>, SqlSurfaceError> {
        Ok((name == "v").then(|| ViewDef {
            base_relation: "t".to_string(),
            body_sql: "SELECT a FROM t".to_string(),
            column_aliases: Vec::new(),
        }))
    }
    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        Ok((name == "t").then(|| vec!["a".to_string(), "b".to_string(), "id".to_string()]))
    }
    fn buffered_depth(&self) -> usize {
        3
    }
}

// ---- カタログ単体 ----

#[test]
fn table_44_register_get_remove_and_duplicate() {
    let mut cat = SessionTempCatalog::default();
    assert!(cat.is_empty());
    let id = cat.register(schema("t", &["x"]), 4).expect("register");
    assert!(cat.contains("t"));
    assert_eq!(cat.get("t").map(|e| e.id), Some(id));
    let dup = cat.register(schema("t", &["x"]), 4).expect_err("dup");
    assert_eq!(code(&dup), "42P07");
    assert!(cat.remove("t"));
    assert!(!cat.remove("t"));
    // 取り除いた後は再登録でき、識別子は別物になる。
    let id2 = cat.register(schema("t", &["x"]), 4).expect("re-register");
    assert_ne!(id, id2);
}

#[test]
fn table_44_limit_is_enforced_before_allocation() {
    let mut cat = SessionTempCatalog::default();
    for n in ["a", "b"] {
        cat.register(schema(n, &["x"]), 2).expect("within limit");
    }
    let err = cat
        .register(schema("c", &["x"]), 2)
        .expect_err("over limit");
    assert_eq!(code(&err), "54000");
    assert_eq!(cat.len(), 2);
}

#[test]
fn table_44_invalid_identifier_is_rejected() {
    let mut cat = SessionTempCatalog::default();
    for bad in ["", "1abc", "a b", "a-b"] {
        assert!(cat.register(schema(bad, &["x"]), 4).is_err(), "{bad:?}");
    }
    assert!(cat.is_empty());
}

#[test]
fn table_44_names_are_deterministic() {
    let mut cat = SessionTempCatalog::default();
    for n in ["zeta", "alpha", "mid"] {
        cat.register(schema(n, &["x"]), 8).expect("register");
    }
    assert_eq!(cat.names().collect::<Vec<_>>(), ["alpha", "mid", "zeta"]);
}

#[test]
fn table_44_resolution_prefers_temp() {
    let mut cat = SessionTempCatalog::default();
    cat.register(schema("t", &["x"]), 4).expect("register");
    assert!(matches!(
        cat.resolve_unqualified("t"),
        ResolvedRelation::Temp(_)
    ));
    assert!(matches!(
        cat.resolve_unqualified("other"),
        ResolvedRelation::Persistent("other")
    ));
}

// ---- ラッパー ----

#[test]
fn table_44_wrapper_hides_persistent_table_and_view() {
    let mut cat = SessionTempCatalog::default();
    cat.register(schema("t", &["x", "y"]), 4).expect("t");
    cat.register(schema("v", &["z"]), 4).expect("v");
    let hit = Cell::new(false);
    let w = SessionRelationLookup::new(&Inner, &cat, &hit);

    assert_eq!(
        w.table_columns("t").expect("cols"),
        Some(vec!["x".to_string(), "y".to_string(), "id".to_string()])
    );
    assert_eq!(
        w.table_star_columns("t").expect("star"),
        Some(vec!["id".to_string(), "x".to_string(), "y".to_string()])
    );
    assert!(w.view_definition("v").expect("view").is_none());
    assert!(w.table_exists("v").expect("exists"));
    assert!(hit.get());
    assert_eq!(w.buffered_depth(), 3);
}

#[test]
fn table_44_wrapper_passes_through_unmatched_names_and_empty_catalog() {
    let mut cat = SessionTempCatalog::default();
    let hit = Cell::new(false);
    {
        let w = SessionRelationLookup::new(&Inner, &cat, &hit);
        assert!(w.table_exists("t").expect("exists"));
        assert!(w.view_definition("v").expect("view").is_some());
        assert_eq!(
            w.table_columns("t").expect("cols"),
            Inner.table_columns("t").expect("cols")
        );
        assert!(!hit.get());
    }
    cat.register(schema("u", &["x"]), 4).expect("u");
    let w = SessionRelationLookup::new(&Inner, &cat, &hit);
    assert!(w.table_exists("t").expect("exists"));
    assert!(w.view_definition("v").expect("view").is_some());
    assert!(!hit.get(), "unmatched names must not record a hit");
}

#[test]
fn table_44_token_scan_is_case_insensitive_and_empty_safe() {
    let toks = crate::sql::lexer::tokenize_marked("DROP TABLE Tmp1").expect("lex");
    let mut cat = SessionTempCatalog::default();
    assert!(!tokens_reference_temp(&toks, &cat));
    cat.register(schema("tmp1", &["x"]), 4).expect("register");
    assert!(tokens_reference_temp(&toks, &cat));
    let other = crate::sql::lexer::tokenize_marked("DROP TABLE zzz").expect("lex");
    assert!(!tokens_reference_temp(&other, &cat));
}

#[test]
fn table_44_token_scan_matches_quoted_null_relation_name() {
    let mut cat = SessionTempCatalog::default();
    cat.register(schema("null", &["x"]), 4).expect("register");
    let toks = crate::sql::lexer::tokenize_marked("DROP TABLE \"NULL\"").expect("lex");
    assert!(tokens_reference_temp(&toks, &cat));
}

// ---- EngineCore 経由（受入基準: 同名の永続表を隠す） ----

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<usize, String> {
    match core.execute_sql_in_session(&ctx(tenant), session, sql) {
        Ok(crate::sql::SqlOutcome::Query(r)) => Ok(r.rows.len()),
        Ok(_) => Ok(0),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

fn setup_persistent(core: &EngineCore) {
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE TABLE t (embedding VECTOR(2), a TEXT)",
        "CREATE TABLE u (embedding VECTOR(2), a TEXT)",
        "INSERT INTO t (id, embedding, a) VALUES (1, '[0.1,0.2]', 'p') USING OPERATION_ID 'op-1'",
    ] {
        run(core, &mut s, "tenant-a", sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn session_with_temp_t() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s.temp_tables_mut()
        .register(schema("t", &["x"]), DEFAULT_MAX_TEMP_TABLES_PER_SESSION)
        .expect("register temp t");
    s
}

#[test]
fn table_44_temp_name_never_touches_the_persistent_table() {
    let path = unique_db_path("temp-catalog-hide");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup_persistent(&core);
    let mut a = session_with_temp_t();

    for sql in [
        "SELECT * FROM t LIMIT 10",
        "DELETE FROM t WHERE id = 1",
        "DROP TABLE t",
        "TRUNCATE t",
        "TRUNCATE u, T",
        "CREATE INDEX i ON t (a)",
        "INSERT INTO t (id, embedding, a) VALUES (2, '[0.1,0.2]', 'q') USING OPERATION_ID 'op-2'",
    ] {
        assert_eq!(
            run(&core, &mut a, "tenant-a", sql),
            Err("0A000".to_string()),
            "{sql}"
        );
    }

    // 同じテナントの別セッション・別テナントは永続表を従来どおり読める。
    let mut b = SessionState::default();
    assert_eq!(
        run(&core, &mut b, "tenant-a", "SELECT * FROM t LIMIT 10"),
        Ok(1)
    );
    let mut c = SessionState::default();
    assert_eq!(
        run(&core, &mut c, "tenant-b", "SELECT * FROM t LIMIT 10"),
        Ok(0)
    );

    // 無関係の永続表は A でも従来どおり使える。
    assert_eq!(
        run(&core, &mut a, "tenant-a", "SELECT * FROM u LIMIT 10"),
        Ok(0)
    );
    assert_eq!(
        run(
            &core,
            &mut a,
            "tenant-a",
            "INSERT INTO u (id, embedding, a) VALUES (1, '[0.1,0.2]', 'x') \
             USING OPERATION_ID 'op-u'"
        ),
        Ok(0)
    );
}

#[test]
fn table_44_transaction_control_is_never_blocked_and_rollback_restores_catalog() {
    let path = unique_db_path("temp-catalog-txn");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup_persistent(&core);
    let c = ctx("tenant-a");

    // 一時表の参照は Failed に遷移し、ROLLBACK で戻れる。
    let mut a = session_with_temp_t();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&c, &mut a, &mut txn, "SELECT * FROM t LIMIT 1")
        .expect_err("temp ref");
    assert_eq!(err.wire_code(), "0A000");
    let err = core
        .execute_sql_in_txn(&c, &mut a, &mut txn, "SELECT * FROM u LIMIT 1")
        .expect_err("failed txn");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut a, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(a.temp_tables().contains("t"), "BEGIN 前の一時表は残る");

    // BEGIN の後に登録した一時表は ROLLBACK で消える。
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    s.temp_tables_mut()
        .register(schema("late", &["x"]), 4)
        .expect("register");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(s.temp_tables().is_empty());
}

#[test]
fn table_44_coarse_gate_rejects_prebuilt_statements() {
    let path = unique_db_path("temp-catalog-coarse");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    setup_persistent(&core);
    let c = ctx("tenant-a");
    let parsed = core.parse_sql("SELECT * FROM t LIMIT 10").expect("parse");

    let mut a = session_with_temp_t();
    let e = core
        .execute_parsed_in_session(&c, &mut a, &parsed)
        .expect_err("gate");
    assert_eq!(e.wire_code(), "0A000");
    let e = core
        .describe_parsed_in_session(&a, &parsed)
        .expect_err("describe gate");
    assert_eq!(e.wire_code(), "0A000");
    let mut txn = core.new_session_transaction();
    let e = core
        .execute_parsed_in_txn(&c, &mut a, &mut txn, &parsed)
        .expect_err("txn gate");
    assert_eq!(e.wire_code(), "0A000");

    // 一時カタログが空なら従来どおり実行できる。
    let mut empty = SessionState::default();
    core.execute_parsed_in_session(&c, &mut empty, &parsed)
        .expect("no gate when empty");
    assert!(core.describe_parsed_in_session(&empty, &parsed).is_ok());
}

#[test]
fn table_44_quoted_null_temp_name_never_touches_the_persistent_table() {
    let path = unique_db_path("temp-catalog-quoted-null");
    let _g = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut p = SessionState::default();
    p.allow_ddl();
    run(
        &core,
        &mut p,
        "tenant-a",
        "CREATE TABLE \"NULL\" (embedding VECTOR(2), a TEXT)",
    )
    .expect("persistent quoted NULL table");
    let mut a = SessionState::default();
    a.allow_ddl();
    a.temp_tables_mut()
        .register(schema("null", &["x"]), DEFAULT_MAX_TEMP_TABLES_PER_SESSION)
        .expect("register temp");
    for sql in [
        "DROP TABLE \"NULL\"",
        "ALTER TABLE \"NULL\" ADD COLUMN b TEXT",
        "CREATE INDEX i ON \"NULL\" (a)",
        "TRUNCATE \"NULL\"",
    ] {
        assert_eq!(
            run(&core, &mut a, "tenant-a", sql),
            Err("0A000".to_string()),
            "{sql}"
        );
    }
    let err = core
        .begin_copy(&ctx("tenant-a"), &a, "COPY \"NULL\" FROM STDIN")
        .expect_err("copy gate");
    assert_eq!(err.wire_code(), "0A000");
}
