//! 既定の invoker（本番の PL/pgSQL トリガ実行）で、分割実行 DML（`UPDATE`／`DELETE ... USING
//! OPERATION_ID '<id>' PARTITIONED [CHUNK n]`）が対象表の行トリガ（BEFORE／AFTER ROW）を発火することの、
//! 公開 API だけを使う結合テスト（TABLE-45・SQL-19・RLS-10。Issue #3283）。
//!
//! 文トリガのある表は固定文言の `0A000`（黙って飛ばさない）。テナント境界・RLS ポリシー併用も固定する。
//! 設計記録は `docs/design/trigger-partitioned-dml.md`。対象表は `s.tt`（共有 public 表へのトリガは定義時に
//! 拒否される。Issue #3361）、監査表 `audit` は public のまま。

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

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn err_of(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn ints(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let SqlOutcome::Query(q) = ok(core, tenant, sql) else {
        panic!("expected query");
    };
    let mut v: Vec<i64> = q
        .rows
        .into_iter()
        .filter_map(|r| r.cells.into_iter().next())
        .map(|c| match c {
            Cell::SignedInteger(v) => v,
            Cell::Integer(v) => i64::try_from(v).expect("fits"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Update(o) => o.rows_affected,
        SqlOutcome::Delete(o) => o.rows_affected,
        other => panic!("expected update or delete, got {other:?}"),
    }
}

fn tables(core: &EngineCore, tenant: &str) {
    ok(core, tenant, "CREATE SCHEMA s");
    ok(core, tenant, "CREATE TABLE s.tt (n INTEGER)");
    ok(
        core,
        tenant,
        "CREATE TABLE audit (n INTEGER, lv TEXT, op TEXT)",
    );
}

fn func(core: &EngineCore, tenant: &str, name: &str, body: &str) {
    ok(
        core,
        tenant,
        &format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ {body} $$"),
    );
}

fn trigger(core: &EngineCore, tenant: &str, name: &str, spec: &str, f: &str) {
    let (when, level) = spec.split_once(" FOR ").expect("spec has FOR");
    ok(
        core,
        tenant,
        &format!("CREATE TRIGGER {name} {when} ON s.tt FOR {level} EXECUTE FUNCTION {f}()"),
    );
}

const BUMP: &str = "BEGIN NEW.n := NEW.n + 100; RETURN NEW; END";
const AUDIT_NEW: &str =
    "BEGIN INSERT INTO audit (n, lv, op) VALUES (NEW.n, TG_LEVEL, TG_OP); RETURN NULL; END";
const AUDIT_OLD: &str =
    "BEGIN INSERT INTO audit (n, lv, op) VALUES (OLD.n, TG_LEVEL, TG_OP); RETURN NULL; END";
const KEEP_OLD: &str = "BEGIN RETURN OLD; END";

fn seed(core: &EngineCore, tenant: &str) {
    ok(core, tenant, "INSERT INTO s.tt (n) VALUES (1), (2), (3)");
}

// UPDATE: BEFORE ROW の書き換えと AFTER ROW の監査が、チャンク幅によらず全行に 1 回ずつ効く。
// 末尾 `;` 付きの単独送信でも同じ（psql と同じ形）。
#[test]
fn update_fires_row_triggers_for_every_chunk_width() {
    for (chunk, semi) in [(1, ""), (2, ""), (100, ""), (1, ";")] {
        let (_g, core) = open(&format!("t45part-upd-{chunk}{}", semi.len()));
        tables(&core, "t1");
        func(&core, "t1", "bump", BUMP);
        func(&core, "t1", "audit_new", AUDIT_NEW);
        trigger(&core, "t1", "tb", "BEFORE UPDATE FOR EACH ROW", "bump");
        trigger(&core, "t1", "ta", "AFTER UPDATE FOR EACH ROW", "audit_new");
        seed(&core, "t1");
        let out = ok(
            &core,
            "t1",
            &format!(
                "UPDATE s.tt SET n = n WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK {chunk}{semi}"
            ),
        );
        assert_eq!(affected(out), 3, "chunk {chunk}");
        assert_eq!(
            ints(&core, "t1", "SELECT n FROM s.tt"),
            vec![101, 102, 103],
            "chunk {chunk}"
        );
        assert_eq!(
            ints(&core, "t1", "SELECT n FROM audit"),
            vec![101, 102, 103],
            "chunk {chunk}"
        );
    }
}

// DELETE: BEFORE ROW が `OLD` を返せば削除され、AFTER ROW の監査が行ごとに 1 回入る。
#[test]
fn delete_fires_row_triggers_for_every_chunk_width() {
    for chunk in [1, 2, 100] {
        let (_g, core) = open(&format!("t45part-del-{chunk}"));
        tables(&core, "t1");
        func(&core, "t1", "keep", KEEP_OLD);
        func(&core, "t1", "audit_old", AUDIT_OLD);
        trigger(&core, "t1", "tb", "BEFORE DELETE FOR EACH ROW", "keep");
        trigger(&core, "t1", "ta", "AFTER DELETE FOR EACH ROW", "audit_old");
        seed(&core, "t1");
        let out = ok(
            &core,
            "t1",
            &format!(
                "DELETE FROM s.tt WHERE n > 0 USING OPERATION_ID 'd1' PARTITIONED CHUNK {chunk}"
            ),
        );
        assert_eq!(affected(out), 3, "chunk {chunk}");
        assert!(ints(&core, "t1", "SELECT n FROM s.tt").is_empty());
        assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![1, 2, 3]);
    }
}

// 文トリガのある表の分割実行は固定文言の `0A000`。行・トリガ・ジョブ記録を残さない。
// intentional: docs/design/trigger-partitioned-dml.md
// 根拠は同書 D1（文トリガは非原子実行で保証できない）。
#[test]
fn statement_trigger_is_rejected_with_fixed_message() {
    let (_g, core) = open("t45part-stmt");
    tables(&core, "t1");
    func(&core, "t1", "audit_old", AUDIT_OLD);
    trigger(
        &core,
        "t1",
        "ts",
        "AFTER DELETE FOR EACH STATEMENT",
        "audit_old",
    );
    seed(&core, "t1");
    let e = err_of(
        &core,
        "t1",
        "DELETE FROM s.tt WHERE n > 0 USING OPERATION_ID 'd1' PARTITIONED CHUNK 1",
    );
    assert_eq!(e.wire_code(), "0A000");
    let msg = format!("{e:?}");
    assert!(msg.contains("statement triggers are not supported for partitioned DML"));
    assert!(!msg.contains("ts") || msg.contains("statement"), "{msg}");
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1, 2, 3]);
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
}

