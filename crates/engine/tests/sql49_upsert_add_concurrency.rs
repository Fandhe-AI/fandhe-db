//! 同じキーへの 64 並列 `INSERT ... ON CONFLICT (id) DO UPDATE SET n = n + <k>`
//! （SQL-20・SQL-49、Issue #2428）の並行結合テスト。
//!
//! ポインタ: `docs/spec/04-behavior/` の SQL-20・SQL-49（不可視な衝突先の `42501` 分岐は
//! RLS-10・RLS-11 も）。production 経路は `EngineCore::execute_sql_in_session` →
//! `bind_upsert_assignments` → `tenant::upsert_typed_rows_unchecked` で、加算は衝突判定と
//! 同じ write トランザクション内で既存行の値へ行われる。write トランザクションは redb の
//! 排他ロックで直列化されるため、数え漏れ（lost update）が無いことを 3 テナント同時で固定する。
//!
//! 方針: 開始は `Barrier` で揃え、sleep・実時間の閾値は使わない。スレッド数は定数
//! （3 テナント × 64）で上限化する。書き込みロック待ちは上限（60 s）に設定し、許容する
//! エラーは `55P03` だけ（それ以外は fail-closed でテスト失敗）。
//! `$n` の受理（#2427）・wire 経由の並行・NoSQL 表層は対象外。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};
use std::sync::{Arc, Barrier};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "counters";
/// Issue #2428 が求める同一キーへの並列要求数（テナントごと）。無制限に増やさない。
const PARALLEL: usize = 64;
const TENANTS: [&str; 3] = ["tenant-a", "tenant-b", "tenant-c"];

fn open_engine(name: &str) -> (Arc<EngineCore>, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let wait = validate_write_lock_wait_ms(MAX_WRITE_LOCK_WAIT_MS).expect("valid wait");
    // Ledgered を付けないと operation_recorded が NoLedger を返し台帳の検証が空振りする。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered)
        .with_write_lock_wait(wait);
    (Arc::new(core), path)
}

