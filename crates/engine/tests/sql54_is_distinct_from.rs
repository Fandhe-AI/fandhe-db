//! `WHERE` の `IS [NOT] DISTINCT FROM`（NULL 安全な等価比較）の結合テスト（Issue #1783）。
//!
//! ポインタ: SQL-54（`docs/spec/04-behavior/` 配下。本文は転記しない）。`EngineCore::
//! execute_sql_in_session` の本番経路を実 `Storage` 越しに通し、真理値表（NULL／非 NULL の全組）・
//! 型ごとの比較・式の被演算子・述語形 `UPDATE`／`DELETE`・`NOT` の押し下げ・fail-closed の拒否形・
//! RLS 相当のテナント境界・スカラー索引の対象外（NULL 意味論を壊さない）を固定する。
//! 拡張クエリ（`$n`）の経路は別ファイル `sql54_is_distinct_from_params.rs` が担う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
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

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a INTEGER, b INTEGER, big BIGINT, dbl DOUBLE PRECISION, s TEXT, \
         s2 TEXT, d DATE, ts TIMESTAMP, iv INTERVAL, flag BOOLEAN)",
    )
    .expect("create table");
    (core, CleanupGuard(path))
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql)
        .map_err(|e| e.wire_code().to_string())
}

fn insert(core: &EngineCore, tenant: &str, id: u64, cols: &str, vals: &str) {
    let sql = format!(
        "INSERT INTO t (id, {cols}) VALUES ({id}, {vals}) USING OPERATION_ID '{tenant}-ins-{id}'"
    );
    exec(core, tenant, &sql).unwrap_or_else(|e| panic!("insert {sql:?} failed: {e}"));
}

