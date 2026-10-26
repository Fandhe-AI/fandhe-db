//! `JOIN ... ON` の結合キー／残余述語への分解の結合テスト（Issue #1829。ポインタ: SQL-60）。
//!
//! 分類器そのものの単体テストは `sql::join::on_split`。ここでは production 経路
//! （`EngineCore`）で、(a) 等価キーのみの ON が従来どおり動くこと、(b) キー無しは `42601`、
//! (c) 残余述語（左のみ・右のみ・混在）付きは評価が #1830 の責務のため全結合種別で `0A000`
//! の fail-closed、(d) 未知列は分類より前に `42703`、(e) 拒否がテナントに依存しないこと
//! を確認する。

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

/// l(k,tag,n)・r(k,tag,n): 結合キー `k`・単側述語用の `tag`・混在述語用の `n`。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("on-split");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE l (k BIGINT, tag TEXT, n BIGINT)",
        "CREATE TABLE r (k BIGINT, tag TEXT, n BIGINT)",
        "INSERT INTO l (k, tag, n) VALUES (1, 'x', 1), (2, 'y', 5)",
        "INSERT INTO r (k, tag, n) VALUES (1, 'x', 2), (2, 'w', 1), (3, 'z', 9)",
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

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    let (code, detail) = err_of(core, "tenant-a", sql);
    assert_eq!(code, wire_code, "sql={sql:?} err={detail}");
}

#[test]
fn equi_key_only_on_is_unchanged() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let res = try_run(
        &core,
        "tenant-a",
        "SELECT l.k, r.n FROM l JOIN r ON l.k = r.k LIMIT 10",
    )
    .expect("equi join");
    let mut rows: Vec<Vec<String>> = res
        .rows
        .iter()
        .map(|row| row.cells.iter().map(render).collect())
        .collect();
    rows.sort();
    assert_eq!(rows, vec![vec!["1", "2"], vec!["2", "1"]]);
    // 複数キー。
    let res = try_run(
        &core,
        "tenant-a",
        "SELECT l.k FROM l JOIN r ON l.k = r.k AND l.tag = r.tag LIMIT 10",
    )
    .expect("multi-key equi join");
    assert_eq!(res.rows.len(), 1);
}

/// 結果を `(l.k, r.k)` の文字列行へ描画してソートする。
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

/// 置き換え注記: #1829 時点の暫定契約（残余付き ON は `0A000`）を、#1830 の仕様どおりの
/// 結果検証へ置き換えたもの（アサーションの弱体化ではない）。
/// 等価キーだけの一致ペアは (l1,r1)・(l2,r2)。残余ごとに (l1,r1) は真・(l2,r2) は偽。
#[test]
fn on_residual_applies_match_semantics_for_every_join_kind() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    // (残余, 種別名) ごとに INNER / LEFT / RIGHT / FULL の期待値は共通。
    for residual in ["l.tag = 'x'", "r.tag = 'x'", "l.n < r.n"] {
        let cases: [(&str, Vec<Vec<String>>); 4] = [
            ("JOIN", expect(&[&["1", "1"]])),
            ("LEFT JOIN", expect(&[&["1", "1"], &["2", "NULL"]])),
            (
                "RIGHT JOIN",
                expect(&[&["1", "1"], &["NULL", "2"], &["NULL", "3"]]),
            ),
            (
                "FULL JOIN",
                expect(&[&["1", "1"], &["2", "NULL"], &["NULL", "2"], &["NULL", "3"]]),
            ),
        ];
        for (kind, want) in cases {
            let sql =
                format!("SELECT l.k, r.k FROM l {kind} r ON l.k = r.k AND {residual} LIMIT 10");
            assert_eq!(run_sorted(&core, "tenant-a", &sql), want, "sql={sql}");
        }
    }
}

