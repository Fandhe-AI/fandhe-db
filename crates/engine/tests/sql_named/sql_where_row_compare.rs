//! WHERE の行値比較 `(e1, e2, ...) <op> (f1, f2, ...)` の構文段（対象ビヘイビア: SQL-9。
//! 親 Issue #3254・本 Issue #3255）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛）を production 経路として、
//! 構文段が受理した形が辞書式・三値論理（Issue #3256）で評価されること、成分数不一致・
//! 構文違反が `42601` のままであること、DML の副作用範囲、テナント境界を固定する。

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

/// 失敗を前提にした SQL の `(wire_code, メッセージ)`。成功してしまった場合は SQL を添えて落とす。
fn fail(core: &EngineCore, tenant: &str, sql: &str) -> (&'static str, String) {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code(), e.to_string()),
    }
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    fail(core, "alice", sql).0
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

/// 構文段が受理する形が評価され、期待する id 集合になる（fixture は `setup` の値から手で導出）。
#[test]
fn row_compare_forms_are_evaluated() {
    let (path, _g) = setup("row-cmp-eval");
    let core = core_at(&path);
    for (sql, want) in [
        ("SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10", vec![1]),
        ("SELECT id FROM t WHERE (a, b) = (3, 5) LIMIT 10", vec![2]),
        ("SELECT id FROM t WHERE (a, b) <> (3, 5) LIMIT 10", vec![1]),
        ("SELECT id FROM t WHERE (a, b) != (3, 5) LIMIT 10", vec![1]),
        (
            "SELECT id FROM t WHERE (a, b) <= (3, 5) LIMIT 10",
            vec![1, 2],
        ),
        ("SELECT id FROM t WHERE (a, b) > (3, 5) LIMIT 10", vec![]),
        ("SELECT id FROM t WHERE (a, b) >= (3, 5) LIMIT 10", vec![2]),
        (
            "SELECT id FROM t WHERE (a + 1, lower(s)) = (4, 'x') LIMIT 10",
            vec![2, 3],
        ),
        ("SELECT id FROM t WHERE ((a, b) < (3, 5)) LIMIT 10", vec![1]),
        (
            "SELECT id FROM t WHERE NOT (a, b) < (3, 5) LIMIT 10",
            vec![2],
        ),
        (
            "SELECT id FROM t WHERE a = 1 AND (a, b) < (3, 5) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM t WHERE a = 3 OR (a, b) < (3, 5) LIMIT 10",
            vec![1, 2, 3],
        ),
        ("SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10;", vec![1]),
        (
            "SELECT id FROM t WHERE (a, b) < (3, 6) LIMIT 10",
            vec![1, 2],
        ),
        ("SELECT id FROM t WHERE (a, b) > (3, 4) LIMIT 10", vec![2]),
    ] {
        assert_eq!(ids(&core, "alice", sql), want, "{sql}");
    }
    let r = core
        .execute_sql(
            &ctx("alice"),
            "SELECT DISTINCT a FROM t WHERE (a, b) < (3, 6) LIMIT 10",
        )
        .expect("distinct");
    assert_eq!(r.rows.len(), 2);
    run(
        &core,
        "alice",
        "SELECT count(*) FROM t WHERE (a, b) < (3, 5)",
    )
    .expect("count");
    run(
        &core,
        "alice",
        "EXPLAIN SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10",
    )
    .expect("explain");
}

