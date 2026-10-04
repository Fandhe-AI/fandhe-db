//! DROP TABLE（TASK-203、対象ビヘイビア: TABLE-15・SQL-23。関連: SQL-18・RLS-9・
//! RECOVER-2、Issue #1200）と並行するクエリ、および同一プロセス内の同名再作成の
//! 結合テスト。
//!
//! 1. 並行 DROP: 別スレッド（別 `SessionState`・別 `PolicyContext`）のクエリが
//!    DROP の実行中・直後に「DROP 前と完全一致する結果」か `42P01` のどちらかだけを
//!    返し（panic なし・部分結果なし・空の `Ok` なし）、DROP 完了後は全 DML／SELECT が
//!    `42P01` になること。オラクルは exact な集合一致のみでタイミングには依存しない
//!    （sleep 不使用。全スレッドを `Barrier` で揃えて開始する）。
//!    クエリと DROP の実行区間の重なりは、feature `test-sync-points` 限定の
//!    `sync_point_overlap` モジュール（Issue #1363）が同期点で決定的に作って検査する。
//! 2. 既定エンジンで `EngineCore` を保持したまま DROP → 次元違いの同名 CREATE を行い、
//!    arena・可視ビットマップ・スカラー・疎の各キャッシュから旧行が返らないこと、
//!    旧 `operation_id` の台帳もプロセス内で一掃されていること。
//!
//! `truncate_table.rs`／`sql_drop_table.rs` の流儀（実 `Storage`、`unique_db_path`／
//! `CleanupGuard`）に従う。他テナントとの区別は Private 行だけで組む
//! （`Public` はグローバル可視のため）。

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Barrier;
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, c: &PolicyContext, sql: &str) -> Result<SqlOutcome, String> {
    let mut s = session();
    core.execute_sql_in_session(c, &mut s, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn ok(core: &EngineCore, c: &PolicyContext, sql: &str) -> SqlOutcome {
    run(core, c, sql).unwrap_or_else(|code| panic!("{sql} must succeed, got {code}"))
}

fn ids_of(outcome: SqlOutcome) -> Vec<u64> {
    match outcome {
        SqlOutcome::Query(q) => {
            let mut ids: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
            ids.sort_unstable();
            ids
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn count_of(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Query(q) => match &q.rows[0].cells[0] {
            Cell::Integer(v) => *v,
            other => panic!("expected Integer, got {other:?}"),
        },
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn new_core(label: &str, dim: u32) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    ok(
        &core,
        &ctx("sys"),
        &format!(
            "CREATE TABLE docs (embedding VECTOR({dim}) NOT NULL, kind TEXT NOT NULL, body TEXT)"
        ),
    );
    (core, guard)
}

fn vec_lit(dim: u32, id: u64) -> String {
    let mut parts = vec![format!("{id}.0")];
    parts.extend((1..dim).map(|_| "0.0".to_string()));
    format!("[{}]", parts.join(","))
}

fn seed(
    core: &EngineCore,
    c: &PolicyContext,
    dim: u32,
    ids: std::ops::RangeInclusive<u64>,
    tag: &str,
) {
    for id in ids {
        let (kind, body) = if id % 2 == 0 {
            ("a", "vector database engine")
        } else {
            ("b", "unrelated text")
        };
        ok(
            core,
            c,
            &format!(
                "INSERT INTO docs (id, embedding, kind, body) VALUES ({id}, '{}', '{kind}', '{body}') \
                 USING OPERATION_ID 'seed-{tag}-{id}'",
                vec_lit(dim, id)
            ),
        );
    }
}

const READER_ITERATION_CAP: u64 = 200_000;

/// DROP と並行クエリのラウンド数（各ラウンドは新しい DB で行う）。
const DROP_ROUNDS: u32 = 5;

/// TABLE-15: DROP と並行するクエリは「事前結果と完全一致」か `42P01` のみ。
///
/// 各ラウンドは全 reader が DROP 前の完全な結果を 1 回以上観測してから DROP を発行し
/// （reader はその後も `42P01` を受け取るまで反復し続ける）、全ラウンドで結果の原子性
/// （完全一致か `42P01`）を検査する。本テストは自由なスケジューリングで
/// 原子性を確かめるもので、重なりの発生は必須条件にせず、クエリ実行区間と DROP 実行区間
/// （`Instant` 計測）が重なったラウンド数を情報として出力する。重なりの決定的な保証は
/// feature `test-sync-points` 限定の `sync_point_overlap` モジュール（Issue #1363）が担う。
#[test]
fn concurrent_queries_during_drop_see_either_full_result_or_42p01() {
    let mut overlapped = 0u32;
    for round in 0..DROP_ROUNDS {
        if drop_round_with_concurrent_readers(round) {
            overlapped += 1;
        }
    }
    eprintln!("reader query overlapped DROP TABLE in {overlapped}/{DROP_ROUNDS} rounds");
}

/// 1 ラウンド分の DROP と並行クエリ。結果の原子性を検査し、reader のクエリ実行区間の
/// いずれかが DROP の実行区間と重なったか（情報出力用）を返す。
fn drop_round_with_concurrent_readers(round: u32) -> bool {
    let (core, _guard) = new_core("t15-drop-concurrent", 2);
    let alice = ctx("alice");
    let bob = ctx("bob");
    seed(&core, &alice, 2, 1..=6, "a");
    seed(&core, &bob, 2, 101..=104, "b");

    let q_distance = "SELECT id FROM docs ORDER BY embedding <=> '[3.0,0.0]' LIMIT 50";
    let q_count = "SELECT COUNT(*) FROM docs";
    let q_where =
        "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[3.0,0.0]' LIMIT 50";

    // (ctx, sql, 期待される ids または COUNT) の事前結果。
    let readers: Vec<(&PolicyContext, &str)> =
        vec![(&alice, q_distance), (&alice, q_count), (&bob, q_where)];
    let expected_ids_alice = ids_of(ok(&core, &alice, q_distance));
    assert_eq!(expected_ids_alice, vec![1, 2, 3, 4, 5, 6]);
    let expected_count_alice = count_of(ok(&core, &alice, q_count));
    assert_eq!(expected_count_alice, 6);
    let expected_ids_bob = ids_of(ok(&core, &bob, q_where));
    assert_eq!(expected_ids_bob, vec![102, 104]);

    let barrier = Barrier::new(readers.len() + 1);
    // DROP は全 reader が DROP 前の完全な結果を 1 回以上観測してから発行する
    // （reader はその後も `42P01` を受け取るまで反復し続ける）。
    let warmed_readers = AtomicUsize::new(0);
    let core_ref = &core;

    let (drop_window, query_windows) = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (idx, (c, sql)) in readers.iter().enumerate() {
            let barrier = &barrier;
            let expected_ids_alice = &expected_ids_alice;
            let expected_ids_bob = &expected_ids_bob;
            let warmed_readers = &warmed_readers;
            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut oks = 0u64;
                let mut windows: Vec<(Instant, Instant)> = Vec::new();
                for _ in 0..READER_ITERATION_CAP {
                    let started = Instant::now();
                    let res = run(core_ref, c, sql);
                    windows.push((started, Instant::now()));
                    match res {
                        Ok(outcome) => {
                            oks += 1;
                            match idx {
                                0 => assert_eq!(&ids_of(outcome), expected_ids_alice),
                                1 => assert_eq!(count_of(outcome), expected_count_alice),
                                _ => assert_eq!(&ids_of(outcome), expected_ids_bob),
                            }
                            if oks == 1 {
                                warmed_readers.fetch_add(1, AtomicOrdering::SeqCst);
                            }
                        }
                        Err(code) => {
                            assert_eq!(code, "42P01", "only 42P01 is allowed during DROP: {sql}");
                            assert!(
                                oks >= 1,
                                "reader must observe a full result before DROP: {sql}"
                            );
                            return (oks, windows);
                        }
                    }
                }
                panic!("reader never observed 42P01 within the iteration cap: {sql}");
            }));
        }
        let ddl = scope.spawn(|| {
            barrier.wait();
            // reader が先に panic した場合に無限待ちにしない（上限到達は失敗として報告）。
            let deadline = Instant::now() + Duration::from_secs(60);
            while warmed_readers.load(AtomicOrdering::SeqCst) < readers.len() {
                assert!(
                    Instant::now() < deadline,
                    "readers did not observe a full result before the DROP deadline"
                );
                std::thread::yield_now();
            }
            let started = Instant::now();
            ok(core_ref, &ctx("sys"), "DROP TABLE docs");
            (started, Instant::now())
        });
        let drop_window = ddl.join().expect("DDL thread must not panic");
        let mut query_windows = Vec::new();
        for h in handles {
            let (oks, windows) = h.join().expect("reader thread must not panic");
            eprintln!("round {round}: reader observed {oks} full results before 42P01");
            query_windows.extend(windows);
        }
        (drop_window, query_windows)
    });

    // DROP 完了後は全操作が 42P01（他テナントを含む）。
    let stmts = [
        "SELECT id FROM docs ORDER BY embedding <=> '[3.0,0.0]' LIMIT 5",
        "SELECT COUNT(*) FROM docs",
        "INSERT INTO docs (id, embedding, kind) VALUES (900, '[1.0,0.0]', 'a') USING OPERATION_ID 'post-drop-ins'",
        "TRUNCATE TABLE docs USING OPERATION_ID 'post-drop-trunc'",
        "UPDATE docs SET kind = 'x' WHERE id = 1 USING OPERATION_ID 'post-drop-upd'",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'post-drop-del'",
    ];
    for c in [&alice, &bob] {
        for sql in stmts {
            assert_eq!(
                run(&core, c, sql).expect_err("must fail after DROP"),
                "42P01",
                "{sql}"
            );
        }
    }

    let (drop_start, drop_end) = drop_window;
    query_windows
        .iter()
        .any(|(start, end)| *start < drop_end && *end > drop_start)
}

