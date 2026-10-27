//! 非等価・`OR` を含む `JOIN ... ON` の SQL 経路の受け入れテスト（Issue #1839。ポインタ: SQL-60）。
//!
//! ON 分解（`sql::join::on_split`）の結果で、結合段ごとにハッシュ結合（等価キーあり）と
//! ネステッドループ結合（キー無し）が自動で選ばれることを、production 経路（`EngineCore`）で
//! 確認する。期待行は PostgreSQL の意味論による手計算
//! （l: (1,x,1),(2,y,5),(3,x,NULL)、r: (1,x,2),(2,w,1),(3,z,9)、s: (1,3),(2,10)）。
//! 比較回数上限（`54000`）は `sql::join` の単体テストが担う。ここでは、キー無し FULL の
//! `0A000`・未対応構文の fail-closed・テナント境界を併せて固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
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

/// DDL を許可したセッションで文を実行する（`CREATE TABLE`・`INSERT` の準備用）。
fn exec(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// l(k,tag,n)・r(k,tag,n,lo,hi)・s(k,n)。`l.n` の NULL セルを 1 行含む。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("nlj-join");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE l (k BIGINT, tag TEXT, n BIGINT)",
        "CREATE TABLE r (k BIGINT, tag TEXT, n BIGINT, lo BIGINT, hi BIGINT)",
        "CREATE TABLE s (k BIGINT, n BIGINT)",
        "INSERT INTO l (k, tag, n) VALUES (1, 'x', 1), (2, 'y', 5)",
        "INSERT INTO l (k, tag) VALUES (3, 'x')",
        "INSERT INTO r (k, tag, n, lo, hi) \
         VALUES (1, 'x', 2, 0, 2), (2, 'w', 1, 3, 6), (3, 'z', 9, 7, 9)",
        "INSERT INTO s (k, n) VALUES (1, 3), (2, 10)",
    ] {
        exec(&core, sql);
    }
    (core, path)
}

fn try_run(
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

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn err_of(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    match try_run(core, tenant, sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => (e.wire_code().to_string(), format!("{e:?}")),
    }
}

/// 結果を文字列行へ描画してソートする。
fn run_sorted(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    let res = try_run(core, tenant, sql).unwrap_or_else(|e| panic!("sql={sql:?} err={e:?}"));
    let mut rows: Vec<Vec<String>> = res
        .rows
        .iter()
        .map(|row| row.cells.iter().map(render).collect())
        .collect();
    rows.sort();
    rows
}

fn expect(rows: &[&[&str]]) -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    v.sort();
    v
}

/// `(sql, 期待行)` の列を tenant-a で実行して照合する。
fn check(core: &EngineCore, cases: &[(&str, Vec<Vec<String>>)]) {
    for (sql, want) in cases {
        assert_eq!(&run_sorted(core, "tenant-a", sql), want, "sql={sql:?}");
    }
}

/// 非等価のみの ON（キー無し → NLJ）。INNER／LEFT／RIGHT と NULL セル・範囲条件。
#[test]
fn non_equi_only_on_for_inner_left_right() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    check(
        &core,
        &[
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.n < r.n LIMIT 10",
                expect(&[&["1", "1"], &["1", "3"], &["2", "3"]]),
            ),
            (
                "SELECT l.k, r.k FROM l LEFT JOIN r ON l.n < r.n LIMIT 10",
                expect(&[&["1", "1"], &["1", "3"], &["2", "3"], &["3", "NULL"]]),
            ),
            (
                "SELECT l.k, r.k FROM l RIGHT JOIN r ON l.n < r.n LIMIT 10",
                expect(&[&["1", "1"], &["1", "3"], &["2", "3"], &["NULL", "2"]]),
            ),
            // BETWEEN と同値の `>= AND <=` 形。NULL セルは一致しない。
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.n >= r.lo AND l.n <= r.hi LIMIT 10",
                expect(&[&["1", "1"], &["2", "2"]]),
            ),
            (
                "SELECT l.k, r.k FROM l LEFT JOIN r ON l.n >= r.lo AND l.n <= r.hi LIMIT 10",
                expect(&[&["1", "1"], &["2", "2"], &["3", "NULL"]]),
            ),
            (
                "SELECT l.k, r.k FROM l RIGHT JOIN r ON l.n >= r.lo AND l.n <= r.hi LIMIT 10",
                expect(&[&["1", "1"], &["2", "2"], &["NULL", "3"]]),
            ),
        ],
    );
}

