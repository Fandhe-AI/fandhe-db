//! 左辺が式の `IS [NOT] NULL`・`[NOT] BETWEEN`・`[NOT] IN (list)` の結合テスト（SQL-24 の拡張。
//! PL/pgSQL の埋め込み SQL で変数が左辺に現れても書ける形を、通常の SQL としても受理する）。
//!
//! ポインタ: SQL-24（`docs/spec/04-behavior/` 配下。本文は転記しない）。`EngineCore::
//! execute_sql_in_session` の本番経路を実 `Storage` 越しに通し、三値論理（NULL 行・NULL 要素・
//! NULL 境界）の結果が標準 SQL（PostgreSQL）と一致すること、前置・後置 `NOT` の押し下げ、
//! 述語形 `UPDATE`／`DELETE`、CHECK 本体での拒否（従来どおり）、fail-closed の拒否形、
//! テナント境界を固定する。期待値は各述語を手書きの連言／選言へ展開した結果とも突き合わせる。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn insert(core: &EngineCore, id: u64, a: Option<i64>, b: Option<i64>, s: Option<&str>) {
    let lit = |v: Option<String>| v.unwrap_or_else(|| "NULL".to_string());
    let sql = format!(
        "INSERT INTO t (id, a, b, s) VALUES ({id}, {}, {}, {}) USING OPERATION_ID 'ins-{id}'",
        lit(a.map(|n| n.to_string())),
        lit(b.map(|n| n.to_string())),
        lit(s.map(|v| format!("'{v}'"))),
    );
    exec(core, "tenant-a", &sql).unwrap_or_else(|e| panic!("insert {sql:?} failed: {e}"));
}

/// 5 行（NULL を含む）。
/// id=1 (a=1, b=1, s='x')／id=2 (a=2, b=NULL, s='y')／id=3 (a=NULL, b=3, s=NULL)／
/// id=4 (a=5, b=1, s='z')／id=5 (a=9, b=1, s='w')
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a INTEGER, b INTEGER, s TEXT)",
    )
    .expect("create table");
    insert(&core, 1, Some(1), Some(1), Some("x"));
    insert(&core, 2, Some(2), None, Some("y"));
    insert(&core, 3, None, Some(3), None);
    insert(&core, 4, Some(5), Some(1), Some("z"));
    insert(&core, 5, Some(9), Some(1), Some("w"));
    (core, CleanupGuard(path))
}

fn ids_of(core: &EngineCore, tenant: &str, where_clause: &str) -> Result<Vec<u64>, String> {
    let sql = format!("SELECT id FROM t WHERE {where_clause} LIMIT 100");
    match exec(core, tenant, &sql)? {
        SqlOutcome::Query(r) => {
            let mut v: Vec<u64> = r
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Integer(n)) => *n,
                    Some(Cell::SignedInteger(n)) => u64::try_from(*n).expect("non-negative id"),
                    other => panic!("expected id cell, got {other:?}"),
                })
                .collect();
            v.sort_unstable();
            Ok(v)
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn ids(core: &EngineCore, where_clause: &str) -> Vec<u64> {
    ids_of(core, "tenant-a", where_clause)
        .unwrap_or_else(|e| panic!("{where_clause:?} should succeed, got {e}"))
}

fn err_code(core: &EngineCore, where_clause: &str) -> String {
    ids_of(core, "tenant-a", where_clause)
        .expect_err(&format!("{where_clause:?} should be rejected"))
}

/// `IS [NOT] NULL`: 式の結果が NULL かどうか（結果自体は NULL にならない）。
#[test]
fn is_null_on_expression() {
    let (core, _g) = setup("sql24-isnull");
    assert_eq!(ids(&core, "a + 1 IS NULL"), vec![3]);
    assert_eq!(ids(&core, "a + 1 IS NOT NULL"), vec![1, 2, 4, 5]);
    assert_eq!(ids(&core, "a + b IS NULL"), vec![2, 3]);
    assert_eq!(ids(&core, "lower(s) IS NULL"), vec![3]);
    assert_eq!(ids(&core, "a + b IS NOT NULL"), vec![1, 4, 5]);
    // 前置 NOT・二重 NOT は結果が NULL にならないので単純な反転と一致する。
    assert_eq!(ids(&core, "NOT a + 1 IS NULL"), vec![1, 2, 4, 5]);
    assert_eq!(ids(&core, "NOT a + 1 IS NOT NULL"), vec![3]);
    assert_eq!(ids(&core, "NOT NOT a + 1 IS NULL"), vec![3]);
    // 手書きの `IS NOT DISTINCT FROM NULL` と同じ集合。
    assert_eq!(
        ids(&core, "a + b IS NULL"),
        ids(&core, "a + b IS NOT DISTINCT FROM NULL")
    );
    // 合成（AND／OR）。
    assert_eq!(ids(&core, "a + 1 IS NOT NULL AND lower(s) = 'x'"), vec![1]);
    assert_eq!(ids(&core, "a + 1 IS NULL OR lower(s) = 'x'"), vec![1, 3]);
}

