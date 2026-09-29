//! DROP TABLE（TASK-203、対象ビヘイビア: TABLE-15・SQL-23。関連: SQL-18・RLS-9・
//! RECOVER-2、Issue #1200）と並行するクエリ、および同一プロセス内の同名再作成の
//! 結合テスト。
//!
//! 1. 並行 DROP: 別スレッド（別 `SessionState`・別 `PolicyContext`）のクエリが
//!    DROP の実行中・直後に「DROP 前と完全一致する結果」か `42P01` のどちらかだけを
//!    返し（panic なし・部分結果なし・空の `Ok` なし）、DROP 完了後は全 DML／SELECT が
//!    `42P01` になること。オラクルは exact な集合一致のみでタイミングには依存しない
//!    （sleep 不使用。全スレッドを `Barrier` で揃えて開始する）。
//! 2. 既定エンジンで `EngineCore` を保持したまま DROP → 次元違いの同名 CREATE を行い、
//!    arena・可視ビットマップ・スカラー・疎の各キャッシュから旧行が返らないこと、
//!    旧 `operation_id` の台帳もプロセス内で一掃されていること。
//!
//! `truncate_table.rs`／`sql_drop_table.rs` の流儀（実 `Storage`、`unique_db_path`／
//! `CleanupGuard`）に従う。他テナントとの区別は Private 行だけで組む
//! （`Public` はグローバル可視のため）。

use std::sync::Barrier;

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

/// TABLE-15: DROP と並行するクエリは「事前結果と完全一致」か `42P01` のみ。
#[test]
fn concurrent_queries_during_drop_see_either_full_result_or_42p01() {
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
    let core_ref = &core;

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (idx, (c, sql)) in readers.iter().enumerate() {
            let barrier = &barrier;
            let expected_ids_alice = &expected_ids_alice;
            let expected_ids_bob = &expected_ids_bob;
            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut oks = 0u64;
                for _ in 0..READER_ITERATION_CAP {
                    match run(core_ref, c, sql) {
                        Ok(outcome) => {
                            oks += 1;
                            match idx {
                                0 => assert_eq!(&ids_of(outcome), expected_ids_alice),
                                1 => assert_eq!(count_of(outcome), expected_count_alice),
                                _ => assert_eq!(&ids_of(outcome), expected_ids_bob),
                            }
                        }
                        Err(code) => {
                            assert_eq!(code, "42P01", "only 42P01 is allowed during DROP: {sql}");
                            return oks;
                        }
                    }
                }
                panic!("reader never observed 42P01 within the iteration cap: {sql}");
            }));
        }
        let ddl = scope.spawn(|| {
            barrier.wait();
            ok(core_ref, &ctx("sys"), "DROP TABLE docs");
        });
        ddl.join().expect("DDL thread must not panic");
        for h in handles {
            let oks = h.join().expect("reader thread must not panic");
            eprintln!("reader observed {oks} full results before 42P01");
        }
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
