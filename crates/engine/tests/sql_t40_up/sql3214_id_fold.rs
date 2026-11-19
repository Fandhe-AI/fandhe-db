//! `SUM`／`AVG`／`MIN`／`MAX(id)` を可視 id スライスの畳み込みで返す変更（Issue #3214・SQL-25・
//! SQL-71 のポインタ。設計記録: `docs/design/aggregate-loop-regression.md`）の結合テスト。
//!
//! 1 回目（キャッシュ構築の全走査）と 2 回目以降（キャッシュ命中の畳み込み）のビット一致、従来経路
//! （`WHERE`・`FILTER`・式入力）とのオラクル比較、空・1 行・u64 上限・テナント境界・中断契約を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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

/// 従来方式（疑似列 `id` を持つ）の表 `t(v INTEGER)` を作る。`SUM(id)` 等が `IdU64` に束縛されるのは
/// この方式の表だけ（`id` を宣言した採番方式の表は列集計の経路になる）。
fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("v", ColumnType::Integer, true)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
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

const ALL5: &str = "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM t";

/// 初回と命中が一致し、従来経路（WHERE で Fast を外す・FILTER・式入力）と値が一致する。
#[test]
fn sql3214_cached_equals_first_and_oracles() {
    let (core, _g) = open("sql3214-oracle");
    seed(&core, &alice(), 0, 2500, 5);
    let first = dbg(&core, &alice(), ALL5);
    for _ in 0..3 {
        assert_eq!(dbg(&core, &alice(), ALL5), first);
    }
    // 0..2500 の総和 3123750、平均 1249.5 は整数 AVG の丸めに従う（従来経路と同値で比較する）。
    assert!(
        first.contains("3123750") && first.contains("2499"),
        "{first}"
    );
    let oracle = dbg(
        &core,
        &alice(),
        "SELECT COUNT(*), SUM(id), AVG(id), MIN(id), MAX(id) FROM t WHERE id >= 0",
    );
    assert_eq!(oracle, first);
    let filt = dbg(
        &core,
        &alice(),
        "SELECT COUNT(*) FILTER (WHERE true), SUM(id) FILTER (WHERE true), AVG(id) FILTER (WHERE true), MIN(id) FILTER (WHERE true), MAX(id) FILTER (WHERE true) FROM t",
    );
    assert_eq!(filt, first);
    for single in ["SUM(id)", "AVG(id)", "MIN(id)", "MAX(id)", "COUNT(id)"] {
        let sql = format!("SELECT {single} FROM t");
        let a = dbg(&core, &alice(), &sql);
        let b = dbg(&core, &alice(), &sql);
        let w = dbg(&core, &alice(), &format!("{sql} WHERE id >= 0"));
        assert_eq!(a, b, "{sql}");
        assert_eq!(a, w, "{sql}");
    }
}

/// 0 行・全行が他テナント・id 0 の 1 行。
#[test]
fn sql3214_empty_invisible_and_zero_id() {
    let (core, _g) = open("sql3214-empty");
    let first = dbg(&core, &alice(), ALL5);
    assert_eq!(dbg(&core, &alice(), ALL5), first);
    assert_eq!(
        first,
        dbg(&core, &alice(), &format!("{ALL5} WHERE id >= 0"))
    );
    seed(&core, &ctx_of("bob"), 5000, 6500, 0);
    for _ in 0..3 {
        assert_eq!(dbg(&core, &alice(), ALL5), first);
    }
    core.execute_insert_sql(&alice(), "INSERT INTO t (id, v) VALUES (0, 1)")
        .expect("one row");
    for _ in 0..2 {
        let a = dbg(&core, &alice(), ALL5);
        assert_eq!(a, dbg(&core, &alice(), &format!("{ALL5} WHERE id >= 0")));
        assert!(!a.contains("Null") || a == first, "{a}");
        let m = dbg(&core, &alice(), "SELECT MIN(id) FROM t");
        assert!(!m.contains("Null"), "{m}");
    }
}

