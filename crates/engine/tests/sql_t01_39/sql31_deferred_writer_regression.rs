//! writer 遅延取得（Issue #2400）の前後で既存挙動が変わらないことを固定する結合テスト
//! （Issue #2402。親 #2396・TASK-333。ポインタ: SQL-19・SQL-31）。
//!
//! 「BEGIN → 読み取りだけの区間（writer なし）→ 最初の書き込み（writer 取得）→ 読み取り」の形で、
//! (1) 自トランザクションの未 commit 変更の読み取り、(2) COMMIT までの隔離（他セッション・他テナント）、
//! (3) ROLLBACK 後のキャッシュ（辞書・検索・集計）非汚染、(4) ファイル形 `INSERT` と `USING PLAN`・
//! `EXPLAIN` の既存挙動、(5) 最初の書き込みでの `55P03` の副作用なしを確かめる。
//! writer の有無そのものの直接観測はクレート内の `core/deferred_writer_regression_tests.rs` が担う。
//!
//! 同一スレッドで順に実行し時間計測に依存しない（writer 保持中の別セッション書き込みは自己
//! デッドロック防御で即 `55P03`）。他の兄弟 Issue と両立させるため、取得前の読み取りが BEGIN 時点の
//! スナップショットを見ることはアサートせず、本 Tx が読んだ表へ別セッションが commit した後に
//! 本 Tx が書く形も作らない（別セッションの書き込みは本 Tx が読まない `probe` 表へ向ける）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

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

use crate::temp_db;
// `CREATE TABLE` を従来方式（疑似列 `id` あり）の表作成へ振り向ける（TABLE-23・Issue #2199）。
use crate::legacy_table;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 8;
const EXPANSION: &str = r#"{"search_terms": ["alpha"], "path_hint": null, "kind_hint": null}"#;
const USING_PLAN: &str = "SELECT path FROM files USING PLAN('find alpha') LIMIT 10";
const SCAN_PATHS: &str = "SELECT path FROM files LIMIT 100";
const SCAN_DOCS: &str = "SELECT n FROM docs LIMIT 100";

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

type Prompts = Arc<Mutex<Vec<String>>>;

