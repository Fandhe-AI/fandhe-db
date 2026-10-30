//! トリガの入れ子の発火と深さ上限（`54001`）の結合テスト（TABLE-45 (5)・Issue #1727）。
//!
//! トリガ関数の実体（Issue #2228）が未整備のため、`FnInvoker` が `TriggerCall::writes` で次の表へ
//! 書く振る舞いを注入し、循環・チェーン・境界・後始末・テナント境界を `EngineCore` に SQL を流す水準で
//! 固定する。検査点は `trigger::runtime::TriggerRuntime::invoke`、設計記録は
//! `docs/design/trigger-depth-limit-cli.md`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::trigger_insert_tests::{
    add_trigger, before_row, count, ctx, run, trg_schema, trigger_def, FnInvoker,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::sql::mode::SessionState;
use crate::storage::Storage;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;
use crate::trigger_limits::TriggerLimits;

const INSERT_TRG: &str = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)";

fn after_row(name: &str) -> crate::catalog::trigger::TriggerDef {
    trigger_def(name, TriggerTiming::After, TriggerLevel::Row)
}

/// 全チェーン表（採番方式）の名前。`trg` だけが利用者指定 id の入口表。
const ALLOCATED: [&str; 21] = [
    "c1",
    "c2",
    "c3",
    "c4",
    "c5",
    "c6",
    "c7",
    "c8",
    "c9",
    "c10",
    "c11",
    "c12",
    "c13",
    "c14",
    "c15",
    "c16",
    "c17",
    "cyc_alpha",
    "cyc_beta",
    "self_t",
    "plain_a",
];

fn open_storage(dir: &std::path::Path) -> Storage {
    let storage = Storage::open(dir.join("db.redb")).expect("open storage");
    storage.create_table(&trg_schema("trg")).expect("trg");
    for name in ALLOCATED {
        storage
            .create_table(
                &trg_schema(name).with_row_identity(crate::catalog::RowIdentity::Allocated),
            )
            .expect(name);
    }
    storage
}

