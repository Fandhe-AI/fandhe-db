//! `DROP TABLE|VIEW ... CASCADE／RESTRICT`（Issue #1641・親 #1638、対象ビヘイビア:
//! TABLE-37 (2)(3)(4)(5)・SQL-23）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-37。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → DDL 権限ゲート →
//! `sql::ddl::execute_drop_*` → `catalog::Storage::drop_tables`／`drop_view_with_behavior`）を
//! production 経路として検証する。契約:
//! - `RESTRICT`（明示・既定）は依存（ビュー・他表の FK）があれば `2BP01` で何も削除しない (2)
//! - `CASCADE` は依存する FK 制約とビュー（推移的）を連鎖削除し、参照元の表と行は残す (3)
//! - 連鎖削除は他テナントへ波及せず、報告・エラーに行数・テナント情報を含めない (4)
//! - 未対応の形（`DROP INDEX|TYPE ... CASCADE`）は `42601` のまま
//!
//! クラッシュ後の原子性は `table37_drop_cascade_crash_atomicity.rs` が検証する (5)。

use engine::catalog::{CascadedObject, DropBehavior};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
use crate::legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn granted() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = granted();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) -> SqlOutcome {
    run_as(core, "sys", sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    run_as(core, "sys", sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn exists(core: &EngineCore, relation: &str) -> bool {
    match run_as(core, "sys", &format!("SELECT id FROM {relation} LIMIT 1")) {
        Ok(_) => true,
        Err(e) => {
            assert_eq!(
                e.wire_code(),
                "42P01",
                "unexpected error probing {relation}"
            );
            false
        }
    }
}

fn row_count(core: &EngineCore, tenant: &str, table: &str) -> usize {
    match run_as(core, tenant, &format!("SELECT id FROM {table} LIMIT 1000"))
        .unwrap_or_else(|e| panic!("select {table}: {e:?}"))
    {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn insert(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    op: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, tenant, &format!("{sql} USING OPERATION_ID '{op}'"))
}

fn cascaded_of(out: SqlOutcome) -> Vec<CascadedObject> {
    match out {
        SqlOutcome::DropTable(o) => o.cascaded().to_vec(),
        SqlOutcome::DropView(o) => o.cascaded().to_vec(),
        other => panic!("expected drop outcome, got {other:?}"),
    }
}

/// 親 `p`、子 `c`（`pid` が `p` を参照）、推移的なビュー `v1`（`p` 上）・`v2`（`v1` 上）、
/// alice・bob の行を持つ fixture。
fn fixture(core: &EngineCore) {
    ok(core, "CREATE TABLE p (name TEXT)");
    ok(core, "CREATE TABLE c (pid BIGINT REFERENCES p, note TEXT)");
    ok(core, "CREATE VIEW v1 AS SELECT id FROM p LIMIT 5");
    ok(core, "CREATE VIEW v2 AS SELECT id FROM v1 LIMIT 5");
    insert(
        core,
        "alice",
        "INSERT INTO p (id, name) VALUES (1, 'a')",
        "p-a1",
    )
    .expect("p alice");
    insert(
        core,
        "bob",
        "INSERT INTO p (id, name) VALUES (2, 'b')",
        "p-b2",
    )
    .expect("p bob");
    insert(
        core,
        "alice",
        "INSERT INTO c (id, pid) VALUES (10, 1)",
        "c-a10",
    )
    .expect("c alice");
    insert(
        core,
        "bob",
        "INSERT INTO c (id, pid) VALUES (20, 2)",
        "c-b20",
    )
    .expect("c bob");
}

/// TABLE-37 (2): `RESTRICT`（明示・既定）はビュー依存・FK 依存で `2BP01`、何も削除しない。
#[test]
fn restrict_explicit_and_default_reject_with_2bp01() {
    let (core, _g) = new_core("drop-cascade-restrict");
    fixture(&core);
    for sql in [
        "DROP TABLE p",
        "DROP TABLE p RESTRICT",
        "DROP TABLE IF EXISTS p RESTRICT",
    ] {
        assert_eq!(code(&core, sql), "2BP01", "{sql}");
    }
    assert_eq!(code(&core, "DROP VIEW v1"), "2BP01");
    assert_eq!(code(&core, "DROP VIEW v1 RESTRICT"), "2BP01");
    for r in ["p", "c", "v1", "v2"] {
        assert!(exists(&core, r), "{r} must survive RESTRICT");
    }
    assert_eq!(row_count(&core, "alice", "c"), 1);
}

/// TABLE-37 (3)(4): `CASCADE` はビュー（推移的）と FK 制約を連鎖削除し、子表と行は残る。
#[test]
fn cascade_drops_views_and_foreign_key_but_keeps_child_rows() {
    let (core, _g) = new_core("drop-cascade-basic");
    fixture(&core);
    // 事前: FK が効いている。
    assert_eq!(
        insert(
            &core,
            "alice",
            "INSERT INTO c (id, pid) VALUES (11, 999)",
            "c-pre"
        )
        .expect_err("fk violation")
        .wire_code(),
        "23503"
    );
    let cascaded = cascaded_of(ok(&core, "DROP TABLE p CASCADE"));
    // 依存する側が先（v2 → v1）で、FK 制約は子表 c のもの。
    let pos = |pred: &dyn Fn(&CascadedObject) -> bool| cascaded.iter().position(pred);
    let v2 = pos(&|o| matches!(o, CascadedObject::View { name } if name == "v2")).expect("v2");
    let v1 = pos(&|o| matches!(o, CascadedObject::View { name } if name == "v1")).expect("v1");
    assert!(v2 < v1, "dependent view must come first: {cascaded:?}");
    assert!(
        cascaded
            .iter()
            .any(|o| matches!(o, CascadedObject::ForeignKey { table, .. } if table == "c")),
        "{cascaded:?}"
    );
    assert_eq!(cascaded.len(), 3, "{cascaded:?}");

    assert!(!exists(&core, "p"));
    assert!(!exists(&core, "v1") && !exists(&core, "v2"));
    assert!(exists(&core, "c"));
    // 子表の行はテナントごとに保持される。
    assert_eq!(row_count(&core, "alice", "c"), 1);
    assert_eq!(row_count(&core, "bob", "c"), 1);
    // FK が外れている: 存在しない参照値も挿入できる。
    insert(
        &core,
        "alice",
        "INSERT INTO c (id, pid) VALUES (11, 999)",
        "c-post",
    )
    .expect("fk must be gone");
    // 同名再作成は古い FK・ビューを引き継がない。
    ok(&core, "CREATE TABLE p (name TEXT)");
    assert!(!exists(&core, "v1"));
    insert(
        &core,
        "alice",
        "INSERT INTO c (id, pid) VALUES (12, 998)",
        "c-re",
    )
    .expect("recreated p has no FK from c");
}

/// 決定的な順序: 同じ fixture を 2 回作ると `cascaded()` が一致する。
#[test]
fn cascaded_order_is_deterministic() {
    let mut runs = Vec::new();
    for label in ["drop-cascade-det-1", "drop-cascade-det-2"] {
        let (core, _g) = new_core(label);
        fixture(&core);
        runs.push(cascaded_of(ok(&core, "DROP TABLE p CASCADE")));
    }
    assert_eq!(runs[0], runs[1]);
}

/// 複数名 + CASCADE: 並べた名前どうしの依存は連鎖報告に入らない。
#[test]
fn cascade_with_multiple_names_ignores_dependencies_among_listed() {
    let (core, _g) = new_core("drop-cascade-multi");
    ok(&core, "CREATE TABLE p (name TEXT)");
    ok(&core, "CREATE TABLE c (pid BIGINT REFERENCES p, note TEXT)");
    ok(&core, "CREATE TABLE x (name TEXT)");
    ok(&core, "CREATE TABLE d (xid BIGINT REFERENCES x, note TEXT)");
    let cascaded = cascaded_of(ok(&core, "DROP TABLE p, c, x CASCADE"));
    assert_eq!(cascaded.len(), 1, "{cascaded:?}");
    assert!(matches!(&cascaded[0], CascadedObject::ForeignKey { table, .. } if table == "d"));
    assert!(!exists(&core, "p") && !exists(&core, "c") && !exists(&core, "x"));
    assert!(exists(&core, "d"));
}

/// `IF EXISTS` + CASCADE: 不在名は skip し、存在する名前は連鎖削除する。
#[test]
fn cascade_with_if_exists() {
    let (core, _g) = new_core("drop-cascade-ie");
    fixture(&core);
    let cascaded = cascaded_of(ok(&core, "DROP TABLE IF EXISTS ghost, p CASCADE"));
    assert_eq!(cascaded.len(), 3, "{cascaded:?}");
    assert!(!exists(&core, "p"));
    // 全件不在なら何も起きない。
    assert!(cascaded_of(ok(&core, "DROP TABLE IF EXISTS g1, g2 CASCADE")).is_empty());
}

/// 自己参照 FK だけを持つ表の CASCADE は連鎖報告が空。
#[test]
fn cascade_on_self_referencing_table_reports_nothing() {
    let (core, _g) = new_core("drop-cascade-self");
    ok(
        &core,
        "CREATE TABLE s (parent BIGINT REFERENCES s, name TEXT)",
    );
    assert!(cascaded_of(ok(&core, "DROP TABLE s CASCADE")).is_empty());
    assert!(!exists(&core, "s"));
}

/// `DROP VIEW ... CASCADE`: 依存ビューも消え、基底の表は残る。
#[test]
fn drop_view_cascade_removes_dependent_views_only() {
    let (core, _g) = new_core("drop-cascade-view");
    fixture(&core);
    let cascaded = cascaded_of(ok(&core, "DROP VIEW v1 CASCADE"));
    assert_eq!(
        cascaded,
        vec![CascadedObject::View {
            name: "v2".to_string()
        }]
    );
    assert!(!exists(&core, "v1") && !exists(&core, "v2"));
    assert!(exists(&core, "p") && exists(&core, "c"));
    // 不在は 42P01（ビュー不在）、IF EXISTS なら成功。
    assert_eq!(code(&core, "DROP VIEW v1 CASCADE"), "42P01");
    ok(&core, "DROP VIEW IF EXISTS v1 CASCADE");
}

/// 判定順序: 権限の無いセッションは存否に関わらず `42501`、明示トランザクション内は `0A000`。
#[test]
fn permission_and_transaction_gates_apply_to_cascade() {
    let (core, _g) = new_core("drop-cascade-gates");
    fixture(&core);
    let mut denied = SessionState::default();
    for sql in [
        "DROP TABLE p CASCADE",
        "DROP TABLE ghost CASCADE",
        "DROP VIEW v1 CASCADE",
    ] {
        let err = core
            .execute_sql_in_session_legacy(&ctx("alice"), &mut denied, sql)
            .expect_err("denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
    }
    let mut s = granted();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "DROP TABLE p CASCADE")
        .expect_err("DDL in txn");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_txn(&ctx("sys"), &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    for r in ["p", "c", "v1", "v2"] {
        assert!(exists(&core, r));
    }
}

/// 未対応の形は fail-closed（`42601`）のまま。`RESTRICT` は INDEX／TYPE でも受理される。
#[test]
fn index_and_type_cascade_stay_unsupported() {
    let (core, _g) = new_core("drop-cascade-unsupported");
    ok(&core, "CREATE TABLE a (name TEXT)");
    ok(&core, "CREATE INDEX idx ON a (name)");
    ok(&core, "CREATE TYPE mood AS ENUM ('ok', 'bad')");
    assert_eq!(code(&core, "DROP INDEX idx CASCADE"), "42601");
    assert_eq!(code(&core, "DROP TYPE mood CASCADE"), "42601");
    assert_eq!(code(&core, "DROP TABLE a CASCADE RESTRICT"), "42601");
    assert_eq!(code(&core, "DROP VIEW v CASCADE CASCADE"), "42601");
    ok(&core, "DROP INDEX idx RESTRICT");
    ok(&core, "DROP TYPE mood RESTRICT");
    assert!(exists(&core, "a"));
}

/// `cascade`／`restrict` という名前の表は従来どおり名前として解釈される。
#[test]
fn cascade_named_table_is_still_a_name() {
    let (core, _g) = new_core("drop-cascade-name");
    ok(&core, "CREATE TABLE cascade (name TEXT)");
    ok(&core, "CREATE TABLE restrict (name TEXT)");
    ok(&core, "DROP TABLE cascade");
    ok(&core, "DROP TABLE restrict CASCADE");
    assert!(!exists(&core, "cascade") && !exists(&core, "restrict"));
}

/// `DropBehavior` の既定は `Restrict`（無指定の意味）。
#[test]
fn default_behavior_is_restrict() {
    assert_eq!(DropBehavior::default(), DropBehavior::Restrict);
}