/// 等価条件と非等価条件が混在する ON（キーあり → ハッシュ結合＋残余）。キーありの FULL は受理する。
#[test]
fn equi_and_non_equi_mixed_on() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    check(
        &core,
        &[
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.k = r.k AND l.n < r.n LIMIT 10",
                expect(&[&["1", "1"]]),
            ),
            (
                "SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k AND l.n < r.n LIMIT 10",
                expect(&[&["1", "1"], &["2", "NULL"], &["3", "NULL"]]),
            ),
            (
                "SELECT l.k, r.k FROM l RIGHT JOIN r ON l.k = r.k AND l.n < r.n LIMIT 10",
                expect(&[&["1", "1"], &["NULL", "2"], &["NULL", "3"]]),
            ),
            (
                "SELECT l.k, r.k FROM l FULL JOIN r ON l.k = r.k AND l.n < r.n LIMIT 10",
                expect(&[
                    &["1", "1"],
                    &["2", "NULL"],
                    &["3", "NULL"],
                    &["NULL", "2"],
                    &["NULL", "3"],
                ]),
            ),
            // キー＋OR 残余。l2/r2 は OR の右枝（r.tag = 'w'）で一致し、l3/r3 は NULL で一致しない。
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.k = r.k AND (l.n < r.n OR r.tag = 'w') LIMIT 10",
                expect(&[&["1", "1"], &["2", "2"]]),
            ),
            (
                "SELECT l.k, r.k FROM l FULL JOIN r \
                 ON l.k = r.k AND (l.n < r.n OR r.tag = 'w') LIMIT 10",
                expect(&[&["1", "1"], &["2", "2"], &["3", "NULL"], &["NULL", "3"]]),
            ),
            // 括弧付き AND からも結合キーを取り出し、括弧無しと同じ結果になる。
            (
                "SELECT l.k, r.k FROM l JOIN r ON (l.k = r.k AND l.n < r.n) LIMIT 10",
                expect(&[&["1", "1"]]),
            ),
        ],
    );
}

/// `OR` を含む ON。キー無しになる形（NLJ）と、括弧・単側だけの OR。
#[test]
fn or_in_on_clause() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    check(
        &core,
        &[
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.k = r.k OR l.n > r.n LIMIT 10",
                expect(&[&["1", "1"], &["2", "1"], &["2", "2"], &["3", "3"]]),
            ),
            (
                "SELECT l.k, r.k FROM l JOIN r ON (l.n < r.n OR l.tag = r.tag) LIMIT 10",
                expect(&[&["1", "1"], &["1", "3"], &["2", "3"], &["3", "1"]]),
            ),
            (
                "SELECT l.k, r.k FROM l RIGHT JOIN r ON (l.n < r.n OR l.tag = r.tag) LIMIT 10",
                expect(&[
                    &["1", "1"],
                    &["1", "3"],
                    &["2", "3"],
                    &["3", "1"],
                    &["NULL", "2"],
                ]),
            ),
            // 単側だけの OR（右のみ残余）。
            (
                "SELECT l.k, r.k FROM l JOIN r ON r.tag = 'x' OR r.tag = 'z' LIMIT 10",
                expect(&[
                    &["1", "1"],
                    &["2", "1"],
                    &["3", "1"],
                    &["1", "3"],
                    &["2", "3"],
                    &["3", "3"],
                ]),
            ),
            (
                "SELECT l.k, r.k FROM l RIGHT JOIN r ON r.tag = 'x' OR r.tag = 'z' LIMIT 10",
                expect(&[
                    &["1", "1"],
                    &["2", "1"],
                    &["3", "1"],
                    &["1", "3"],
                    &["2", "3"],
                    &["3", "3"],
                    &["NULL", "2"],
                ]),
            ),
        ],
    );
}

/// 3 表以上の連鎖でハッシュ段と NLJ 段が混在しても動く。
#[test]
fn mixed_hash_and_nested_loop_stages_in_a_chain() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    check(
        &core,
        &[
            // ハッシュ → NLJ。
            (
                "SELECT l.k, s.k FROM l JOIN r ON l.k = r.k JOIN s ON r.n < s.n LIMIT 10",
                expect(&[
                    &["1", "1"],
                    &["1", "2"],
                    &["2", "1"],
                    &["2", "2"],
                    &["3", "2"],
                ]),
            ),
            // NLJ → ハッシュ。
            (
                "SELECT l.k, r.k, s.k FROM l JOIN r ON l.n > r.n JOIN s ON s.k = l.k LIMIT 10",
                expect(&[&["2", "1", "2"], &["2", "2", "2"]]),
            ),
            // LEFT を含む連鎖（NULL 補完が後段へ伝わる）。
            (
                "SELECT l.k, r.k, s.k FROM l LEFT JOIN r ON l.n > r.n \
                 LEFT JOIN s ON r.k = s.k LIMIT 10",
                expect(&[
                    &["1", "NULL", "NULL"],
                    &["2", "1", "1"],
                    &["2", "2", "2"],
                    &["3", "NULL", "NULL"],
                ]),
            ),
            // OR を含む段との混在。
            (
                "SELECT l.k, s.k FROM l JOIN r ON l.k = r.k \
                 JOIN s ON (r.tag = 'x' OR s.k = r.k) LIMIT 10",
                expect(&[&["1", "1"], &["1", "2"], &["2", "2"]]),
            ),
        ],
    );
}

