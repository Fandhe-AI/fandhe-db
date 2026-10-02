//! 分割実行 DML と通常の DML・検索が実スレッドで並行したときの整合の結合テスト
//! （Issue #1131。ポインタ: RECOVER-11・TABLE-3・ADR `docs/design/partitioned-dml.md`
//! 7.2 G1・8.2）。`Arc<EngineCore>` を 3 スレッドで共有する。
//!
//! 実時間の閾値は使わず、時間に依存しない不変条件だけを assert する（#1213 の間欠失敗の
//! 再発防止）。通常の書き込みは「一度も一致しない行」だけを触るため、分割実行の対象集合は
//! 並行書き込みの影響を受けず、完了後の状態が決定的になる。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// 一致行の数（`tag = 'x'`。偶数 id）。SELECT の LIMIT 上限に収まる範囲。
const MATCHING: u64 = 300;
/// 通常の書き込みスレッドの繰り返し回数の上限（安全弁）。
const MAX_NORMAL_WRITES: u64 = 2_000;
/// 検索スレッドの繰り返し回数の上限（安全弁）。
const MAX_SEARCHES: u64 = 20_000;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

#[test]
fn partitioned_delete_coexists_with_normal_writes_and_searches() {
    let path = unique_db_path("pdml-concurrency");
    let _g = CleanupGuard(path.clone());
    let core = Arc::new(EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    ));
    {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &ctx("sys"),
            &mut session,
            "CREATE TABLE docs (tag TEXT, n BIGINT)",
        )
        .expect("create table");
    }
    // 偶数 id（0..2*MATCHING）が一致行 'x'、奇数 id が非一致行 'y'。64 行ずつ投入する。
    let total = MATCHING * 2;
    let mut start = 0;
    while start < total {
        let end = (start + 64).min(total);
        let values: Vec<String> = (start..end)
            .map(|id| format!("({id}, '{}', {id})", if id % 2 == 0 { 'x' } else { 'y' }))
            .collect();
        run(
            &core,
            &format!(
                "INSERT INTO docs (id, tag, n) VALUES {} USING OPERATION_ID 'seed-{start}'",
                values.join(", ")
            ),
        )
        .expect("seed");
        start = end;
    }

    let stop = Arc::new(AtomicBool::new(false));

    // スレッド N: 一度も一致しない行（tag = 'y'）だけを書く。
    let normal = {
        let core = Arc::clone(&core);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut failures: Vec<String> = Vec::new();
            let mut last = 0u64;
            // stop が先に立っても最低 1 周は処理する（空振り成功の防止）ため、
            // stop の確認は 1 周の処理後に行う。
            for i in 0..MAX_NORMAL_WRITES {
                last = i + 1;
                // 奇数 id 1 の n を更新する（tag は 'y' のまま）。
                if let Err(e) = run(
                    &core,
                    &format!(
                        "UPDATE docs SET n = {last} WHERE id = 1 USING OPERATION_ID 'n-upd-{i}'"
                    ),
                ) {
                    failures.push(format!("update {i}: {}", e.wire_code()));
                }
                // 範囲外の新しい id へ非一致行を INSERT する。
                let new_id = 100_001 + i;
                if let Err(e) = run(
                    &core,
                    &format!(
                        "INSERT INTO docs (id, tag, n) VALUES ({new_id}, 'y', 0) USING OPERATION_ID 'n-ins-{i}'"
                    ),
                ) {
                    failures.push(format!("insert {i}: {}", e.wire_code()));
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            (failures, last)
        })
    };

    // スレッド S: 一致行の件数を数え続け、単調に減る（増えない）ことを確認する。
    let search = {
        let core = Arc::clone(&core);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut prev = u64::MAX;
            let mut violations = 0u64;
            let mut searches = 0u64;
            // stop が先に立っても最低 1 周は検索する（stop の確認は 1 周の処理後）。
            for _ in 0..MAX_SEARCHES {
                if let Ok(SqlOutcome::Query(q)) =
                    run(&core, "SELECT id FROM docs WHERE tag = 'x' LIMIT 1000")
                {
                    let n = q.rows.len() as u64;
                    searches += 1;
                    if n > prev {
                        violations += 1;
                    }
                    prev = n;
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            (violations, searches)
        })
    };

    // スレッド W（本スレッド）: 分割 DELETE。
    let deleted = match run(
        &core,
        "DELETE FROM docs WHERE tag = 'x' USING OPERATION_ID 'pjob' PARTITIONED CHUNK 1",
    )
    .expect("partitioned delete completes")
    {
        SqlOutcome::Delete(o) => o.rows_affected,
        other => panic!("unexpected outcome {other:?}"),
    };
    stop.store(true, Ordering::Relaxed);
    let (failures, last_n) = normal.join().expect("normal thread");
    let (violations, searches) = search.join().expect("search thread");

    assert_eq!(deleted, MATCHING);
    // 通常の書き込みは分割実行に待たされて拒否されない（ADR 8.2。55P03 が 0 件）。
    assert!(failures.is_empty(), "normal writes failed: {failures:?}");
    assert_eq!(violations, 0, "matching count must never increase");
    // 並行動作が空振りしていない（両スレッドが実際に処理した）ことを確認する。
    assert!(last_n >= 1, "normal write thread never ran");
    assert!(searches >= 1, "search thread never completed a search");

    // 完了後: 一致行は 0 件・SHOW は completed と一致数・N の最後の値が残っている。
    match run(&core, "SELECT id FROM docs WHERE tag = 'x' LIMIT 1000").expect("select") {
        SqlOutcome::Query(q) => assert!(q.rows.is_empty()),
        other => panic!("unexpected outcome {other:?}"),
    }
    match run(&core, "SHOW PARTITIONED DML 'pjob' ON docs").expect("show") {
        SqlOutcome::Query(q) => {
            let row = q.rows.first().expect("one row");
            assert_eq!(
                row.cells.first(),
                Some(&Cell::Text("completed".to_string()))
            );
            assert_eq!(
                row.cells.get(1),
                Some(&Cell::SignedInteger(MATCHING as i64))
            );
        }
        other => panic!("unexpected outcome {other:?}"),
    }
    match run(&core, "SELECT id, n FROM docs WHERE id = 1 LIMIT 1").expect("select n") {
        SqlOutcome::Query(q) => {
            let row = q.rows.first().expect("row 1");
            assert!(row.cells.contains(&Cell::SignedInteger(last_n as i64)));
        }
        other => panic!("unexpected outcome {other:?}"),
    }
}
