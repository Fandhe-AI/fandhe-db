//! 対象省略の `INSERT ... ON CONFLICT DO NOTHING`（SQL-49、Issue #1736）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-49。関連ポインタ: TABLE-12・RLS-9
//! （他テナントの鍵は衝突しない）・RECOVER-1〜3（`operation_id`・台帳）。
//!
//! `EngineCore::execute_sql_in_session`（構文段 → 束縛 → `sql::exec::execute_upsert` →
//! `tenant::upsert_typed_rows_impl` の `UpsertTarget::AnyUnique`）を production 経路として検証する。
//! 疑似列 `id` を持つ従来方式の表（`LegacyExec`）で、行キー・PRIMARY KEY・UNIQUE 制約・
//! UNIQUE 索引（列・式・部分）のどれに違反した行も「その行だけ」が飛ばされることを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

static OP: AtomicU64 = AtomicU64::new(0);

fn op() -> String {
    format!("s49-op-{}", OP.fetch_add(1, Ordering::Relaxed))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    s: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session_legacy(&ctx(tenant), s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> &'static str {
    run(core, s, tenant, sql).expect_err(sql).wire_code()
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        other => panic!("expected Insert/Returning outcome, got {other:?}"),
    }
}

/// `docs (k PRIMARY KEY, u UNIQUE, w, n, note)` を作った環境。`w` は後続テストで索引を張る。
fn setup(label: &str) -> (EngineCore, CleanupGuard, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    run(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE docs (k TEXT PRIMARY KEY, u TEXT UNIQUE, w TEXT, n INTEGER, note TEXT)",
    )
    .expect("create table");
    (core, guard, s)
}

fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let mut s = SessionState::default();
    let out = core
        .execute_sql_in_session(&ctx(tenant), &mut s, "SELECT k FROM docs LIMIT 100")
        .expect("select");
    let SqlOutcome::Query(result) = out else {
        panic!("expected Query outcome");
    };
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn seed(core: &EngineCore, s: &mut SessionState, tenant: &str) {
    run(
        core,
        s,
        tenant,
        &format!(
            "INSERT INTO docs (id, k, u, w, n) VALUES (1, 'k1', 'u1', 'w1', 1) \
             USING OPERATION_ID '{}'",
            op()
        ),
    )
    .expect("seed");
}

fn upsert(id: u64, k: &str, u: &str, w: &str) -> String {
    format!(
        "INSERT INTO docs (id, k, u, w, n) VALUES ({id}, '{k}', '{u}', '{w}', 5) \
         ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
        op()
    )
}

#[test]
fn each_unique_violation_skips_only_that_row() {
    let (core, _g, mut s) = setup("s49-each");
    seed(&core, &mut s, "alice");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE UNIQUE INDEX docs_w ON docs (w)",
    )
    .expect("index");
    // 行キー（id=1）・PRIMARY KEY（k）・UNIQUE 制約（u）・UNIQUE 索引（w）の各違反。
    let sql = format!(
        "INSERT INTO docs (id, k, u, w, n) VALUES \
         (1, 'a', 'a', 'a', 5), (2, 'k1', 'b', 'b', 5), (3, 'c', 'u1', 'c', 5), \
         (4, 'd', 'd', 'w1', 5), (5, 'ok', 'ok', 'ok', 5) \
         ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(
        affected(run(&core, &mut s, "alice", &sql).expect("skip")),
        1
    );
    assert_eq!(ids(&core, "alice"), vec![1, 5]);
}