/// PostgreSQL の三値論理（辞書式・Kleene）の参照実装。`None` は UNKNOWN。
fn oracle(l: &[Option<i32>], op: &str, r: &[Option<i32>]) -> Option<bool> {
    match op {
        "=" => {
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
        "<>" => oracle(l, "=", r).map(|v| !v),
        _ => {
            for (x, y) in l.iter().zip(r) {
                match (x, y) {
                    (Some(x), Some(y)) if x == y => continue,
                    (Some(x), Some(y)) => {
                        return Some(match op {
                            "<" | "<=" => x < y,
                            _ => x > y,
                        })
                    }
                    _ => return None,
                }
            }
            Some(op == "<=" || op == ">=")
        }
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

/// 真理値表: `{NULL,1,2}` の全組み合わせについて、6 演算子の `op` と `NOT op` の結果集合が
/// 参照実装（PG の三値論理）と一致する（どちらにも出ない行が UNKNOWN）。
#[test]
fn row_compare_truth_table_matches_postgres() {
    let path = unique_db_path("row-cmp-truth");
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
    for op in ["<", "<=", ">", ">=", "=", "<>"] {
        let mut want_true = Vec::new();
        let mut want_false = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            match oracle(&r[0..2], op, &r[2..4]) {
                Some(true) => want_true.push(i as u64 + 1),
                Some(false) => want_false.push(i as u64 + 1),
                None => {}
            }
        }
        let t = ids(
            &core,
            "alice",
            &format!("SELECT id FROM u WHERE (a, b) {op} (c, d) LIMIT 200"),
        );
        assert_eq!(t, want_true, "(a,b) {op} (c,d)");
        let f = ids(
            &core,
            "alice",
            &format!("SELECT id FROM u WHERE NOT (a, b) {op} (c, d) LIMIT 200"),
        );
        assert_eq!(f, want_false, "NOT (a,b) {op} (c,d)");
        // 3 成分 × 非 NULL 定数（右辺の NULL リテラルは束縛で拒否されるため列で表現しない）
        let mut want3 = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let l = [r[0], r[1], r[2]];
            if oracle(&l, op, &[Some(1), Some(2), Some(1)]) == Some(true) {
                want3.push(i as u64 + 1);
            }
        }
        let t3 = ids(
            &core,
            "alice",
            &format!("SELECT id FROM u WHERE (a, b, c) {op} (1, 2, 1) LIMIT 200"),
        );
        assert_eq!(t3, want3, "(a,b,c) {op} (1,2,1)");
    }
}

/// PostgreSQL で結果が分かれる判別ケース（列で表現）。
#[test]
fn row_compare_postgres_discriminating_cases() {
    let path = unique_db_path("row-cmp-disc");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE_U);
    insert_u(&core, 1, &[Some(1), None, Some(2), Some(0)]);
    insert_u(&core, 2, &[None, Some(1), Some(2), Some(2)]);
    insert_u(&core, 3, &[Some(1), None, Some(1), None]);
    insert_u(&core, 4, &[Some(1), None, Some(1), Some(0)]);
    let q = |w: &str| {
        ids(
            &core,
            "alice",
            &format!("SELECT id FROM u WHERE {w} LIMIT 10"),
        )
    };
    // (1,NULL) < (2,0) は TRUE
    assert!(q("(a, b) < (c, d)").contains(&1));
    // (NULL,1) = (2,2) は FALSE
    assert_eq!(q("NOT (a, b) = (c, d)"), vec![1, 2]);
    // (1,NULL) = (1,NULL) は UNKNOWN
    assert!(!q("(a, b) = (c, d)").contains(&3) && !q("(a, b) <> (c, d)").contains(&3));
    // (1,NULL) <= (1,0) は UNKNOWN
    assert!(!q("(a, b) <= (c, d)").contains(&4) && !q("NOT (a, b) <= (c, d)").contains(&4));
}