/// `(a, b)` の 5 通り（`(1,1)`／`(1,2)`／`(1,NULL)`／`(NULL,1)`／`(NULL,NULL)`）を id 1..=5 で投入する。
fn seed_pairs(core: &EngineCore) {
    insert(core, "tenant-a", 1, "a, b", "1, 1");
    insert(core, "tenant-a", 2, "a, b", "1, 2");
    insert(core, "tenant-a", 3, "a", "1");
    insert(core, "tenant-a", 4, "b", "1");
    insert(core, "tenant-a", 5, "a", "NULL");
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

/// SQL-54: 真理値表（両辺とも列）。PG 17 と同じ集合になる。
#[test]
fn truth_table_column_vs_column() {
    let (core, _g) = setup("sql54-truth-cols");
    seed_pairs(&core);
    assert_eq!(ids(&core, "a IS DISTINCT FROM b"), vec![2, 3, 4]);
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM b"), vec![1, 5]);
    // 前置 NOT は `negated` の反転（結果が NULL にならないため三値論理でも厳密）。
    assert_eq!(ids(&core, "NOT a IS DISTINCT FROM b"), vec![1, 5]);
    assert_eq!(ids(&core, "NOT a IS NOT DISTINCT FROM b"), vec![2, 3, 4]);
    assert_eq!(ids(&core, "NOT NOT a IS DISTINCT FROM b"), vec![2, 3, 4]);
}

/// SQL-54: 片側が `NULL` リテラル・両側が `NULL` リテラル。
#[test]
fn truth_table_with_null_literals() {
    let (core, _g) = setup("sql54-truth-null");
    seed_pairs(&core);
    assert_eq!(ids(&core, "a IS DISTINCT FROM NULL"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM NULL"), vec![4, 5]);
    assert_eq!(ids(&core, "NULL IS DISTINCT FROM a"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "NULL IS NOT DISTINCT FROM a"), vec![4, 5]);
    assert_eq!(ids(&core, "NULL IS DISTINCT FROM NULL"), Vec::<u64>::new());
    assert_eq!(
        ids(&core, "NULL IS NOT DISTINCT FROM NULL"),
        vec![1, 2, 3, 4, 5]
    );
}

/// SQL-54: 列とリテラル。非 NULL 同士は `=` の否定と同じ。
#[test]
fn truth_table_column_vs_literal() {
    let (core, _g) = setup("sql54-truth-lit");
    seed_pairs(&core);
    assert_eq!(ids(&core, "a IS DISTINCT FROM 1"), vec![4, 5]);
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM 1"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "a IS DISTINCT FROM 2"), vec![1, 2, 3, 4, 5]);
    assert_eq!(ids(&core, "1 IS NOT DISTINCT FROM a"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "1 IS DISTINCT FROM 1"), Vec::<u64>::new());
}

/// SQL-54: AND／OR／括弧との組み合わせと既存述語の回帰。
#[test]
fn combines_with_boolean_structure_and_keeps_existing_predicates() {
    let (core, _g) = setup("sql54-combine");
    seed_pairs(&core);
    assert_eq!(
        ids(&core, "(a IS DISTINCT FROM 1 OR b IS NOT DISTINCT FROM 2)"),
        vec![2, 4, 5]
    );
    assert_eq!(
        ids(
            &core,
            "a IS DISTINCT FROM 1 AND b IS NOT DISTINCT FROM NULL"
        ),
        vec![5]
    );
    // 回帰: `IS NULL`／`IS NOT NULL`／`NOT IN`／`BETWEEN` は従来どおり。
    assert_eq!(ids(&core, "a IS NULL"), vec![4, 5]);
    assert_eq!(ids(&core, "b IS NOT NULL"), vec![1, 2, 4]);
    assert_eq!(ids(&core, "a NOT IN (2, 3)"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "a BETWEEN 1 AND 1"), vec![1, 2, 3]);
}

/// SQL-54: 式の被演算子（算術・関数呼び出し）。`FROM` 直後の関数呼び出しが表関数スロットと
/// 誤認されないこと（検索パス解決・修飾子正規化の回帰確認）。
#[test]
fn expression_operands() {
    let (core, _g) = setup("sql54-expr");
    insert(&core, "tenant-a", 1, "a, b, s, s2", "1, 2, 'Foo', 'foo'");
    insert(&core, "tenant-a", 2, "a, b, s, s2", "1, 1, 'Bar', 'baz'");
    insert(&core, "tenant-a", 3, "a, s", "5, 'Baz'");
    assert_eq!(ids(&core, "a + 1 IS NOT DISTINCT FROM b"), vec![1]);
    assert_eq!(ids(&core, "a + 1 IS DISTINCT FROM b"), vec![2, 3]);
    assert_eq!(ids(&core, "lower(s) IS NOT DISTINCT FROM 'foo'"), vec![1]);
    assert_eq!(
        ids(&core, "lower(s) IS NOT DISTINCT FROM lower(s2)"),
        vec![1]
    );
    assert_eq!(ids(&core, "lower(s) IS DISTINCT FROM s2"), vec![2, 3]);
    assert_eq!(ids(&core, "lower(s2) IS DISTINCT FROM NULL"), vec![1, 2]);
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM s"), vec![1, 2, 3]);
}

/// SQL-54: 型ごとの比較（列×列・列×リテラル・列×`NULL`）。
#[test]
fn per_type_comparisons() {
    let (core, _g) = setup("sql54-types");
    insert(
        &core,
        "tenant-a",
        1,
        "big, dbl, s, d, ts, iv",
        "10, 1.5, 'x', '2024-01-01', '2024-01-01 00:00:00', '1 day'",
    );
    insert(&core, "tenant-a", 2, "big", "11");
    for (col, lit_match, lit_other) in [
        ("big", "10", "12"),
        ("dbl", "1.5", "2.5"),
        ("s", "'x'", "'y'"),
        ("d", "'2024-01-01'", "'2024-02-02'"),
        ("ts", "'2024-01-01 00:00:00'", "'2024-02-02 00:00:00'"),
    ] {
        let not_distinct_match = format!("{col} IS NOT DISTINCT FROM {lit_match}");
        let distinct_match = format!("{col} IS DISTINCT FROM {lit_match}");
        let distinct_other = format!("{col} IS DISTINCT FROM {lit_other}");
        let not_distinct_null = format!("{col} IS NOT DISTINCT FROM NULL");
        let distinct_null = format!("{col} IS DISTINCT FROM NULL");
        let self_cmp = format!("{col} IS NOT DISTINCT FROM {col}");
        if col == "big" {
            assert_eq!(ids(&core, &not_distinct_match), vec![1], "{col}");
            assert_eq!(ids(&core, &distinct_match), vec![2], "{col}");
            assert_eq!(ids(&core, &distinct_other), vec![1, 2], "{col}");
            assert_eq!(ids(&core, &not_distinct_null), Vec::<u64>::new(), "{col}");
            assert_eq!(ids(&core, &distinct_null), vec![1, 2], "{col}");
        } else {
            assert_eq!(ids(&core, &not_distinct_match), vec![1], "{col}");
            assert_eq!(ids(&core, &distinct_match), vec![2], "{col}");
            assert_eq!(ids(&core, &distinct_other), vec![1, 2], "{col}");
            assert_eq!(ids(&core, &not_distinct_null), vec![2], "{col}");
            assert_eq!(ids(&core, &distinct_null), vec![1], "{col}");
        }
        assert_eq!(ids(&core, &self_cmp), vec![1, 2], "{col}");
    }
    // INTERVAL は NULL 判定だけ対応（式経路の `=` が未対応のため値の比較は従来どおり 42883）。
    assert_eq!(ids(&core, "iv IS DISTINCT FROM NULL"), vec![1]);
    assert_eq!(ids(&core, "iv IS NOT DISTINCT FROM NULL"), vec![2]);
    assert_eq!(err_code(&core, "iv IS NOT DISTINCT FROM '1 day'"), "42883");
    // 主キー列 id。
    assert_eq!(ids(&core, "id IS NOT DISTINCT FROM 2"), vec![2]);
    assert_eq!(ids(&core, "id IS DISTINCT FROM 2"), vec![1]);
    assert_eq!(ids(&core, "id IS DISTINCT FROM NULL"), vec![1, 2]);
}

/// SQL-54: 型の不一致は既存の `=` 比較と同じ SQLSTATE を返す。
#[test]
fn type_mismatch_matches_equality_sqlstate() {
    let (core, _g) = setup("sql54-mismatch");
    insert(&core, "tenant-a", 1, "a, s", "1, 'x'");
    for (distinct, eq) in [
        ("a IS DISTINCT FROM 'abc'", "a = 'abc'"),
        ("a IS DISTINCT FROM s", "a = s"),
        ("s IS NOT DISTINCT FROM 1", "s = 1"),
    ] {
        let d = err_code(&core, distinct);
        let e = err_code(&core, eq);
        assert_eq!(d, e, "{distinct} vs {eq}");
    }
}

/// SQL-54: 述語形 `UPDATE`／`DELETE`（NULL 行を正しく対象にする。同一 operation_id の再送は既存契約）。
#[test]
fn predicate_update_and_delete() {
    let (core, _g) = setup("sql54-dml");
    seed_pairs(&core);
    let upd = "UPDATE t SET s = 'hit' WHERE a IS NOT DISTINCT FROM b USING OPERATION_ID 'u-1'";
    exec(&core, "tenant-a", upd).expect("update");
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM 'hit'"), vec![1, 5]);
    // 同一内容の再送は適用済み（23505。content_hash が安定）。
    assert_eq!(exec(&core, "tenant-a", upd).expect_err("replay"), "23505");
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM 'hit'"), vec![1, 5]);
    // 同じ operation_id で `IS DISTINCT` に変えた別内容は内容違い（22023。negated がハッシュに入る）。
    let changed = "UPDATE t SET s = 'hit' WHERE a IS DISTINCT FROM b USING OPERATION_ID 'u-1'";
    assert_eq!(
        exec(&core, "tenant-a", changed).expect_err("changed"),
        "22023"
    );
    // 左右を入れ替えた別内容も内容違い。
    let swapped = "UPDATE t SET s = 'hit' WHERE b IS NOT DISTINCT FROM a USING OPERATION_ID 'u-1'";
    assert_eq!(
        exec(&core, "tenant-a", swapped).expect_err("swapped"),
        "22023"
    );
    exec(
        &core,
        "tenant-a",
        "DELETE FROM t WHERE a IS DISTINCT FROM NULL USING OPERATION_ID 'd-1'",
    )
    .expect("delete");
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM a"), vec![4, 5]);
}

/// SQL-54: 未対応の形は従来どおり fail-closed（`42601`／`0A000`）で拒否する。
#[test]
fn unsupported_forms_fail_closed() {
    let (core, _g) = setup("sql54-failclosed");
    seed_pairs(&core);
    // その他の `IS` 形は従来どおり 42601。
    assert_eq!(err_code(&core, "a IS TRUE"), "42601");
    assert_eq!(err_code(&core, "a IS DISTINCT b"), "42601");
    assert_eq!(err_code(&core, "a IS DISTINCT FROM"), "42601");
    assert_eq!(err_code(&core, "a IS NOT DISTINCT FROM b = 1"), "42601");
    // サブクエリ右辺は 0A000（内側は実行しない）。
    assert_eq!(
        err_code(&core, "a IS DISTINCT FROM (SELECT a FROM t LIMIT 1)"),
        "0A000"
    );
    // SELECT リスト・HAVING・JOIN の WHERE は対象外。
    for sql in [
        "SELECT a IS DISTINCT FROM b FROM t LIMIT 5",
        "SELECT CASE WHEN a IS DISTINCT FROM b THEN 1 ELSE 0 END FROM t LIMIT 5",
        "SELECT a, COUNT(*) AS c FROM t GROUP BY a HAVING c IS DISTINCT FROM 1",
        "SELECT x.id FROM t x JOIN t y ON x.id = y.id WHERE x.a IS DISTINCT FROM 1 LIMIT 10",
    ] {
        assert!(
            exec(&core, "tenant-a", sql).is_err(),
            "{sql} must be rejected"
        );
    }
    // 対照: 同じ形で `IS DISTINCT FROM` を除いた文は受理される（拒否理由が文法の誤りでないこと）。
    for sql in [
        "SELECT a, COUNT(*) AS c FROM t GROUP BY a HAVING c > 0",
        "SELECT x.id FROM t x JOIN t y ON x.id = y.id LIMIT 10",
    ] {
        exec(&core, "tenant-a", sql).unwrap_or_else(|e| panic!("control {sql:?} failed: {e}"));
    }
    // ビュー本体の WHERE にも置けない（式述語は従来どおり拒否）。
    let mut session = SessionState::default();
    session.allow_ddl();
    assert!(
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "CREATE VIEW v AS SELECT id FROM t WHERE a IS DISTINCT FROM b",
        )
        .is_err(),
        "view body must reject IS DISTINCT FROM"
    );
    // BOOLEAN 列の式経路は対象外（0A000）。
    assert!(
        ids_of(&core, "tenant-a", "flag IS DISTINCT FROM NULL").is_err(),
        "boolean column is out of scope"
    );
}

/// SQL-54: CHECK 本体では受理しない（永続化する述語の評価経路の保証範囲外）。
#[test]
fn check_constraint_rejects_is_distinct_from() {
    let (core, _g) = setup("sql54-check");
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "CREATE TABLE c (a INTEGER, b INTEGER, CHECK (a IS DISTINCT FROM b))",
        )
        .expect_err("CHECK with IS DISTINCT FROM must be rejected");
    assert!(
        matches!(err.wire_code(), "0A000" | "42601"),
        "unexpected code {}",
        err.wire_code()
    );
}

/// SQL-54（P0・RLS 相当のテナント境界）: 別テナントの NULL 行が `IS NOT DISTINCT FROM NULL` で漏れない。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, _g) = setup("sql54-tenant");
    insert(&core, "tenant-a", 1, "a", "NULL");
    insert(&core, "tenant-a", 2, "a", "7");
    insert(&core, "tenant-b", 3, "a", "NULL");
    insert(&core, "tenant-b", 4, "a", "7");
    assert_eq!(
        ids_of(&core, "tenant-a", "a IS NOT DISTINCT FROM NULL").expect("a"),
        vec![1]
    );
    assert_eq!(
        ids_of(&core, "tenant-b", "a IS NOT DISTINCT FROM NULL").expect("b"),
        vec![3]
    );
    assert_eq!(
        ids_of(&core, "tenant-a", "NULL IS NOT DISTINCT FROM NULL").expect("a all"),
        vec![1, 2]
    );
    assert_eq!(
        ids_of(&core, "tenant-a", "a IS DISTINCT FROM 7").expect("a distinct"),
        vec![1]
    );
}

