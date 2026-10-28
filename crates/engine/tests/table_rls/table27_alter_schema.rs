//! `ALTER SCHEMA ... RENAME TO` と `ALTER TABLE ... SET SCHEMA`（TABLE-27・SQL-23、Issue #1716）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-27、設計記録は
//! `docs/design/schema-tenant-namespace.md`（D2・D3・D7・D10 と §8）。
//!
//! `table27_schema_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - RENAME は内部 ID を保つ 1 行の付け替えで、旧名は不在・テナント独立・再オープン後も保持
//! - RENAME の拒否（組み込み名 `42501`・不在／他テナント `3F000`・重複 `42P06`・予約名 `42939`）
//! - SET SCHEMA は `public` から `public` が no-op、`public` からテナント SCHEMA は `42501`、
//!   不在／他テナントは同一の `3F000`、システム SCHEMA・`pg_temp`・一時表は `0A000`
//! - 拒否後も表・行・索引が不変で、権限ゲートは対象の有無によらず `42501`
//!
//! テナント SCHEMA に表を置く経路は #1717 の担当のため、テナント SCHEMA 間の実移動は本ファイルの
//! 対象外（設計記録 §8 の申し送り）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn core_over(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_over(Storage::open(&path).expect("open storage"));
    (path, guard, core)
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
}

