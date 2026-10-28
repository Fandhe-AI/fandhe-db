//! WHERE の等価条件の結合キー昇格テスト（Issue #1834。ポインタ: SQL-60・SQL-28・RLS-10）。
//!
//! `tests/sql60_comma_join.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。検証対象は
//! (a) 昇格あり（素の WHERE）と明示 `JOIN ... ON` の結果一致、(b) 結合順（段 1 への昇格）、
//! (c) 混在、(d) 出力行数上限が直積でなく一致件数で判定されること（昇格が効いている証拠）、
//! (e) 外部結合との併用、(f) エラー契約の不変、(g) テナント境界、(h) 昇格あり／なし／総当たり
//! オラクルの 3 者一致のプロパティテスト。「昇格なし」は等価を `(p OR p)` で包んで昇格対象外に
//! した同値な問い合わせで作る。期待値は手計算と総当たりオラクル。

use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

fn exec(core: &EngineCore, sql: &str) {
    exec_as(core, "tenant-a", sql);
}

fn try_run_as(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<QueryResult, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    try_run_as(core, "tenant-a", sql)
        .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}"))
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    match try_run_as(core, "tenant-a", sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => assert_eq!(e.wire_code(), wire_code, "sql={sql:?} err={e:?}"),
    }
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(render).collect())
        .collect()
}

fn sorted_rows(result: &QueryResult) -> Vec<Vec<String>> {
    let mut rows = rows_of(result);
    rows.sort();
    rows
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn open_core(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// a: k=1,2,3、b: k=2,3、c: k=2,4（`sql60_comma_join.rs` と同じ）。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let (core, path) = open_core("where-key-promotion");
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
    ] {
        exec(&core, sql);
    }
    (core, path)
}

#[test]
fn promoted_where_matches_explicit_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for (promoted, explicit) in [
        (
            "SELECT a.v, b.w, c.x FROM a, b, c WHERE a.k = b.k AND b.k = c.k LIMIT 100",
            "SELECT a.v, b.w, c.x FROM a JOIN b ON a.k = b.k JOIN c ON b.k = c.k LIMIT 100",
        ),
        // 段 1 への昇格（段 0 は直積のまま）と左右逆の記述。
        (
            "SELECT a.v, b.w, c.x FROM a, b, c WHERE a.k = c.k LIMIT 100",
            "SELECT a.v, b.w, c.x FROM a CROSS JOIN b JOIN c ON a.k = c.k LIMIT 100",
        ),
        (
            "SELECT a.v, b.w, c.x FROM a, b, c WHERE c.k = a.k LIMIT 100",
            "SELECT a.v, b.w, c.x FROM a CROSS JOIN b JOIN c ON a.k = c.k LIMIT 100",
        ),
        // 混在: ON 結合 + カンマ、CROSS + WHERE。
        (
            "SELECT a.v, b.w, c.x FROM a JOIN b ON a.k = b.k, c WHERE b.k = c.k LIMIT 100",
            "SELECT a.v, b.w, c.x FROM a JOIN b ON a.k = b.k JOIN c ON b.k = c.k LIMIT 100",
        ),
        (
            "SELECT a.v, b.w FROM a CROSS JOIN b WHERE a.k = b.k LIMIT 100",
            "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k LIMIT 100",
        ),
        // 等価 + 非等価 + 単一 relation 条件の併用（非等価は残余に残る）。
        (
            "SELECT a.v, b.w FROM a, b WHERE a.k = b.k AND a.k < b.k AND a.v = 'a2' LIMIT 100",
            "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k WHERE a.k < b.k AND a.v = 'a2' LIMIT 100",
        ),
    ] {
        let l = run(&core, promoted);
        let r = run(&core, explicit);
        assert_eq!(sorted_rows(&l), sorted_rows(&r), "sql={promoted:?}");
    }
    let r = run(
        &core,
        "SELECT a.v, b.w, c.x FROM a, b, c WHERE a.k = b.k AND b.k = c.k LIMIT 100",
    );
    assert_eq!(sorted_rows(&r), strs(&[&["a2", "b2", "c2"]]));
    let r = run(
        &core,
        "SELECT a.v, b.w FROM a, b WHERE a.k = b.k AND a.k < b.k LIMIT 100",
    );
    assert!(r.rows.is_empty());
}

