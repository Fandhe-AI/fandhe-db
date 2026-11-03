//! BYTEA 列の UNIQUE 制約と `ON CONFLICT` の衝突判定が、入力表記ではなく復号後の
//! バイト列の比較であることの結合テスト（Issue #2422。ポインタ: TABLE-16・TASK-341、
//! 仕様は `docs/spec/04-behavior/`。親 Issue #2420）。
//!
//! `EngineCore::execute_sql_in_session`（production 経路）で、16 進表記の大文字・小文字違いは
//! 衝突し、長さの違う値（前置一致・末尾 `00` 付き）は衝突しないこと、空バイト列と NULL の区別、
//! `DO NOTHING`／`DO UPDATE`／UPDATE／バッチ内重複／再オープン後、他テナントの同値で違反しない
//! こととエラー文言に値が漏れないことを固定する。本番コードは変更しない。

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

fn allowed() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut allowed(), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
}

/// 失敗するはずの SQL の `wire_code` とエラー文言を返す。
fn fail(core: &EngineCore, tenant: &str, sql: &str) -> (&'static str, String) {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code(), e.to_string()),
    }
}

fn affected(o: &SqlOutcome) -> u64 {
    match o {
        SqlOutcome::Insert(i) => i.rows_affected,
        other => panic!("expected Insert outcome, got {other:?}"),
    }
}

fn ids(core: &EngineCore, tenant: &str, table: &str) -> Vec<u64> {
    let sql = format!("SELECT id FROM {table} LIMIT 100");
    let mut v: Vec<u64> = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::SignedInteger(n)) => u64::try_from(*n).expect("id >= 0"),
            Some(Cell::Integer(n)) => *n,
            other => panic!("unexpected id cell: {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn note_of(core: &EngineCore, tenant: &str, id: u64) -> Cell {
    let r = core
        .execute_sql(
            &ctx(tenant),
            &format!("SELECT note FROM bu WHERE id = {id} LIMIT 1"),
        )
        .expect("select note");
    r.rows[0].cells[0].clone()
}

const CREATE: &str = "CREATE TABLE bu (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, \
    b BYTEA UNIQUE, note TEXT)";

fn setup(label: &str) -> (std::path::PathBuf, EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    (path, core, guard)
}

/// 操作ごとに一意な `USING OPERATION_ID` を付けた INSERT を組み立てる。
fn ins(id: u64, b: &str, note: &str, tail: &str) -> String {
    format!(
        "INSERT INTO bu (id, embedding, b, note) VALUES ({id}, '[0.1,0.2]', {b}, '{note}') \
         {tail} USING OPERATION_ID 'op-{id}-{note}'"
    )
}

#[test]
fn insert_case_variants_of_hex_collide() {
    let (_p, core, _g) = setup("bu-case");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "a", ""));
    assert_eq!(
        fail(&core, "alice", &ins(2, r"'\xdeadbeef'", "b", "")).0,
        "23505"
    );
    assert_eq!(
        fail(&core, "alice", &ins(3, r"'\xDeAdBeEf'", "c", "")).0,
        "23505"
    );
    assert_eq!(ids(&core, "alice", "bu"), vec![1]);
}

#[test]
fn insert_different_length_values_do_not_collide() {
    let (_p, core, _g) = setup("bu-len");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "a", ""));
    ok(&core, "alice", &ins(2, r"'\xdeadbeef00'", "b", ""));
    ok(&core, "alice", &ins(3, r"'\xDEAD'", "c", ""));
    ok(&core, "alice", &ins(4, r"'\xDE'", "d", ""));
    ok(&core, "alice", &ins(5, r"'\x00'", "e", ""));
    ok(&core, "alice", &ins(6, r"'\x0000'", "f", ""));
    assert_eq!(ids(&core, "alice", "bu"), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn empty_bytes_collide_with_each_other_but_not_with_null() {
    let (_p, core, _g) = setup("bu-empty");
    ok(&core, "alice", &ins(1, r"'\x'", "a", ""));
    assert_eq!(fail(&core, "alice", &ins(2, r"'\x'", "b", "")).0, "23505");
    // NULL は何行でも入り（NULLS DISTINCT）、空バイト列とも衝突しない。
    ok(&core, "alice", &ins(3, "NULL", "c", ""));
    ok(&core, "alice", &ins(4, "NULL", "d", ""));
    assert_eq!(ids(&core, "alice", "bu"), vec![1, 3, 4]);
}

#[test]
fn on_conflict_do_nothing_matches_by_decoded_bytes() {
    let (_p, core, _g) = setup("bu-nothing");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "a", ""));
    let o = ok(
        &core,
        "alice",
        &ins(2, r"'\xdeadbeef'", "b", "ON CONFLICT (b) DO NOTHING"),
    );
    assert_eq!(affected(&o), 0);
    assert_eq!(ids(&core, "alice", "bu"), vec![1]);
    let o = ok(
        &core,
        "alice",
        &ins(3, r"'\xdeadbeef00'", "c", "ON CONFLICT (b) DO NOTHING"),
    );
    assert_eq!(affected(&o), 1);
    assert_eq!(ids(&core, "alice", "bu"), vec![1, 3]);
}

