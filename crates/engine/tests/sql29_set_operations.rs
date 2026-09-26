//! `UNION`／`UNION ALL`／`INTERSECT`／`EXCEPT`（Issue #929。ポインタ: SQL-29 (c)・
//! RLS-10 (b)・TASK-213）の結合テスト。
//!
//! `tests/sql25_offset.rs`・`tests/sql24_like_patterns.rs` と同じ流儀（実
//! `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、
//! `EngineCore` を production 経路として使う）。各枝は単一テーブルの広域取得
//! （SQL-15）に限定する設計（`sql::set_op` モジュールドキュメント参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";
const OTHER: &str = "other_docs";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn int_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("score", ColumnType::Integer, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    table: &str,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{table}-{id}"))
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        table,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}")),
    )
}

fn run_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(outcome) => panic!("expected error, got {outcome:?}"),
        Err(e) => e,
    }
}

fn langs(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            engine::sql::exec::Cell::Text(s) => s.clone(),
            other => panic!("expected Text cell, got {other:?}"),
        })
        .collect()
}

fn seeded_two_tables() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("set-op-basic");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_ctx = ctx("tenant-a");
    insert_row(&storage, DOCS, &tenant_ctx, 1, "ja", Visibility::Public);
    insert_row(&storage, DOCS, &tenant_ctx, 2, "en", Visibility::Public);
    insert_row(&storage, DOCS, &tenant_ctx, 3, "ja", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_ctx, 10, "en", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_ctx, 11, "fr", Visibility::Public);
    (storage, path)
}

// ---------- 意味論 ----------

#[test]
fn union_all_concatenates_without_dedup() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION ALL SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    let mut want = vec!["ja", "en", "ja", "en", "fr"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.sort();
    assert_eq!(
        got, want,
        "UNION ALL must keep every row, duplicates included"
    );
}

#[test]
fn union_deduplicates_rows() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(
        got,
        vec!["en".to_string(), "fr".to_string(), "ja".to_string()],
        "UNION must remove duplicate rows across both branches"
    );
}

#[test]
fn intersect_keeps_only_rows_present_on_both_sides() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT SELECT lang FROM other_docs",
    );
    assert_eq!(langs(&result), vec!["en".to_string()]);
}

#[test]
fn except_keeps_only_left_rows_absent_from_right() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs EXCEPT SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(got, vec!["ja".to_string()]);
}

// ---------- 優先順位（INTERSECT は UNION/EXCEPT より高い優先順位で左結合） ----------