/// FULL: キー無し（`OR` 含む）は `0A000`。WHERE の strict 述語で INNER に簡約された段は NLJ で動く。
/// WHERE の等価昇格でキーを得た INNER 段はハッシュ経路へ入り、ON 残余は一致条件として評価される。
#[test]
fn keyless_full_is_rejected_but_reduced_and_promoted_stages_run() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for sql in [
        "SELECT l.k FROM l FULL JOIN r ON l.n > r.n LIMIT 10",
        "SELECT l.k FROM l FULL JOIN r ON l.k = r.k OR l.n > r.n LIMIT 10",
    ] {
        let (code, detail) = err_of(&core, "tenant-a", sql);
        assert_eq!(code, "0A000", "sql={sql:?} err={detail}");
    }
    check(
        &core,
        &[
            (
                "SELECT l.k, r.k FROM l FULL JOIN r ON l.n < r.n \
                 WHERE l.tag = 'x' AND r.tag = 'x' LIMIT 10",
                expect(&[&["1", "1"]]),
            ),
            (
                "SELECT l.k, r.k FROM l JOIN r ON l.n < r.n WHERE l.k = r.k LIMIT 10",
                expect(&[&["1", "1"]]),
            ),
        ],
    );
}

/// 未対応の形・型エラーは従来どおり fail-closed。
#[test]
fn unsupported_shapes_and_errors_stay_fail_closed() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for (sql, code) in [
        (
            "SELECT l.k FROM l JOIN r ON l.n BETWEEN r.lo AND r.hi LIMIT 10",
            "42601",
        ),
        (
            "SELECT l.k FROM l JOIN r ON NOT l.k = r.k LIMIT 10",
            "42601",
        ),
        (
            "SELECT l.k FROM l JOIN r ON l.k = r.k OR r.nope = 'x' LIMIT 10",
            "42703",
        ),
        ("SELECT l.k FROM l JOIN r ON l.n < r.tag LIMIT 10", "42804"),
        (
            "SELECT l.k FROM l JOIN r ON l.n < r.tag OR r.tag = 'x' LIMIT 10",
            "42804",
        ),
    ] {
        let (got, detail) = err_of(&core, "tenant-a", sql);
        assert_eq!(got, code, "sql={sql:?} err={detail}");
    }
    // 葉が 9 個の ON は 54000。
    let nine = ["l.n < r.n"; 9].join(" OR ");
    let (got, _) = err_of(
        &core,
        "tenant-a",
        &format!("SELECT l.k FROM l JOIN r ON {nine} LIMIT 10"),
    );
    assert_eq!(got, "54000");
}

/// NLJ の結果にも NULL 補完行にも他テナントの行は現れず、`0A000` はテナントに依存しない。
#[test]
fn nested_loop_stays_inside_the_tenant_boundary() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for sql in [
        "SELECT l.k, r.k FROM l JOIN r ON l.n < r.n LIMIT 10",
        "SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k OR l.n > r.n LIMIT 10",
        "SELECT l.k, r.k FROM l RIGHT JOIN r ON r.tag = 'x' OR r.tag = 'z' LIMIT 10",
    ] {
        let b = run_sorted(&core, "tenant-b", sql);
        assert!(
            b.is_empty(),
            "tenant-b must not observe tenant-a rows: {b:?}"
        );
        assert!(!run_sorted(&core, "tenant-a", sql).is_empty());
    }
    let sql = "SELECT l.k FROM l FULL JOIN r ON l.k = r.k OR l.n > r.n LIMIT 10";
    let a = err_of(&core, "tenant-a", sql);
    let b = err_of(&core, "tenant-b", sql);
    assert_eq!(a.0, "0A000");
    assert_eq!(a, b);
}