const DISTANCE_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '{Q}' LIMIT 50";
const COUNT_SQL: &str = "SELECT COUNT(*) FROM docs";
const KIND_SQL: &str = "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '{Q}' LIMIT 50";
const HYBRID_SQL: &str =
    "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '{Q}', body, 'vector database') LIMIT 50";

fn with_q(sql: &str, dim: u32) -> String {
    sql.replace("{Q}", &vec_lit(dim, 1))
}

/// TABLE-15・RECOVER-2: 既定エンジンで同一プロセス内 DROP → 次元違いの同名 CREATE。
/// 全キャッシュが世代失効し、旧 `operation_id` も再利用できる。
#[test]
fn drop_then_recreate_in_same_process_invalidates_all_default_caches() {
    let (core, _guard) = new_core("t15-drop-recreate", 2);
    let alice = ctx("alice");
    seed(&core, &alice, 2, 1..=10, "x");

    // 各キャッシュを温める（2 回実行して hit を発生させる）。
    for _ in 0..2 {
        for sql in [DISTANCE_SQL, KIND_SQL, HYBRID_SQL] {
            assert!(!ids_of(ok(&core, &alice, &with_q(sql, 2))).is_empty());
        }
        assert_eq!(count_of(ok(&core, &alice, COUNT_SQL)), 10);
    }
    let arena = core.sql_arena_cache_stats();
    let bitmap = core.visible_bitmap_cache_stats();
    let scalar = core.scalar_index_cache_stats();
    let sparse = core.sparse_index_cache_stats();
    assert!(arena.hits > 0 && bitmap.hits > 0 && scalar.builds > 0 && sparse.hits > 0);

    ok(&core, &ctx("sys"), "DROP TABLE docs");
    ok(
        &core,
        &ctx("sys"),
        "CREATE TABLE docs (embedding VECTOR(3) NOT NULL, kind TEXT NOT NULL, body TEXT)",
    );
    // 旧 operation_id（`seed-x-<id>`）を再利用: 台帳が一掃されていなければ 23505。
    seed(&core, &alice, 3, 201..=210, "x");
    // 上の tag は旧 id 帯と異なる id を使うため、旧 op id そのものも別途再利用する。
    ok(
        &core,
        &alice,
        "INSERT INTO docs (id, embedding, kind, body) VALUES (300, '[300.0,0.0,0.0]', 'a', 'vector database engine') \
         USING OPERATION_ID 'seed-x-1'",
    );

    let new_ids: Vec<u64> = (201..=210).chain(std::iter::once(300)).collect();
    let new_a: Vec<u64> = (201..=210)
        .filter(|i| i % 2 == 0)
        .chain(std::iter::once(300))
        .collect();
    let mut got = ids_of(ok(&core, &alice, &with_q(DISTANCE_SQL, 3)));
    got.sort_unstable();
    assert_eq!(got, new_ids);
    assert_eq!(count_of(ok(&core, &alice, COUNT_SQL)), 11);
    assert_eq!(ids_of(ok(&core, &alice, &with_q(KIND_SQL, 3))), new_a);
    let hybrid = ids_of(ok(&core, &alice, &with_q(HYBRID_SQL, 3)));
    assert!(
        !hybrid.is_empty() && hybrid.iter().all(|id| *id > 10),
        "{hybrid:?}"
    );

    let (a2, b2, sc2, sp2) = (
        core.sql_arena_cache_stats(),
        core.visible_bitmap_cache_stats(),
        core.scalar_index_cache_stats(),
        core.sparse_index_cache_stats(),
    );
    assert!(a2.misses + a2.stale_evictions > arena.misses + arena.stale_evictions);
    assert!(b2.misses + b2.stale_evictions > bitmap.misses + bitmap.stale_evictions);
    assert!(sc2.builds > scalar.builds);
    assert!(sp2.misses + sp2.stale_evictions > sparse.misses + sparse.stale_evictions);
}

