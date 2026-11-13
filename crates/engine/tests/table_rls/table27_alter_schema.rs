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
//! テナント SCHEMA 間の実移動（Issue #3198。表・行・索引・制約・`OWNED BY` の SEQUENCE を移し、ビューは
//! 移さない。移動先の名前の衝突は `42P07`、関数の依存・SEQUENCE の外部参照は `2BP01`、移動先の CREATE
//! 権限が無ければ `42501`）も本ファイルで固定する。`public` とテナント SCHEMA の間の移動は意図的な `42501`
//! （設計記録 D7）。

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
    // `OWNER TO` は Issue #2236 で対応した。この未束縛のセッションは束縛ゲートで `42501`
    // （実行の契約は `sql40_object_owner.rs`）。
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER SCHEMA a OWNER TO bob"),
        "42501"
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

fn query_cells(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match ok_as(core, tenant, s, sql) {
        SqlOutcome::Query(r) => format!("{:?}", r.rows),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `s1.t`（SERIAL 主キー・UNIQUE・CHECK・索引つき）を作って 2 行入れる。
fn setup_schema_table(core: &EngineCore, s: &mut SessionState) {
    for sql in [
        "CREATE SCHEMA s1",
        "CREATE SCHEMA s2",
        "CREATE TABLE s1.t (id BIGSERIAL PRIMARY KEY, name TEXT UNIQUE, n INTEGER CHECK (n > 0))",
        "CREATE INDEX ti ON s1.t (n)",
        "INSERT INTO s1.t (name, n) VALUES ('a', 1), ('b', 2)",
    ] {
        ok_as(core, "alice", s, sql);
    }
}

/// テナント SCHEMA 間の移動で、行・索引・制約・SERIAL の採番が移動先へ移り、移動元は空になる
/// （末尾 `;` つきの単独送信。再オープン後も保たれる）。
#[test]
fn set_schema_between_tenant_schemas_moves_table_indexes_constraints_and_sequences() {
    let (p, _g, core) = open_fixture("alt-tbl-move");
    let mut s = allowed_session();
    setup_schema_table(&core, &mut s);
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA s1");
    ok_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s2;");
    let check = |core: &EngineCore, s: &mut SessionState| {
        assert_eq!(count(core, "alice", s, "s2.t"), "Integer(2)".to_string());
        assert_eq!(code_as(core, "alice", s, "SELECT * FROM s1.t"), "42P01");
        assert_eq!(
            code_as(
                core,
                "alice",
                s,
                "INSERT INTO s2.t (name, n) VALUES ('a', 5)"
            ),
            "23505"
        );
        assert_eq!(
            code_as(
                core,
                "alice",
                s,
                "INSERT INTO s2.t (name, n) VALUES ('c', 0)"
            ),
            "23514"
        );
        // SERIAL の採番は巻き戻らず続く。
        ok_as(
            core,
            "alice",
            s,
            "INSERT INTO s2.t (name, n) VALUES ('c', 3)",
        );
        assert_eq!(
            count(core, "alice", s, "s2.t WHERE n = 3 AND id > 2"),
            "Integer(1)".to_string()
        );
        ok_as(core, "alice", s, "DELETE FROM s2.t WHERE n = 3");
    };
    check(&core, &mut s);
    let core = reopen(core, &p);
    let mut s = allowed_session();
    assert_eq!(
        count(&core, "alice", &mut s, "s2.t"),
        "Integer(2)".to_string()
    );
    // 索引は移動先の名前で外せる。移動元の名前では 42704。
    assert_eq!(code_as(&core, "alice", &mut s, "DROP INDEX s1.ti"), "42704");
    ok_as(&core, "alice", &mut s, "DROP INDEX s2.ti");
    // 移動元は空になり、DROP SCHEMA（RESTRICT）が通る。移動先は空でない。
    ok_as(&core, "alice", &mut s, "DROP SCHEMA s1");
    assert_eq!(code_as(&core, "alice", &mut s, "DROP SCHEMA s2"), "2BP01");
}

/// ビューは移らず、表への参照だけが追随する。FK・POLICY の追随と、同一 SCHEMA への移動の no-op。
#[test]
fn set_schema_keeps_views_and_follows_foreign_keys() {
    let (_p, _g, core) = open_fixture("alt-tbl-deps");
    let mut s = allowed_session();
    setup_schema_table(&core, &mut s);
    ok_as(&core, "alice", &mut s, "CREATE SCHEMA s3");
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE VIEW s1.v AS SELECT id, n FROM s1.t",
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE TABLE s3.child (cid BIGINT PRIMARY KEY, pname TEXT REFERENCES s1.t (name))",
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "INSERT INTO s3.child (cid, pname) VALUES (1, 'a')",
    );
    ok_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s1");
    ok_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s2");
    // ビューは s1 に残り、移動先の表を読む。
    assert_eq!(
        query_cells(&core, "alice", &mut s, "SELECT COUNT(*) FROM s1.v"),
        query_cells(&core, "alice", &mut s, "SELECT COUNT(*) FROM s2.t")
    );
    // FK は移動先の表を指し続ける。
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "INSERT INTO s3.child (cid, pname) VALUES (2, 'zz')"
        ),
        "23503"
    );
    ok_as(
        &core,
        "alice",
        &mut s,
        "INSERT INTO s3.child (cid, pname) VALUES (2, 'b')",
    );
}