#[test]
fn on_conflict_do_update_hits_existing_row_by_decoded_bytes() {
    let (_p, core, _g) = setup("bu-update-set");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "old", ""));
    let o = ok(
        &core,
        "alice",
        &ins(
            99,
            r"'\xdeadbeef'",
            "new",
            "ON CONFLICT (b) DO UPDATE SET note = EXCLUDED.note",
        ),
    );
    assert_eq!(affected(&o), 1);
    assert_eq!(ids(&core, "alice", "bu"), vec![1]);
    assert_eq!(note_of(&core, "alice", 1), Cell::Text("new".to_string()));
    // 長さ違いは新規挿入になる。
    ok(
        &core,
        "alice",
        &ins(
            2,
            r"'\xDEADBE'",
            "x",
            "ON CONFLICT (b) DO UPDATE SET note = EXCLUDED.note",
        ),
    );
    assert_eq!(ids(&core, "alice", "bu"), vec![1, 2]);
}

#[test]
fn update_collision_is_decided_on_decoded_bytes() {
    let (_p, core, _g) = setup("bu-upd");
    ok(&core, "alice", &ins(1, r"'\xdeadbeef'", "a", ""));
    ok(&core, "alice", &ins(2, r"'\x01'", "b", ""));
    let (c, _) = fail(
        &core,
        "alice",
        r"UPDATE bu SET b = '\xDEADBEEF' WHERE id = 2 USING OPERATION_ID 'op-u1'",
    );
    assert_eq!(c, "23505");
    // 自分自身への大文字表記の自己代入は衝突しない。
    ok(
        &core,
        "alice",
        r"UPDATE bu SET b = '\xDEADBEEF' WHERE id = 1 USING OPERATION_ID 'op-u2'",
    );
}

#[test]
fn batch_with_case_variant_duplicates_writes_nothing() {
    let (_p, core, _g) = setup("bu-batch");
    let batch = |tail: &str, op: &str| {
        format!(
            "INSERT INTO bu (id, embedding, b, note) VALUES \
             (1, '[0.1,0.2]', '\\xABCD', 'a'), (2, '[0.1,0.2]', '\\xabcd', 'b') \
             {tail} USING OPERATION_ID '{op}'"
        )
    };
    assert_eq!(fail(&core, "alice", &batch("", "op-b1")).0, "23505");
    assert!(ids(&core, "alice", "bu").is_empty());
    // TEXT 列と同じく、DO NOTHING でもバッチ内の対象キー重複は拒否される。
    let (c, _) = fail(
        &core,
        "alice",
        &batch("ON CONFLICT (b) DO NOTHING", "op-b2"),
    );
    assert_eq!(c, "22000");
    assert!(ids(&core, "alice", "bu").is_empty());
}

#[test]
fn uniqueness_survives_reopen() {
    let (path, core, _g) = setup("bu-reopen");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "a", ""));
    drop(core);
    let core = core_at(&path);
    assert_eq!(
        fail(&core, "alice", &ins(2, r"'\xdeadbeef'", "b", "")).0,
        "23505"
    );
    let o = ok(
        &core,
        "alice",
        &ins(3, r"'\xdeadbeef'", "c", "ON CONFLICT (b) DO NOTHING"),
    );
    assert_eq!(affected(&o), 0);
    assert_eq!(ids(&core, "alice", "bu"), vec![1]);
}

#[test]
fn tenants_do_not_collide_and_errors_do_not_leak() {
    let (_p, core, _g) = setup("bu-tenant");
    ok(&core, "alice", &ins(1, r"'\xDEADBEEF'", "a", ""));
    // 他テナントの同値では違反しない。
    ok(&core, "bob", &ins(1, r"'\xdeadbeef'", "b", ""));
    let o = ok(
        &core,
        "bob",
        &ins(2, r"'\xCAFE'", "c", "ON CONFLICT (b) DO NOTHING"),
    );
    assert_eq!(affected(&o), 1);
    // 逆方向: bob だけが持つ値を alice が入れられる。
    ok(&core, "alice", &ins(3, r"'\xcafe'", "d", ""));
    assert_eq!(ids(&core, "alice", "bu"), vec![1, 3]);
    assert_eq!(ids(&core, "bob", "bu"), vec![1, 2]);
    // 自テナント内の重複のエラー文言に値・テナント名が出ない。
    let (c, msg) = fail(&core, "alice", &ins(4, r"'\xdeadbeef'", "e", ""));
    assert_eq!(c, "23505");
    let lower = msg.to_lowercase();
    assert!(!lower.contains("deadbeef"), "leaked value: {msg}");
    assert!(
        !lower.contains("alice") && !lower.contains("bob"),
        "leaked tenant: {msg}"
    );
}

#[test]
fn composite_unique_compares_each_column_with_length() {
    let (_p, core, _g) = setup("bu-composite");
    ok(
        &core,
        "alice",
        "CREATE TABLE bc (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, \
         b1 BYTEA, b2 BYTEA, UNIQUE (b1, b2))",
    );
    let ins2 = |id: u64, b1: &str, b2: &str, tail: &str| {
        format!(
            "INSERT INTO bc (id, embedding, b1, b2) VALUES ({id}, '[0.1,0.2]', '{b1}', '{b2}') \
             {tail} USING OPERATION_ID 'op-c{id}'"
        )
    };
    ok(&core, "alice", &ins2(1, r"\x01", r"\x0203", ""));
    // 連結すると同じ 010203 だが別キー。
    ok(&core, "alice", &ins2(2, r"\x0102", r"\x03", ""));
    assert_eq!(
        fail(&core, "alice", &ins2(3, r"\x01", r"\x0203", "")).0,
        "23505"
    );
    let o = ok(
        &core,
        "alice",
        &ins2(4, r"\x0102", r"\x03", "ON CONFLICT (b2, b1) DO NOTHING"),
    );
    assert_eq!(affected(&o), 0);
    assert_eq!(ids(&core, "alice", "bc"), vec![1, 2]);
}