// テナント境界: t1 のトリガは t2 の分割実行で発火せず、t1 の監査行は t2 から見えない。
#[test]
fn trigger_does_not_fire_across_tenants() {
    let (_g, core) = open("t45part-tenant");
    tables(&core, "t1");
    ok(&core, "t2", "CREATE SCHEMA s");
    ok(&core, "t2", "CREATE TABLE s.tt (n INTEGER)");
    func(&core, "t1", "bump", BUMP);
    func(&core, "t1", "audit_new", AUDIT_NEW);
    trigger(&core, "t1", "tb", "BEFORE UPDATE FOR EACH ROW", "bump");
    trigger(&core, "t1", "ta", "AFTER UPDATE FOR EACH ROW", "audit_new");
    seed(&core, "t1");
    seed(&core, "t2");
    let out = ok(
        &core,
        "t2",
        "UPDATE s.tt SET n = n + 10 WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK 1",
    );
    assert_eq!(affected(out), 3);
    assert_eq!(ints(&core, "t2", "SELECT n FROM s.tt"), vec![11, 12, 13]);
    assert!(ints(&core, "t1", "SELECT n FROM audit").is_empty());
    assert!(ints(&core, "t2", "SELECT n FROM audit").is_empty());
    assert_eq!(ints(&core, "t1", "SELECT n FROM s.tt"), vec![1, 2, 3]);
}

// RLS ポリシー併用: ポリシーを通る行だけが対象で、トリガも通った行にだけ発火する。
#[test]
fn row_policy_narrows_targets_before_triggers_fire() {
    let (_g, core) = open("t45part-policy");
    tables(&core, "t1");
    func(&core, "t1", "audit_new", AUDIT_NEW);
    trigger(&core, "t1", "ta", "AFTER UPDATE FOR EACH ROW", "audit_new");
    seed(&core, "t1");
    ok(&core, "t1", "ALTER TABLE s.tt ENABLE ROW LEVEL SECURITY");
    ok(
        &core,
        "t1",
        "CREATE POLICY p ON s.tt FOR ALL USING (n >= 2) WITH CHECK (true)",
    );
    let out = ok(
        &core,
        "t1",
        "UPDATE s.tt SET n = n + 10 WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK 1",
    );
    assert_eq!(affected(out), 2);
    assert_eq!(ints(&core, "t1", "SELECT n FROM audit"), vec![12, 13]);
}