/// `map` の `(元の表, 次の表)` に従い、元の表のトリガが呼ばれたら NEW を次の表へそのまま書く。
fn invoker(map: Vec<(&'static str, &'static str)>, calls: Arc<AtomicUsize>) -> Arc<FnInvoker> {
    Arc::new(FnInvoker {
        behavior: Box::new(move |c| {
            if let Some((_, next)) = map.iter().find(|(from, _)| *from == c.table) {
                c.writes.insert_row(next, None, c.new_row)?;
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
        calls,
    })
}

/// 上限 `limit`（`None` なら未設定の既定）と `map` で構成した core と呼び出し回数。
fn core_with_map(
    dir: &std::path::Path,
    map: Vec<(&'static str, &'static str)>,
    limit: Option<usize>,
) -> (EngineCore, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut core = EngineCore::from_storage(
        open_storage(dir),
        Box::new(crate::kernel::CpuScalarProvider),
    )
    .with_trigger_invoker_for_test(invoker(map, Arc::clone(&calls)));
    if let Some(limit) = limit {
        core = core.with_trigger_limits(TriggerLimits::new(limit));
    }
    (core, calls)
}

/// `trg`→`c1`→…→`c{n}` のチェーン。`trg`・`c1`…`c{n-1}` に AFTER ROW トリガを置く
/// （トリガ呼び出しは n 回。最後の表 `c{n}` にはトリガが無い）。
fn chain_core(
    dir: &std::path::Path,
    n: usize,
    limit: Option<usize>,
) -> (EngineCore, Arc<AtomicUsize>) {
    const NAMES: [&str; 18] = [
        "trg", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "c10", "c11", "c12", "c13",
        "c14", "c15", "c16", "c17",
    ];
    let map: Vec<(&'static str, &'static str)> = (0..n).map(|i| (NAMES[i], NAMES[i + 1])).collect();
    let (core, calls) = core_with_map(dir, map, limit);
    for name in NAMES.iter().take(n) {
        add_trigger(&core.storage, "tenant-a", name, &after_row("chain"));
    }
    (core, calls)
}

/// `trg`→`cyc_alpha`→`cyc_beta`→`cyc_alpha`… の循環（相互再帰）。
fn cycle_core(dir: &std::path::Path, limit: Option<usize>) -> (EngineCore, Arc<AtomicUsize>) {
    let (core, calls) = core_with_map(
        dir,
        vec![
            ("trg", "cyc_alpha"),
            ("cyc_alpha", "cyc_beta"),
            ("cyc_beta", "cyc_alpha"),
        ],
        limit,
    );
    for t in ["trg", "cyc_alpha", "cyc_beta"] {
        add_trigger(&core.storage, "tenant-a", t, &after_row("cyc"));
    }
    (core, calls)
}

fn assert_no_rows(core: &EngineCore, tables: &[&str]) {
    let a = ctx("tenant-a");
    for t in tables {
        assert_eq!(count(core, &a, t), 0, "{t} must stay empty");
    }
}

// (a) 相互再帰が上限で止まり、`54001` で文全体が巻き戻る。
#[test]
fn mutual_recursion_stops_at_limit_with_54001() {
    let dir = tempdir();
    let (core, calls) = cycle_core(dir.path(), Some(3));
    let a = ctx("tenant-a");
    let err = run(&core, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_no_rows(&core, &["trg", "cyc_alpha", "cyc_beta"]);
}

// (b) 自己再帰（BEFORE 経路でも停止する）。
#[test]
fn self_recursion_in_before_row_stops_with_54001() {
    let dir = tempdir();
    let (core, calls) = core_with_map(
        dir.path(),
        vec![("trg", "self_t"), ("self_t", "self_t")],
        Some(4),
    );
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b0"));
    add_trigger(&core.storage, "tenant-a", "self_t", &before_row("b1"));
    let a = ctx("tenant-a");
    let err = run(&core, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_no_rows(&core, &["trg", "self_t"]);
}

// (c) 既定構成（`with_trigger_limits` 未呼び出し）では深さ 16 で止まる。
#[test]
fn default_limit_is_sixteen() {
    let dir = tempdir();
    let (core, calls) = cycle_core(dir.path(), None);
    assert_eq!(core.trigger_limits(), TriggerLimits::default());
    let a = ctx("tenant-a");
    let err = run(&core, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls.load(Ordering::SeqCst), 16);
    assert_no_rows(&core, &["trg", "cyc_alpha", "cyc_beta"]);
}

// (d) 上限ちょうどの非循環チェーンは成功し、1 段深いと `54001`（境界）。
#[test]
fn chain_at_limit_succeeds_and_one_deeper_fails() {
    let dir = tempdir();
    let (core, calls) = chain_core(dir.path(), 4, Some(4));
    let a = ctx("tenant-a");
    run(&core, &a, INSERT_TRG).expect("depth == limit passes");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    for t in ["trg", "c1", "c2", "c3", "c4"] {
        assert_eq!(count(&core, &a, t), 1, "{t}");
    }

    let dir2 = tempdir();
    let (core2, calls2) = chain_core(dir2.path(), 5, Some(4));
    let err = run(&core2, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls2.load(Ordering::SeqCst), 4);
    assert_no_rows(&core2, &["trg", "c1", "c2", "c3", "c4", "c5"]);
}

// (e) 同じ階層の兄弟トリガ・複数行は深さを累積しない（入れ子でのみ増える）。
#[test]
fn sibling_triggers_and_rows_do_not_accumulate_depth() {
    let dir = tempdir();
    let (core, calls) = core_with_map(dir.path(), Vec::new(), Some(1));
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b"));
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a1"));
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a2"));
    let a = ctx("tenant-a");
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES \
         (1, '[1.0,0.0]', 'x', 1), (2, '[2.0,0.0]', 'y', 2)",
    )
    .expect("siblings pass at limit 1");
    assert_eq!(calls.load(Ordering::SeqCst), 6);
    assert_eq!(count(&core, &a, "trg"), 2);
}

// (f) `54001` の後に深さカウンタが 0 へ戻り、同じスレッドの次の文が通る。
#[test]
fn depth_counter_is_restored_after_rejection() {
    let dir = tempdir();
    let (core, _calls) = cycle_core(dir.path(), Some(3));
    let a = ctx("tenant-a");
    assert!(run(&core, &a, INSERT_TRG).is_err());
    assert_eq!(crate::trigger::depth::current_depth(), 0);
    // 循環の入口を外した別の文（`plain_a` にはトリガが無い）は成功する。
    let dir2 = tempdir();
    let (core2, calls2) = chain_core(dir2.path(), 2, Some(3));
    run(&core2, &a, INSERT_TRG).expect("next statement");
    assert_eq!(calls2.load(Ordering::SeqCst), 2);
    assert_eq!(crate::trigger::depth::current_depth(), 0);
}

// (g) 明示トランザクション内の `54001`: 文が失敗し以降は `25P02`。ROLLBACK 後に副作用なし。
#[test]
fn rejection_in_explicit_transaction_fails_the_session() {
    let dir = tempdir();
    let (core, _calls) = cycle_core(dir.path(), Some(3));
    let a = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, INSERT_TRG)
        .unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, "SELECT COUNT(*) FROM trg")
        .unwrap_err();
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_no_rows(&core, &["trg", "cyc_alpha", "cyc_beta"]);
}

// (h) 入れ子で書いた先の遅延 FK 違反は検出される（fail-open にならない）。
#[test]
fn deferred_fk_written_in_nested_trigger_is_checked() {
    let dir = tempdir();
    let calls = Arc::new(AtomicUsize::new(0));
    let core = EngineCore::from_storage(
        open_storage(dir.path()),
        Box::new(crate::kernel::CpuScalarProvider),
    )
    .with_trigger_invoker_for_test(Arc::new(FnInvoker {
        behavior: Box::new(|c| {
            match c.table {
                "trg" => c.writes.insert_row("c1", None, c.new_row)?,
                "c1" => c
                    .writes
                    .insert_row("fk_child", None, &[Value::BigInt(999)])?,
                _ => {}
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
        calls,
    }));
    let sys = ctx("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE TABLE fk_parent (k BIGINT PRIMARY KEY)",
        "CREATE TABLE fk_child (parent_id BIGINT REFERENCES fk_parent DEFERRABLE INITIALLY DEFERRED)",
    ] {
        core.execute_sql_in_session(&sys, &mut s, sql).expect(sql);
    }
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("t0"));
    add_trigger(&core.storage, "tenant-a", "c1", &after_row("t1"));
    let err = run(&core, &sys, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "23503");
    assert_no_rows(&core, &["trg", "c1", "fk_child"]);
}

// (i) `with_trigger_limits` と `with_trigger_invoker_for_test` の呼び出し順に依存しない。
#[test]
fn limit_survives_either_builder_order() {
    let dir = tempdir();
    let calls = Arc::new(AtomicUsize::new(0));
    let map = vec![
        ("trg", "cyc_alpha"),
        ("cyc_alpha", "cyc_beta"),
        ("cyc_beta", "cyc_alpha"),
    ];
    // 上限を先に設定し、あとから invoker を差し替える順。
    let core = EngineCore::from_storage(
        open_storage(dir.path()),
        Box::new(crate::kernel::CpuScalarProvider),
    )
    .with_trigger_limits(TriggerLimits::new(2))
    .with_trigger_invoker_for_test(invoker(map, Arc::clone(&calls)));
    assert_eq!(core.trigger_limits().max_trigger_depth(), 2);
    for t in ["trg", "cyc_alpha", "cyc_beta"] {
        add_trigger(&core.storage, "tenant-a", t, &after_row("cyc"));
    }
    let a = ctx("tenant-a");
    let err = run(&core, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // invoker を先に差し替え、あとから上限を設定する順（`cycle_core`）。
    let dir2 = tempdir();
    let (core2, calls2) = cycle_core(dir2.path(), Some(2));
    let err = run(&core2, &a, INSERT_TRG).unwrap_err();
    assert_eq!(err.wire_code(), "54001");
    assert_eq!(calls2.load(Ordering::SeqCst), 2);
}

// (j) 既定の深さ 16 が 2 MiB スタックのスレッドに収まる（オーバーフローしない）。
#[test]
fn default_depth_fits_a_two_mebibyte_stack() {
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let dir = tempdir();
            let (core, calls) = cycle_core(dir.path(), None);
            let a = ctx("tenant-a");
            let err = run(&core, &a, INSERT_TRG).unwrap_err();
            assert_eq!(err.wire_code(), "54001");
            assert_eq!(calls.load(Ordering::SeqCst), 16);
        })
        .expect("spawn");
    handle.join().expect("no stack overflow");
}

// (k) `54001` の応答は固定文言のみ（表名・テナント・値を含まない）。
#[test]
fn rejection_message_is_fixed_and_leaks_nothing() {
    let dir = tempdir();
    let (core, _calls) = cycle_core(dir.path(), Some(3));
    let a = ctx("tenant-a");
    let err = run(&core, &a, INSERT_TRG).unwrap_err();
    let text = format!("{err}");
    assert!(
        text.contains("trigger nesting depth limit exceeded"),
        "{text}"
    );
    for needle in ["cyc_alpha", "cyc_beta", "trg", "tenant-a"] {
        assert!(!text.contains(needle), "{needle} leaked in {text}");
    }
}

// (l) 通過証の無い（未結線の）書き込み窓口では、入れ子の書き込み先のトリガは従来どおり `0A000`。
#[test]
fn handle_without_runtime_keeps_0a000() {
    let dir = tempdir();
    let storage = open_storage(dir.path());
    add_trigger(&storage, "tenant-a", "c1", &after_row("t1"));
    let pctx = ctx("tenant-a");
    let txn = storage.begin_write_txn().expect("txn");
    let handle = crate::trigger::write_handle::TriggerWriteHandle::new(&txn, &pctx, None);
    let row = [
        Value::Vector(vec![1.0, 0.0]),
        Value::Text("x".to_string()),
        Value::BigInt(1),
    ];
    let err = handle.insert_row("c1", None, &row).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}