/// Issue #1363（TABLE-15）: テスト専用の同期点で、DROP TABLE の実行区間とクエリの実行区間の
/// 重なりを決定的に作り、重なり中のクエリ結果が「DROP 前と完全一致」か `42P01` のどちらか
/// だけであること（部分結果・空の `Ok`・他コード・panic は不可）を固定する。
/// feature `test-sync-points` 限定（既定ビルドではコンパイルされない）。
#[cfg(feature = "test-sync-points")]
mod sync_point_overlap {
    use super::*;
    use engine::test_sync::SyncPoint;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};

    /// 待機の上限。超過は隠れたロック等によるデッドロックとみなして panic（CI をハングさせない）。
    const WAIT: Duration = Duration::from_secs(30);

    /// 同期点コールバックの制御（テスト側コーディネータ）。arm された種別でだけ 1 回止まる。
    struct Gate {
        arm_drop: AtomicBool,
        arm_read: AtomicBool,
        drop_hits: AtomicUsize,
        read_hits: AtomicUsize,
        paused: Mutex<Sender<SyncPoint>>,
        resume: Mutex<Receiver<()>>,
    }

    struct GateHandle {
        gate: Arc<Gate>,
        paused_rx: Receiver<SyncPoint>,
        resume_tx: Sender<()>,
    }

    impl GateHandle {
        fn new() -> Self {
            let (ptx, paused_rx) = channel();
            let (resume_tx, rrx) = channel();
            let gate = Arc::new(Gate {
                arm_drop: AtomicBool::new(false),
                arm_read: AtomicBool::new(false),
                drop_hits: AtomicUsize::new(0),
                read_hits: AtomicUsize::new(0),
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
                let (armed, hits) = match point {
                    SyncPoint::DropTableBeforeCommit => (&gate.arm_drop, &gate.drop_hits),
                    SyncPoint::ReadStatementSnapshotAcquired => (&gate.arm_read, &gate.read_hits),
                    _ => return,
                };
                if !armed.swap(false, AtomicOrdering::SeqCst) {
                    return;
                }
                hits.fetch_add(1, AtomicOrdering::SeqCst);
                gate.paused
                    .lock()
                    .expect("paused lock")
                    .send(point)
                    .expect("notify pause");
                gate.resume
                    .lock()
                    .expect("resume lock")
                    .recv_timeout(WAIT)
                    .expect("resume signal must arrive");
            })
        }

        fn wait_paused(&self, expected: SyncPoint) {
            let got = self
                .paused_rx
                .recv_timeout(WAIT)
                .expect("sync point must be reached");
            assert_eq!(got, expected);
        }

        fn resume(&self) {
            self.resume_tx.send(()).expect("resume");
        }
    }

    #[derive(Debug, PartialEq)]
    enum Observed {
        Ids(Vec<u64>),
        Count(u64),
    }

    /// COUNT は 1 行 1 セルの `Integer`、距離系は id 列（`SELECT id` のみ）。
    fn observe(outcome: SqlOutcome, is_count: bool) -> Observed {
        if is_count {
            Observed::Count(count_of(outcome))
        } else {
            Observed::Ids(ids_of(outcome))
        }
    }

    fn gated_core(label: &str, gate: &GateHandle) -> (EngineCore, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.set_sync_hook(Some(gate.hook()));
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        ok(
            &core,
            &ctx("sys"),
            "CREATE TABLE docs (embedding VECTOR(2) NOT NULL, kind TEXT NOT NULL, body TEXT)",
        );
        (core, guard)
    }

    /// (tenant, sql, COUNT か)。
    const QUERIES: [(&str, &str, bool); 3] = [
        (
            "alice",
            "SELECT id FROM docs ORDER BY embedding <=> '[3.0,0.0]' LIMIT 50",
            false,
        ),
        ("alice", "SELECT COUNT(*) FROM docs", true),
        (
            "bob",
            "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[3.0,0.0]' LIMIT 50",
            false,
        ),
    ];

    fn seeded(label: &str, gate: &GateHandle) -> (EngineCore, CleanupGuard, Vec<Observed>) {
        let (core, guard) = gated_core(label, gate);
        seed(&core, &ctx("alice"), 2, 1..=6, "a");
        seed(&core, &ctx("bob"), 2, 101..=104, "b");
        let expected: Vec<Observed> = QUERIES
            .iter()
            .map(|(t, sql, cnt)| observe(ok(&core, &ctx(t), sql), *cnt))
            .collect();
        assert_eq!(expected[0], Observed::Ids(vec![1, 2, 3, 4, 5, 6]));
        assert_eq!(expected[1], Observed::Count(6));
        assert_eq!(expected[2], Observed::Ids(vec![102, 104]));
        (core, guard, expected)
    }

    /// 結果が「事前結果と完全一致」なら true、`42P01` なら false。それ以外は失敗。
    fn assert_full_or_42p01(
        res: Result<SqlOutcome, String>,
        expected: &Observed,
        is_count: bool,
        sql: &str,
    ) -> bool {
        match res {
            Ok(outcome) => {
                assert_eq!(
                    &observe(outcome, is_count),
                    expected,
                    "partial/foreign result: {sql}"
                );
                true
            }
            Err(code) => {
                assert_eq!(code, "42P01", "only 42P01 is allowed: {sql}");
                false
            }
        }
    }

    fn assert_all_42p01_after_drop(core: &EngineCore) {
        let stmts = [
            "SELECT id FROM docs ORDER BY embedding <=> '[3.0,0.0]' LIMIT 5",
            "SELECT COUNT(*) FROM docs",
            "INSERT INTO docs (id, embedding, kind) VALUES (900, '[1.0,0.0]', 'a') USING OPERATION_ID 'post-drop-ins'",
            "TRUNCATE TABLE docs USING OPERATION_ID 'post-drop-trunc'",
            "UPDATE docs SET kind = 'x' WHERE id = 1 USING OPERATION_ID 'post-drop-upd'",
            "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'post-drop-del'",
        ];
        for t in ["alice", "bob"] {
            for sql in stmts {
                assert_eq!(
                    run(core, &ctx(t), sql).expect_err("must fail after DROP"),
                    "42P01",
                    "{sql}"
                );
            }
        }
    }

    /// シナリオ A: DROP を commit 直前で止め、その間にクエリを重ねる。
    #[test]
    fn drop_paused_before_commit_queries_see_full_result_or_42p01() {
        let gate = GateHandle::new();
        let (core, _guard, expected) = seeded("t15-sync-a", &gate);
        gate.gate.arm_drop.store(true, AtomicOrdering::SeqCst);

        let mut full_during_pause = 0u32;
        std::thread::scope(|scope| {
            let ddl = scope.spawn(|| ok(&core, &ctx("sys"), "DROP TABLE docs"));
            gate.wait_paused(SyncPoint::DropTableBeforeCommit);
            for _ in 0..20 {
                for ((t, sql, cnt), exp) in QUERIES.iter().zip(&expected) {
                    if assert_full_or_42p01(run(&core, &ctx(t), sql), exp, *cnt, sql) {
                        full_during_pause += 1;
                    }
                }
            }
            gate.resume();
            ddl.join().expect("DDL thread must not panic");
        });

        assert_eq!(gate.gate.drop_hits.load(AtomicOrdering::SeqCst), 1);
        assert!(
            full_during_pause > 0,
            "queries must have run while DROP was paused before commit"
        );
        assert_all_42p01_after_drop(&core);
    }

    /// シナリオ B／B′: クエリをスナップショット確定後で止め、その下で DROP を commit させる。
    /// `recreate` が true なら、再開前に次元違いの同名テーブルを作り新しい行を入れる
    /// （旧テーブルの全件か `42P01` に限り、新しい行・新旧の混在は不可）。
    fn paused_query_round(query_idx: usize, recreate: bool) {
        let gate = GateHandle::new();
        let label = format!("t15-sync-b{query_idx}{}", if recreate { "r" } else { "" });
        let (core, _guard, expected) = seeded(&label, &gate);
        let (tenant, sql, is_count) = QUERIES[query_idx];
        gate.gate.arm_read.store(true, AtomicOrdering::SeqCst);

        let res = std::thread::scope(|scope| {
            let reader = scope.spawn(|| run(&core, &ctx(tenant), sql));
            gate.wait_paused(SyncPoint::ReadStatementSnapshotAcquired);
            ok(&core, &ctx("sys"), "DROP TABLE docs");
            if recreate {
                ok(
                    &core,
                    &ctx("sys"),
                    "CREATE TABLE docs (embedding VECTOR(3) NOT NULL, kind TEXT NOT NULL, body TEXT)",
                );
                seed(&core, &ctx("alice"), 3, 201..=204, "n");
                seed(&core, &ctx("bob"), 3, 301..=304, "n");
            }
            gate.resume();
            reader.join().expect("reader thread must not panic")
        });

        assert_eq!(gate.gate.read_hits.load(AtomicOrdering::SeqCst), 1);
        assert_full_or_42p01(res, &expected[query_idx], is_count, sql);
        if !recreate {
            assert_all_42p01_after_drop(&core);
        }
    }

    #[test]
    fn drop_committed_during_paused_query_yields_full_result_or_42p01() {
        for idx in 0..QUERIES.len() {
            paused_query_round(idx, false);
        }
    }

    #[test]
    fn drop_and_recreate_during_paused_query_never_mixes_generations() {
        for idx in 0..QUERIES.len() {
            paused_query_round(idx, true);
        }
    }
}