/// SQL-54: スカラー索引（宣言済みの列）があっても索引の選択対象にならず、NULL 行を正しく返す。
#[test]
fn scalar_index_does_not_change_null_semantics() {
    let (core, _g) = setup("sql54-index");
    seed_pairs(&core);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX t_a_idx ON t (a)",
    )
    .expect("create index");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX t_s_idx ON t (s)",
    )
    .expect("create text index");
    // TEXT 列の索引でも同様（`IS DISTINCT` は常に残余述語の全走査で、NULL 行を落とさない）。
    exec(
        &core,
        "tenant-a",
        "UPDATE t SET s = 'x' WHERE id = 1 USING OPERATION_ID 'sx-1'",
    )
    .expect("set s");
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM 'x'"), vec![1]);
    assert_eq!(ids(&core, "s IS DISTINCT FROM 'x'"), vec![2, 3, 4, 5]);
    assert_eq!(ids(&core, "s IS NOT DISTINCT FROM NULL"), vec![2, 3, 4, 5]);
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM 1"), vec![1, 2, 3]);
    assert_eq!(ids(&core, "a IS DISTINCT FROM 1"), vec![4, 5]);
    assert_eq!(ids(&core, "a IS NOT DISTINCT FROM NULL"), vec![4, 5]);
    assert_eq!(ids(&core, "a IS DISTINCT FROM NULL"), vec![1, 2, 3]);
}