/// 他テナントの大きな id が混入せず、書き込み後に古い値を返さない。
#[test]
fn sql3214_tenant_boundary_and_write_invalidation() {
    let (core, _g) = open("sql3214-tenant");
    seed(&core, &ctx_of("bob"), 100_000, 103_000, 0);
    seed(&core, &alice(), 1, 4, 0);
    for _ in 0..3 {
        let a = dbg(&core, &alice(), "SELECT SUM(id), MIN(id), MAX(id) FROM t");
        assert!(a.contains('6') && !a.contains("100"), "{a}");
    }
    seed(&core, &alice(), 4, 6, 0);
    for _ in 0..2 {
        let a = dbg(&core, &alice(), "SELECT SUM(id), MAX(id) FROM t");
        assert!(a.contains("15") && a.contains('5'), "{a}");
    }
}

/// 合計が u64 を超えると 22003、上限ちょうどは成功する。
#[test]
fn sql3214_overflow_is_numeric_out_of_range() {
    let (core, _g) = open("sql3214-overflow");
    let big = u64::MAX / 2;
    for id in [big, big - 1] {
        core.execute_insert_sql(&alice(), &format!("INSERT INTO t (id, v) VALUES ({id}, 1)"))
            .expect("big id");
    }
    for _ in 0..2 {
        let s = dbg(&core, &alice(), "SELECT SUM(id) FROM t");
        assert!(s.contains("18446744073709551613"), "{s}");
    }
    core.execute_insert_sql(
        &alice(),
        &format!("INSERT INTO t (id, v) VALUES ({}, 1)", big - 2),
    )
    .expect("third");
    for sql in ["SELECT SUM(id) FROM t", "SELECT AVG(id) FROM t"] {
        for _ in 0..2 {
            let e = run(&core, &alice(), sql).expect_err(sql);
            assert_eq!(e.wire_code(), "22003", "{sql}");
        }
    }
    // MIN／MAX／COUNT はオーバーフローしない。
    for _ in 0..2 {
        let m = dbg(&core, &alice(), "SELECT MIN(id), MAX(id), COUNT(*) FROM t");
        assert!(m.contains('3'), "{m}");
    }
}

/// 対象外の形は従来どおりの結果になる。
#[test]
fn sql3214_other_shapes_unchanged() {
    let (core, _g) = open("sql3214-shapes");
    seed(&core, &alice(), 0, 100, 0);
    for _ in 0..2 {
        assert!(dbg(&core, &alice(), "SELECT SUM(id) FROM t WHERE id < 10").contains("45"));
        assert!(dbg(
            &core,
            &alice(),
            "SELECT SUM(id) FILTER (WHERE id < 5) FROM t"
        )
        .contains("10"));
        assert!(dbg(&core, &alice(), "SELECT SUM(id + 0) FROM t").contains("4950"));
        assert!(dbg(&core, &alice(), "SELECT MAX(v) FROM t").contains('6'));
        assert!(dbg(
            &core,
            &alice(),
            "SELECT SUM(id) FROM t HAVING COUNT(*) > 50"
        )
        .contains("4950"));
        let g = dbg(&core, &alice(), "SELECT v, MAX(id) FROM t GROUP BY v");
        assert!(g.contains("99"), "{g}");
    }
}

/// キャッシュ命中時、非空なら確認し 57014 になりうる。複数チャンクでも u32::MAX なら成功する。
#[test]
fn sql3214_cache_hit_checks_and_fires() {
    let (core, _g) = open("sql3214-check");
    seed(&core, &alice(), 0, 2500, 0);
    for _ in 0..2 {
        run(&core, &alice(), ALL5).expect("warm");
    }
    {
        let _p = install_fire_after_checks(u32::MAX);
        run(&core, &alice(), ALL5).expect("fold");
        assert!(checks_seen() >= 1);
    }
    let _p = install_fire_after_checks(0);
    let e = run(&core, &alice(), ALL5).expect_err("fire");
    assert_eq!(e.wire_code(), "57014");
}
