//! DDL の途中のエラーで何も残らないことと、DDL・書き込みの writer 待機の `55P03` の層 A テスト
//! （Issue #2419・TASK-332。対象ビヘイビア: SQL-23。関連: SQL-31・SQL-46・TABLE-10・PERSIST-1。
//! spec 本文は転記しない）。
//!
//! 書き込みの choke point は `Storage::begin_write_txn`（writer ゲート）で、DDL も DML も同じ
//! ゲートを通る。本ファイルは engine の `EngineCore::execute_sql_in_session` を production 経路として、
//! 次の 2 点を固定する。SIGKILL 側の原子性は #2418 の担当で、ここでは扱わない。
//!
//! - パート A: DDL が `wire_code` のエラーで終わったとき、カタログ・行・索引・ビュー・SEQUENCE が
//!   文の前と完全に同じであること。観測は (1) カタログ API（表スキーマ・索引宣言）、(2) 生の redb の
//!   全表の件数、(3) 同一プロセス内の SELECT の三段で行い、失敗は「write txn 内の書き込みの後」に
//!   起きるケースを含める（書く前に失敗するケースは対照として明示する）。
//! - パート B: 他セッションが writer を持つ間の DDL は待ち、上限（`with_write_lock_wait`）で `55P03`。
//!   逆に DDL の実行中（同期点で停止）に他セッションが書くと待ち、上限で `55P03`。
//!
//! `write_lock_wait.rs`・`table15_drop_table_concurrency.rs` と同じ流儀。時間計測と同期点を使うため
//! 束ねず単独バイナリとする。writer を握る側は `BEGIN` の後の最初の書き込み文で取得する。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut ddl_session(), sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {}", e.wire_code()))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    match run(core, tenant, sql) {
        Ok(_) => panic!("{sql} must fail"),
        Err(e) => e.wire_code(),
    }
}

fn open_core(path: &std::path::Path, wait_ms: u64) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(validate_write_lock_wait_ms(wait_ms).expect("in range"))
}

/// 永続状態のスナップショット。`logical` はカタログ API、`physical` は生の redb の表ごとの件数。
/// 取る前に `EngineCore`／`Storage` を drop する（redb は同一プロセスで二重 open できない）。
#[derive(Debug, PartialEq)]
struct Snapshot {
    logical: Vec<String>,
    physical: BTreeMap<String, u64>,
}

fn snapshot(path: &std::path::Path) -> Snapshot {
    let mut logical = Vec::new();
    {
        let st = Storage::open(path).expect("open storage");
        for t in st.list_tables().expect("list tables") {
            let schema = st.get_table_schema(&t).expect("schema");
            logical.push(format!("table {t}: {schema:?}"));
        }
        for i in st.list_indexes().expect("list indexes") {
            logical.push(format!("index {i:?}"));
        }
    }
    let mut physical = BTreeMap::new();
    {
        use redb::{MultimapTableHandle, ReadableDatabase, ReadableTableMetadata, TableHandle};
        let db = redb::Database::open(path).expect("open redb");
        let rt = db.begin_read().expect("read");
        for h in rt.list_tables().expect("list") {
            let name = h.name().to_string();
            let len = rt
                .open_untyped_table(h)
                .expect("untyped")
                .len()
                .expect("len");
            physical.insert(format!("table {name}"), len);
        }
        for h in rt.list_multimap_tables().expect("list mm") {
            let name = h.name().to_string();
            let len = rt
                .open_untyped_multimap_table(h)
                .expect("untyped mm")
                .len()
                .expect("len");
            physical.insert(format!("multimap {name}"), len);
        }
    }
    Snapshot { logical, physical }
}

