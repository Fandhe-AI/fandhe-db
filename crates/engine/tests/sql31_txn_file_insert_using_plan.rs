//! 明示トランザクション内のファイル形 `INSERT` と、書き込み済みテーブルへの
//! `USING PLAN`・`EXPLAIN` の結合テスト（Issue #1353。ポインタ:
//! `docs/spec/05-tasks.md` TASK-221・`docs/spec/04-behavior/sql-surface.md` SQL-31）。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として検証する。確認する契約:
//! (1) ファイル形 `INSERT` は `COMMIT` で確定し、`ROLLBACK`／drop で行・台帳とも残らない、
//! (2) 同一トランザクション内の同一 path は置換される、`operation_id` 再利用は `25000`、
//! (3) テナント境界（他テナントの同一 path の行を消さない）、
//! (4) dirty テーブルへの `USING PLAN` は未 commit の行を検索・辞書へ反映し、他セッションには
//! 見えない、(5) ROLLBACK 後にキャッシュ（辞書・検索系）へ痕跡が残らない（P0）、
//! (6) dirty テーブルへの `EXPLAIN`（全 variant）が受理される。

use std::sync::{Arc, Mutex};

use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
const EXPANSION: &str = r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#;

/// LLM へ渡されたプロンプト（辞書接頭辞を含む）を記録するスタブ。
struct PromptRecorder {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmClient for PromptRecorder {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.prompts.lock().expect("lock").push(prompt.to_string());
        Ok(EXPANSION.to_string())
    }
}

