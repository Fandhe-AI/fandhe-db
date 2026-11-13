//! 集計の行ループの中断確認を N 行ごとにする変更（Issue #3208・SQL-71 のポインタ。設計記録:
//! `docs/design/aggregate-loop-regression.md`）の結合テスト。
//!
//! 確認回数（`checks_seen`）で「行ごとの確認が行数ではなく ⌈行数/N⌉ 回になった」ことを固定し、
//! 後方の確認点でも `57014` で中断されること、結果が従来と一致すること、RLS で不可視な行だけの
//! 走査でも先頭行で中断できることを確かめる。時計・sleep には依存しない。

use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::interrupt::{
    checks_seen, install_fire_after_checks, DeadlineScope, InterruptReason, InterruptScope,
    StatementDeadline,
};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// `engine::interrupt::CHECK_STRIDE` と同じ（行ループ局所の間引き間隔）。
const N: u64 = 1024;
const ROWS: u64 = 2 * N + 452;

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn alice() -> PolicyContext {
    ctx_of("alice")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &alice(),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INTEGER)",
    )
    .expect("ddl");
    (core, CleanupGuard(path))
}

fn seed(core: &EngineCore, ctx: &PolicyContext, from: u64, to: u64, null_every: u64) {
    let mut id = from;
    while id < to {
        let end = (id + 60).min(to);
        let values: Vec<String> = (id..end)
            .map(|i| {
                if null_every != 0 && i % null_every == 0 {
                    format!("({i}, NULL)")
                } else {
                    format!("({i}, {})", i % 7)
                }
            })
            .collect();
        core.execute_insert_sql(
            ctx,
            &format!("INSERT INTO t (id, v) VALUES {}", values.join(", ")),
        )
        .expect("seed");
        id = end;
    }
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
}

const QUERIES: &[&str] = &[
    "SELECT SUM(v) FROM t",
    "SELECT COUNT(*), SUM(v), AVG(v), MIN(v), MAX(v) FROM t",
    "SELECT COUNT(*) FROM t",
    "SELECT SUM(v) FROM t WHERE id < 0",
];

/// 確認回数が行数ではなく ⌈行数/N⌉ の水準になる（最適化の回帰ピン）。
#[test]
fn sql3208_checks_scale_with_interval_not_rows() {
    let (core, _g) = open("sql3208-count");
    seed(&core, &alice(), 0, ROWS, 5);
    for sql in &QUERIES[..3] {
        // 2 回流して可視キャッシュ経路（Fast）にも到達させる。
        for _ in 0..2 {
            let _probe = install_fire_after_checks(u32::MAX);
            run(&core, &alice(), sql).expect(sql);
            let seen = checks_seen();
            assert!(seen >= 1, "{sql}: first row must be checked");
            assert!(u64::from(seen) < ROWS / 8, "{sql}: seen={seen} not per-row");
        }
    }
}

/// 後方の確認点（n 回通過後）でも 57014 になり、全確認点を通れば成功する。
#[test]
fn sql3208_every_check_point_can_interrupt() {
    let (core, _g) = open("sql3208-fire");
    seed(&core, &alice(), 0, ROWS, 0);
    for sql in &QUERIES[..2] {
        for _ in 0..2 {
            let total = {
                let _p = install_fire_after_checks(u32::MAX);
                run(&core, &alice(), sql).expect(sql);
                checks_seen()
            };
            for n in 0..total {
                let _p = install_fire_after_checks(n);
                let e = run(&core, &alice(), sql).expect_err(sql);
                assert_eq!(e.wire_code(), "57014", "{sql} n={n}");
            }
            let _p = install_fire_after_checks(total);
            run(&core, &alice(), sql).expect(sql);
        }
    }
}

/// `InterruptScope` の poll が 2 回目で発火しても 57014（部分結果を返さない）。
#[test]
fn sql3208_liveness_poll_fires_on_later_interval() {
    let (core, _g) = open("sql3208-poll");
    seed(&core, &alice(), 0, ROWS, 0);
    let mut calls = 0u32;
    let _s = InterruptScope::install(Box::new(move || {
        calls += 1;
        (calls >= 2).then_some(InterruptReason::ClientGone)
    }));
    let e = run(&core, &alice(), "SELECT SUM(v) FROM t").expect_err("poll");
    assert_eq!(e.wire_code(), "57014");
}

/// スコープ無し・遠い期限・発火しない poll の 3 条件で結果が完全に一致する（NULL・空集合を含む）。
#[test]
fn sql3208_results_identical_with_and_without_scopes() {
    let (core, _g) = open("sql3208-same");
    seed(&core, &alice(), 0, ROWS, 5);
    let base: Vec<String> = QUERIES
        .iter()
        .map(|q| format!("{:?}", run(&core, &alice(), q).expect(q)))
        .collect();
    {
        let _d = DeadlineScope::install(StatementDeadline::after(
            Instant::now(),
            Duration::from_secs(3600),
        ));
        for (q, b) in QUERIES.iter().zip(&base) {
            assert_eq!(
                &format!("{:?}", run(&core, &alice(), q).expect(q)),
                b,
                "{q}"
            );
        }
    }
    let _s = InterruptScope::install(Box::new(|| None));
    for (q, b) in QUERIES.iter().zip(&base) {
        assert_eq!(
            &format!("{:?}", run(&core, &alice(), q).expect(q)),
            b,
            "{q}"
        );
    }
}

/// RLS: 他テナントの行が 1 境界以上続いても結果は自テナント分だけ。期限切れなら先頭行の確認で
/// 57014 になる。
#[test]
fn sql3208_rls_unchanged_and_invisible_rows_are_checked() {
    let (core, _g) = open("sql3208-rls");
    seed(&core, &ctx_of("bob"), 0, ROWS, 0);
    seed(&core, &alice(), ROWS, ROWS + 3, 0);
    let out = format!(
        "{:?}",
        run(&core, &alice(), "SELECT COUNT(*) FROM t").expect("count")
    );
    assert!(out.contains('3') && !out.contains("2500"), "{out}");
    let _d = DeadlineScope::install(StatementDeadline::at(Instant::now()));
    let e = run(&core, &alice(), "SELECT SUM(v) FROM t").expect_err("expired");
    assert_eq!(e.wire_code(), "57014");
}