#[test]
fn output_limit_is_judged_on_matches_not_on_the_product() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 2));
    // 昇格形は一致件数（段 0: 2 行、段 1: 1 行）で判定されるため成功する。
    assert_eq!(
        run(
            &core,
            "SELECT a.v FROM a, b, c WHERE a.k = b.k AND b.k = c.k LIMIT 100"
        )
        .rows
        .len(),
        1
    );
    // 昇格対象外の形（OR で包む・非等価）は直積の件数で判定され 54000。
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE (a.k = b.k OR a.k = b.k) LIMIT 100",
        "54000",
    );
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE a.k < b.k LIMIT 100",
        "54000",
    );
}

#[test]
fn outer_join_reduction_enables_promotion() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for (promoted, wrapped) in [
        (
            "SELECT a.v, b.w, c.x FROM a LEFT JOIN b ON a.k = b.k, c WHERE b.k = c.k LIMIT 100",
            "SELECT a.v, b.w, c.x FROM a LEFT JOIN b ON a.k = b.k, c \
             WHERE (b.k = c.k OR b.k = c.k) LIMIT 100",
        ),
        (
            "SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k WHERE a.k = b.k LIMIT 100",
            "SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k \
             WHERE (a.k = b.k OR a.k = b.k) LIMIT 100",
        ),
        (
            "SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k, c WHERE a.k = c.k LIMIT 100",
            "SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k, c \
             WHERE (a.k = c.k OR a.k = c.k) LIMIT 100",
        ),
    ] {
        let l = run(&core, promoted);
        let r = run(&core, wrapped);
        assert_eq!(rows_of(&l), rows_of(&r), "sql={promoted:?}");
    }
    // 段 0（LEFT）は WHERE が b を参照しないので保存され、a2 だけが c と一致する。
    let r = run(
        &core,
        "SELECT a.v, b.w FROM a LEFT JOIN b ON a.k = b.k, c WHERE a.k = c.k LIMIT 100",
    );
    assert_eq!(sorted_rows(&r), strs(&[&["a2", "b2"]]));
}

#[test]
fn error_contract_is_unchanged() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE a.v = b.k LIMIT 10",
        "42804",
    );
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE a.k = b.nope LIMIT 10",
        "42703",
    );
    // DOUBLE 列同士の等価は昇格せず従来どおり残余で評価される。
    exec(&core, "CREATE TABLE fa (f DOUBLE PRECISION)");
    exec(&core, "CREATE TABLE fb (f DOUBLE PRECISION)");
    exec(&core, "INSERT INTO fa (f) VALUES (1.5), (2.5)");
    exec(&core, "INSERT INTO fb (f) VALUES (2.5), (3.5)");
    let r = run(&core, "SELECT fa.f FROM fa, fb WHERE fa.f = fb.f LIMIT 10");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn other_tenant_rows_never_surface_through_promoted_key() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    exec_as(
        &core,
        "tenant-b",
        "INSERT INTO b (k, w) VALUES (1, 'other-b1'), (2, 'other-b2')",
    );
    let r = run(&core, "SELECT a.v, b.w FROM a, b WHERE a.k = b.k LIMIT 100");
    assert_eq!(sorted_rows(&r), strs(&[&["a2", "b2"], &["a3", "b3"]]));
    // 上限判定も自テナントの可視行だけで行われる。
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 2));
    assert_eq!(
        run(&core, "SELECT a.v, b.w FROM a, b WHERE a.k = b.k LIMIT 100")
            .rows
            .len(),
        2
    );
}

/// 固定シードの擬似乱数（テストの再現性だけが目的。暗号用途ではない）。
struct Xorshift64(u64);

