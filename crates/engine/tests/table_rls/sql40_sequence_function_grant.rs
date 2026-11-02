//! SEQUENCE・関数単位の `GRANT`／`REVOKE`（SQL-40・RLS-12 のポインタ、Issue #2235）の SQL 水準の結合テスト。
//! 設計記録: `docs/design/role-privilege-model.md`「Issue #2235 実装記録」。
//!
//! 公開 API だけで固定できる契約を扱う: 未束縛セッションは SEQUENCE・関数形でも対象の有無を問わず同じ
//! `42501`、構文エラー `42601`、許されない権限 `0LP01`、`ALL ... IN SCHEMA`・`PROCEDURE`・`ROUTINE` は
//! `0A000`、未束縛・所有者相当の採番と関数呼び出しは従来どおり通る（束縛ロールの検査は `core` の単体テスト
//! `sequence_function_acl_tests` が固定する。ロールを作る `Storage::create_role` が crate 内限定のため）。
//! IDENTITY／SERIAL 列への INSERT が SEQUENCE 権限なしで成功すること（受け入れ条件 A2）の未束縛側も確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_fixture(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("a", ColumnType::Text, true)],
        ))
        .expect("create table");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("t1"), session, sql)
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// 未束縛のセッションは、SEQUENCE・関数形でも存在する対象・存在しない対象のどちらも同じ `42501`・同じ文言。
#[test]
fn unbound_session_gets_identical_42501_for_sequence_and_function_forms() {
    let (_g, core) = open_fixture("sql40_seqfn_unbound");
    let mut owner = SessionState::default();
    owner.allow_ddl();
    run(&core, &mut owner, "CREATE SEQUENCE q").expect("sequence");
    run(&core, &mut owner, "CREATE FUNCTION f(x) AS x + 1").expect("function");
    for with_ddl in [false, true] {
        let mut s = SessionState::default();
        if with_ddl {
            s.allow_ddl();
        }
        let mut seen = Vec::new();
        for sql in [
            "GRANT USAGE ON SEQUENCE q TO alice",
            "GRANT SELECT ON SEQUENCE nosuch TO PUBLIC",
            "REVOKE ALL ON SEQUENCE q FROM alice CASCADE",
            "GRANT EXECUTE ON FUNCTION f TO alice",
            "GRANT EXECUTE ON FUNCTION nosuch(integer) TO PUBLIC",
            "REVOKE EXECUTE ON FUNCTION f FROM alice",
        ] {
            let e = run(&core, &mut s, sql).expect_err("denied");
            assert_eq!(e.wire_code(), "42501", "{sql}");
            seen.push(e.to_string());
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
    }
}

/// 構文・種別・範囲外の形は状態に依存しない固定の SQLSTATE。
#[test]
fn sequence_and_function_form_static_rejections() {
    let (_g, core) = open_fixture("sql40_seqfn_static");
    let mut s = SessionState::default();
    for (sql, expected) in [
        ("GRANT USAGE ON SEQUENCE q", "42601"),
        ("GRANT USAGE (c) ON SEQUENCE q TO x", "42601"),
        ("GRANT INSERT ON SEQUENCE q TO x", "0LP01"),
        ("GRANT EXECUTE ON SEQUENCE q TO x", "0LP01"),
        ("GRANT SELECT ON FUNCTION f TO x", "0LP01"),
        (
            "GRANT USAGE ON SEQUENCE q TO PUBLIC WITH GRANT OPTION",
            "0LP01",
        ),
        ("GRANT EXECUTE ON FUNCTION f(integer TO x", "42601"),
        ("GRANT USAGE ON ALL SEQUENCES IN SCHEMA s TO x", "0A000"),
        ("GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA s TO x", "0A000"),
        ("GRANT EXECUTE ON ALL ROUTINES IN SCHEMA s TO x", "0A000"),
        ("GRANT EXECUTE ON PROCEDURE p TO x", "0A000"),
        ("GRANT EXECUTE ON ROUTINE r TO x", "0A000"),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
}

/// (A2) の未束縛・所有者相当側: IDENTITY・SERIAL 列への INSERT と採番関数は従来どおり通る。
#[test]
fn unbound_and_owner_sessions_keep_sequences_and_functions_working() {
    let (_g, core) = open_fixture("sql40_seqfn_unbound_ops");
    let mut s = SessionState::default();
    s.allow_ddl();
    run(&core, &mut s, "CREATE SEQUENCE q").expect("sequence");
    run(
        &core,
        &mut s,
        "CREATE TABLE ti (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT)",
    )
    .expect("identity table");
    run(&core, &mut s, "INSERT INTO ti (v) VALUES ('a')").expect("identity insert");
    run(&core, &mut s, "SELECT setval('q', 10)").expect("setval");
    run(&core, &mut s, "CREATE FUNCTION f(x) AS x + 1").expect("function");
    let mut plain = SessionState::default();
    run(&core, &mut plain, "INSERT INTO ti (v) VALUES ('b')").expect("unbound insert");
    run(&core, &mut plain, "SELECT setval('q', 20)").expect("unbound setval");
}