/// ON 残余は strict マスク簡約（外部結合 → 内部結合）へ流れない。WHERE に同じ述語を書くと
/// 簡約され保存側の行が消える。
#[test]
fn on_residual_is_not_a_where_filter() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let q = |s: &str| run_sorted(&core, "tenant-a", s);
    assert_eq!(
        q("SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k AND r.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"], &["2", "NULL"]])
    );
    assert_eq!(
        q("SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k WHERE r.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"]])
    );
    assert_eq!(
        q("SELECT l.k, r.k FROM l RIGHT JOIN r ON l.k = r.k AND l.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"], &["NULL", "2"], &["NULL", "3"]])
    );
    assert_eq!(
        q("SELECT l.k, r.k FROM l RIGHT JOIN r ON l.k = r.k WHERE l.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"]])
    );
    assert_eq!(
        q("SELECT l.k, r.k FROM l FULL JOIN r ON l.k = r.k AND l.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"], &["2", "NULL"], &["NULL", "2"], &["NULL", "3"]])
    );
    assert_eq!(
        q("SELECT l.k, r.k FROM l FULL JOIN r ON l.k = r.k WHERE l.tag = 'x' LIMIT 10"),
        expect(&[&["1", "1"]])
    );
    // INNER では ON と WHERE が同値。
    for residual in ["l.tag = 'x'", "r.tag = 'x'", "l.n < r.n"] {
        let on = q(&format!(
            "SELECT l.k, r.k FROM l JOIN r ON l.k = r.k AND {residual} LIMIT 10"
        ));
        let wh = q(&format!(
            "SELECT l.k, r.k FROM l JOIN r ON l.k = r.k WHERE {residual} LIMIT 10"
        ));
        assert_eq!(on, wh, "residual={residual}");
    }
    // ON 残余と WHERE の併用: WHERE は保存側だけを絞り、ON 由来の NULL 補完は残る。
    assert_eq!(
        q("SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k AND r.tag = 'x' WHERE l.tag = 'y' LIMIT 10"),
        expect(&[&["2", "NULL"]])
    );
}

#[test]
fn on_residual_with_null_cell_and_multiple_residuals() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    exec(&core, "INSERT INTO l (k, tag) VALUES (3, 'x')");
    // l3 は n が NULL。`l.n < r.n` は偽（不明）として一致せず、保存側なら NULL 補完される。
    assert_eq!(
        run_sorted(
            &core,
            "tenant-a",
            "SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k AND l.n < r.n LIMIT 10"
        ),
        expect(&[&["1", "1"], &["2", "NULL"], &["3", "NULL"]])
    );
    // 左のみ・右のみ・mixed を同時に持つ ON。
    assert_eq!(
        run_sorted(
            &core,
            "tenant-a",
            "SELECT l.k, r.k FROM l FULL JOIN r \
             ON l.k = r.k AND l.tag = 'x' AND r.tag = 'x' AND l.n < r.n LIMIT 10"
        ),
        expect(&[
            &["1", "1"],
            &["2", "NULL"],
            &["3", "NULL"],
            &["NULL", "2"],
            &["NULL", "3"]
        ])
    );
}

#[test]
fn on_residual_in_three_way_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    exec(&core, "CREATE TABLE s (k BIGINT, tag TEXT)");
    exec(&core, "INSERT INTO s (k, tag) VALUES (1, 'a'), (2, 'b')");
    // 段 2 の左のみ述語が relation 0 を参照する。
    assert_eq!(
        run_sorted(
            &core,
            "tenant-a",
            "SELECT l.k, s.k FROM l JOIN r ON l.k = r.k JOIN s ON r.k = s.k AND l.tag = 'x' LIMIT 10"
        ),
        expect(&[&["1", "1"]])
    );
    // 前段で NULL 補完された relation（r）を参照する左のみ述語は偽になり、s 側も NULL 補完される。
    assert_eq!(
        run_sorted(
            &core,
            "tenant-a",
            "SELECT l.k, r.k, s.k FROM l LEFT JOIN r ON l.k = r.k AND r.tag = 'x' \
             LEFT JOIN s ON l.k = s.k AND r.tag = 'x' LIMIT 10"
        ),
        expect(&[&["1", "1", "1"], &["2", "NULL", "NULL"]])
    );
}

#[test]
fn on_residual_type_mismatch_is_rejected() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.k = r.k AND l.n > r.tag LIMIT 10",
        "42804",
    );
}

#[test]
fn on_without_equi_key_is_rejected() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.n > r.n LIMIT 10",
        "42601",
    );
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.k = l.n LIMIT 10",
        "42601",
    );
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON r.tag = 'x' LIMIT 10",
        "42601",
    );
}

#[test]
fn unknown_column_in_on_is_resolved_before_classification() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.k = r.k AND r.nonexistent = 'x' LIMIT 10",
        "42703",
    );
}

/// ON 残余の評価は可視行だけに閉じる: 他テナントの視点では tenant-a の行が結果にも
/// NULL 補完行にも現れず、同じクエリの結果が一致する。
#[test]
fn on_residual_does_not_leak_across_tenants() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for sql in [
        "SELECT l.k, r.k FROM l FULL JOIN r ON l.k = r.k AND r.tag = 'x' LIMIT 10",
        "SELECT l.k, r.k FROM l LEFT JOIN r ON l.k = r.k AND l.tag = 'x' LIMIT 10",
    ] {
        let b = run_sorted(&core, "tenant-b", sql);
        assert!(
            b.is_empty(),
            "tenant-b must not observe tenant-a rows: {b:?}"
        );
        assert!(!run_sorted(&core, "tenant-a", sql).is_empty());
    }
}