#[test]
fn in_statement_duplicates_insert_first_and_skip_the_rest() {
    let (core, _g, mut s) = setup("s49-instmt");
    let sql = format!(
        "INSERT INTO docs (id, k, u, w, n) VALUES \
         (1, 'x', 'p', 'a', 1), (1, 'y', 'q', 'b', 1), (2, 'x', 'r', 'c', 1), \
         (3, 'z', 'p', 'd', 1), (4, 'm', NULL, 'e', 1), (5, 'n', NULL, 'f', 1) \
         ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    // id 重複・PK 重複・UNIQUE 重複は後の行を飛ばし、NULL を含む鍵は衝突しない。
    assert_eq!(affected(run(&core, &mut s, "alice", &sql).expect("ok")), 3);
    assert_eq!(ids(&core, "alice"), vec![1, 4, 5]);
}

#[test]
fn expression_and_partial_unique_indexes_are_honoured() {
    let (core, _g, mut s) = setup("s49-expr");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE UNIQUE INDEX docs_lw ON docs ((lower(w)))",
    )
    .expect("expr index");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE UNIQUE INDEX docs_pn ON docs (note) WHERE n > 0",
    )
    .expect("partial index");
    run(
        &core,
        &mut s,
        "alice",
        &format!(
            "INSERT INTO docs (id, k, u, w, n, note) VALUES (1, 'k1', 'u1', 'AbC', 1, 'n1') \
             USING OPERATION_ID '{}'",
            op()
        ),
    )
    .expect("seed");
    let sql = format!(
        "INSERT INTO docs (id, k, u, w, n, note) VALUES \
         (2, 'k2', 'u2', 'aBc', 1, 'x'), \
         (3, 'k3', 'u3', 'w3', 1, 'n1'), \
         (4, 'k4', 'u4', 'w4', 0, 'n1'), \
         (5, 'k5', 'u5', 'w5', 1, 'n5') \
         ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    // 2 は式一意索引、3 は部分索引（述語内）の違反。4 は述語外なので挿入される。
    assert_eq!(affected(run(&core, &mut s, "alice", &sql).expect("ok")), 2);
    assert_eq!(ids(&core, "alice"), vec![1, 4, 5]);
}

#[test]
fn other_constraint_violations_still_error_without_side_effects() {
    let (core, _g, mut s) = setup("s49-other");
    seed(&core, &mut s, "alice");
    let before = ids(&core, "alice");
    // NOT NULL 違反（PRIMARY KEY 列 k の NULL）は従来どおりエラー。
    let sql = format!(
        "INSERT INTO docs (id, k, u, w, n) VALUES (2, NULL, 'z', 'z', 1) \
         ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
        op()
    );
    assert_eq!(code(&core, &mut s, "alice", &sql), "23502");
    assert_eq!(ids(&core, "alice"), before);
}

#[test]
fn syntax_forms_are_pinned() {
    let (core, _g, mut s) = setup("s49-syntax");
    for sql in [
        "INSERT INTO docs (id, k, u, w, n) VALUES (9, 'q', 'q', 'q', 1) \
         ON CONFLICT DO UPDATE SET n = 2 USING OPERATION_ID 'op-s49-a'",
        "INSERT INTO docs (id, k, u, w, n) VALUES (9, 'q', 'q', 'q', 1) \
         ON CONFLICT ON CONSTRAINT docs_pkey DO NOTHING USING OPERATION_ID 'op-s49-b'",
        "INSERT INTO docs (id, k, u, w, n) VALUES (9, 'q', 'q', 'q', 1) \
         ON CONFLICT () DO NOTHING USING OPERATION_ID 'op-s49-c'",
    ] {
        assert_eq!(code(&core, &mut s, "alice", sql), "42601", "{sql}");
    }
    assert!(ids(&core, "alice").is_empty());
}

