//! 単一テーブル `FROM <table> [[AS] alias]` の別名受理と、公開名に一致しない修飾子の
//! `42P01` 拒否（SQL-44。`docs/spec/04-behavior/` のポインタのみ。Issue #1534、親 #1533）の
//! 結合テスト。実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、
//! 通常 SELECT・集計・DISTINCT・ORDER BY・サブクエリ・テナント境界を固定する。
//! 修飾列の受理（`t.col`）自体は #1535、`t.*` は #1536 の担当で、ここでは一致修飾子が
//! 従来どおり `42601` のままであることだけを回帰固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ClassifiedError;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql44-from-alias");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create docs");
    storage
        .create_table(&schema("langs"))
        .expect("create langs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, lang: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {table} (id, embedding, lang) VALUES ({id}, '[0.{id},0.1]', '{lang}') \
             USING OPERATION_ID 'seed-{table}-{id}'"
        ),
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let r = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match core.execute_sql(ctx, sql) {
        Ok(_) => panic!("{sql:?} should be rejected"),
        Err(e) => e.error_class().wire_code().to_string(),
    }
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let (core, path) = new_core();
    let guard = CleanupGuard(path);
    let ctx = ctx_for("t1");
    seed(&core, &ctx, "docs", 1, "en");
    seed(&core, &ctx, "docs", 2, "ja");
    seed(&core, &ctx, "langs", 1, "en");
    (core, ctx, guard)
}

#[test]
fn sql44_alias_forms_return_same_rows_as_unaliased() {
    let (core, ctx, _g) = setup();
    let base = ids(&core, &ctx, "SELECT id FROM docs LIMIT 10");
    assert_eq!(base, vec![1, 2]);
    assert_eq!(ids(&core, &ctx, "SELECT id FROM docs AS x LIMIT 10"), base);
    assert_eq!(ids(&core, &ctx, "SELECT id FROM docs x LIMIT 10"), base);
}

#[test]
fn sql44_alias_with_where_order_by_aggregate_distinct() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs AS x WHERE lang = 'en' LIMIT 10"
        ),
        vec![1]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs d WHERE lang = 'en' ORDER BY embedding <=> '[0.5,0.1]' LIMIT 10"
        ),
        vec![1]
    );
    core.execute_sql(&ctx, "SELECT COUNT(*) FROM docs AS x")
        .expect("aggregate with alias");
    core.execute_sql(&ctx, "SELECT lang, COUNT(*) FROM docs x GROUP BY lang")
        .expect("group by with alias");
    core.execute_sql(&ctx, "SELECT DISTINCT lang FROM docs AS x")
        .expect("distinct with alias");
    core.execute_sql_in_session(
        &ctx,
        &mut SessionState::default(),
        "EXPLAIN SELECT id FROM docs AS x LIMIT 5",
    )
    .expect("explain with alias");
}

#[test]
fn sql44_alias_inside_subquery() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs AS x WHERE lang IN (SELECT lang FROM langs AS y LIMIT 10) LIMIT 10"
        ),
        vec![1]
    );
}

#[test]
fn sql44_original_table_name_is_undefined_after_alias() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM docs AS x WHERE docs.id = 1 LIMIT 10"
        ),
        "42P01"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM docs x WHERE docs.lang = 'en' LIMIT 10"
        ),
        "42P01"
    );
}

#[test]
fn sql44_unknown_qualifier_is_undefined_table_without_oracle() {
    let (core, ctx, _g) = setup();
    // 実在する別テーブル名でも、存在しない名前でも同じ 42P01（存在オラクルにしない）。
    for q in ["langs", "zzz"] {
        let sql = format!("SELECT id FROM docs AS x WHERE {q}.id = 1 LIMIT 10");
        assert_eq!(code(&core, &ctx, &sql), "42P01");
    }
    assert_eq!(
        code(&core, &ctx, "SELECT id FROM docs WHERE zzz.id = 1 LIMIT 10"),
        "42P01"
    );
}

#[test]
fn sql44_matching_qualifier_stays_unsupported_until_followup() {
    let (core, ctx, _g) = setup();
    // 修飾列の受理は #1535 の担当。一致修飾子は従来どおり 42601。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM docs WHERE docs.id = 1 LIMIT 10"
        ),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM docs AS x WHERE x.id = 1 LIMIT 10"
        ),
        "42601"
    );
}

#[test]
fn sql44_reserved_or_invalid_alias_is_rejected() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id FROM docs AS where LIMIT 10",
        "SELECT id FROM docs AS limit LIMIT 10",
        "SELECT id FROM docs AS union LIMIT 10",
        "SELECT id FROM docs AS 1x LIMIT 10",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

/// 集合演算の枝（集計・LIMIT 付き枝を含む）では FROM 別名を受理せず `42601`。
#[test]
fn sql44_alias_is_rejected_inside_set_operation_branches() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT COUNT(*) FROM docs AS x UNION SELECT COUNT(*) FROM docs",
        "SELECT COUNT(*) FROM docs UNION SELECT COUNT(*) FROM docs x",
        "(SELECT id FROM docs AS x LIMIT 1) UNION (SELECT id FROM docs LIMIT 1)",
        "SELECT id FROM docs AS x UNION SELECT id FROM docs",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn sql44_following_clauses_are_not_taken_as_alias() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        ids(&core, &ctx, "SELECT id FROM docs AS x LIMIT 1 OFFSET 1").len(),
        1
    );
    core.execute_sql(&ctx, "SELECT id FROM docs UNION SELECT id FROM docs")
        .expect("set operation keeps working without alias confusion");
}

#[test]
fn sql44_alias_does_not_cross_tenant_boundary() {
    let (core, ctx, _g) = setup();
    let other = ctx_for("t2");
    assert!(ids(&core, &other, "SELECT id FROM docs AS x LIMIT 10").is_empty());
    assert_eq!(
        ids(&core, &ctx, "SELECT id FROM docs AS x LIMIT 10"),
        vec![1, 2]
    );
}