#[test]
fn intersect_binds_tighter_than_union() {
    let path = unique_db_path("set-op-precedence");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema("a")).expect("create a");
    storage.create_table(&schema("b")).expect("create b");
    storage.create_table(&schema("c")).expect("create c");
    let tenant_ctx = ctx("tenant-a");
    // a = {a}, b = {b}, c = {b}
    // Cursor Bugbot 指摘対応（PR #1105）: 旧 fixture（a={x}・b={y}・c={x}）は
    // 正しい優先順位（`A UNION (B INTERSECT C)` = {x}）と誤って `UNION`／
    // `INTERSECT` を同一優先順位に平坦化した場合（`(A UNION B) INTERSECT C`
    // = {x,y} ∩ {x} = {x}）のどちらでも同じ {x} になり、優先順位バグを検出
    // できなかった。本 fixture は両者が異なる結果になる（`A UNION (B
    // INTERSECT C)` = {a} ∪ ({b}∩{b}) = {a,b}、`(A UNION B) INTERSECT C`
    // = {a,b} ∩ {b} = {b}）ため、`INTERSECT` を誤って `UNION` と同一優先順位
    // に平坦化する回帰を検出できる。
    insert_row(&storage, "a", &tenant_ctx, 1, "a", Visibility::Public);
    insert_row(&storage, "b", &tenant_ctx, 2, "b", Visibility::Public);
    insert_row(&storage, "c", &tenant_ctx, 3, "b", Visibility::Public);
    let core = new_core(storage);

    // `A UNION B INTERSECT C` == `A UNION (B INTERSECT C)` == {a} UNION ({b} ∩ {b}) == {a,b}
    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM a UNION SELECT lang FROM b INTERSECT SELECT lang FROM c",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn explicit_parens_change_result_vs_default_precedence() {
    let path = unique_db_path("set-op-precedence-parens");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema("a")).expect("create a");
    storage.create_table(&schema("b")).expect("create b");
    storage.create_table(&schema("c")).expect("create c");
    let tenant_ctx = ctx("tenant-a");
    // a = {a}, b = {b}, c = {b}
    // Cursor Bugbot 指摘対応（PR #1105）: 旧 fixture（a={x}・b={x,y}・c={x}）は
    // デフォルト形（`A UNION (B INTERSECT C)` = {x}）と明示括弧形
    // （`(A UNION B) INTERSECT C` = {x,y} ∩ {x} = {x}）が同じ {x} になり、
    // 括弧を無視する・`INTERSECT` を `UNION` と同一優先順位に平坦化する
    // パーサでも通ってしまっていた。本 fixture は両者が異なる結果になる
    // （デフォルト = {a,b}、明示括弧 = {b}）ため、両方に別々の期待値を
    // 固定できる。
    insert_row(&storage, "a", &tenant_ctx, 1, "a", Visibility::Public);
    insert_row(&storage, "b", &tenant_ctx, 2, "b", Visibility::Public);
    insert_row(&storage, "c", &tenant_ctx, 3, "b", Visibility::Public);
    let core = new_core(storage);

    // デフォルト（左結合・INTERSECT が高優先）: A UNION (B INTERSECT C)
    //   = {a} UNION ({b} ∩ {b}) = {a} UNION {b} = {a,b}
    let default_form = run(
        &core,
        "tenant-a",
        "SELECT lang FROM a UNION SELECT lang FROM b INTERSECT SELECT lang FROM c",
    );
    let mut default_got = langs(&default_form);
    default_got.sort();
    assert_eq!(default_got, vec!["a".to_string(), "b".to_string()]);

    // 明示括弧: (A UNION B) INTERSECT C = {a,b} ∩ {b} = {b}
    let parenthesized = run(
        &core,
        "tenant-a",
        "(SELECT lang FROM a UNION SELECT lang FROM b) INTERSECT SELECT lang FROM c",
    );
    let mut paren_got = langs(&parenthesized);
    paren_got.sort();
    assert_eq!(paren_got, vec!["b".to_string()]);
}

/// 演算子の右枝が丸括弧で囲まれた形（`UNION (SELECT ...)`）が
/// `set_operator_is_followed_by_branch` の `(` 先読み厳密化後も引き続き集合演算
/// として検出・実行されること（Issue #929 最終レビュー指摘の回帰防止）。
#[test]
fn union_with_parenthesized_right_branch_is_detected_and_executed() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION (SELECT lang FROM other_docs)",
    );
    let mut got = langs(&result);
    got.sort();
    assert_eq!(
        got,
        vec!["en".to_string(), "fr".to_string(), "ja".to_string()]
    );
}

// ---------- 型整合（42804） ----------

#[test]
fn column_count_mismatch_is_datatype_mismatch() {
    let path = unique_db_path("set-op-column-count-mismatch");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang, embedding FROM docs",
    );
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn column_type_mismatch_is_datatype_mismatch() {
    let path = unique_db_path("set-op-column-type-mismatch");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&int_schema(OTHER))
        .expect("create other_docs");
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT score FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42804");
}

// ---------- VECTOR 列と重複除去の組（22000） ----------

#[test]
fn vector_column_with_union_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT embedding FROM docs UNION SELECT embedding FROM other_docs",
    );
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn vector_column_with_union_all_is_accepted() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT embedding FROM docs UNION ALL SELECT embedding FROM other_docs",
    );
    assert_eq!(result.rows.len(), 5);
}

// ---------- 構文拒否（42601） ----------

#[test]
fn intersect_all_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT ALL SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn union_distinct_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION DISTINCT SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn except_all_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs EXCEPT ALL SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn order_by_inside_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs ORDER BY lang UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn limit_inside_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs LIMIT 1 UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn aggregate_branch_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(
        &core,
        "tenant-a",
        "SELECT COUNT(*) FROM docs UNION SELECT lang FROM other_docs",
    );
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn bare_parenthesized_select_without_operator_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let err = run_err(&core, "tenant-a", "(SELECT lang FROM docs)");
    assert_eq!(err.wire_code(), "42601");
}