/// `src` の行をテナントごとに観測する（id と全セルの Debug 表現）。
fn rows_of(core: &EngineCore, tenant: &str) -> Vec<String> {
    match ok(core, tenant, "SELECT id, a, t FROM src ORDER BY id") {
        SqlOutcome::Query(q) => q
            .rows
            .iter()
            .map(|r| format!("{}:{:?}", r.id, r.cells))
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

const OWNER_ALICE: &str = "tenant-alice-private";
const OWNER_BOB: &str = "tenant-bob-private";

fn seed_src(core: &EngineCore) {
    ok(
        core,
        OWNER_ALICE,
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
    );
    ok(
        core,
        OWNER_ALICE,
        "INSERT INTO src (id, a, t) VALUES (1, 10, '10'), (2, 20, '20') USING OPERATION_ID 'seed-a'",
    );
    // bob の行が失敗の原因になる（a = 5 でゼロ除算・t = 'zz' で数値変換不可・a が NULL・t の重複）。
    ok(
        core,
        OWNER_BOB,
        "INSERT INTO src (id, a, t) VALUES (3, 5, 'zz'), (4, NULL, 'zz') USING OPERATION_ID 'seed-b'",
    );
}

/// パート A の共通手順。`setup` 後に `fail_sql` を `who` で実行して `expected` の `wire_code` を確かめ、
/// 痕跡が無いこと・前後のスナップショットの一致・writer の解放・原因除去後の再実行成功を確かめる。
/// 準備は 1 つ目の core で行い、drop してからスナップショット A を取る。
fn atomicity_case(
    label: &str,
    setup: &[&str],
    who: &str,
    fail_sql: &str,
    expected: &str,
    no_trace: impl Fn(&EngineCore),
    repair: &[&str],
) {
    let path = unique_db_path(label);
    let _g = CleanupGuard(path.clone());
    {
        let core = open_core(&path, 30_000);
        seed_src(&core);
        for s in setup {
            ok(&core, OWNER_ALICE, s);
        }
    }
    let before = snapshot(&path);

    let (alice_rows, bob_rows) =
        {
            // 待機上限は短くする（writer が残っていれば 55P03 で素早く失敗させるため）。
            let core = open_core(&path, 2_000);
            let alice_rows = rows_of(&core, OWNER_ALICE);
            let bob_rows = rows_of(&core, OWNER_BOB);
            let err = run(&core, who, fail_sql).expect_err(fail_sql);
            assert_eq!(err.wire_code(), expected, "{label}: {err}");
            let msg = err.to_string();
            for leak in [OWNER_ALICE, OWNER_BOB, "'zz'"] {
                assert!(!msg.contains(leak), "{label}: leaked {leak}: {msg}");
            }
            no_trace(&core);
            assert_eq!(
                rows_of(&core, OWNER_ALICE),
                alice_rows,
                "{label}: alice rows"
            );
            assert_eq!(rows_of(&core, OWNER_BOB), bob_rows, "{label}: bob rows");
            // 失敗した DDL を実行した core が生存したまま、writer が解放されていること。
            // 明示 Tx の書き込み（writer を取る）が 55P03 にならず通る。ROLLBACK で永続状態を汚さない。
            {
                let caller = ctx("tenant-carol-private");
                let mut session = SessionState::default();
                let mut txn = core.new_session_transaction();
                core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
                    .expect("begin");
                core.execute_sql_in_txn(
                &caller,
                &mut session,
                &mut txn,
                "INSERT INTO src (id, a, t) VALUES (901, 1, '1') USING OPERATION_ID 'probe-live'",
            )
            .unwrap_or_else(|e| {
                panic!("{label}: writer must be free on the live core, got {}", e.wire_code())
            });
                core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
                    .expect("rollback");
            }
            (alice_rows, bob_rows)
        };
    let after = snapshot(&path);
    assert_eq!(before, after, "{label}: persisted state must be unchanged");

    // エラー経路で writer が解放されていること（別セッションの autocommit 書き込みが通る）。
    let core = open_core(&path, 30_000);
    ok(
        &core,
        "tenant-carol-private",
        "INSERT INTO src (id, a, t) VALUES (900, 1, '1') USING OPERATION_ID 'after-fail'",
    );
    assert_eq!(rows_of(&core, OWNER_ALICE), alice_rows);
    assert_eq!(rows_of(&core, OWNER_BOB), bob_rows);
    // 原因を取り除くと同じ DDL が成功する（残骸があれば 42P07・42701 等になる）。
    for r in repair {
        ok(&core, OWNER_BOB, r);
    }
    ok(&core, who, fail_sql);
}

const FIX_BOB_ROW3: &str = "UPDATE src SET a = 7, t = '3' WHERE id = 3 USING OPERATION_ID 'fix-3'";
const FIX_BOB_ROW4: &str = "UPDATE src SET a = 8, t = '4' WHERE id = 4 USING OPERATION_ID 'fix-4'";

/// CREATE 系: CTAS は表を作り行を投入してから評価に失敗する（SQL-52 の同一 write txn 契約）。
#[test]
fn ctas_runtime_error_leaves_nothing() {
    atomicity_case(
        "sql23-ctas",
        &[],
        OWNER_BOB,
        "CREATE TABLE c1 AS SELECT id, 10 / (a - 5) AS q FROM src",
        "22012",
        |core| assert_eq!(code(core, OWNER_BOB, "SELECT id FROM c1"), "42P01"),
        &[FIX_BOB_ROW3],
    );
}

/// ALTER 系: 全行書き換えの途中（bob の行だけ変換不可）で失敗しても型は元のまま。
#[test]
fn alter_column_type_rewrite_error_leaves_nothing() {
    atomicity_case(
        "sql23-alter-type",
        &[],
        OWNER_ALICE,
        "ALTER TABLE src ALTER COLUMN t TYPE INTEGER USING t::INTEGER",
        "22P02",
        |core| {
            let rows = rows_of(core, OWNER_ALICE);
            assert!(rows[0].contains("\"10\""), "t must stay TEXT: {rows:?}");
        },
        &[FIX_BOB_ROW3, FIX_BOB_ROW4],
    );
}

/// ALTER 系: 複数アクションの前のアクション（ADD COLUMN）が適用済みでも全体が戻る（TABLE-39）。
#[test]
fn alter_multi_action_error_leaves_nothing() {
    atomicity_case(
        "sql23-alter-multi",
        &[],
        OWNER_ALICE,
        "ALTER TABLE src ADD COLUMN x TEXT, ALTER COLUMN a SET NOT NULL",
        "23502",
        |core| assert_eq!(code(core, OWNER_ALICE, "SELECT x FROM src"), "42703"),
        &[FIX_BOB_ROW4],
    );
}

/// INDEX 系: 既存行に重複があれば宣言（索引・制約）が残らない（INDEX-9）。
#[test]
fn create_unique_index_error_leaves_nothing() {
    atomicity_case(
        "sql23-unique-index",
        &[],
        OWNER_ALICE,
        "CREATE UNIQUE INDEX src_t_uq ON src (t)",
        "23505",
        |core| assert_eq!(code(core, OWNER_ALICE, "DROP INDEX src_t_uq"), "42704"),
        &[FIX_BOB_ROW4],
    );
}

/// DROP 系: 全削除の後に所有 SEQUENCE が参照されていると分かって失敗しても、表も行も残る。
#[test]
fn drop_table_dependent_sequence_error_leaves_nothing() {
    atomicity_case(
        "sql23-drop-owned-seq",
        &[
            "CREATE TABLE t1 (id BIGSERIAL PRIMARY KEY, n BIGINT)",
            "CREATE TABLE t2 (id BIGINT PRIMARY KEY, k BIGINT DEFAULT nextval('t1_id_seq'))",
        ],
        OWNER_ALICE,
        "DROP TABLE t1",
        "2BP01",
        |core| {
            ok(core, OWNER_ALICE, "SELECT id FROM t1");
        },
        &["ALTER TABLE t2 ALTER COLUMN k DROP DEFAULT"],
    );
}

/// 対照: 事前検証で失敗する DROP は書く前に失敗する（上のケースは書いてから失敗する側）。
#[test]
fn drop_table_missing_name_fails_before_writing() {
    atomicity_case(
        "sql23-drop-missing",
        &[],
        OWNER_ALICE,
        "DROP TABLE src, nope",
        "42P01",
        |core| {
            ok(core, OWNER_ALICE, "SELECT id FROM src");
        },
        &["CREATE TABLE nope (id BIGINT PRIMARY KEY)"],
    );
}

// ---------------------------------------------------------------- パート B

fn insert_docs(id: u64, op: &str) -> String {
    format!("INSERT INTO docs (id, a) VALUES ({id}, 'x') USING OPERATION_ID '{op}'")
}

/// holder が `BEGIN` と最初の書き込み文で writer を握り、解放シグナル後に `ROLLBACK` する。
fn spawn_holder(
    engine: &Arc<EngineCore>,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
    let (began_tx, began_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let e = Arc::clone(engine);
    let h = std::thread::spawn(move || {
        let caller = ctx("tenant-a");
        let mut session = SessionState::default();
        let mut txn = e.new_session_transaction();
        e.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        e.execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_docs(900, "op-holder"),
        )
        .expect("first write takes the writer");
        began_tx.send(()).expect("notify");
        release_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("release signal");
        e.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    });
    began_rx.recv().expect("holder began");
    (h, release_tx)
}

fn prepare_b(path: &std::path::Path) {
    let core = open_core(path, 30_000);
    for s in [
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, a TEXT)",
        "CREATE TABLE base (id BIGINT PRIMARY KEY, a TEXT)",
        "CREATE TABLE victim (id BIGINT PRIMARY KEY, a TEXT)",
        "CREATE INDEX base_ix ON base (a)",
        "CREATE VIEW v1 AS SELECT id FROM base",
    ] {
        ok(&core, "sys", s);
    }
}

/// writer 待機の対象にする DDL。いずれも成功時にカタログを変える。
const WAITING_DDLS: [&str; 8] = [
    "CREATE TABLE z1 (id BIGINT PRIMARY KEY)",
    "ALTER TABLE base ADD COLUMN c TEXT",
    "CREATE INDEX base_ix2 ON base (id)",
    "DROP INDEX base_ix",
    "CREATE VIEW v2 AS SELECT id FROM base",
    "DROP VIEW v1",
    "CREATE TABLE z2 AS SELECT id FROM base",
    "DROP TABLE victim",
];

/// B-1: 他セッションが writer を持つ間の DDL は待ち、上限で `55P03`。何も残らず、解放後は成功する。
#[test]
fn ddl_waits_for_held_writer_then_55p03() {
    let path = unique_db_path("sql23-ddl-wait");
    let _g = CleanupGuard(path.clone());
    prepare_b(&path);
    let before = snapshot(&path);

    let engine = Arc::new(open_core(&path, 100));
    let (holder, release) = spawn_holder(&engine);
    let mut results = Vec::new();
    for ddl in WAITING_DDLS {
        let started = Instant::now();
        let res = run(&engine, "sys", ddl)
            .map(|_| ())
            .map_err(|e| e.wire_code());
        results.push((ddl, res, started.elapsed()));
    }
    release.send(()).expect("release");
    holder.join().expect("holder");
    for (ddl, res, elapsed) in &results {
        assert_eq!(*res, Err("55P03"), "{ddl}");
        assert!(
            *elapsed >= Duration::from_millis(100),
            "{ddl}: elapsed={elapsed:?}"
        );
        assert!(
            *elapsed < Duration::from_secs(10),
            "{ddl}: elapsed={elapsed:?}"
        );
    }
    drop(Arc::into_inner(engine).expect("sole owner"));
    assert_eq!(snapshot(&path), before, "timed-out DDL must leave nothing");

    // 待ちの失敗が残骸を残していない: 同じ DDL がそのまま成功する。
    let core = open_core(&path, 100);
    for ddl in WAITING_DDLS {
        ok(&core, "sys", ddl);
    }
}

/// B-1 の対照: 上限内で holder が解放されれば、待っていた DDL は成功する。
#[test]
fn ddl_succeeds_when_writer_released_within_wait() {
    let path = unique_db_path("sql23-ddl-wait-ok");
    let _g = CleanupGuard(path.clone());
    prepare_b(&path);
    let engine = Arc::new(open_core(&path, 5_000));
    let (holder, release) = spawn_holder(&engine);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).expect("release");
    });
    let started = Instant::now();
    let res = run(&engine, "sys", "CREATE TABLE z1 (id BIGINT PRIMARY KEY)");
    let elapsed = started.elapsed();
    releaser.join().expect("releaser");
    holder.join().expect("holder");
    res.expect("blocked DDL must succeed after release");
    assert!(elapsed >= Duration::from_millis(250), "elapsed={elapsed:?}");
    ok(&engine, "sys", "SELECT id FROM z1");
}