fn ok_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlOutcome {
    run_as(core, tenant, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn err_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
    match run_as(core, tenant, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn code_as(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
    err_as(core, tenant, s, sql).wire_code()
}

fn reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    core_over(Storage::open(path).expect("reopen storage"))
}

fn count(core: &EngineCore, tenant: &str, s: &mut SessionState, table: &str) -> String {
    match ok_as(core, tenant, s, &format!("SELECT COUNT(*) FROM {table}")) {
        SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn rename_moves_the_name_and_survives_reopen() {
    let (p, _g, core) = open_fixture("alt-sch-basic");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    assert!(matches!(
        run_as(&core, "alice", &mut s, "ALTER SCHEMA a RENAME TO b;"),
        Ok(SqlOutcome::AlterSchema(_))
    ));
    // 旧名は SCHEMA 不在（3F000）、新名は SCHEMA が在って表が無い（42P01）で観測する。
    let probe_old = "SELECT x FROM a.t ORDER BY x LIMIT 1";
    let probe_new = "SELECT x FROM b.t ORDER BY x LIMIT 1";
    assert_eq!(code_as(&core, "alice", &mut s, probe_old), "3F000");
    assert_eq!(code_as(&core, "alice", &mut s, probe_new), "42P01");
    let core = reopen(core, &p);
    assert_eq!(code_as(&core, "alice", &mut s, probe_new), "42P01");
    // 旧名は再び作れ、新名は使用中。
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    assert_eq!(code_as(&core, "alice", &mut s, "CREATE SCHEMA b"), "42P06");
}

#[test]
fn rename_rejections() {
    let (_p, _g, core) = open_fixture("alt-sch-rej");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA c");
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA only_bob");
    for old in ["public", "PUBLIC", "pg_catalog", "information_schema"] {
        let sql = format!("ALTER SCHEMA {old} RENAME TO z");
        assert_eq!(code_as(&core, "alice", &mut s, &sql), "42501", "{sql}");
    }
    // 不在と他テナントにしか無い名前は同一の応答（SQLSTATE・文言）。
    let ghost = err_as(&core, "alice", &mut s, "ALTER SCHEMA ghost RENAME TO z");
    let other = err_as(&core, "alice", &mut s, "ALTER SCHEMA only_bob RENAME TO z");
    assert_eq!(ghost.wire_code(), "3F000");
    assert_eq!(other.wire_code(), "3F000");
    assert_eq!(
        ghost.to_string().replace("ghost", "N"),
        other.to_string().replace("only_bob", "N")
    );
    for new in ["c", "a", "public", "information_schema"] {
        let sql = format!("ALTER SCHEMA a RENAME TO {new}");
        assert_eq!(code_as(&core, "alice", &mut s, &sql), "42P06", "{sql}");
    }
    for new in ["pg_x", "PG_X"] {
        let sql = format!("ALTER SCHEMA a RENAME TO {new}");
        assert_eq!(code_as(&core, "alice", &mut s, &sql), "42939", "{sql}");
    }
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER SCHEMA a OWNER TO bob"),
        "0A000"
    );
    for sql in [
        "ALTER SCHEMA",
        "ALTER SCHEMA a",
        "ALTER SCHEMA a RENAME",
        "ALTER SCHEMA a RENAME TO",
        "ALTER SCHEMA x.a RENAME TO b",
        "ALTER SCHEMA a RENAME TO x.b",
        "ALTER SCHEMA a RENAME TO 1x",
        "ALTER SCHEMA a RENAME TO b extra",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42601", "{sql}");
    }
    // 拒否は何も変えない。
    ok_as(&core, "alice", &mut s, "ALTER SCHEMA a RENAME TO a2");
}

#[test]
fn rename_is_tenant_independent() {
    let (_p, _g, core) = open_fixture("alt-sch-tenant");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA a");
    ok_as(&core, "alice", &mut s, "ALTER SCHEMA a RENAME TO b");
    // bob の `a` は残り、`b` は存在しない。
    assert_eq!(code_as(&core, "bob", &mut s, "CREATE SCHEMA a"), "42P06");
    assert_eq!(
        code_as(&core, "bob", &mut s, "ALTER SCHEMA b RENAME TO c"),
        "3F000"
    );
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA b");
}

fn setup_items(core: &EngineCore, s: &mut SessionState) {
    ok_as(
        core,
        "alice",
        s,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, code TEXT UNIQUE, qty INTEGER)",
    );
    ok_as(
        core,
        "alice",
        s,
        "INSERT INTO items (id, code, qty) VALUES (1, 'c1', 10), (2, 'c2', 20)",
    );
    ok_as(
        core,
        "bob",
        s,
        "INSERT INTO items (id, code, qty) VALUES (1, 'c1', 99)",
    );
    ok_as(core, "alice", s, "CREATE SEQUENCE sq");
}

#[test]
fn set_schema_public_to_public_is_a_noop_that_keeps_rows_and_indexes() {
    let (p, _g, core) = open_fixture("alt-tbl-noop");
    let mut s = allowed_session();
    setup_items(&core, &mut s);
    let before = count(&core, "alice", &mut s, "items");
    for sql in [
        "ALTER TABLE items SET SCHEMA public",
        "ALTER TABLE public.items SET SCHEMA PUBLIC",
    ] {
        assert!(matches!(
            run_as(&core, "alice", &mut s, sql),
            Ok(SqlOutcome::AlterTable(_))
        ));
    }
    let core = reopen(core, &p);
    assert_eq!(count(&core, "alice", &mut s, "items"), before);
    assert_eq!(count(&core, "alice", &mut s, "public.items"), before);
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "INSERT INTO items (id, code, qty) VALUES (3, 'c1', 1)"
        ),
        "23505"
    );
    assert_eq!(
        code_as(
            &core,
            "bob",
            &mut s,
            "INSERT INTO items (id, code, qty) VALUES (1, 'c9', 1)"
        ),
        "23505"
    );
}