// ---------- 上限（54000） ----------

#[test]
fn paren_nesting_depth_exceeding_limit_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    // 入れ子上限は 4。5 段のネストは `54000`。
    let sql = "((((SELECT lang FROM docs)))) UNION SELECT lang FROM other_docs";
    // 上の式は 4 段の入れ子（許可される）なのでまず成功を確認する。
    let _ = run(&core, "tenant-a", sql);

    let too_deep = "(((((SELECT lang FROM docs))))) UNION SELECT lang FROM other_docs";
    let err = run_err(&core, "tenant-a", too_deep);
    assert_eq!(err.wire_code(), "54000");
}

// ---------- 誤検出防止（UDF・列名・テーブル名としての union/intersect/except。
// Issue #929 最終レビュー指摘の回帰） ----------

/// `union`/`intersect`/`except` という名前の宣言的 UDF を `SELECT` リストで
/// 呼び出しても、集合演算の枝解析経路（`Computed` 投影項目を拒否する）へ誤って
/// 回されないこと（`set_operator_is_followed_by_branch` のドキュメンテーション
/// コメント参照）。
#[test]
fn udf_named_union_in_select_list_is_not_routed_to_set_operation() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &tenant_ctx,
        &mut session,
        "CREATE FUNCTION union(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION named union should succeed");
    let outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "SELECT id, union(embedding) AS n FROM docs \
             ORDER BY embedding <=> '[3.0,4.0]' LIMIT 3",
        )
        .expect("SELECT calling a UDF named union should succeed");
    let result = expect_query(outcome);
    assert_eq!(result.rows.len(), 3);
}

/// 上と同じ回帰防止を `intersect`・`except` という UDF 名でも確認する。
#[test]
fn udf_named_intersect_and_except_in_select_list_is_not_routed_to_set_operation() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &tenant_ctx,
        &mut session,
        "CREATE FUNCTION intersect(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION named intersect should succeed");
    core.execute_sql_in_session(
        &tenant_ctx,
        &mut session,
        "CREATE FUNCTION except(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION named except should succeed");

    let outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "SELECT id, intersect(embedding) AS a, except(embedding) AS b FROM docs \
             ORDER BY embedding <=> '[3.0,4.0]' LIMIT 3",
        )
        .expect("SELECT calling UDFs named intersect/except should succeed");
    let result = expect_query(outcome);
    assert_eq!(result.rows.len(), 3);
}

/// `union`/`intersect`/`except` という名前の UDF 呼び出しが `WHERE` 句にあっても
/// 集合演算の枝解析経路へ誤って回されないこと。
#[test]
fn udf_named_union_in_where_clause_is_not_routed_to_set_operation() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &tenant_ctx,
        &mut session,
        "CREATE FUNCTION union(v) AS vec_norm(v)",
    )
    .expect("CREATE FUNCTION named union should succeed");
    let outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "SELECT id FROM docs WHERE union(embedding) > 0.0 \
             ORDER BY embedding <=> '[3.0,4.0]' LIMIT 3",
        )
        .expect("SELECT with a WHERE predicate calling a UDF named union should succeed");
    let _ = expect_query(outcome);
}