/// 型照合は通常の比較と同じ経路（数値の暗黙キャスト・TEXT・NUMERIC・BOOLEAN・DATE）。
#[test]
fn row_compare_type_matching_follows_scalar_comparison() {
    let path = unique_db_path("row-cmp-types");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE y (embedding VECTOR(2) NOT NULL, i INTEGER, n NUMERIC(8,2), s TEXT, f BOOLEAN, d DATE)",
    );
    ok(&core, "alice", "INSERT INTO y (id, embedding, i, n, s, f, d) VALUES (1, '[0.1,0.2]', 1, 1.50, 'a', TRUE, '2024-01-01') USING OPERATION_ID 'o1'");
    ok(&core, "alice", "INSERT INTO y (id, embedding, i, n, s, f, d) VALUES (2, '[0.1,0.2]', 2, 2.50, 'b', FALSE, '2024-06-01') USING OPERATION_ID 'o2'");
    for (sql, want) in [
        ("SELECT id FROM y WHERE (i, s) < (2, 'a') LIMIT 10", vec![1]),
        (
            "SELECT id FROM y WHERE (i, s) < (1.5, 'a') LIMIT 10",
            vec![1],
        ),
        ("SELECT id FROM y WHERE (n, i) = (1.5, 1) LIMIT 10", vec![1]),
        (
            "SELECT id FROM y WHERE (n, i) >= (1.5, 1) LIMIT 10",
            vec![1, 2],
        ),
        ("SELECT id FROM y WHERE (s, i) > ('a', 1) LIMIT 10", vec![2]),
        (
            "SELECT id FROM y WHERE (d, i) < (DATE '2024-03-01', 5) LIMIT 10",
            vec![1],
        ),
    ] {
        assert_eq!(ids(&core, "alice", sql), want, "{sql}");
    }
    // 文字列リテラル形の DATE 成分もスカラの `d <op> '<lit>'` と同じ宣言的経路で評価する（両向き・<>）。
    for (sql, want) in [
        (
            "SELECT id FROM y WHERE (d, i) < ('2024-03-01', 5) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM y WHERE ('2024-03-01', i) > (d, 0) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM y WHERE (d, i) = ('2024-06-01', 2) LIMIT 10",
            vec![2],
        ),
        (
            "SELECT id FROM y WHERE (d, i) <> ('2024-06-01', 2) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM y WHERE (d, i) >= ('2024-01-01', 1) LIMIT 10",
            vec![1, 2],
        ),
    ] {
        assert_eq!(ids(&core, "alice", sql), want, "{sql}");
    }
    // 型の組が通常の比較で拒否されるなら、行値でも同じコードで拒否される。
    let scalar = code(&core, "SELECT id FROM y WHERE i = 'abc' LIMIT 10");
    assert_eq!(
        code(
            &core,
            "SELECT id FROM y WHERE (i, s) = ('abc', 'a') LIMIT 10"
        ),
        scalar
    );
}

/// BOOLEAN 成分は通常の `col = TRUE` と同じ宣言的経路で評価し、INTERVAL 成分は通常の比較と同じコードで拒否する。
#[test]
fn row_compare_boolean_and_interval_components() {
    let path = unique_db_path("row-cmp-bool-interval");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE y (embedding VECTOR(2) NOT NULL, i INTEGER, f BOOLEAN, v INTERVAL)",
    );
    ok(&core, "alice", "INSERT INTO y (id, embedding, i, f, v) VALUES (1, '[0.1,0.2]', 1, TRUE, INTERVAL '1 day') USING OPERATION_ID 'o1'");
    ok(&core, "alice", "INSERT INTO y (id, embedding, i, f, v) VALUES (2, '[0.1,0.2]', 1, FALSE, INTERVAL '2 day') USING OPERATION_ID 'o2'");
    for (sql, want) in [
        (
            "SELECT id FROM y WHERE (f, i) = (TRUE, 1) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM y WHERE (i, f) = (1, FALSE) LIMIT 10",
            vec![2],
        ),
        (
            "SELECT id FROM y WHERE (i, f) <> (1, FALSE) LIMIT 10",
            vec![1],
        ),
        (
            "SELECT id FROM y WHERE (i, f) <> (2, FALSE) LIMIT 10",
            vec![1, 2],
        ),
    ] {
        assert_eq!(ids(&core, "alice", sql), want, "{sql}");
    }
    let scalar = code(
        &core,
        "SELECT id FROM y WHERE v = INTERVAL '1 day' LIMIT 10",
    );
    for sql in [
        "SELECT id FROM y WHERE (v, i) = (INTERVAL '1 day', 1) LIMIT 10",
        "SELECT id FROM y WHERE (i, v) < (1, INTERVAL '1 day') LIMIT 10",
    ] {
        assert_eq!(code(&core, sql), scalar, "{sql}");
    }
    // 非等価比較の BOOLEAN 成分は fail-closed（成功にもパニックにもならない）。
    assert_eq!(
        code(&core, "SELECT id FROM y WHERE (f, i) < (TRUE, 1) LIMIT 10"),
        "22000"
    );
}