#[test]
fn set_schema_boundary_and_rejections() {
    let (p, _g, core) = open_fixture("alt-tbl-rej");
    let mut s = allowed_session();
    setup_items(&core, &mut s);
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s");
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA only_bob");
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE VIEW v AS SELECT id FROM items",
    );
    let before = count(&core, "alice", &mut s, "items");
    // public -> テナント SCHEMA は 42501（AC2）。
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE items SET SCHEMA s"),
        "42501"
    );
    // 不在と他テナントの SCHEMA は同一応答。
    let ghost = err_as(&core, "alice", &mut s, "ALTER TABLE items SET SCHEMA ghost");
    let other = err_as(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE items SET SCHEMA only_bob",
    );
    assert_eq!(ghost.wire_code(), "3F000");
    assert_eq!(other.wire_code(), "3F000");
    assert_eq!(
        ghost.to_string().replace("ghost", "N"),
        other.to_string().replace("only_bob", "N")
    );
    for target in ["pg_catalog", "information_schema", "pg_temp"] {
        let sql = format!("ALTER TABLE items SET SCHEMA {target}");
        assert_eq!(code_as(&core, "alice", &mut s, &sql), "0A000", "{sql}");
    }
    // 判定順: 表の存在が先、次に移動先、最後に境界。
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE nope SET SCHEMA s"),
        "42P01"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE nope SET SCHEMA ghost"),
        "42P01"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE v SET SCHEMA s"),
        "42809"
    );
    // 構文。
    for sql in [
        "ALTER TABLE items SET SCHEMA a.b",
        "ALTER TABLE items SET SCHEMA",
        "ALTER TABLE items SET SCHEMA s, ADD COLUMN x INTEGER",
        "ALTER TABLE items ADD COLUMN x INTEGER, SET SCHEMA s",
    ] {
        assert_eq!(code_as(&core, "alice", &mut s, sql), "42601", "{sql}");
    }
    // 一時表は従来どおり 0A000。
    ok_as(&core, "alice", &mut s, "CREATE TEMP TABLE tmpt (a INTEGER)");
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE tmpt SET SCHEMA public"),
        "0A000"
    );
    // 拒否後も表・行・索引・SEQUENCE が不変（再オープン後も）。
    let core = reopen(core, &p);
    let mut s2 = allowed_session();
    assert_eq!(count(&core, "alice", &mut s2, "items"), before);
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s2,
            "INSERT INTO items (id, code, qty) VALUES (3, 'c1', 1)"
        ),
        "23505"
    );
    ok_as(&core, "alice", &mut s2, "DROP SEQUENCE sq");
}

#[test]
fn permission_gate_precedes_catalog_lookups() {
    let (_p, _g, core) = open_fixture("alt-perm");
    let mut allowed = allowed_session();
    setup_items(&core, &mut allowed);
    ok_as(&core, "alice", &mut allowed, "CREATE SCHEMA s");
    let mut denied = SessionState::default();
    for sql in [
        "ALTER SCHEMA s RENAME TO s2",
        "ALTER SCHEMA ghost RENAME TO s2",
        "ALTER SCHEMA public RENAME TO s2",
        "ALTER TABLE items SET SCHEMA public",
        "ALTER TABLE items SET SCHEMA s",
        "ALTER TABLE nope SET SCHEMA ghost",
    ] {
        assert_eq!(code_as(&core, "alice", &mut denied, sql), "42501", "{sql}");
    }
    ok_as(&core, "alice", &mut allowed, "ALTER SCHEMA s RENAME TO s2");
}

#[test]
fn alter_schema_inside_explicit_transaction_is_rejected() {
    let (_p, _g, core) = open_fixture("alt-sch-txn");
    let mut s = allowed_session();
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &ctx("alice"),
            &mut s,
            &mut txn,
            "ALTER SCHEMA a RENAME TO b",
        )
        .expect_err("must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(code_as(&core, "alice", &mut s, "CREATE SCHEMA a"), "42P06");
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA b");
}

#[test]
fn alter_schema_is_not_blocked_by_session_temp_tables_and_sequence_set_schema_stays_unsupported() {
    let (_p, _g, core) = open_fixture("alt-sch-temp");
    let mut s = allowed_session();
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE TEMP TABLE alter (schema INTEGER)",
    );
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA a");
    ok_as(&core, "alice", &mut s, "ALTER SCHEMA a RENAME TO rename");
    ok_as(&core, "alice", &mut s, "CREATE SEQUENCE sq");
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "ALTER SEQUENCE sq SET SCHEMA public"
        ),
        "0A000"
    );
}
