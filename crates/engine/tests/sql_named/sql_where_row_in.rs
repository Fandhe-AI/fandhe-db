//! WHERE の行値の `(e1, e2, ...) [NOT] IN ((f1, f2, ...), ...)`（対象ビヘイビア: SQL-9。
//! 親 Issue #3298・本 Issue #3299）の結合テスト。
//!
//! 構文段が RowCompare の Or／連言へ脱糖した述語が、束縛段（`row_compare`）で既存の比較葉へ展開され
//! PostgreSQL と同じ三値論理で評価されること、形の誤りの fail-closed、DML の副作用範囲、
//! テナント境界を `EngineCore::execute_sql_in_session` 経由で固定する。
//! 拡張クエリの `$n`（Issue #3300）の評価・EXPLAIN の展開形一致もここで固定する。RLS 越境は
//! `table_rls` の `rls10_row_in_paths`。サブクエリ形・定義文脈は #3254 の対象外。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INTEGER, b INTEGER, s TEXT)";

/// alice: id1 (1,1)・id2 (3,5)・id3 (3,NULL)、bob: id4 (3,5)。
fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    for (tenant, id, a, b) in [
        ("alice", 1, "1", "1"),
        ("alice", 2, "3", "5"),
        ("alice", 3, "3", "NULL"),
        ("bob", 4, "3", "5"),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO t (id, embedding, a, b, s) VALUES \
                 ({id}, '[0.1,0.2]', {a}, {b}, 'x') USING OPERATION_ID 'op{id}'"
            ),
        );
    }
    (path, guard)
}