/// 非自明な成分を上限個並べても、複製ノード数は予算に計上され 54000 で一貫して拒否される。
#[test]
fn row_compare_expansion_charges_expression_budget() {
    let path = unique_db_path("row-cmp-budget");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE y (embedding VECTOR(2) NOT NULL, i INTEGER)",
    );
    let n = 64;
    let lhs = vec!["i + 1"; n].join(", ");
    let rhs = vec!["2"; n].join(", ");
    for op in ["<", "<=", ">", ">="] {
        let sql = format!("SELECT id FROM y WHERE ({lhs}) {op} ({rhs}) LIMIT 10");
        assert_eq!(code(&core, &sql), "54000", "{op}");
    }
}

/// 述語形 UPDATE／DELETE は対象行だけを変え、同じ `OPERATION_ID` の再送は冪等。他テナントの行は変わらない。
#[test]
fn row_compare_in_predicate_dml_affects_only_matching_rows() {
    let (path, _g) = setup("row-cmp-dml");
    let core = core_at(&path);
    let upd = "UPDATE t SET s = 'y' WHERE (a, b) < (3, 5) USING OPERATION_ID 'u1'";
    ok(&core, "alice", upd);
    // 同じ OPERATION_ID の再送は再適用されず拒否される（冪等）。
    assert!(matches!(
        run(&core, "alice", upd),
        Err(SqlSurfaceError::DuplicateOperationId)
    ));
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t WHERE s = 'y' LIMIT 10"),
        vec![1]
    );
    assert_eq!(
        ids(&core, "bob", "SELECT id FROM t WHERE s = 'x' LIMIT 10"),
        vec![4]
    );
    ok(
        &core,
        "alice",
        "DELETE FROM t WHERE (a, b) = (3, 5) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        ids(&core, "alice", "SELECT id FROM t WHERE a >= 0 LIMIT 10"),
        vec![1, 3]
    );
    assert_eq!(
        ids(&core, "bob", "SELECT id FROM t WHERE a >= 0 LIMIT 10"),
        vec![4]
    );
}

/// 成分数の不一致・形の誤りは `42601`、成分数の上限超過は `54000`。
#[test]
fn row_compare_malformed_forms_fail_closed() {
    let (path, _g) = setup("row-cmp-bad");
    let core = core_at(&path);
    for sql in [
        "SELECT id FROM t WHERE (a, b) < (1, 2, 3) LIMIT 10",
        "SELECT id FROM t WHERE (a, b, s) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < 3 LIMIT 10",
        "SELECT id FROM t WHERE a < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < ROW(1, 2) LIMIT 10",
        "SELECT id FROM t WHERE ROW(a, b) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE ((a, b), s) < ((1, 2), 3) LIMIT 10",
        "SELECT id FROM t WHERE (a, ) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) IS NULL LIMIT 10",
        "SELECT id FROM t WHERE (a, b) BETWEEN (1, 2) AND (3, 4) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) = ANY (ARRAY[1]) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < (1, 2) + 1 LIMIT 10",
        "SELECT (a, b) FROM t LIMIT 10",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE (a, b) = (SELECT 1, 2) LIMIT 10"
        ),
        "0A000"
    );
    // 上限ちょうど（64 成分）は評価され、+1 は 54000。
    let row = |n: usize| (0..n).map(|_| "a").collect::<Vec<_>>().join(", ");
    for op in ["=", "<"] {
        let at_limit = format!("SELECT id FROM t WHERE ({0}) {op} ({0}) LIMIT 10", row(64));
        let want: Vec<u64> = if op == "=" { vec![1, 2, 3] } else { vec![] };
        assert_eq!(ids(&core, "alice", &at_limit), want, "{op}");
    }
    let over = format!("SELECT id FROM t WHERE ({0}) = ({0}) LIMIT 10", row(65));
    assert_eq!(code(&core, &over), "54000");
}