fn new_core(label: &str) -> (EngineCore, Prompts, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(PromptRecorder {
            prompts: prompts.clone(),
        }))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_write_lock_wait(Duration::from_millis(100));
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        format!(
            "CREATE TABLE files (path TEXT NOT NULL, body TEXT NOT NULL, embedding VECTOR({DIM}))"
        ),
        "CREATE TABLE docs (n BIGINT, embedding VECTOR(2))".to_string(),
        "CREATE TABLE probe (n BIGINT, embedding VECTOR(2))".to_string(),
    ] {
        core.execute_sql_in_session_legacy(&ctx("sys"), &mut session, &sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    (core, prompts, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn file_insert(path: &str, body: &str, op: &str) -> String {
    format!("INSERT INTO files (path, body) VALUES ('{path}', '{body}') USING OPERATION_ID '{op}'")
}

fn doc_insert(id: u64, n: i64, op: &str) -> String {
    format!(
        "INSERT INTO docs (id, n, embedding) VALUES ({id}, {n}, '[1.0, 0.0]') \
         USING OPERATION_ID '{op}'"
    )
}

fn probe_insert(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO probe (id, n, embedding) VALUES ({id}, 1, '[1.0, 0.0]') \
         USING OPERATION_ID '{op}'"
    )
}

fn rows(outcome: SqlOutcome) -> Vec<Vec<String>> {
    match outcome {
        SqlOutcome::Query(result) | SqlOutcome::Explain(result) | SqlOutcome::Fetch(result) => {
            result
                .rows
                .iter()
                .map(|r| r.cells.iter().map(|c| format!("{c:?}")).collect())
                .collect()
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn auto(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn auto_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    rows(auto(core, tenant, sql))
}

fn auto_code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut SessionState::default(), sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
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

    fn rows(&mut self, sql: &str) -> Vec<Vec<String>> {
        rows(self.ok(sql))
    }

    fn err_code(&mut self, sql: &str) -> String {
        self.run(sql)
            .map(|o| panic!("{sql} must fail, got {o:?}"))
            .unwrap_err()
            .wire_code()
            .to_string()
    }
}

fn recorded(prompts: &Prompts) -> Vec<String> {
    prompts.lock().expect("lock").clone()
}

fn contains_marker(rows: &[Vec<String>], marker: &str) -> bool {
    rows.iter().any(|r| r.iter().any(|c| c.contains(marker)))
}

/// 取得の前後で、自トランザクションの未 commit の INSERT・UPDATE・DELETE が Scan・集計に反映され、
/// 他セッションからは COMMIT まで見えない。
#[test]
fn read_your_writes_across_writer_acquisition() {
    let (core, _p, _g) = new_core("sql31-regress-ryw");
    auto(&core, "alice", &doc_insert(1, 10, "seed"));

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert_eq!(tx.rows(SCAN_DOCS).len(), 1);
    assert_eq!(
        tx.rows("SELECT COUNT(*) FROM docs"),
        vec![vec!["Integer(1)"]]
    );

    tx.ok(&doc_insert(2, 20, "w1"));
    assert_eq!(tx.rows(SCAN_DOCS).len(), 2);
    assert_eq!(
        tx.rows("SELECT COUNT(*) FROM docs"),
        vec![vec!["Integer(2)"]]
    );

    tx.ok("UPDATE docs SET n = 99 WHERE id = 1 USING OPERATION_ID 'w2'");
    assert!(contains_marker(&tx.rows(SCAN_DOCS), "99"));

    tx.ok("DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'w3'");
    assert_eq!(
        tx.rows("SELECT COUNT(*) FROM docs"),
        vec![vec!["Integer(1)"]]
    );

    let outside = auto_rows(&core, "alice", SCAN_DOCS);
    assert_eq!(outside.len(), 1);
    assert!(!contains_marker(&outside, "99"), "{outside:?}");

    tx.ok("COMMIT");
    let committed = auto_rows(&core, "alice", SCAN_DOCS);
    assert_eq!(committed.len(), 1);
    assert!(contains_marker(&committed, "99"), "{committed:?}");
}

/// 書き込み前に DECLARE したカーソルは書き込み前の行集合を保ち、書き込み後の DECLARE は未 commit の行を含む。
#[test]
fn cursor_declared_before_first_write_keeps_its_rows() {
    let (core, _p, _g) = new_core("sql31-regress-cursor");
    auto(&core, "alice", &doc_insert(1, 10, "seed"));

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    tx.ok("DECLARE c1 CURSOR FOR SELECT n FROM docs LIMIT 100");
    tx.ok(&doc_insert(2, 20, "w1"));
    tx.ok("DECLARE c2 CURSOR FOR SELECT n FROM docs LIMIT 100");
    assert_eq!(tx.rows("FETCH 100 FROM c1").len(), 1);
    assert_eq!(tx.rows("FETCH 100 FROM c2").len(), 2);
    tx.ok("ROLLBACK");
}

/// 読み取り後の最初の書き込みの後、別セッション・別テナントの読み取りは待たされず未 commit の行が見えない。
/// 別セッションの書き込みは `55P03`（writer を保持している証跡）。COMMIT 後は同じテナントだけに見える。
#[test]
fn isolation_until_commit_with_read_prefix() {
    let (core, _p, _g) = new_core("sql31-regress-isolation");
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert!(tx.rows(SCAN_DOCS).is_empty());
    tx.ok(&doc_insert(1, 10, "w1"));
    assert_eq!(tx.rows(SCAN_DOCS).len(), 1);

    assert!(auto_rows(&core, "alice", SCAN_DOCS).is_empty());
    assert!(auto_rows(&core, "bob", SCAN_DOCS).is_empty());
    assert_eq!(
        auto_code(&core, "bob", &probe_insert(1, "p1")),
        "55P03",
        "the writer is held after the first write"
    );

    tx.ok("COMMIT");
    assert_eq!(auto_rows(&core, "alice", SCAN_DOCS).len(), 1);
    assert!(auto_rows(&core, "bob", SCAN_DOCS).is_empty(), "RLS");
    auto(&core, "bob", &probe_insert(1, "p1"));
}

/// ROLLBACK した Tx の未 commit 行が、取得前の読み取りでキャッシュを温めた後でも、世代の再利用の後に
/// 同一テナント・別テナント・同一セッションの新しい Tx へ漏れない（P0）。
#[test]
fn rollback_after_read_prefix_never_leaks_through_caches() {
    let (core, _p, _g) = new_core("sql31-regress-cache");
    auto(&core, "alice", &doc_insert(1, 10, "seed"));
    let aggregate = "SELECT COUNT(*) FROM docs";
    let vector = "SELECT n FROM docs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";
    for sql in [SCAN_DOCS, aggregate, vector] {
        auto(&core, "alice", sql);
    }

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    for sql in [SCAN_DOCS, aggregate, vector] {
        tx.ok(sql);
    }
    tx.ok(&doc_insert(2, 777, "leak"));
    assert!(contains_marker(&tx.rows(SCAN_DOCS), "777"));
    assert!(contains_marker(&tx.rows(vector), "777"));
    tx.ok("ROLLBACK");

    // 世代が再利用されうる別の書き込み。
    auto(&core, "alice", &doc_insert(3, 30, "after"));
    for tenant in ["alice", "bob"] {
        for sql in [SCAN_DOCS, vector, aggregate] {
            let got = auto_rows(&core, tenant, sql);
            assert!(!contains_marker(&got, "777"), "{tenant} {sql}: {got:?}");
        }
    }
    assert_eq!(
        auto_rows(&core, "alice", aggregate),
        vec![vec!["Integer(2)"]]
    );

    let mut next = Tx::new(&core, "alice");
    next.ok("BEGIN");
    assert!(!contains_marker(&next.rows(SCAN_DOCS), "777"));
    assert!(!contains_marker(&next.rows(vector), "777"));
    next.ok("ROLLBACK");
}

/// `USING PLAN` で辞書キャッシュを温めた後のファイル形 `INSERT` が、未 commit の path を辞書へ反映し、
/// 他セッションと ROLLBACK 後には痕跡が無い。
#[test]
fn file_form_insert_after_using_plan_prefix_reflects_uncommitted_dictionary() {
    let (core, prompts, _g) = new_core("sql31-regress-plan-dict");
    auto(
        &core,
        "alice",
        &file_insert("seed/alpha.txt", "alpha seed", "s1"),
    );

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert_eq!(tx.rows(USING_PLAN).len(), 1);
    tx.ok(&file_insert("zzmarker/alpha.txt", "alpha marker", "f1"));
    let found = tx.rows(USING_PLAN);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        recorded(&prompts)
            .iter()
            .any(|p| p.contains("zzmarker/alpha.txt")),
        "the dictionary must not reuse the cache warmed before the first write"
    );

    let before = recorded(&prompts).len();
    let other = auto_rows(&core, "alice", USING_PLAN);
    assert!(!contains_marker(&other, "zzmarker"), "{other:?}");
    assert!(recorded(&prompts)[before..]
        .iter()
        .all(|p| !p.contains("zzmarker")));

    tx.ok("ROLLBACK");
    let before = recorded(&prompts).len();
    for tenant in ["alice", "bob"] {
        let got = auto_rows(&core, tenant, USING_PLAN);
        assert!(!contains_marker(&got, "zzmarker"), "{tenant}: {got:?}");
    }
    assert!(recorded(&prompts)[before..]
        .iter()
        .all(|p| !p.contains("zzmarker")));
}

/// 取得前の読み取りを挟んでも、ファイル形 `INSERT` の COMMIT・ROLLBACK・置換・`operation_id` 再利用・
/// テナント境界の契約が変わらない。
#[test]
fn file_form_insert_after_read_prefix_keeps_commit_rollback_and_replacement_contracts() {
    let (core, _p, _g) = new_core("sql31-regress-file-contract");
    auto(&core, "bob", &file_insert("a.txt", "bob content", "b1"));

    // ROLLBACK では行も台帳も残らない。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert!(tx.rows(SCAN_PATHS).is_empty());
    tx.ok(&file_insert("a.txt", "hello world", "f1"));
    tx.ok("ROLLBACK");
    assert!(auto_rows(&core, "alice", SCAN_PATHS).is_empty());
    auto(&core, "alice", &file_insert("a.txt", "hello world", "f1"));
    assert_eq!(auto_rows(&core, "alice", SCAN_PATHS).len(), 1);

    // 同じ path は置換され、operation_id の再利用は 25000。COMMIT で確定する。
    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert_eq!(tx.rows(SCAN_PATHS).len(), 1);
    tx.ok(&file_insert("a.txt", "first version", "f2"));
    tx.ok(&file_insert("a.txt", "second version", "f3"));
    let bodies = tx.rows("SELECT body FROM files LIMIT 100");
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(bodies[0][0].contains("second"), "{bodies:?}");
    assert_eq!(tx.err_code(&file_insert("b.txt", "x", "f3")), "25000");
    tx.ok("ROLLBACK");

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    assert_eq!(tx.rows(SCAN_PATHS).len(), 1);
    tx.ok(&file_insert("a.txt", "committed version", "f4"));
    tx.ok("COMMIT");
    let alice = auto_rows(&core, "alice", "SELECT body FROM files LIMIT 100");
    assert_eq!(alice.len(), 1, "{alice:?}");
    assert!(alice[0][0].contains("committed"), "{alice:?}");

    // 他テナントの同じ path の行は消えない。
    let bob = auto_rows(&core, "bob", "SELECT body FROM files LIMIT 100");
    assert_eq!(bob.len(), 1, "{bob:?}");
    assert!(bob[0][0].contains("bob"), "{bob:?}");
}

/// 取得前と取得後（dirty）のどちらでも、EXPLAIN の出力が autocommit と一致し、`USING PLAN` 形も受理される。
#[test]
fn explain_before_and_after_writer_acquisition_matches_autocommit() {
    let (core, _p, _g) = new_core("sql31-regress-explain");
    let vector = format!("[{}]", ["0.1"; DIM as usize].join(", "));
    let statements = [
        format!("EXPLAIN SELECT path FROM files ORDER BY embedding <=> '{vector}' LIMIT 5"),
        "EXPLAIN SELECT COUNT(*) FROM files".to_string(),
        "EXPLAIN SELECT path FROM files WHERE path = 'a.txt' LIMIT 10".to_string(),
    ];
    let plan = "EXPLAIN SELECT path FROM files USING PLAN('find alpha') LIMIT 10";

    let mut tx = Tx::new(&core, "alice");
    tx.ok("BEGIN");
    for sql in &statements {
        assert_eq!(tx.rows(sql), auto_rows(&core, "alice", sql), "{sql}");
    }
    assert!(!tx.rows(plan).is_empty());
    assert_eq!(tx.txn.status(), TransactionStatus::InTransaction);

    tx.ok(&file_insert("a.txt", "alpha content", "f1"));
    for sql in &statements {
        assert_eq!(tx.rows(sql), auto_rows(&core, "alice", sql), "{sql}");
    }
    assert!(!tx.rows(plan).is_empty());
    assert_eq!(tx.txn.status(), TransactionStatus::InTransaction);
    tx.ok("ROLLBACK");
}

/// 読み取りの後の最初の書き込みが writer 待ちで `55P03` になっても、副作用（行・台帳）を残さず fail-closed になる。
#[test]
fn first_write_lock_timeout_after_read_prefix_fails_without_side_effects() {
    let (core, _p, _g) = new_core("sql31-regress-timeout");
    let mut a = Tx::new(&core, "alice");
    a.ok("BEGIN");
    a.ok(&probe_insert(1, "a1"));

    let mut b = Tx::new(&core, "bob");
    b.ok("BEGIN");
    assert!(b.rows(SCAN_DOCS).is_empty());
    assert_eq!(b.err_code(&doc_insert(1, 5, "b1")), "55P03");
    assert_eq!(b.txn.status(), TransactionStatus::Failed);
    assert_eq!(b.err_code(SCAN_DOCS), "25P02");
    b.ok("ROLLBACK");
    assert_eq!(b.txn.status(), TransactionStatus::Idle);

    a.ok("COMMIT");
    assert!(auto_rows(&core, "bob", SCAN_DOCS).is_empty());
    // 台帳にも痕跡が無いので、同じ operation_id の再送は成功する。
    auto(&core, "bob", &doc_insert(1, 5, "b1"));
    assert_eq!(auto_rows(&core, "bob", SCAN_DOCS).len(), 1);
}
