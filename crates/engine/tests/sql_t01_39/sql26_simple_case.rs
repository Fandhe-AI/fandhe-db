//! 単純 CASE（`CASE x WHEN v THEN ... END`）の結合テスト（Issue #3279。対象ビヘイビア:
//! `docs/spec/04-behavior/sql-surface.md` SQL-26・SQL-42。ポインタ: `docs/spec/05-tasks.md`
//! TASK-210）。設計記録は `docs/design/case-coalesce-nullif.md` の「単純 CASE」節。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`）で、
//! 次を同値の検索形（`CASE WHEN x = v ...`）と Rust 側の独立オラクルの両方に対して固定する。
//! - 投影・`WHERE`・`ORDER BY`・`GROUP BY`・集計の中での結果、NULL の扱い（`NULL` は一致しない）。
//! - 被演算子が副作用を持つ関数でも 1 回だけ評価されること（条件付き位置の呼び出しは従来どおり拒否）。
//! - CHECK 制約・ビュー・`UPDATE ... SET` での保存→再パース→評価の一致。
//! - 拒否形の `wire_code`（fail-closed）とテナント境界（不可視行・他テナントの表を評価・参照しない）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

fn run_as(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    run_as(core, s, "alice", sql)
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
    core.execute_sql_in_session(&ctx("alice"), s, sql)
        .expect_err(sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    err(core, s, sql).wire_code()
}

fn rows_as(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match run_as(core, s, tenant, sql) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    rows_as(core, s, "alice", sql)
}

fn one(core: &EngineCore, s: &mut SessionState, sql: &str) -> Cell {
    let r = rows(core, s, sql);
    assert_eq!(r.len(), 1, "{sql}");
    r.into_iter()
        .next()
        .and_then(|c| c.into_iter().next())
        .expect("cell")
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn first_column(rows: Vec<Vec<Cell>>) -> Vec<Cell> {
    rows.into_iter()
        .map(|r| r.into_iter().next().expect("cell"))
        .collect()
}

fn count(core: &EngineCore, s: &mut SessionState, table: &str) -> i64 {
    match one(core, s, &format!("SELECT count(*) FROM {table}")) {
        Cell::SignedInteger(n) => n,
        Cell::Integer(n) => i64::try_from(n).expect("count"),
        other => panic!("count: {other:?}"),
    }
}

/// オラクル用の真値。`a` は NULL を含む。
const DATA: [(i32, Option<i32>); 6] = [
    (1, Some(1)),
    (2, Some(2)),
    (3, Some(3)),
    (4, None),
    (5, Some(2)),
    (6, Some(7)),
];

/// 独立オラクル: `CASE a WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END`。
/// `a` が NULL の行はどの WHEN とも一致せず ELSE になる。
fn oracle_label(a: Option<i32>) -> &'static str {
    match a {
        Some(1) => "one",
        Some(2) => "two",
        _ => "many",
    }
}

fn setup(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = session();
    run(&core, &mut s, "CREATE TABLE t (id INTEGER, a INTEGER)");
    run(&core, &mut s, "CREATE TABLE log (n INTEGER)");
    for (id, a) in DATA {
        let a = a.map_or("NULL".to_string(), |v| v.to_string());
        run(&core, &mut s, &format!("INSERT INTO t VALUES ({id}, {a})"));
    }
    (core, s, guard)
}

const SIMPLE: &str = "CASE a WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END";
const SEARCHED: &str = "CASE WHEN a = 1 THEN 'one' WHEN a = 2 THEN 'two' ELSE 'many' END";

#[test]
fn projection_matches_searched_form_and_oracle() {
    let (core, mut s, _g) = setup("sql26_simple_case_proj");
    let simple = rows(
        &core,
        &mut s,
        &format!("SELECT id, {SIMPLE} FROM t ORDER BY id"),
    );
    let searched = rows(
        &core,
        &mut s,
        &format!("SELECT id, {SEARCHED} FROM t ORDER BY id"),
    );
    assert_eq!(simple, searched);
    let expected: Vec<Cell> = DATA.iter().map(|(_, a)| text(oracle_label(*a))).collect();
    let got: Vec<Cell> = simple.into_iter().map(|mut r| r.remove(1)).collect();
    assert_eq!(got, expected);
}

#[test]
fn without_else_unmatched_and_null_operand_rows_are_null() {
    let (core, mut s, _g) = setup("sql26_simple_case_noelse");
    let got = first_column(rows(
        &core,
        &mut s,
        "SELECT CASE a WHEN 1 THEN 'one' END FROM t ORDER BY id",
    ));
    let expected: Vec<Cell> = DATA
        .iter()
        .map(|(_, a)| {
            if *a == Some(1) {
                text("one")
            } else {
                Cell::Null
            }
        })
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn where_and_order_by_use_the_simple_form() {
    let (core, mut s, _g) = setup("sql26_simple_case_where");
    let ids: Vec<Cell> = first_column(rows(
        &core,
        &mut s,
        "SELECT id FROM t WHERE CASE a WHEN 2 THEN 1 ELSE 0 END = 1 ORDER BY id",
    ));
    let expected: Vec<Cell> = DATA
        .iter()
        .filter(|(_, a)| *a == Some(2))
        .map(|(id, _)| Cell::SignedInteger(i64::from(*id)))
        .collect();
    assert_eq!(ids, expected);

    // ORDER BY: 'one' → 0、'two' → 1、それ以外 → 2。同点は id 昇順。
    let ordered = first_column(rows(
        &core,
        &mut s,
        "SELECT id FROM t ORDER BY CASE a WHEN 1 THEN 0 WHEN 2 THEN 1 ELSE 2 END, id",
    ));
    let mut oracle: Vec<(i32, i32)> = DATA
        .iter()
        .map(|(id, a)| {
            let rank = match a {
                Some(1) => 0,
                Some(2) => 1,
                _ => 2,
            };
            (rank, *id)
        })
        .collect();
    oracle.sort();
    let expected: Vec<Cell> = oracle
        .into_iter()
        .map(|(_, id)| Cell::SignedInteger(i64::from(id)))
        .collect();
    assert_eq!(ordered, expected);
}

#[test]
fn group_by_and_aggregates_use_the_simple_form() {
    let (core, mut s, _g) = setup("sql26_simple_case_group");
    // 投影と GROUP BY キーに同じ単純 CASE を書く。
    let groups = rows(
        &core,
        &mut s,
        &format!("SELECT {SIMPLE} AS l, count(*) FROM t GROUP BY {SIMPLE} ORDER BY l"),
    );
    let mut oracle: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
    for (_, a) in DATA {
        *oracle.entry(oracle_label(a)).or_default() += 1;
    }
    let expected: Vec<(Cell, i64)> = oracle.into_iter().map(|(k, n)| (text(k), n)).collect();
    let got: Vec<(Cell, i64)> = groups
        .into_iter()
        .map(|mut r| {
            let n = match r.remove(1) {
                Cell::SignedInteger(n) => n,
                Cell::Integer(n) => i64::try_from(n).expect("count"),
                other => panic!("count cell: {other:?}"),
            };
            (r.remove(0), n)
        })
        .collect();
    assert_eq!(got, expected);

    // 集計の内側: count(CASE a WHEN 2 THEN 1 END) は a = 2 の行数。
    let n = one(
        &core,
        &mut s,
        "SELECT count(CASE a WHEN 2 THEN 1 END) FROM t",
    );
    let expected = DATA.iter().filter(|(_, a)| *a == Some(2)).count() as i64;
    assert!(
        n == Cell::SignedInteger(expected) || n == Cell::Integer(expected as u64),
        "{n:?}"
    );

    // 集計を被演算子にする形。
    let label = one(
        &core,
        &mut s,
        "SELECT CASE count(*) WHEN 6 THEN 'six' ELSE 'other' END FROM t",
    );
    assert_eq!(label, text("six"));
}

#[test]
fn null_never_matches() {
    let (core, mut s, _g) = setup("sql26_simple_case_null");
    // `CASE NULL WHEN NULL` は一致しない（ELSE）。FROM の無い SELECT・投影の両方。
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT CASE NULL WHEN NULL THEN 'a' ELSE 'b' END"
        ),
        text("b")
    );
    let got = first_column(rows(
        &core,
        &mut s,
        "SELECT CASE NULL WHEN NULL THEN 'a' ELSE 'b' END FROM t ORDER BY id",
    ));
    assert_eq!(got, vec![text("b"); DATA.len()]);
    // 列が NULL の行も、WHEN 値が NULL の分岐も一致しない。
    let got = first_column(rows(
        &core,
        &mut s,
        "SELECT CASE a WHEN NULL THEN 'a' ELSE 'b' END FROM t ORDER BY id",
    ));
    assert_eq!(got, vec![text("b"); DATA.len()]);
    // 型付き NULL の被演算子。
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT CASE NULL::integer WHEN 1 THEN 'a' ELSE 'b' END"
        ),
        text("b")
    );
    // INSERT の定数式でも同じ。
    run(&core, &mut s, "CREATE TABLE c1 (v TEXT)");
    run(
        &core,
        &mut s,
        "INSERT INTO c1 VALUES (CASE NULL WHEN NULL THEN 'a' ELSE 'b' END)",
    );
    assert_eq!(one(&core, &mut s, "SELECT v FROM c1"), text("b"));
}

