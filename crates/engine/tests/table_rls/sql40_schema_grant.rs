//! SCHEMA 単位の `GRANT`／`REVOKE`（SQL-40・RLS-12 のポインタ、Issue #2234）の SQL 水準の結合テスト。設計記録:
//! `docs/design/role-privilege-model.md`「Issue #2234 実装記録」。
//!
//! 公開 API だけで固定できる契約を扱う: 未束縛セッションは SCHEMA 形でも対象の有無を問わず同じ `42501`、
//! 構文エラー `42601`、SCHEMA に許されない権限 `0LP01`、組み込み SCHEMA・`ALL SEQUENCES IN SCHEMA` は
//! `0A000`、未束縛の SCHEMA 修飾参照は従来どおり通る（束縛ロールの USAGE 検査は `core` の単体テストが固定する。
//! ロールを作る `Storage::create_role` が crate 内限定のため）。

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

/// 未束縛のセッションは、SCHEMA 形でも存在する SCHEMA・存在しない SCHEMA のどちらも同じ `42501`・同じ文言。
#[test]
fn unbound_session_gets_identical_42501_for_schema_forms() {
    let (_g, core) = open_fixture("sql40_schema_unbound");
    let mut owner = SessionState::default();
    owner.allow_ddl();
    run(&core, &mut owner, "CREATE SCHEMA s").expect("schema");
    for with_ddl in [false, true] {
        let mut s = SessionState::default();
        if with_ddl {
            s.allow_ddl();
        }
        let mut seen = Vec::new();
        for sql in [
            "GRANT USAGE ON SCHEMA s TO alice",
            "GRANT CREATE ON SCHEMA nosuch TO PUBLIC",
            "REVOKE ALL ON SCHEMA s FROM alice CASCADE",
            "GRANT SELECT ON ALL TABLES IN SCHEMA s TO alice",
            "REVOKE SELECT ON ALL TABLES IN SCHEMA nosuch FROM alice",
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
fn schema_form_static_rejections() {
    let (_g, core) = open_fixture("sql40_schema_static");
    let mut s = SessionState::default();
    for (sql, expected) in [
        ("GRANT USAGE ON SCHEMA s", "42601"),
        ("GRANT USAGE ON SCHEMA a.b TO x", "42601"),
        ("GRANT USAGE (c) ON SCHEMA s TO x", "42601"),
        ("GRANT SELECT ON SCHEMA s TO x", "0LP01"),
        (
            "GRANT USAGE ON SCHEMA s TO PUBLIC WITH GRANT OPTION",
            "0LP01",
        ),
        ("GRANT USAGE ON ALL TABLES IN SCHEMA s TO x", "0LP01"),
        ("GRANT USAGE ON SCHEMA public TO x", "0A000"),
        ("GRANT USAGE ON SCHEMA pg_catalog TO x", "0A000"),
        ("GRANT SELECT ON ALL TABLES IN SCHEMA public TO x", "0A000"),
        ("GRANT USAGE ON ALL SEQUENCES IN SCHEMA s TO x", "0A000"),
        ("GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA s TO x", "0A000"),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
}

/// 現在ロール未束縛のセッションは SCHEMA 修飾の参照を従来どおり検査しない（束縛ロールにだけ USAGE を課す）。
#[test]
fn unbound_session_still_reads_qualified_names() {
    let (_g, core) = open_fixture("sql40_schema_unbound_read");
    let mut s = SessionState::default();
    s.allow_ddl();
    run(&core, &mut s, "CREATE SCHEMA s").expect("schema");
    run(&core, &mut s, "CREATE TABLE s.u (id BIGINT PRIMARY KEY)").expect("table");
    run(&core, &mut s, "SELECT id FROM s.u").expect("unbound sessions are not checked");
    // 他テナントから見ると不在と同じ `3F000`。
    let mut other = SessionState::default();
    assert_eq!(
        core.execute_sql_in_session(&ctx("t2"), &mut other, "SELECT id FROM s.u")
            .expect_err("missing")
            .wire_code(),
        "3F000"
    );
}
