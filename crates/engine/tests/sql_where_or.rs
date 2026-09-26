//! `WHERE` の `OR` 結合・括弧グルーピング（TASK-208・SQL-24、Issue #912）の
//! 結合テスト。`tests/boolean_column.rs`・`tests/sql_surface.rs` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として
//! 検証）。検索 SELECT・広域取得（scan）・集計（`COUNT(*)`）・述語つき
//! `DELETE`／`UPDATE`・RLS 境界の各経路で OR が独立オラクルと一致することを
//! 固定する。索引最適化（`ScalarPlan::IndexDisjunction` 相当）は本 Issue の
//! スコープ外（`sql::scalar_plan` の単体テストで `PlainScan` への縮退を別途
//! 固定済み）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-where-or");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, flag: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, flag) VALUES ({id}, '[0.{id},0.1]', '{lang}', {flag}) \
             USING OPERATION_ID 'seed-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert id={id} should succeed: {e:?}"));
}

fn select_ids_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("scan with predicate={predicate:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn distance_ids_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE {predicate} ORDER BY embedding <=> '[0.5,0.1]' LIMIT 100"
            ),
        )
        .unwrap_or_else(|e| panic!("distance search with predicate={predicate:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn count_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> u64 {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {predicate}"),
        )
        .unwrap_or_else(|e| panic!("count(*) with predicate={predicate:?} should succeed: {e:?}"));
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// 5 行のコーパス: 1=ja/true, 2=en/true, 3=fr/false, 4=en/false, 5=de/NULL。
fn seed_corpus(core: &EngineCore, ctx: &PolicyContext) {
    insert(core, ctx, 1, "ja", "true");
    insert(core, ctx, 2, "en", "true");
    insert(core, ctx, 3, "fr", "false");
    insert(core, ctx, 4, "en", "false");
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (5, '[0.5,0.1]', 'de') \
             USING OPERATION_ID 'seed-5'"
        ),
    )
    .expect("insert id=5 (flag NULL) should succeed");
}

// --- 検索 SELECT（広域取得・distance の両経路） ---------------------------------

#[test]
fn scan_with_simple_or_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    let ids = select_ids_where(&core, &ctx, "lang = 'ja' OR lang = 'fr'");
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn distance_search_with_simple_or_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    let ids = distance_ids_where(&core, &ctx, "lang = 'ja' OR lang = 'fr'");
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![1, 3]);
}

#[test]
fn scan_with_and_of_two_or_groups_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    // (lang = 'en' OR lang = 'ja') AND (flag OR flag IS NULL 相当の flag 未指定)
    // ここでは flag が既知のブール等価述語のみを使い、`(lang='en' OR lang='ja')
    // AND flag` を独立オラクルと比較する: id=1(ja,true) と id=2(en,true) のみ。
    let ids = select_ids_where(&core, &ctx, "(lang = 'en' OR lang = 'ja') AND flag");
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn scan_with_nested_or_inside_and_branch_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    // `lang = 'de' OR (flag = false AND lang = 'fr')`: id=5(de) と id=3(fr,false)。
    let ids = select_ids_where(&core, &ctx, "lang = 'de' OR (flag = false AND lang = 'fr')");
    assert_eq!(ids, vec![3, 5]);
}

#[test]
fn scan_with_or_and_paren_value_expression_mixed_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    // `(id + 1) > 4 OR lang = 'ja'`: id=4 (5>4), id=5 (6>4), id=1 (ja)。
    let ids = select_ids_where(&core, &ctx, "(id + 1) > 4 OR lang = 'ja'");
    assert_eq!(ids, vec![1, 4, 5]);
}

// --- 集計（COUNT(*)） -----------------------------------------------------------

#[test]
fn count_star_with_or_matches_independent_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    assert_eq!(count_where(&core, &ctx, "lang = 'ja' OR lang = 'fr'"), 2);
    assert_eq!(
        count_where(&core, &ctx, "lang = 'zz' OR lang = 'yy'"),
        0,
        "OR of two non-matching branches must not fall back to unfiltered count"
    );
}

// --- 述語つき DELETE／UPDATE ------------------------------------------------------

#[test]
fn predicate_delete_with_or_affects_the_union_of_branches() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' OR lang = 'fr' USING OPERATION_ID 'op-delete-or'"
            ),
        )
        .expect("predicate DELETE with OR should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(
        select_ids_where(&core, &ctx, "lang = 'ja'"),
        Vec::<u64>::new()
    );
    assert_eq!(
        select_ids_where(&core, &ctx, "lang = 'fr'"),
        Vec::<u64>::new()
    );
    // 他の行は無傷。
    assert_eq!(select_ids_where(&core, &ctx, "lang = 'en'"), vec![2, 4]);
}

#[test]
fn predicate_update_with_or_affects_the_union_of_branches() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'xx' WHERE lang = 'ja' OR lang = 'fr' USING OPERATION_ID 'op-update-or'"
            ),
        )
        .expect("predicate UPDATE with OR should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    assert_eq!(count_where(&core, &ctx, "lang = 'xx'"), 2);
    assert_eq!(count_where(&core, &ctx, "lang = 'ja'"), 0);
    assert_eq!(count_where(&core, &ctx, "lang = 'fr'"), 0);
}

#[test]
fn predicate_update_where_clause_with_only_or_predicate_is_not_treated_as_unconditional() {
    // TASK-208・Issue #912 の回帰: `WHERE a OR b` だけの述語（`metadata_filters`・
    // `expr_filters` は両方空、内容は `or_filters` のみ）が「実質無条件 UPDATE」と
    // 誤判定されて拒否されないことを固定する（`sql::parser::bind_update_form` の
    // 空判定バグの回帰）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed_corpus(&core, &ctx);

    core.execute_sql_in_session(
        &ctx,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET lang = 'ja' WHERE lang = 'en' OR lang = 'fr' USING OPERATION_ID 'op-update-or-only'"
        ),
    )
    .expect("UPDATE whose WHERE is entirely an OR group must not be rejected as unconditional");
}

// --- RLS 境界 --------------------------------------------------------------------

#[test]
fn or_predicate_does_not_leak_other_tenants_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("tenant-a");
    let bob = ctx_for("tenant-b");
    seed_corpus(&core, &alice);
    insert(&core, &bob, 100, "ja", "true");

    // tenant-b から見ると、広い OR（あらゆる lang にマッチしうる形）を書いても
    // tenant-a の行は一切見えない。
    let ids = select_ids_where(
        &core,
        &bob,
        "lang = 'ja' OR lang = 'en' OR lang = 'fr' OR lang = 'de'",
    );
    assert_eq!(ids, vec![100]);
}