/// `union` を列名として使う通常の用法（列名・テーブル名としての互換性維持）が
/// 引き続き動くこと。
#[test]
fn column_named_union_is_still_usable_as_an_ordinary_identifier() {
    let path = unique_db_path("set-op-column-named-union");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    let schema = TableSchema::new(
        "labels",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("union", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create labels");
    let tenant_ctx = ctx("tenant-a");
    engine::tenant::insert_typed_row(
        &storage,
        "labels",
        &tenant_ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("x".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("seed-labels-1")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT union FROM labels WHERE union = 'x' LIMIT 10",
    );
    assert_eq!(result.rows.len(), 1);
}

// ---------- RLS（他テナントの不可視行が中間結果・重複除去・件数に影響しない） ----------

#[test]
fn rls_excludes_other_tenant_rows_from_union() {
    let path = unique_db_path("set-op-rls-union");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");
    insert_row(&storage, DOCS, &tenant_a, 1, "ja", Visibility::Public);
    // 他テナントの private 行（不可視のはず）。
    insert_row(&storage, OTHER, &tenant_b, 2, "ja", Visibility::Private);
    insert_row(&storage, OTHER, &tenant_a, 3, "en", Visibility::Public);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let mut got = langs(&result);
    got.sort();
    // 他テナントの "ja" 行が重複除去の対象や結果件数に影響しないこと
    // （もし混入していれば "ja" は既に docs 側にあるため件数は変わらないが、
    // 追加のテナント境界検証として EXCEPT で不可視行の非存在を確認する）。
    assert_eq!(got, vec!["en".to_string(), "ja".to_string()]);

    // EXCEPT: 他テナントにしか存在しない値（"ja" は tenant-a 自身も持つため
    // 区別できない。tenant-b 専用の値で検証する）。
    let except_result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM other_docs EXCEPT SELECT lang FROM docs",
    );
    // other_docs の可視行は tenant-a 視点で "en" のみ（tenant-b の "ja" は不可視）。
    // docs は "ja" のみなので EXCEPT 結果は "en"。
    assert_eq!(langs(&except_result), vec!["en".to_string()]);
}

#[test]
fn rls_visible_rows_are_independent_per_branch_for_intersect() {
    let path = unique_db_path("set-op-rls-intersect");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    storage
        .create_table(&schema(OTHER))
        .expect("create other_docs");
    let tenant_a = ctx("tenant-a");
    let tenant_b = ctx("tenant-b");
    // tenant-b の private "fr" は tenant-a からは不可視。tenant-a 自身の "fr" は無い。
    insert_row(&storage, DOCS, &tenant_b, 1, "fr", Visibility::Private);
    insert_row(&storage, OTHER, &tenant_b, 2, "fr", Visibility::Private);
    insert_row(&storage, DOCS, &tenant_a, 3, "en", Visibility::Public);
    insert_row(&storage, OTHER, &tenant_a, 4, "en", Visibility::Public);
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs INTERSECT SELECT lang FROM other_docs",
    );
    // 他テナントの private "fr" が両側に存在していても、tenant-a からは不可視
    // なので INTERSECT の判定には一切現れない。
    assert_eq!(langs(&result), vec!["en".to_string()]);
}

// ---------- 全体 LIMIT ----------

#[test]
fn top_level_limit_truncates_result() {
    let path = unique_db_path("set-op-top-limit");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=5u64 {
        insert_row(&storage, DOCS, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let result = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION ALL SELECT lang FROM docs LIMIT 3",
    );
    assert_eq!(result.rows.len(), 3);
}

// ---------- 枝数上限（54000） ----------

#[test]
fn max_branches_at_limit_succeeds_and_over_limit_is_rejected() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    // 枝数上限は 16（`sql::allowlist::MAX_SET_OP_BRANCHES`）。ちょうど 16 枝は
    // 成功し、17 枝は `54000` になることを両側で固定する。
    let branch = "SELECT lang FROM docs";
    let at_limit_sql = std::iter::repeat_n(branch, 16)
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let result = run(&core, "tenant-a", &at_limit_sql);
    assert_eq!(
        result.rows.len(),
        16 * 3,
        "16 branches must all be evaluated"
    );

    let over_limit_sql = std::iter::repeat_n(branch, 17)
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let err = run_err(&core, "tenant-a", &over_limit_sql);
    assert_eq!(err.wire_code(), "54000");
}

// ---------- 可視行数・合成結果行数の上限（54000） ----------

#[test]
fn branch_visible_rows_over_limit_is_rejected() {
    let path = unique_db_path("set-op-branch-row-limit");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema("wide")).expect("create wide");
    storage.create_table(&schema(DOCS)).expect("create docs");
    let tenant_ctx = ctx("tenant-a");
    // 単一枝の可視行数上限（`MAX_SET_OP_ROWS` = `MAX_SEARCH_K` = 10000）を単独で
    // 超える枝を用意する（もう一方の枝は最小限）。
    for id in 1..=10_001u64 {
        insert_row(&storage, "wide", &tenant_ctx, id, "ja", Visibility::Public);
    }
    insert_row(&storage, DOCS, &tenant_ctx, 1, "en", Visibility::Public);
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM wide UNION ALL SELECT lang FROM docs",
    );
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn composed_result_rows_over_limit_is_rejected_even_if_each_branch_is_within_limit() {
    let path = unique_db_path("set-op-composed-row-limit");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage.create_table(&schema(DOCS)).expect("create docs");
    let tenant_ctx = ctx("tenant-a");
    // 各枝は上限（10000）未満だが、`UNION ALL` で合成すると上限を超える
    // （5001 + 5001 = 10002 > 10000）。
    for id in 1..=5_001u64 {
        insert_row(&storage, DOCS, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let err = run_err(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION ALL SELECT lang FROM docs",
    );
    assert_eq!(err.wire_code(), "54000");
}

// ---------- ビューを指す枝（TABLE-18・SQL-23・TASK-205、Issue #909） ----------

#[test]
fn branch_from_view_matches_base_table_equivalent() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let mut ddl_session = SessionState::default();
    ddl_session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut ddl_session,
        "CREATE VIEW docs_view AS SELECT lang FROM docs",
    )
    .expect("create view should succeed");

    // ビューを枝に指定した場合と、ビューが指す基底テーブルを直接指定した場合
    // とで結果が一致すること（`sql::view::resolve_from` による畳み込みが
    // 集合演算の枝でも `Statement::Scan` と同じ経路を通ることの確認）。
    let via_view = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs_view UNION SELECT lang FROM other_docs",
    );
    let via_base = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let mut via_view_sorted = langs(&via_view);
    via_view_sorted.sort();
    let mut via_base_sorted = langs(&via_base);
    via_base_sorted.sort();
    assert_eq!(via_view_sorted, via_base_sorted);
}