/// B-2: DDL の実行中（writer を持ったまま commit 直前で停止）に他セッションが書くと待ち、`55P03`。
#[cfg(feature = "test-sync-points")]
mod ddl_in_flight {
    use super::*;
    use engine::test_sync::SyncPoint;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    const WAIT: Duration = Duration::from_secs(30);

    struct Gate {
        armed: AtomicBool,
        paused: Mutex<Sender<()>>,
        resume: Mutex<Receiver<()>>,
    }

    struct GateHandle {
        gate: Arc<Gate>,
        paused_rx: Receiver<()>,
        resume_tx: Sender<()>,
    }

    impl GateHandle {
        fn new() -> Self {
            let (ptx, paused_rx) = channel();
            let (resume_tx, rrx) = channel();
            let gate = Arc::new(Gate {
                armed: AtomicBool::new(false),
                paused: Mutex::new(ptx),
                resume: Mutex::new(rrx),
            });
            Self {
                gate,
                paused_rx,
                resume_tx,
            }
        }

        fn hook(&self) -> engine::test_sync::SyncHook {
            let gate = Arc::clone(&self.gate);
            Arc::new(move |point| {
                if point != SyncPoint::DropTableBeforeCommit
                    || !gate.armed.swap(false, Ordering::SeqCst)
                {
                    return;
                }
                gate.paused.lock().expect("lock").send(()).expect("notify");
                gate.resume
                    .lock()
                    .expect("lock")
                    .recv_timeout(WAIT)
                    .expect("resume signal must arrive");
            })
        }
    }