fn ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<u64> {
    let SqlOutcome::Query(r) = run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    else {
        panic!("{sql}: not a query");
    };
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

type Row = Vec<Option<i32>>;

/// 行値等価（成分ごとの `=` の Kleene AND）の参照実装。
fn eq_oracle(l: &[Option<i32>], r: &[Option<i32>]) -> Option<bool> {
    let mut unknown = false;
    for (x, y) in l.iter().zip(r) {
        match (x, y) {
            (Some(x), Some(y)) if x != y => return Some(false),
            (Some(_), Some(_)) => {}
            _ => unknown = true,
        }
    }
    if unknown {
        None
    } else {
        Some(true)
    }
}

/// 行値 `IN`（各要素との行値等価の Kleene OR）の参照実装。
fn in_oracle(l: &[Option<i32>], elems: &[Row]) -> Option<bool> {
    let mut unknown = false;
    for e in elems {
        match eq_oracle(l, e) {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

fn lit(v: Option<i32>) -> String {
    v.map_or("NULL".to_string(), |x| x.to_string())
}

const CREATE_U: &str =
    "CREATE TABLE u (embedding VECTOR(2) NOT NULL, a INTEGER, b INTEGER, c INTEGER, d INTEGER)";

fn insert_u(core: &EngineCore, id: usize, r: &[Option<i32>; 4]) {
    ok(
        core,
        "alice",
        &format!(
            "INSERT INTO u (id, embedding, a, b, c, d) VALUES ({id}, '[0.1,0.2]', {}, {}, {}, {}) \
             USING OPERATION_ID 'op{id}'",
            lit(r[0]),
            lit(r[1]),
            lit(r[2]),
            lit(r[3])
        ),
    );
}

/// `setup` の値から手で導いた id 集合。
#[test]
fn row_in_forms_are_evaluated() {
    let (path, _g) = setup("row-in-eval");
    let core = core_at(&path);
    for (w, want) in [
        ("(a, b) IN ((1, 1), (3, 5))", vec![1, 2]),
        ("(a, b) IN ((3, 5))", vec![2]),
        ("(a, b) NOT IN ((3, 5))", vec![1]),
        ("(a, b) NOT IN ((1, 1), (3, 5))", vec![]),
        ("NOT (a, b) IN ((1, 1), (3, 5))", vec![]),
        ("NOT (a, b) IN ((3, 5))", vec![1]),
        ("((a, b) IN ((1, 1), (3, 5)))", vec![1, 2]),
        ("NOT ((a, b) NOT IN ((3, 5)))", vec![2]),
        ("a = 1 OR (a, b) IN ((3, 5))", vec![1, 2]),
        ("a = 3 AND (a, b) NOT IN ((1, 1))", vec![2, 3]),
        ("(a + 1, lower(s)) IN ((4, 'x'))", vec![2, 3]),
        ("(s, a) IN (('x', 1))", vec![1]),
        // (3,NULL) は (3,9) と UNKNOWN、(1,1) と FALSE なので全体 UNKNOWN
        ("(a, b) NOT IN ((3, 9), (1, 1))", vec![2]),
        // (3,NULL) vs (4,9) は FALSE なので NOT IN で行 3 も出る
        ("(a, b) NOT IN ((4, 9))", vec![1, 2, 3]),
    ] {
        let sql = format!("SELECT id FROM t WHERE {w} LIMIT 10");
        assert_eq!(ids(&core, "alice", &sql), want, "{w}");
        assert_eq!(ids(&core, "alice", &format!("{sql};")), want, "{w};");
    }
    ok(
        &core,
        "alice",
        "EXPLAIN SELECT id FROM t WHERE (a, b) IN ((3, 5)) LIMIT 10",
    );
}

/// 真理値表: `{NULL,1,2}` の全組み合わせで、`IN`／`NOT IN`／前置 `NOT` の結果集合が参照実装と一致する
/// （どちらの集合にも出ない行が UNKNOWN）。
#[test]
fn row_in_truth_table_matches_postgres() {
    let path = unique_db_path("row-in-truth");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE_U);
    let vals = [None, Some(1), Some(2)];
    let mut rows: Vec<[Option<i32>; 4]> = Vec::new();
    for &a in &vals {
        for &b in &vals {
            for &c in &vals {
                for &d in &vals {
                    rows.push([a, b, c, d]);
                }
            }
        }
    }
    for (i, r) in rows.iter().enumerate() {
        insert_u(&core, i + 1, r);
    }
    let check = |label: &str,
                 lhs_sql: &str,
                 elems_sql: &str,
                 lhs: &dyn Fn(&[Option<i32>; 4]) -> Row,
                 elems: &dyn Fn(&[Option<i32>; 4]) -> Vec<Row>| {
        let mut want_true = Vec::new();
        let mut want_false = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            match in_oracle(&lhs(r), &elems(r)) {
                Some(true) => want_true.push(i as u64 + 1),
                Some(false) => want_false.push(i as u64 + 1),
                None => {}
            }
        }
        let q = |w: String| {
            ids(
                &core,
                "alice",
                &format!("SELECT id FROM u WHERE {w} LIMIT 200"),
            )
        };
        assert_eq!(
            q(format!("{lhs_sql} IN ({elems_sql})")),
            want_true,
            "{label} IN"
        );
        assert_eq!(
            q(format!("NOT {lhs_sql} IN ({elems_sql})")),
            want_false,
            "{label} NOT IN (prefix)"
        );
        assert_eq!(
            q(format!("{lhs_sql} NOT IN ({elems_sql})")),
            want_false,
            "{label} NOT IN"
        );
        assert_eq!(
            q(format!("NOT {lhs_sql} NOT IN ({elems_sql})")),
            want_true,
            "{label} NOT NOT IN"
        );
    };
    check(
        "cols",
        "(a, b)",
        "(c, d), (1, 2)",
        &|r| vec![r[0], r[1]],
        &|r| vec![vec![r[2], r[3]], vec![Some(1), Some(2)]],
    );
    check(
        "consts",
        "(a, b)",
        "(1, 2), (2, 1)",
        &|r| vec![r[0], r[1]],
        &|_| vec![vec![Some(1), Some(2)], vec![Some(2), Some(1)]],
    );
    check(
        "three",
        "(a, b, c)",
        "(1, 2, 1), (d, 1, 2)",
        &|r| vec![r[0], r[1], r[2]],
        &|r| {
            vec![
                vec![Some(1), Some(2), Some(1)],
                vec![r[3], Some(1), Some(2)],
            ]
        },
    );
}

/// PostgreSQL で結果が分かれる判別ケース（NULL は列の値で表現）。
#[test]
fn row_in_postgres_discriminating_cases() {
    let path = unique_db_path("row-in-disc");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE_U);
    // id1: (a,b)=(1,2)・c=NULL / id2: (a,b)=(1,NULL)・c=NULL
    insert_u(&core, 1, &[Some(1), Some(2), None, None]);
    insert_u(&core, 2, &[Some(1), None, None, None]);
    let q = |w: &str| {
        ids(
            &core,
            "alice",
            &format!("SELECT id FROM u WHERE {w} LIMIT 10"),
        )
    };
    // (1,2) NOT IN ((1,NULL)) は UNKNOWN
    assert!(!q("(a, b) NOT IN ((1, c))").contains(&1));
    // (1,2) NOT IN ((3,NULL)) は TRUE
    assert!(q("(a, b) NOT IN ((3, c))").contains(&1));
    // (1,NULL) IN ((1,2)) は UNKNOWN
    assert!(!q("(a, b) IN ((1, 2))").contains(&2) && !q("(a, b) NOT IN ((1, 2))").contains(&2));
    // (1,NULL) IN ((2,2)) は FALSE
    assert!(q("(a, b) NOT IN ((2, 2))").contains(&2));
    // (1,NULL) IN ((2,2),(1,NULL)) は UNKNOWN
    assert!(!q("(a, b) IN ((2, 2), (1, c))").contains(&2));
    assert!(!q("(a, b) NOT IN ((2, 2), (1, c))").contains(&2));
}

#[test]
fn row_in_dml_affects_only_matching_rows() {
    let (path, _g) = setup("row-in-dml");
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "UPDATE t SET s = 'y' WHERE (a, b) IN ((1, 1), (3, 5)) USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t WHERE s = 'y' LIMIT 10"),
        vec![1, 2]
    );
    assert_eq!(
        ids(&core, "bob", "SELECT id FROM t WHERE s = 'y' LIMIT 10"),
        Vec::<u64>::new()
    );
    ok(
        &core,
        "alice",
        "DELETE FROM t WHERE (a, b) NOT IN ((3, 5)) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t WHERE a >= 0 LIMIT 10"),
        vec![2, 3]
    );
    assert_eq!(
        ids(&core, "bob", "SELECT id FROM t WHERE a >= 0 LIMIT 10"),
        vec![4]
    );
}