#[test]
fn returning_excludes_skipped_rows_and_matches_target_form_shape() {
    let (core, _g, mut s) = setup("s49-returning");
    seed(&core, &mut s, "alice");
    let sql = |target: &str| {
        format!(
            "INSERT INTO docs (id, k, u, w, n) VALUES (2, 'k1', 'x', 'x', 1), (3, 'k3', 'u3', 'w3', 1) \
             ON CONFLICT {target} DO NOTHING RETURNING id, k USING OPERATION_ID '{}'",
            op()
        )
    };
    let omitted = run(&core, &mut s, "alice", &sql("")).expect("omitted");
    let SqlOutcome::Returning(o) = omitted else {
        panic!("expected Returning outcome");
    };
    assert_eq!(o.rows_affected, 1);
    assert_eq!(o.result.rows.len(), 1);
    assert_eq!(
        o.result.rows[0].cells,
        vec![Cell::Integer(3), Cell::Text("k3".to_string())]
    );
    // 対象を指定した形（PRIMARY KEY 列）と列・行の形が一致する。
    let explicit = run(&core, &mut s, "alice", &sql("(k)")).expect("explicit");
    let SqlOutcome::Returning(e) = explicit else {
        panic!("expected Returning outcome");
    };
    assert_eq!(e.result.columns, o.result.columns);
    // 全行を飛ばしたら 0 行。
    let none = run(&core, &mut s, "alice", &sql("")).expect("all skipped");
    let SqlOutcome::Returning(n) = none else {
        panic!("expected Returning outcome");
    };
    assert_eq!(n.rows_affected, 0);
    assert!(n.result.rows.is_empty());
}

#[test]
fn other_tenants_keys_do_not_conflict() {
    let (core, _g, mut s) = setup("s49-tenant");
    seed(&core, &mut s, "bob");
    // alice は bob と同じ id・鍵でも挿入される（RLS-9）。
    let n = affected(run(&core, &mut s, "alice", &upsert(1, "k1", "u1", "w1")).expect("insert"));
    assert_eq!(n, 1);
    assert_eq!(ids(&core, "alice"), vec![1]);
    assert_eq!(ids(&core, "bob"), vec![1]);
    // alice 自身の鍵とは衝突して飛ばされる。
    let n = affected(run(&core, &mut s, "alice", &upsert(2, "k1", "u9", "w9")).expect("skip"));
    assert_eq!(n, 0);
}

#[test]
fn ledger_replay_is_rejected_and_all_skipped_statement_is_recorded() {
    let (core, _g, mut s) = setup("s49-ledger");
    seed(&core, &mut s, "alice");
    let sql = "INSERT INTO docs (id, k, u, w, n) VALUES (7, 'k1', 'x', 'x', 1) \
               ON CONFLICT DO NOTHING USING OPERATION_ID 'op-s49-fixed'";
    assert_eq!(
        affected(run(&core, &mut s, "alice", sql).expect("first")),
        0
    );
    // 全行を飛ばしても台帳に残るため、同じ operation_id の再送は 23505。
    assert_eq!(code(&core, &mut s, "alice", sql), "23505");
    // 同じ operation_id で対象指定形・plain INSERT を送ると内容不一致（22023）。
    let explicit = "INSERT INTO docs (id, k, u, w, n) VALUES (7, 'k1', 'x', 'x', 1) \
                    ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-s49-fixed'";
    assert_eq!(code(&core, &mut s, "alice", explicit), "22023");
    let plain = "INSERT INTO docs (id, k, u, w, n) VALUES (7, 'k1', 'x', 'x', 1) \
                 USING OPERATION_ID 'op-s49-fixed'";
    assert_eq!(code(&core, &mut s, "alice", plain), "22023");
}

#[test]
fn allocated_table_supports_omitted_target() {
    let path = unique_db_path("s49-allocated");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut s,
        "CREATE TABLE t (code TEXT PRIMARY KEY, note TEXT)",
    )
    .expect("create");
    let ins = |vals: &str| {
        format!(
            "INSERT INTO t (code, note) VALUES {vals} ON CONFLICT DO NOTHING USING OPERATION_ID '{}'",
            op()
        )
    };
    let first = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut s,
            &ins("('a', 'x'), ('a', 'y'), ('b', 'z')"),
        )
        .expect("first");
    assert_eq!(affected(first), 2);
    let second = core
        .execute_sql_in_session(&ctx("alice"), &mut s, &ins("('a', 'q'), ('c', 'r')"))
        .expect("second");
    assert_eq!(affected(second), 1);
}