    fn gated_core(path: &std::path::Path, wait_ms: u64, gate: &GateHandle) -> Arc<EngineCore> {
        let storage = Storage::open(path).expect("open storage");
        storage.set_sync_hook(Some(gate.hook()));
        Arc::new(
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
                .with_write_lock_wait(validate_write_lock_wait_ms(wait_ms).expect("in range")),
        )
    }

    fn prepare(core: &EngineCore) {
        for s in [
            "CREATE TABLE victim (id BIGINT PRIMARY KEY, a TEXT)",
            "CREATE TABLE other (id BIGINT PRIMARY KEY, a TEXT)",
            "INSERT INTO victim (id, a) VALUES (1, 'v') USING OPERATION_ID 'v1'",
        ] {
            ok(core, "sys", s);
        }
    }

    fn insert_other(id: u64, op: &str) -> String {
        format!("INSERT INTO other (id, a) VALUES ({id}, 'x') USING OPERATION_ID '{op}'")
    }

    /// DROP を別スレッドで開始し、同期点で停止するまで待つ。
    fn start_paused_drop(
        core: &Arc<EngineCore>,
        gate: &GateHandle,
    ) -> std::thread::JoinHandle<Result<(), &'static str>> {
        gate.gate.armed.store(true, Ordering::SeqCst);
        let c = Arc::clone(core);
        let h = std::thread::spawn(move || {
            run(&c, "sys", "DROP TABLE victim")
                .map(|_| ())
                .map_err(|e| e.wire_code())
        });
        gate.paused_rx
            .recv_timeout(WAIT)
            .expect("sync point reached");
        h
    }

    #[test]
    fn writes_wait_while_ddl_holds_writer_then_55p03() {
        let path = unique_db_path("sql23-inflight");
        let _g = CleanupGuard(path.clone());
        let gate = GateHandle::new();
        let core = gated_core(&path, 100, &gate);
        prepare(&core);
        let dropper = start_paused_drop(&core, &gate);

        // B2-1: 別の表への autocommit INSERT も待つ（writer は表単位でなく単一）。
        let started = Instant::now();
        assert_eq!(code(&core, "tenant-a", &insert_other(10, "w1")), "55P03");
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(10));

        // B2-2: 明示 Tx の BEGIN は待たず、最初の書き込み文が 55P03。
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        let caller = ctx("tenant-a");
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("BEGIN must not wait");
        let started = Instant::now();
        let err = core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_other(11, "w2"))
            .expect_err("first write waits");
        assert_eq!(err.wire_code(), "55P03");
        assert!(started.elapsed() >= Duration::from_millis(100));
        let _ = core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK");

        // B2-3: 別の DDL も待ち、何も残らない。
        assert_eq!(
            code(&core, "sys", "CREATE TABLE z (id BIGINT PRIMARY KEY)"),
            "55P03"
        );

        gate.resume_tx.send(()).expect("resume");
        assert_eq!(dropper.join().expect("dropper"), Ok(()));
        assert_eq!(code(&core, "sys", "SELECT id FROM victim"), "42P01");
        assert_eq!(code(&core, "sys", "SELECT id FROM z"), "42P01");
        ok(&core, "tenant-a", &insert_other(10, "w1"));
    }

    /// 対照: 上限内で DDL が終われば、待っていた書き込みは成功する。
    #[test]
    fn write_succeeds_when_ddl_finishes_within_wait() {
        let path = unique_db_path("sql23-inflight-ok");
        let _g = CleanupGuard(path.clone());
        let gate = GateHandle::new();
        let core = gated_core(&path, 5_000, &gate);
        prepare(&core);
        let dropper = start_paused_drop(&core, &gate);
        let tx = gate.resume_tx.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            tx.send(()).expect("resume");
        });
        let started = Instant::now();
        let res = run(&core, "tenant-a", &insert_other(10, "w1"));
        let elapsed = started.elapsed();
        releaser.join().expect("releaser");
        assert_eq!(dropper.join().expect("dropper"), Ok(()));
        res.expect("blocked write must succeed after DDL commit");
        assert!(elapsed >= Duration::from_millis(250), "elapsed={elapsed:?}");
    }
}