impl Xorshift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// `(i, k, t)`。`i` は表をまたいで一意な行識別子。
type Row = (i64, Option<i64>, Option<&'static str>);

const TEXTS: [&str; 3] = ["p", "q", "r"];

fn gen_rows(rng: &mut Xorshift64, base: i64) -> Vec<Row> {
    let n = rng.below(7);
    (0..n as i64)
        .map(|i| {
            let k = match rng.below(5) {
                0 => None,
                v => Some(v as i64 - 1),
            };
            let t = match rng.below(4) {
                3 => None,
                v => TEXTS.get(v as usize).copied(),
            };
            (base + i, k, t)
        })
        .collect()
}

fn insert_rows(core: &EngineCore, table: &str, rows: &[Row]) {
    for (i, k, t) in rows {
        let k = k.map_or("NULL".to_string(), |v| v.to_string());
        let t = t.map_or("NULL".to_string(), |v| format!("'{v}'"));
        exec(
            core,
            &format!("INSERT INTO {table} (i, k, t) VALUES ({i}, {k}, {t})"),
        );
    }
}

/// WHERE の conjunct 候補。添字は a=0・b=1・c=2。
#[derive(Clone, Copy)]
enum Cond {
    EqK(usize, usize),
    EqT(usize, usize),
    LtK(usize, usize),
    TextIs(usize, &'static str),
}

const REL: [&str; 3] = ["a", "b", "c"];

impl Cond {
    fn sql(self, wrap: bool) -> String {
        match self {
            Cond::EqK(x, y) if wrap => {
                format!("({0}.k = {1}.k OR {0}.k = {1}.k)", REL[x], REL[y])
            }
            Cond::EqT(x, y) if wrap => {
                format!("({0}.t = {1}.t OR {0}.t = {1}.t)", REL[x], REL[y])
            }
            Cond::EqK(x, y) => format!("{}.k = {}.k", REL[x], REL[y]),
            Cond::EqT(x, y) => format!("{}.t = {}.t", REL[x], REL[y]),
            Cond::LtK(x, y) => format!("{}.k < {}.k", REL[x], REL[y]),
            Cond::TextIs(x, v) => format!("{}.t = '{}'", REL[x], v),
        }
    }

    /// 総当たりオラクル。NULL を含む比較は偽。
    fn holds(self, rows: [&Row; 3]) -> bool {
        match self {
            Cond::EqK(x, y) => matches!((rows[x].1, rows[y].1), (Some(l), Some(r)) if l == r),
            Cond::EqT(x, y) => matches!((rows[x].2, rows[y].2), (Some(l), Some(r)) if l == r),
            Cond::LtK(x, y) => matches!((rows[x].1, rows[y].1), (Some(l), Some(r)) if l < r),
            Cond::TextIs(x, v) => rows[x].2 == Some(v),
        }
    }
}

fn gen_cond(rng: &mut Xorshift64) -> Cond {
    let x = rng.below(3) as usize;
    let y = (x + 1 + rng.below(2) as usize) % 3;
    match rng.below(5) {
        0 | 1 => Cond::EqK(x, y),
        2 => Cond::EqT(x, y),
        3 => Cond::LtK(x, y),
        _ => Cond::TextIs(x, TEXTS[rng.below(3) as usize]),
    }
}

#[test]
fn promotion_matches_unpromoted_and_brute_force_oracle() {
    let mut rng = Xorshift64(0x9E37_79B9_7F4A_7C15);
    for case in 0..40 {
        let ra = gen_rows(&mut rng, 100);
        let rb = gen_rows(&mut rng, 200);
        let rc = gen_rows(&mut rng, 300);
        let n = 1 + rng.below(3) as usize;
        let conds: Vec<Cond> = (0..n).map(|_| gen_cond(&mut rng)).collect();
        let (core, path) = open_core("where-key-promotion-prop");
        let _guard = CleanupGuard(path);
        for t in REL {
            exec(
                &core,
                &format!("CREATE TABLE {t} (i BIGINT, k BIGINT, t TEXT)"),
            );
        }
        insert_rows(&core, "a", &ra);
        insert_rows(&core, "b", &rb);
        insert_rows(&core, "c", &rc);

        let mut expected: Vec<Vec<String>> = Vec::new();
        for x in &ra {
            for y in &rb {
                for z in &rc {
                    if conds.iter().all(|c| c.holds([x, y, z])) {
                        expected.push(vec![x.0.to_string(), y.0.to_string(), z.0.to_string()]);
                    }
                }
            }
        }
        expected.sort();

        let promoted = conds
            .iter()
            .map(|c| c.sql(false))
            .collect::<Vec<_>>()
            .join(" AND ");
        let wrapped = conds
            .iter()
            .map(|c| c.sql(true))
            .collect::<Vec<_>>()
            .join(" AND ");
        for from in ["a, b, c", "a CROSS JOIN b CROSS JOIN c"] {
            let q = |w: &str| format!("SELECT a.i, b.i, c.i FROM {from} WHERE {w} LIMIT 1000");
            let ctx =
                format!("case={case} from={from} where={promoted} a={ra:?} b={rb:?} c={rc:?}");
            assert_eq!(
                sorted_rows(&run(&core, &q(&promoted))),
                expected,
                "promoted != oracle: {ctx}"
            );
            assert_eq!(
                sorted_rows(&run(&core, &q(&wrapped))),
                expected,
                "unpromoted != oracle: {ctx}"
            );
        }
    }
}