/// `[NOT] BETWEEN`: `x >= low AND x <= high`。NULL を含む行・境界の三値論理。
#[test]
fn between_on_expression_three_valued() {
    let (core, _g) = setup("sql24-between");
    assert_eq!(ids(&core, "a + 0 BETWEEN 1 AND 2"), vec![1, 2]);
    // NOT BETWEEN は NULL 行を含まない（NOT UNKNOWN は UNKNOWN）。
    assert_eq!(ids(&core, "a + 0 NOT BETWEEN 1 AND 2"), vec![4, 5]);
    assert_eq!(ids(&core, "NOT a + 0 BETWEEN 1 AND 2"), vec![4, 5]);
    assert_eq!(ids(&core, "NOT a + 0 NOT BETWEEN 1 AND 2"), vec![1, 2]);
    // 手書きの連言／選言と同じ集合。
    assert_eq!(
        ids(&core, "a + 0 BETWEEN 1 AND 2"),
        ids(&core, "a + 0 >= 1 AND a + 0 <= 2")
    );
    assert_eq!(
        ids(&core, "a + 0 NOT BETWEEN 1 AND 2"),
        ids(&core, "a + 0 < 1 OR a + 0 > 2")
    );
    // 境界が列（NULL を含む）: id=2 は b が NULL で UNKNOWN。
    assert_eq!(ids(&core, "a + 0 BETWEEN b AND 5"), vec![1, 4]);
    assert_eq!(ids(&core, "a + 0 NOT BETWEEN b AND 5"), vec![5]);
    // 境界が式。
    assert_eq!(ids(&core, "a + 0 BETWEEN b + 1 AND b + 4"), vec![4]);
}

/// 境界が NULL リテラルの `BETWEEN` は、比較の片側が型の決まらない NULL になるため（`=`・`<` 等でも
/// 同じ）束縛段が `0A000` で拒否する（fail-closed。三値論理の UNKNOWN を 2 値へ丸めて
/// `NOT BETWEEN` を fail-open にしない）。NULL を含む行・列の境界は上のテストのとおり PostgreSQL と同じ。
#[test]
fn between_with_null_literal_bound_is_rejected() {
    let (core, _g) = setup("sql24-between-null");
    for w in [
        "a + 0 BETWEEN NULL AND 5",
        "a + 0 NOT BETWEEN NULL AND 5",
        "a + 0 BETWEEN 1 AND NULL",
        "a + 0 NOT BETWEEN 3 AND NULL",
    ] {
        assert_eq!(err_code(&core, w), "0A000", "{w}");
    }
    // 右辺の NULL リテラルを直接比較に書く既存の形も（`NULL` は列名として読まれ）拒否される。
    assert!(ids_of(&core, "tenant-a", "a + 0 >= NULL").is_err());
}

/// `[NOT] IN (list)`: `x = e1 OR x = e2 ...`。NULL を含む行・要素の三値論理。
#[test]
fn in_list_on_expression_three_valued() {
    let (core, _g) = setup("sql24-in");
    assert_eq!(ids(&core, "a + 0 IN (1, 5)"), vec![1, 4]);
    assert_eq!(ids(&core, "a + 0 IN (5)"), vec![4]);
    // NOT IN は NULL 行（a が NULL）を含まない。
    assert_eq!(ids(&core, "a + 0 NOT IN (1, 5)"), vec![2, 5]);
    assert_eq!(ids(&core, "NOT a + 0 IN (1, 5)"), vec![2, 5]);
    assert_eq!(ids(&core, "NOT a + 0 NOT IN (1, 5)"), vec![1, 4]);
    // 文字列の式。
    assert_eq!(ids(&core, "lower(s) IN ('x', 'y')"), vec![1, 2]);
    assert_eq!(ids(&core, "lower(s) NOT IN ('x', 'y')"), vec![4, 5]);
    // 要素が列（NULL を含む）: id=2 の b は NULL だが a=2 が 2 に一致するので真。
    assert_eq!(ids(&core, "a + 0 IN (b, 2)"), vec![1, 2]);
    // 手書きの選言／連言と同じ集合。
    assert_eq!(
        ids(&core, "a + 0 IN (1, 5)"),
        ids(&core, "a + 0 = 1 OR a + 0 = 5")
    );
    assert_eq!(
        ids(&core, "a + 0 NOT IN (1, 5)"),
        ids(&core, "a + 0 <> 1 AND a + 0 <> 5")
    );
}