#[test]
fn constant_select_and_unknown_literal_coercion() {
    let (core, mut s, _g) = setup("sql26_simple_case_const");
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END"
        ),
        text("b")
    );
    // 末尾 `;` 付きの単独送信（psql 相当）。
    assert_eq!(
        one(&core, &mut s, "SELECT CASE 3 WHEN 1 THEN 'a' ELSE 'z' END;"),
        text("z")
    );
    // 文字列リテラルは相手の型（数値）として解釈できなければ 22P02（検索形と同じ）。
    let simple = code(
        &core,
        &mut s,
        "SELECT CASE a WHEN 'abc' THEN 1 ELSE 0 END FROM t",
    );
    let searched = code(
        &core,
        &mut s,
        "SELECT CASE WHEN a = 'abc' THEN 1 ELSE 0 END FROM t",
    );
    assert_eq!(simple, searched);
}

#[test]
fn operand_with_side_effect_is_evaluated_exactly_once() {
    let (core, mut s, _g) = setup("sql26_simple_case_once");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION bump() RETURNS int LANGUAGE sql VOLATILE AS \
         $$ INSERT INTO log VALUES (1) RETURNING n; SELECT 2 $$",
    );
    let before = count(&core, &mut s, "log");
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT CASE bump() WHEN 1 THEN 'a' WHEN 2 THEN 'b' WHEN 3 THEN 'c' END"
        ),
        text("b")
    );
    assert_eq!(count(&core, &mut s, "log"), before + 1);
    // WHEN 1 個・ELSE 付きでも 1 回。
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT CASE bump() WHEN 2 THEN 'x' ELSE 'y' END"
        ),
        text("x")
    );
    assert_eq!(count(&core, &mut s, "log"), before + 2);
    // 結果側・WHEN 値側（2 番目以降）の副作用呼び出しは条件付き位置のため従来どおり拒否され、
    // 副作用は残らない。
    for sql in [
        "SELECT CASE 1 WHEN 1 THEN bump() ELSE 0 END",
        "SELECT CASE 1 WHEN 5 THEN 1 WHEN bump() THEN 2 END",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
        assert_eq!(count(&core, &mut s, "log"), before + 2, "{sql}");
    }
}