/// 成分 1 個の括弧は従来どおり式（行値にならない）。
#[test]
fn single_component_parens_stay_scalar() {
    let (path, _g) = setup("row-cmp-scalar");
    let core = core_at(&path);
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE (a) < 3 LIMIT 10")
        .expect("scalar parens");
    assert_eq!(r.rows.len(), 1);
}

/// JOIN・サブクエリ内側など別の束縛経路: 行値比較が「黙って落ちて全行返却」にならず、
/// 評価されるか fail-closed で拒否される（fail-open にならない）。
#[test]
fn row_compare_in_other_paths_never_drops_the_predicate() {
    let (path, _g) = setup("row-cmp-paths");
    let core = core_at(&path);
    for sql in [
        "SELECT t.id FROM t JOIN t AS u ON t.id = u.id WHERE (t.a, t.b) < (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE (a, b) < (3, 5)) LIMIT 10",
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t AS u WHERE (u.a, u.b) < (3, 5) AND u.id = t.id) LIMIT 10",
    ] {
        match run(&core, "alice", sql) {
            Ok(SqlOutcome::Query(r)) => {
                let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
                v.sort_unstable();
                assert_eq!(v, vec![1], "{sql}");
            }
            Ok(other) => panic!("{sql}: {other:?}"),
            Err(e) => assert!(
                matches!(e.wire_code(), "0A000" | "42601"),
                "{sql}: {e:?}"
            ),
        }
    }
}

/// 永続化される定義文脈（POLICY・VIEW・部分 INDEX・CHECK）は定義時に拒否され、何も残らない。
/// POLICY は明示ゲート・部分 INDEX は索引述語の検査で `0A000`、VIEW 本文・CHECK 本体は
/// 行値を認識せず `42601`。
/// tracked: #3254
#[test]
fn row_compare_is_rejected_in_definition_contexts() {
    let (path, _g) = setup("row-cmp-defs");
    let core = core_at(&path);
    for (sql, want) in [
        (
            "CREATE POLICY p ON t FOR SELECT USING ((a, b) < (3, 5))",
            "0A000",
        ),
        (
            "CREATE VIEW v AS SELECT id FROM t WHERE (a, b) < (3, 5)",
            "42601",
        ),
        ("CREATE INDEX i ON t (a) WHERE (a, b) < (3, 5)", "0A000"),
        (
            "ALTER TABLE t ADD CONSTRAINT c CHECK ((a, b) < (3, 5))",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    // 拒否された定義は残らず、平文の SELECT は従来どおり通る（RLS 述語が増えていない）。
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE s = 'x' LIMIT 10")
        .expect("select after rejected definitions");
    assert_eq!(r.rows.len(), 3);
}

/// テナント境界: 同じ述語でも各テナントは自分の行だけを見る（他テナントの行は出ない）。
#[test]
fn row_compare_does_not_cross_tenants() {
    let (path, _g) = setup("row-cmp-tenant");
    let core = core_at(&path);
    for op in ["=", "<=", ">=", "<>"] {
        let sql = format!("SELECT id FROM t WHERE (a, b) {op} (3, 5) LIMIT 10");
        let alice = ids(&core, "alice", &sql);
        let bob = ids(&core, "bob", &sql);
        assert!(!alice.contains(&4), "{op}: {alice:?}");
        assert!(bob.iter().all(|i| *i == 4), "{op}: {bob:?}");
    }
    assert_eq!(
        ids(
            &core,
            "bob",
            "SELECT id FROM t WHERE (a, b) = (3, 5) LIMIT 10"
        ),
        vec![4]
    );
}

/// `$n` を行値の成分に置く形は Parse の時点で拒否される（fail-closed。誤った型・個数を報告して
/// 成功させない）。型推論・Describe への結線は #3257 の担当。tracked: #3257
#[test]
fn row_compare_with_placeholders_fails_closed_at_parse() {
    let (path, _g) = setup("row-cmp-param");
    let core = core_at(&path);
    let err = core
        .parse_sql_prepared("SELECT id FROM t WHERE (a, b) < ($1, $2) LIMIT 10")
        .expect_err("placeholders in a row value must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
