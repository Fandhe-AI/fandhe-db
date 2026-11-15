//! `COUNT(*)` の可視 id 集計をスライス長で返す変更（Issue #3213・SQL-25・SQL-71 のポインタ。
//! 設計記録: `docs/design/aggregate-loop-regression.md`）の結合テスト。
//!
//! 1 回目（キャッシュ構築の全走査）と 2 回目以降（可視 id キャッシュ命中）の結果一致、従来経路
//! （`COUNT(id)`・`FILTER`・`DISTINCT`）とのオラクル比較、空集合・NULL・テナント境界、中断契約を固定する。

use engine::core::EngineCore;
use engine::interrupt::{checks_seen, install_fire_after_checks};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

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

fn dbg(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    format!("{:?}", run(core, ctx, sql).expect(sql))
}

/// 初回（構築）と 2 回目以降（命中）が同一で、従来経路の `COUNT(id)` 等と件数が一致する。
#[test]
fn sql3213_cached_equals_first_and_oracles() {
    let (core, _g) = open("sql3213-oracle");
    seed(&core, &alice(), 0, 2500, 5);
    let first = dbg(&core, &alice(), "SELECT COUNT(*) FROM t");
    for _ in 0..3 {
        assert_eq!(dbg(&core, &alice(), "SELECT COUNT(*) FROM t"), first);
    }
    assert!(first.contains("2500"), "{first}");
    // 同じ件数を従来経路（行ループ）から得る。
    for oracle in [
        "SELECT COUNT(id) FROM t",
        "SELECT COUNT(*) FILTER (WHERE true) FROM t",
        "SELECT COUNT(DISTINCT id) FROM t",
    ] {
        for _ in 0..2 {
            let o = dbg(&core, &alice(), oracle);
            assert!(o.contains("2500"), "{oracle}: {o}");
        }
    }
    // 複数の COUNT(*) は全項目へ加算される。
    for _ in 0..2 {
        let m = dbg(&core, &alice(), "SELECT COUNT(*), COUNT(*) FROM t");
        assert_eq!(m.matches("2500").count(), 2, "{m}");
    }
    // NULL を含む列でも COUNT(*) は全可視行、COUNT(v) は NULL を除く。
    for _ in 0..2 {
        let v = dbg(&core, &alice(), "SELECT COUNT(v) FROM t");
        assert!(v.contains("2000") && !v.contains("2500"), "{v}");
    }
}

/// 0 行・全行が他テナントのテーブルは 0。
#[test]
fn sql3213_empty_and_invisible_are_zero() {
    let (core, _g) = open("sql3213-empty");
    let first = dbg(&core, &alice(), "SELECT COUNT(*) FROM t");
    assert_eq!(dbg(&core, &alice(), "SELECT COUNT(*) FROM t"), first);
    assert!(first.contains('0'), "{first}");
    seed(&core, &ctx_of("bob"), 0, 1500, 0);
    for _ in 0..3 {
        assert_eq!(dbg(&core, &alice(), "SELECT COUNT(*) FROM t"), first);
    }
    let bob = dbg(&core, &ctx_of("bob"), "SELECT COUNT(*) FROM t");
    assert!(bob.contains("1500"), "{bob}");
}

/// 自テナント分だけを数え、書き込み後はキャッシュが古い件数を返さない。
#[test]
fn sql3213_tenant_boundary_and_write_invalidation() {
    let (core, _g) = open("sql3213-tenant");
    seed(&core, &ctx_of("bob"), 0, 3000, 0);
    seed(&core, &alice(), 3000, 3007, 0);
    for _ in 0..3 {
        let a = dbg(&core, &alice(), "SELECT COUNT(*) FROM t");
        assert!(a.contains('7') && !a.contains("3000"), "{a}");
    }
    seed(&core, &alice(), 3007, 3010, 0);
    for _ in 0..2 {
        let a = dbg(&core, &alice(), "SELECT COUNT(*) FROM t");
        assert!(a.contains("10"), "{a}");
    }
}

/// 対象外の形（WHERE・FILTER・DISTINCT・GROUP BY・HAVING）は従来どおり。
#[test]
fn sql3213_other_shapes_unchanged() {
    let (core, _g) = open("sql3213-shapes");
    seed(&core, &alice(), 0, 100, 0);
    for _ in 0..2 {
        assert!(dbg(&core, &alice(), "SELECT COUNT(*) FROM t WHERE id < 10").contains("10"));
        assert!(dbg(
            &core,
            &alice(),
            "SELECT COUNT(*) FILTER (WHERE id < 5) FROM t"
        )
        .contains('5'));
        assert!(dbg(&core, &alice(), "SELECT COUNT(DISTINCT v) FROM t").contains('7'));
        assert!(dbg(
            &core,
            &alice(),
            "SELECT COUNT(*) FROM t HAVING COUNT(*) > 50"
        )
        .contains("100"));
        let g = dbg(&core, &alice(), "SELECT v, COUNT(*) FROM t GROUP BY v");
        assert!(g.contains("15"), "{g}");
    }
}

/// キャッシュ命中時も非空なら先頭で 1 回確認し、発火すれば 57014 になる。
#[test]
fn sql3213_cache_hit_still_checks_once() {
    let (core, _g) = open("sql3213-check");
    seed(&core, &alice(), 0, 2500, 0);
    for _ in 0..2 {
        run(&core, &alice(), "SELECT COUNT(*) FROM t").expect("warm");
    }
    {
        let _p = install_fire_after_checks(u32::MAX);
        run(&core, &alice(), "SELECT COUNT(*) FROM t").expect("count");
        assert!(checks_seen() >= 1);
    }
    let _p = install_fire_after_checks(0);
    let e = run(&core, &alice(), "SELECT COUNT(*) FROM t").expect_err("fire");
    assert_eq!(e.wire_code(), "57014");
}