#[test]
fn row_in_does_not_cross_tenants() {
    let (path, _g) = setup("row-in-tenant");
    let core = core_at(&path);
    for (w, alice, bob) in [
        ("(a, b) IN ((3, 5))", vec![2], vec![4]),
        ("(a, b) NOT IN ((1, 1))", vec![2, 3], vec![4]),
    ] {
        let sql = format!("SELECT id FROM t WHERE {w} LIMIT 10");
        assert_eq!(ids(&core, "alice", &sql), alice, "{w}");
        assert_eq!(ids(&core, "bob", &sql), bob, "{w}");
    }
}

#[test]
fn row_in_malformed_forms_fail_closed() {
    let (path, _g) = setup("row-in-bad");
    let core = core_at(&path);
    for w in [
        "(a, b) IN ()",
        "(a, b) IN ((1, 2, 3))",
        "(a, b) IN ((1))",
        "(a, b) IN (1, 2)",
        "(a, b) IN (ROW(1, 2))",
        "(a, b) IN (((1, 2)), (3, 4))",
        "(a, b) IN ((1, 2) (3, 4))",
        "(a, b) IN ((1, 2)",
        "(a, b) NOT IN ((1, 2, 3))",
    ] {
        let sql = format!("SELECT id FROM t WHERE {w} LIMIT 10");
        assert_eq!(code(&core, &sql), "42601", "{w}");
    }
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE (a, b) IN ((SELECT 1, 2)) LIMIT 10"
        ),
        "0A000"
    );
    // NULL リテラルの成分は拒否のまま（fail-closed。述語を落として全行を返さない）。
    for w in ["(a, b) IN ((1, NULL))", "(a, b) NOT IN ((1, NULL))"] {
        let sql = format!("SELECT id FROM t WHERE {w} LIMIT 10");
        assert!(run(&core, "alice", &sql).is_err(), "{w}");
    }
    let row = |n: usize| (0..n).map(|_| "a").collect::<Vec<_>>().join(", ");
    let at = format!("SELECT id FROM t WHERE ({0}) IN (({0})) LIMIT 10", row(64));
    assert_eq!(ids(&core, "alice", &at), vec![1, 2, 3]);
    let over = format!("SELECT id FROM t WHERE ({0}) IN (({0})) LIMIT 10", row(65));
    assert_eq!(code(&core, &over), "54000");
    let many = vec!["(1, 2)"; 257].join(", ");
    assert_eq!(
        code(
            &core,
            &format!("SELECT id FROM t WHERE (a, b) IN ({many}) LIMIT 10")
        ),
        "54000"
    );
}

/// 別の束縛経路でも述語が落ちて全行を返す fail-open にならない（評価されるか 0A000／42601 で拒否）。
#[test]
fn row_in_in_other_paths_never_drops_the_predicate() {
    let (path, _g) = setup("row-in-paths");
    let core = core_at(&path);
    for sql in [
        "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE (a, b) IN ((9, 9))) LIMIT 10",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t WHERE (a, b) IN ((9, 9))) LIMIT 10",
    ] {
        match run(&core, "alice", sql) {
            Ok(SqlOutcome::Query(r)) => assert!(r.rows.is_empty(), "{sql}: predicate dropped"),
            Ok(_) => panic!("{sql}: not a query"),
            Err(e) => assert!(["0A000", "42601"].contains(&e.wire_code()), "{sql}: {e:?}"),
        }
    }
}