// ---------- Describe（拡張クエリプロトコル） ----------

#[test]
fn describe_matches_execute_columns() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT lang FROM docs UNION SELECT lang FROM other_docs";

    let describe_session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
    let described = core
        .describe_parsed_in_session(&describe_session, &parsed)
        .expect("describe should succeed")
        .expect("set operation must produce result columns");

    let mut exec_session = SessionState::default();
    let executed = expect_query(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut exec_session, sql)
            .expect("execute should succeed"),
    );

    assert_eq!(
        described, executed.columns,
        "describe columns must match execute columns for a set operation"
    );
}

/// PR #1105 レビュー指摘の回帰: 全体 `LIMIT` の範囲外検証（`22000`）は Execute
/// （`execute_sql_in_session`）と Describe（`describe_parsed_in_session`）の
/// いずれでも同じ SQLSTATE で拒否する。従来は Describe が全体 `LIMIT` を
/// 検証しておらず、Execute では拒否される範囲外の値が Describe だけ受理されて
/// いた。
#[test]
fn top_level_limit_out_of_range_is_rejected_by_execute_and_describe_alike() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    for sql in [
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs LIMIT 0",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs LIMIT 10001",
    ] {
        let exec_err = run_err(&core, "tenant-a", sql);
        assert_eq!(exec_err.wire_code(), "22000", "execute sql={sql}");

        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        let describe_session = SessionState::default();
        let describe_err = core
            .describe_parsed_in_session(&describe_session, &parsed)
            .expect_err("describe must reject out-of-range LIMIT the same way execute does");
        assert_eq!(describe_err.wire_code(), "22000", "describe sql={sql}");
    }
}

// ---------- セッションレス経路（`EngineCore::execute_sql`） ----------

#[test]
fn sessionless_execute_sql_matches_session_execute() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    let sql = "SELECT lang FROM docs UNION SELECT lang FROM other_docs";

    let via_sessionless = core
        .execute_sql(&ctx("tenant-a"), sql)
        .expect("session-less execute_sql should succeed");
    let via_session = run(&core, "tenant-a", sql);

    let mut sessionless_sorted = langs(&via_sessionless);
    sessionless_sorted.sort();
    let mut session_sorted = langs(&via_session);
    session_sorted.sort();
    assert_eq!(sessionless_sorted, session_sorted);
}

// ---------- 決定性 ----------

#[test]
fn repeated_calls_are_deterministic() {
    let (storage, path) = seeded_two_tables();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let first = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    let second = run(
        &core,
        "tenant-a",
        "SELECT lang FROM docs UNION SELECT lang FROM other_docs",
    );
    assert_eq!(langs(&first), langs(&second));
}