fn ctx_full(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ctx_public_only(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn run(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

/// テナントごとの加算幅（混入を検出しやすくするため 1・2・3 と変える）。
fn step(tenant_idx: usize) -> i64 {
    i64::try_from(tenant_idx).expect("small index") + 1
}

fn read_n(core: &EngineCore, ctx: &PolicyContext) -> Vec<Cell> {
    let sql = format!("SELECT n FROM {TABLE} LIMIT 1000");
    let SqlOutcome::Query(result) = run(core, ctx, &sql).expect(&sql) else {
        panic!("expected Query outcome");
    };
    result
        .rows
        .into_iter()
        .map(|r| r.cells[0].clone())
        .collect()
}

fn recorded(core: &EngineCore, ctx: &PolicyContext, op_id: &str) -> LedgerLookup {
    let id = OperationId::parse(op_id).expect("valid operation_id");
    core.operation_recorded(ctx, TABLE, &id).expect("lookup")
}

type Outcome = Result<i64, &'static str>;

#[test]
fn parallel_add_on_same_key_counts_every_success_across_three_tenants() {
    // ガードを core より先に束縛する。ローカルは宣言の逆順に破棄されるため、
    // core（DB ハンドル）を先に閉じてから一時 DB を削除できる（Windows 対策）。
    let (_g, core) = {
        let (core, path) = open_engine("sql49-par-add");
        (CleanupGuard(path), core)
    };
    let barrier = Barrier::new(TENANTS.len() * PARALLEL);

    // キー id = 1 は事前に作らない（最初の挿入と衝突による加算の競合も対象に含める）。
    // 必ず全可視性の ctx を使う: public-only だと最初の挿入行（Private）が他スレッドから
    // 不可視になり、42501 が非決定的に混ざる（それは次のテストが別途固定する）。
    let results: Vec<(usize, String, Outcome)> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for (ti, tenant) in TENANTS.iter().enumerate() {
            for i in 0..PARALLEL {
                let core = &core;
                let barrier = &barrier;
                handles.push(s.spawn(move || {
                    let k = step(ti);
                    let op_id = format!("op-par-add-{tenant}-{i}");
                    let sql = format!(
                        "INSERT INTO {TABLE} (id, embedding, n) VALUES (1, '[0.1,0.2]', {k}) \
                         ON CONFLICT (id) DO UPDATE SET n = n + {k} RETURNING n \
                         USING OPERATION_ID '{op_id}'"
                    );
                    let ctx = ctx_full(tenant);
                    barrier.wait();
                    let out: Outcome = match run(core, &ctx, &sql) {
                        Ok(SqlOutcome::Returning(o)) => {
                            assert_eq!(o.result.rows.len(), 1, "{sql}");
                            match &o.result.rows[0].cells[0] {
                                Cell::SignedInteger(v) => Ok(*v),
                                other => panic!("unexpected cell {other:?}"),
                            }
                        }
                        Ok(_) => panic!("expected Returning outcome: {sql}"),
                        Err(e) => Err(e.wire_code()),
                    };
                    (ti, op_id, out)
                }));
            }
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("worker must not panic"))
            .collect()
    });

    // 許容するエラーは 55P03（書き込みロック待ち上限超過）だけ。他は fail-closed で失敗させる。
    let unexpected: Vec<_> = results
        .iter()
        .filter(|(_, _, o)| matches!(o, Err(c) if *c != "55P03"))
        .collect();
    assert!(unexpected.is_empty(), "unexpected errors: {unexpected:?}");

    for (ti, tenant) in TENANTS.iter().enumerate() {
        let k = step(ti);
        let ctx = ctx_full(tenant);
        let mine: Vec<_> = results.iter().filter(|(t, _, _)| *t == ti).collect();
        let mut returned: Vec<i64> = mine
            .iter()
            .filter_map(|(_, _, o)| o.as_ref().ok().copied())
            .collect();
        let succeeded = returned.len();
        let lock_timeouts = mine.len() - succeeded;
        assert!(
            succeeded >= 1,
            "{tenant}: no success ({lock_timeouts} x 55P03)"
        );

        // 直列化され lost update が無い: RETURNING は k, 2k, ..., S*k の順列
        returned.sort_unstable();
        let succeeded_i64 = i64::try_from(succeeded).expect("small");
        let expected: Vec<i64> = (1..=succeeded_i64).map(|j| j * k).collect();
        assert_eq!(returned, expected, "{tenant}: 55P03 x {lock_timeouts}");

        // 最終値 = 成功数 * k、行は 1 行だけ（二重挿入なし）
        assert_eq!(
            read_n(&core, &ctx),
            vec![Cell::SignedInteger(succeeded_i64 * k)],
            "{tenant}: 55P03 x {lock_timeouts}"
        );

        // 台帳: 成功は Recorded、失敗は NotRecorded
        for (_, op_id, o) in &mine {
            let want = if o.is_ok() {
                LedgerLookup::Recorded
            } else {
                LedgerLookup::NotRecorded
            };
            assert_eq!(recorded(&core, &ctx, op_id), want, "{op_id}");
        }
    }
}

#[test]
fn parallel_add_on_invisible_conflict_is_always_42501_without_side_effects() {
    // ガードを core より先に束縛する。ローカルは宣言の逆順に破棄されるため、
    // core（DB ハンドル）を先に閉じてから一時 DB を削除できる（Windows 対策）。
    let (_g, core) = {
        let (core, path) = open_engine("sql49-par-invisible");
        (CleanupGuard(path), core)
    };

    // SQL 表層の挿入は常に Private。テナントごとに特徴的なシード値を置く。
    let seed = |ti: usize| 7777 + i64::try_from(ti).expect("small") * 1000;
    for (ti, tenant) in TENANTS.iter().enumerate() {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, n) VALUES (2, '[0.1,0.2]', {}) \
             USING OPERATION_ID 'op-par-inv-seed-{tenant}'",
            seed(ti)
        );
        run(&core, &ctx_full(tenant), &sql).expect(&sql);
    }

    let barrier = Barrier::new(TENANTS.len() * PARALLEL);
    type InvOutcome = Result<(), (&'static str, String)>;
    let results: Vec<(usize, String, InvOutcome)> = std::thread::scope(|s| {
        let mut handles = Vec::new();
        for (ti, tenant) in TENANTS.iter().enumerate() {
            for i in 0..PARALLEL {
                let core = &core;
                let barrier = &barrier;
                handles.push(s.spawn(move || {
                    let op_id = format!("op-par-inv-{tenant}-{i}");
                    // 半数は RETURNING 付き（capturing 経路）で両分岐を覆う。
                    let returning = if i % 2 == 0 { "RETURNING n " } else { "" };
                    let sql = format!(
                        "INSERT INTO {TABLE} (id, embedding, n) VALUES (2, '[0.1,0.2]', 1) \
                         ON CONFLICT (id) DO UPDATE SET n = n + 1 {returning}\
                         USING OPERATION_ID '{op_id}'"
                    );
                    let ctx = ctx_public_only(tenant);
                    barrier.wait();
                    let out = match run(core, &ctx, &sql) {
                        Ok(_) => Ok(()),
                        Err(e) => Err((e.wire_code(), e.to_string())),
                    };
                    (ti, op_id, out)
                }));
            }
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("worker must not panic"))
            .collect()
    });

    for (ti, op_id, out) in &results {
        let (code, msg) = out.as_ref().expect_err(op_id);
        assert_eq!(*code, "42501", "{op_id}");
        // エラーにシード値・テナント ID・埋め込み値を含めない
        // 自テナントだけでなく全テナントのシード値の漏出を検査する
        for tj in 0..TENANTS.len() {
            assert!(
                !msg.contains(&seed(tj).to_string()),
                "{op_id}: seed of tenant {tj} leaked: {msg}"
            );
        }
        assert!(
            !msg.contains("tenant-") && !msg.contains("0.1"),
            "{op_id}: {msg}"
        );
        assert_eq!(
            recorded(&core, &ctx_full(TENANTS[*ti]), op_id),
            LedgerLookup::NotRecorded,
            "{op_id}"
        );
    }

    // 値・行数は不変、他テナントへの副作用も無い
    for (ti, tenant) in TENANTS.iter().enumerate() {
        assert_eq!(
            read_n(&core, &ctx_full(tenant)),
            vec![Cell::SignedInteger(seed(ti))],
            "{tenant}"
        );
        assert!(
            read_n(&core, &ctx_public_only(tenant)).is_empty(),
            "{tenant}"
        );
    }

    // 台帳を消費していない: 1 テナントにつき 1 件を可視 ctx で再送すると成功する
    for (ti, tenant) in TENANTS.iter().enumerate() {
        let op_id = format!("op-par-inv-{tenant}-0");
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, n) VALUES (2, '[0.1,0.2]', 1) \
             ON CONFLICT (id) DO UPDATE SET n = n + 1 RETURNING n USING OPERATION_ID '{op_id}'"
        );
        run(&core, &ctx_full(tenant), &sql).expect(&sql);
        assert_eq!(
            read_n(&core, &ctx_full(tenant)),
            vec![Cell::SignedInteger(seed(ti) + 1)],
            "{tenant}"
        );
    }
}