#[test]
fn clock_timestamp_operand_is_rejected_when_it_could_be_reread() {
    let (core, mut s, _g) = setup("sql26_simple_case_clock");
    // WHEN が 2 個以上: 分岐ごとに読み直すと「1 回評価」と食い違うため拒否。
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT CASE clock_timestamp() WHEN '2000-01-01 00:00:00'::timestamp THEN 1 \
             WHEN '2001-01-01 00:00:00'::timestamp THEN 2 END"
        ),
        "0A000"
    );
    // WHEN が 1 個: 評価は 1 回なので受理。
    run(
        &core,
        &mut s,
        "SELECT CASE clock_timestamp() WHEN '2000-01-01 00:00:00'::timestamp THEN 1 ELSE 0 END",
    );
    // 取引固定の時刻関数は対象外（複数 WHEN でも受理）。
    run(
        &core,
        &mut s,
        "SELECT CASE now() WHEN '2000-01-01 00:00:00+00'::timestamptz THEN 1 \
         WHEN '2001-01-01 00:00:00+00'::timestamptz THEN 2 ELSE 0 END",
    );
}

#[test]
fn check_constraint_view_and_update_round_trip() {
    let (core, mut s, _g) = setup("sql26_simple_case_persist");
    // CHECK: 保存テキストが再パースされても評価が一致する。
    run(
        &core,
        &mut s,
        "CREATE TABLE chk (n INTEGER CHECK (CASE n WHEN 1 THEN 10 WHEN 2 THEN 20 ELSE 0 END > 0))",
    );
    run(&core, &mut s, "INSERT INTO chk VALUES (1)");
    run(&core, &mut s, "INSERT INTO chk VALUES (2)");
    assert_eq!(code(&core, &mut s, "INSERT INTO chk VALUES (5)"), "23514");
    assert_eq!(count(&core, &mut s, "chk"), 2);

    // ビュー本文（WHERE の式述語。保存→再パース→評価）。
    run(
        &core,
        &mut s,
        "CREATE VIEW v AS SELECT id FROM t WHERE CASE a WHEN 2 THEN 1 ELSE 0 END = 1",
    );
    let via_view = rows(&core, &mut s, "SELECT id FROM v ORDER BY id");
    let direct = rows(
        &core,
        &mut s,
        "SELECT id FROM t WHERE CASE a WHEN 2 THEN 1 ELSE 0 END = 1 ORDER BY id",
    );
    assert_eq!(via_view, direct);
    assert_eq!(via_view.len(), 2);

    // UPDATE ... SET。
    run(
        &core,
        &mut s,
        "UPDATE t SET a = CASE a WHEN 1 THEN 100 WHEN 2 THEN 200 ELSE a END WHERE id > 0",
    );
    let got = first_column(rows(&core, &mut s, "SELECT a FROM t ORDER BY id"));
    let expected: Vec<Cell> = DATA
        .iter()
        .map(|(_, a)| match a {
            Some(1) => Cell::SignedInteger(100),
            Some(2) => Cell::SignedInteger(200),
            Some(v) => Cell::SignedInteger(i64::from(*v)),
            None => Cell::Null,
        })
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn malformed_and_over_limit_forms_fail_closed() {
    let (core, mut s, _g) = setup("sql26_simple_case_reject");
    for sql in [
        "SELECT CASE a END FROM t",
        "SELECT CASE a WHEN 1 THEN 2 FROM t",
        "SELECT CASE a WHEN 1, 2 THEN 3 END FROM t",
        "SELECT CASE a WHEN 1 END FROM t",
        "SELECT CASE a > 1 WHEN true THEN 1 END FROM t",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42601", "{sql}");
    }
    // 分岐 65 個は上限超過。
    let mut sql = String::from("SELECT CASE a");
    for i in 0..65 {
        sql.push_str(&format!(" WHEN {i} THEN {i}"));
    }
    sql.push_str(" END FROM t");
    assert_eq!(code(&core, &mut s, &sql), "54000");
    // 存在しない列は被演算子・比較値のどちらでも検索形と同じ拒否コード。
    let searched = code(&core, &mut s, "SELECT CASE WHEN nope = 1 THEN 1 END FROM t");
    assert_eq!(
        code(&core, &mut s, "SELECT CASE nope WHEN 1 THEN 1 END FROM t"),
        searched
    );
    assert_eq!(
        code(&core, &mut s, "SELECT CASE a WHEN nope THEN 1 END FROM t"),
        searched
    );
    // NULL 分岐の裏の列も検査される（存在オラクルにならない）。
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT CASE nope WHEN NULL THEN 1 END FROM t"
        ),
        searched
    );
}