fn new_core(
    label: &str,
    with_embedder: bool,
) -> (EngineCore, Arc<Mutex<Vec<String>>>, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(PromptRecorder {
            prompts: prompts.clone(),
        }));
    if with_embedder {
        core = core.with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    }
    (core, prompts, path)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("sys"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn setup(core: &EngineCore) {
    ddl(
        core,
        &format!(
            "CREATE TABLE files (path TEXT NOT NULL, body TEXT NOT NULL, embedding VECTOR({DIM}))"
        ),
    );
}

fn file_insert(path: &str, body: &str, op: &str) -> String {
    format!("INSERT INTO files (path, body) VALUES ('{path}', '{body}') USING OPERATION_ID '{op}'")
}

fn auto(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn rows(outcome: SqlOutcome) -> Vec<Vec<String>> {
    match outcome {
        SqlOutcome::Query(result) | SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
            .collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn auto_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    rows(auto(core, tenant, sql))
}

struct Tx<'e> {
    core: &'e EngineCore,
    ctx: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Tx<'e> {
    fn new(core: &'e EngineCore, tenant: &str) -> Self {
        Self {
            core,
            ctx: ctx(tenant),
            session: SessionState::default(),
            txn: core.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
        self.core
            .execute_sql_in_txn(&self.ctx, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed inside the transaction, got {e:?}"))
    }

    fn err_code(&mut self, sql: &str) -> String {
        self.run(sql)
            .map(|o| panic!("{sql} must fail, got {o:?}"))
            .unwrap_err()
            .wire_code()
            .to_string()
    }
}

const SCAN_PATHS: &str = "SELECT path FROM files LIMIT 100";
const USING_PLAN: &str = "SELECT path FROM files USING PLAN('find alpha') LIMIT 10";

fn recorded(prompts: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    prompts.lock().expect("lock").clone()
}

// --- ファイル形 INSERT ---------------------------------------------------------

#[test]
fn file_insert_inside_transaction_is_isolated_until_commit() {
    let (core, _p, path) = new_core("txn-file-commit", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    let outcome = tx.ok(&file_insert("a.txt", "hello world", "f1"));
    assert!(matches!(outcome, SqlOutcome::Insert(_)));
    assert_eq!(
        rows(tx.ok(SCAN_PATHS)).len(),
        1,
        "own uncommitted chunk is visible"
    );
    assert!(
        auto_rows(&core, "alice", SCAN_PATHS).is_empty(),
        "other sessions must not see the uncommitted chunk"
    );
    tx.ok("COMMIT");
    assert_eq!(auto_rows(&core, "alice", SCAN_PATHS).len(), 1);
}

#[test]
fn file_insert_rollback_leaves_no_rows_and_no_ledger_entry() {
    let (core, _p, path) = new_core("txn-file-rollback", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("a.txt", "hello world", "f1"));
    tx.ok("ROLLBACK");
    assert!(auto_rows(&core, "alice", SCAN_PATHS).is_empty());
    // 台帳にも痕跡が無いので、同じ operation_id を autocommit で再送すると成功する。
    auto(&core, "alice", &file_insert("a.txt", "hello world", "f1"));
    assert_eq!(auto_rows(&core, "alice", SCAN_PATHS).len(), 1);
}

#[test]
fn file_insert_is_discarded_when_the_transaction_is_dropped() {
    let (core, _p, path) = new_core("txn-file-drop", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    {
        let mut tx = Tx::new(&core, "alice");
        tx.ok("BEGIN");
        tx.ok(&file_insert("a.txt", "hello world", "f1"));
        // COMMIT も ROLLBACK もせず drop（接続断相当）。
    }
    assert!(auto_rows(&core, "alice", SCAN_PATHS).is_empty());
    auto(&core, "alice", &file_insert("a.txt", "hello world", "f1"));
}

#[test]
fn same_path_inside_transaction_replaces_old_chunks_and_reused_operation_id_is_rejected() {
    let (core, _p, path) = new_core("txn-file-replace", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("a.txt", "first version", "f1"));
    tx.ok(&file_insert("a.txt", "second version", "f2"));
    let bodies = rows(tx.ok("SELECT body FROM files LIMIT 100"));
    assert_eq!(bodies.len(), 1, "old chunk must be replaced: {bodies:?}");
    assert!(bodies[0][0].contains("second"), "{bodies:?}");
    assert_eq!(tx.err_code(&file_insert("b.txt", "x", "f2")), "25000");
}

#[test]
fn file_insert_replacement_never_touches_other_tenants_rows() {
    let (core, _p, path) = new_core("txn-file-tenant", true);
    let _guard = CleanupGuard(path);
    setup(&core);
    auto(&core, "bob", &file_insert("a.txt", "bob content", "b1"));

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("a.txt", "alice content", "a1"));
    tx.ok("COMMIT");

    let bob_rows = auto_rows(&core, "bob", "SELECT body FROM files LIMIT 100");
    assert_eq!(bob_rows.len(), 1, "bob keeps his own row: {bob_rows:?}");
    assert!(bob_rows[0][0].contains("bob"), "{bob_rows:?}");
}

#[test]
fn file_insert_without_embedder_fails_the_transaction_closed() {
    let (core, _p, path) = new_core("txn-file-no-embedder", false);
    let _guard = CleanupGuard(path);
    setup(&core);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    let code = tx.err_code(&file_insert("a.txt", "hello", "f1"));
    assert_eq!(code, "XX000");
    assert_eq!(tx.txn.status(), TransactionStatus::Failed);
    tx.ok("ROLLBACK");
    assert!(auto_rows(&core, "alice", SCAN_PATHS).is_empty());
}

// --- USING PLAN（dirty テーブル）-------------------------------------------------

#[test]
fn using_plan_over_a_dirty_table_sees_uncommitted_rows_and_dictionary() {
    let (core, prompts, path) = new_core("txn-using-plan", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("zzmarker/alpha.txt", "alpha content", "f1"));
    let found = rows(tx.ok(USING_PLAN));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(tx.txn.status(), TransactionStatus::InTransaction);
    assert!(
        recorded(&prompts)
            .iter()
            .any(|p| p.contains("zzmarker/alpha.txt")),
        "the dictionary must reflect the uncommitted row"
    );

    // 別セッションからは行も辞書も見えない。
    let before = recorded(&prompts).len();
    assert!(auto_rows(&core, "alice", USING_PLAN).is_empty());
    assert!(recorded(&prompts)[before..]
        .iter()
        .all(|p| !p.contains("zzmarker")));

    tx.ok("ROLLBACK");
    let before = recorded(&prompts).len();
    assert!(auto_rows(&core, "alice", USING_PLAN).is_empty());
    assert!(recorded(&prompts)[before..]
        .iter()
        .all(|p| !p.contains("zzmarker")));
}

#[test]
fn using_plan_inside_transaction_applies_rls_to_rows_and_dictionary() {
    let (core, prompts, path) = new_core("txn-using-plan-rls", true);
    let _guard = CleanupGuard(path);
    setup(&core);
    auto(
        &core,
        "bob",
        &file_insert("bobsecret/alpha.txt", "alpha secret", "b1"),
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("alice/alpha.txt", "alpha mine", "a1"));
    let found = rows(tx.ok(USING_PLAN));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        recorded(&prompts).iter().all(|p| !p.contains("bobsecret")),
        "another tenant's private rows must not reach the dictionary"
    );
    tx.ok("ROLLBACK");
}

/// P0: ROLLBACK したトランザクションの行・辞書が、世代の再利用後にキャッシュ経由で
/// 別セッション・別テナントへ漏れない。
#[test]
fn rolled_back_using_plan_state_never_leaks_through_caches() {
    let (core, prompts, path) = new_core("txn-using-plan-cache", true);
    let _guard = CleanupGuard(path);
    setup(&core);
    auto(
        &core,
        "alice",
        &file_insert("seed/alpha.txt", "alpha seed", "s1"),
    );
    // キャッシュを温める。
    assert_eq!(auto_rows(&core, "alice", USING_PLAN).len(), 1);

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("zzleak/alpha.txt", "alpha leak", "f1"));
    assert_eq!(rows(tx.ok(USING_PLAN)).len(), 2);
    tx.ok("ROLLBACK");

    // 世代が再利用されうる別の書き込みの後に、autocommit で読む。
    auto(
        &core,
        "alice",
        &file_insert("after/alpha.txt", "alpha after", "s2"),
    );
    let before = recorded(&prompts).len();
    let alice_rows = auto_rows(&core, "alice", USING_PLAN);
    assert_eq!(alice_rows.len(), 2, "{alice_rows:?}");
    assert!(alice_rows.iter().all(|r| !r[0].contains("zzleak")));
    let hybrid = auto_rows(
        &core,
        "alice",
        &format!(
            "SELECT path FROM files ORDER BY embedding <=> '[{}]' LIMIT 10",
            ["0.1"; DIM as usize].join(", ")
        ),
    );
    assert!(hybrid.iter().all(|r| !r[0].contains("zzleak")));
    let other_tenant = auto_rows(&core, "carol", USING_PLAN);
    assert!(other_tenant.iter().all(|r| !r[0].contains("zzleak")));
    assert!(recorded(&prompts)[before..]
        .iter()
        .all(|p| !p.contains("zzleak")));
}

// --- EXPLAIN（dirty テーブル）-----------------------------------------------------

#[test]
fn explain_over_a_dirty_table_is_accepted_for_every_variant() {
    let (core, _p, path) = new_core("txn-explain", true);
    let _guard = CleanupGuard(path);
    setup(&core);

    let vector = format!("[{}]", ["0.1"; DIM as usize].join(", "));
    let statements = [
        format!("EXPLAIN SELECT path FROM files ORDER BY embedding <=> '{vector}' LIMIT 5"),
        "EXPLAIN SELECT COUNT(*) FROM files".to_string(),
        "EXPLAIN SELECT path FROM files WHERE path = 'a.txt' LIMIT 10".to_string(),
    ];

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok(&file_insert("a.txt", "alpha content", "f1"));
    for sql in &statements {
        let inside = rows(tx.ok(sql));
        let outside = auto_rows(&core, "alice", sql);
        assert_eq!(inside, outside, "{sql}");
    }
    let plan = rows(tx.ok("EXPLAIN SELECT path FROM files USING PLAN('find alpha') LIMIT 10"));
    assert!(!plan.is_empty());
    assert_eq!(tx.txn.status(), TransactionStatus::InTransaction);
    tx.ok("ROLLBACK");
}
/// 生 `redb::Database` を再オープンし、`user_rows/{table}` が物理的に存在するかを確認する
/// （呼び出し元は先に `EngineCore` を drop してファイルロックを解放しておくこと）。
fn user_rows_table_exists(path: &std::path::Path, table: &str) -> bool {
    use redb::{ReadableDatabase, TableHandle};
    let db = redb::Database::open(path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin read txn");
    let name = format!("user_rows/{table}");
    let found = read_txn
        .list_tables()
        .expect("list tables")
        .any(|handle| handle.name() == name);
    found
}

#[test]
fn using_plan_and_explain_over_an_empty_table_never_create_its_row_table() {
    let (core, _p, path) = new_core("txn-using-plan-no-create", true);
    let _guard = CleanupGuard(path.clone());
    setup(&core);
    ddl(
        &core,
        &format!(
            "CREATE TABLE other (path TEXT NOT NULL, body TEXT NOT NULL, embedding VECTOR({DIM}))"
        ),
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    // 別テーブルへの書き込みでトランザクションを dirty にする（`files` は行なしのまま）。
    tx.ok("INSERT INTO other (path, body) VALUES ('o.txt', 'beta') USING OPERATION_ID 'o1'");
    assert!(rows(tx.ok(USING_PLAN)).is_empty());
    tx.ok("EXPLAIN SELECT path FROM files USING PLAN('find alpha') LIMIT 10");
    tx.ok("COMMIT");
    drop(tx);
    drop(core);

    assert!(
        !user_rows_table_exists(&path, "files"),
        "reading through USING PLAN / EXPLAIN must not create the row table"
    );
}