/// 要素に NULL リテラルを含む `IN` は、`= NULL` の比較と同じく束縛段が `0A000` で拒否する
/// （`NOT IN` の UNKNOWN を丸めて fail-open にしない）。NULL を含む行・列の要素は上のとおり
/// PostgreSQL と同じ結果になる。
#[test]
fn in_list_with_null_literal_element_is_rejected() {
    let (core, _g) = setup("sql24-in-null");
    for w in [
        "a + 0 IN (1, NULL)",
        "a + 0 NOT IN (1, NULL)",
        "a + 0 IN (NULL)",
        "NOT a + 0 IN (1, NULL)",
    ] {
        assert_eq!(err_code(&core, w), "0A000", "{w}");
    }
}

/// 述語形 `UPDATE`／`DELETE` でも同じ述語が使える（NULL 行は対象外）。
#[test]
fn predicate_update_and_delete() {
    let (core, _g) = setup("sql24-dml");
    exec(
        &core,
        "tenant-a",
        "UPDATE t SET s = 'hit' WHERE a + 0 BETWEEN 1 AND 2 USING OPERATION_ID 'u-1'",
    )
    .expect("update");
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM 'hit'"), vec![1, 2]);
    exec(
        &core,
        "tenant-a",
        "DELETE FROM t WHERE a + 1 IS NULL USING OPERATION_ID 'd-1'",
    )
    .expect("delete is null");
    assert_eq!(ids(&core, "id >= 1"), vec![1, 2, 4, 5]);
    exec(
        &core,
        "tenant-a",
        "DELETE FROM t WHERE a + 0 NOT IN (1, 2) USING OPERATION_ID 'd-2'",
    )
    .expect("delete not in");
    assert_eq!(ids(&core, "id >= 1"), vec![1, 2]);
}

/// 未対応の形は従来どおり fail-closed で拒否する。
#[test]
fn unsupported_forms_stay_rejected() {
    let (core, _g) = setup("sql24-rejects");
    // 空リスト・閉じ括弧の不備・SYMMETRIC・IN のサブクエリ（式左辺）。
    assert_eq!(err_code(&core, "a + 0 IN ()"), "42601");
    assert_eq!(err_code(&core, "a + 0 IN (1, 2"), "42601");
    assert_eq!(err_code(&core, "a + 0 IN (1 2)"), "42601");
    assert_eq!(err_code(&core, "a + 0 BETWEEN SYMMETRIC 1 AND 2"), "42601");
    assert_eq!(err_code(&core, "a + 0 BETWEEN 1 OR 2"), "42601");
    assert_eq!(err_code(&core, "a + 0 IN (SELECT a FROM t)"), "42601");
    // `IS TRUE` 等の他の `IS` 述語は引き続き未対応。
    assert_eq!(err_code(&core, "a + 0 IS TRUE"), "42601");
    // 型の不一致は `=` と同じ SQLSTATE（束縛段の規則を共有する）。
    assert_eq!(
        err_code(&core, "a + 0 IN ('x')"),
        err_code(&core, "a + 0 = 'x'")
    );
}

/// CHECK 本体では受理しない（従来どおり。永続化する述語の評価経路の保証範囲外）。
#[test]
fn check_constraint_rejects_expression_lhs_forms() {
    let (core, _g) = setup("sql24-check");
    for check in [
        "CHECK (a + 1 IS NULL)",
        "CHECK (a + 1 BETWEEN 1 AND 2)",
        "CHECK (a + 1 IN (1, 2))",
    ] {
        let mut session = SessionState::default();
        session.allow_ddl();
        let sql = format!("CREATE TABLE c (a INTEGER, {check})");
        let err = core
            .execute_sql_in_session(&ctx("tenant-a"), &mut session, &sql)
            .expect_err(&format!("{check} must be rejected"));
        assert!(
            matches!(err.wire_code(), "0A000" | "42601"),
            "{check}: unexpected code {}",
            err.wire_code()
        );
    }
}

/// P0（テナント境界）: 別テナントには行が見えない。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, _g) = setup("sql24-tenant");
    for w in [
        "a + 1 IS NOT NULL",
        "a + 0 BETWEEN 0 AND 100",
        "a + 0 IN (1, 2, 5, 9)",
    ] {
        assert!(!ids(&core, w).is_empty(), "{w}");
        assert_eq!(
            ids_of(&core, "tenant-b", w).unwrap_or_default(),
            Vec::<u64>::new(),
            "{w}"
        );
    }
}