#[test]
fn large_operand_with_many_branches_hits_the_bind_node_budget() {
    let (core, mut s, _g) = setup("sql26_simple_case_budget");
    // 構文段は被演算子を 1 回しか数えないため通るが、分岐ごとに下げた条件（被演算子の複製）は
    // 束縛段のノード予算で有界になる（54000。メモリを際限なく使わない）。
    let operand = (0..24).map(|_| "a").collect::<Vec<_>>().join(" + ");
    let mut sql = format!("SELECT CASE {operand}");
    for i in 0..60 {
        sql.push_str(&format!(" WHEN {i} THEN {i}"));
    }
    sql.push_str(" END FROM t");
    assert_eq!(code(&core, &mut s, &sql), "54000");
}

#[test]
fn column_named_case_keeps_working() {
    let (core, mut s, _g) = setup("sql26_simple_case_colname");
    run(&core, &mut s, "CREATE TABLE k (\"case\" INTEGER)");
    run(&core, &mut s, "INSERT INTO k VALUES (3)");
    run(&core, &mut s, "INSERT INTO k VALUES (-1)");
    let got = first_column(rows(&core, &mut s, "SELECT case FROM k WHERE case > 0"));
    assert_eq!(got, vec![Cell::SignedInteger(3)]);
}

#[test]
fn rls_invisible_rows_are_not_evaluated_and_other_tenants_stay_hidden() {
    let path = unique_db_path("sql26_simple_case_rls");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = session();
    run_as(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE t (id INTEGER, a INTEGER)",
    );
    run_as(&core, &mut s, "alice", "INSERT INTO t VALUES (1, 1)");
    // 他テナントの表は被演算子だけに書いても見えない（存在を漏らさない）。
    let mut other = session();
    let seen = rows_as(
        &core,
        &mut other,
        "bob",
        "SELECT CASE a WHEN 1 THEN 'x' END FROM t",
    );
    assert!(seen.is_empty(), "bob must not see alice's rows: {seen:?}");
    // 0 除算を含む被演算子は、可視行が 0 件なら評価されずエラーにならない。
    let empty = rows_as(
        &core,
        &mut s,
        "alice",
        "SELECT CASE 1 / (a - 1) WHEN 1 THEN 'x' END FROM t WHERE id > 100",
    );
    assert!(empty.is_empty());
}