/// 定義文脈（POLICY・VIEW・部分索引・CHECK）では拒否のまま（受け皿は #3254）。
/// 行値の IN の要素の成分の `$n` は #3300 で受理し、リテラル形と同じ行集合になる。
#[test]
fn row_in_is_rejected_in_definition_contexts_and_accepts_placeholders() {
    let (path, _g) = setup("row-in-defs");
    let core = core_at(&path);
    for sql in [
        "CREATE POLICY p ON t FOR SELECT USING ((a, b) IN ((3, 5)))",
        "CREATE VIEW v AS SELECT id FROM t WHERE (a, b) IN ((3, 5))",
        "CREATE INDEX i ON t (a) WHERE (a, b) IN ((3, 5))",
        "ALTER TABLE t ADD CONSTRAINT c CHECK ((a, b) IN ((3, 5)))",
    ] {
        let c = code(&core, sql);
        assert!(["0A000", "42601"].contains(&c), "{sql}: {c}");
    }
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE s = 'x' LIMIT 10")
        .expect("select after rejected definitions");
    assert_eq!(r.rows.len(), 3);
    let prepared = core
        .parse_sql_prepared("SELECT id FROM t WHERE (a, b) IN (($1, $2), ($3, $4)) LIMIT 10")
        .expect("placeholders in row IN elements are accepted");
    core.describe_prepared_in_session(&SessionState::default(), &prepared)
        .expect("describe");
    let v = |s: &str| Some(s.as_bytes().to_vec());
    let bound = core
        .bind_prepared(&prepared, &[v("1"), v("1"), v("3"), v("5")])
        .expect("bind");
    let mut s = SessionState::default();
    let got = core
        .execute_parsed_in_session(&ctx("alice"), &mut s, &bound)
        .expect("execute");
    let SqlOutcome::Query(r) = got else {
        panic!("not a query")
    };
    let mut got_ids: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    got_ids.sort_unstable();
    assert_eq!(
        got_ids,
        ids(
            &core,
            "alice",
            "SELECT id FROM t WHERE (a, b) IN ((1, 1), (3, 5)) LIMIT 10"
        )
    );
    assert_eq!(got_ids, vec![1, 2]);
}

/// EXPLAIN は脱糖後の束縛結果から導かれる閉じた語彙の出力で、等価な展開形を手で書いた文の EXPLAIN と
/// 完全に一致する（SQL-75・SQL-27 のポインタ。出力は値を含まない。DML は `filter: present`）。
#[test]
fn explain_of_row_in_equals_explain_of_the_expanded_form() {
    let (path, _g) = setup("row-in-explain");
    let core = core_at(&path);
    let pairs = [
        (
            "(a, b) IN ((1, 1), (3, 5))",
            "(a = 1 AND b = 1) OR (a = 3 AND b = 5)",
        ),
        (
            "(a, b) NOT IN ((1, 1), (3, 5))",
            "(a <> 1 OR b <> 1) AND (a <> 3 OR b <> 5)",
        ),
        ("(a, s) IN ((3, 'x'))", "a = 3 AND s = 'x'"),
    ];
    let prefixes = [
        "EXPLAIN SELECT id FROM t WHERE ",
        "EXPLAIN (FORMAT JSON) SELECT id FROM t WHERE ",
        "EXPLAIN VERBOSE SELECT id FROM t WHERE ",
        "EXPLAIN DELETE FROM t WHERE ",
        "EXPLAIN UPDATE t SET s = 'z' WHERE ",
    ];
    for (row, expanded) in pairs {
        for prefix in prefixes {
            let tail = if prefix.contains("SELECT") {
                " LIMIT 10"
            } else {
                ""
            };
            let a = run(&core, "alice", &format!("{prefix}{row}{tail}"));
            let b = run(&core, "alice", &format!("{prefix}{expanded}{tail}"));
            let (a, b) = (
                a.unwrap_or_else(|e| panic!("{prefix}{row}: {e:?}")),
                b.unwrap_or_else(|e| panic!("{prefix}{expanded}: {e:?}")),
            );
            assert_eq!(a, b, "{prefix}{row} vs {expanded}");
            let text = format!("{a:?}");
            assert!(!text.contains("'x'"), "{text}");
            let bob = run(&core, "bob", &format!("{prefix}{row}{tail}")).expect("bob explain");
            assert_eq!(a, bob, "{prefix}{row}: tenant-independent");
        }
    }
    run(
        &core,
        "alice",
        "EXPLAIN ANALYZE SELECT id FROM t WHERE (a, b) IN ((3, 5)) LIMIT 10",
    )
    .expect("explain analyze");
}

/// `$n` を含む行値の IN の EXPLAIN 前置は Parse で `42601`（fail-closed。パターン 12 と同じ扱い）。
/// intentional: docs/design/wire-extended-query-param-binding.md
#[test]
fn explain_with_row_in_placeholders_is_rejected_at_parse() {
    let (path, _g) = setup("row-in-explain-param");
    let core = core_at(&path);
    let err = core
        .parse_sql_prepared("EXPLAIN SELECT id FROM t WHERE (a, b) IN (($1, $2)) LIMIT 10")
        .expect_err("EXPLAIN with row IN placeholders is rejected");
    assert_eq!(err.wire_code(), "42601");
}