/// 移動先に同名の表・索引・SEQUENCE があると 42P07 で、表・行・索引は不変（再オープン後も）。
#[test]
fn set_schema_name_collisions_are_rejected_without_side_effects() {
    let (p, _g, core) = open_fixture("alt-tbl-collide");
    let mut s = allowed_session();
    setup_schema_table(&core, &mut s);
    for (setup, label) in [
        ("CREATE TABLE s2.t (a INTEGER)", "table"),
        ("CREATE TABLE s2.ti (a INTEGER)", "index name"),
        ("CREATE SEQUENCE s2.t_id_seq", "sequence name"),
    ] {
        ok_as(&core, "alice", &mut s, setup);
        assert_eq!(
            code_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s2"),
            "42P07",
            "{label}"
        );
        let cleanup = match label {
            "table" => "DROP TABLE s2.t",
            "index name" => "DROP TABLE s2.ti",
            _ => "DROP SEQUENCE s2.t_id_seq",
        };
        ok_as(&core, "alice", &mut s, cleanup);
    }
    assert_eq!(
        count(&core, "alice", &mut s, "s1.t"),
        "Integer(2)".to_string()
    );
    let core = reopen(core, &p);
    let mut s = allowed_session();
    assert_eq!(
        count(&core, "alice", &mut s, "s1.t"),
        "Integer(2)".to_string()
    );
    ok_as(&core, "alice", &mut s, "DROP INDEX s1.ti");
    // 衝突が無くなれば移せる。
    ok_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s2");
}

/// OWNED BY の SEQUENCE を別の表が DEFAULT で参照していると 2BP01 で拒否し、何も移らない。
#[test]
fn set_schema_rejects_when_owned_sequence_is_used_by_another_table() {
    let (_p, _g, core) = open_fixture("alt-tbl-seqdep");
    let mut s = allowed_session();
    setup_schema_table(&core, &mut s);
    ok_as(
        &core,
        "alice",
        &mut s,
        "CREATE TABLE s1.u (k BIGINT DEFAULT nextval('s1.t_id_seq'))",
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA s2"),
        "2BP01"
    );
    assert_eq!(
        count(&core, "alice", &mut s, "s1.t"),
        "Integer(2)".to_string()
    );
}

/// 越境・不在・権限の判定順。`public` との間は両方向とも 42501、移動先不在は 3F000、権限の無い
/// セッションは対象の有無によらず 42501。
#[test]
fn set_schema_boundaries_and_gate_for_tenant_schema_tables() {
    let (_p, _g, core) = open_fixture("alt-tbl-bound");
    let mut s = allowed_session();
    setup_schema_table(&core, &mut s);
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA public"),
        "42501"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE s1.t SET SCHEMA ghost"),
        "3F000"
    );
    assert_eq!(
        code_as(&core, "alice", &mut s, "ALTER TABLE s1.nope SET SCHEMA s2"),
        "42P01"
    );
    ok_as(&core, "bob", &mut s, "CREATE SCHEMA only_bob");
    assert_eq!(
        code_as(
            &core,
            "alice",
            &mut s,
            "ALTER TABLE s1.t SET SCHEMA only_bob"
        ),
        "3F000"
    );
    let mut denied = SessionState::default();
    for sql in [
        "ALTER TABLE s1.t SET SCHEMA s2",
        "ALTER TABLE s1.nope SET SCHEMA ghost",
    ] {
        assert_eq!(code_as(&core, "alice", &mut denied, sql), "42501", "{sql}");
    }
    assert_eq!(
        count(&core, "alice", &mut s, "s1.t"),
        "Integer(2)".to_string()
    );
}
