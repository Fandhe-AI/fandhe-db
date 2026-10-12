//! 単一テーブル `FROM <table> [[AS] alias]` の別名受理と、公開名に一致しない修飾子の
//! `42P01` 拒否（SQL-44。`docs/spec/04-behavior/` のポインタのみ。Issue #1534、親 #1533）の
//! 結合テスト。実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、
//! 通常 SELECT・集計・DISTINCT・ORDER BY・サブクエリ・テナント境界を固定する。
//! 修飾列の受理（`t.col`。Issue #1535）は投影・WHERE・ORDER BY・GROUP BY・HAVING で
//! 非修飾の同義文と同一結果になることを固定する。`t.*` は `sql44_qualified_star.rs`（Issue #1536）で固定する。

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

/// Issue #1535: 一致する修飾列は非修飾の同義文と列メタ・行が完全に一致する。
fn assert_same(core: &EngineCore, ctx: &PolicyContext, qualified: &str, plain: &str) {
    let q = core
        .execute_sql(ctx, qualified)
        .unwrap_or_else(|e| panic!("{qualified:?} should succeed: {e:?}"));
    let p = core
        .execute_sql(ctx, plain)
        .unwrap_or_else(|e| panic!("{plain:?} should succeed: {e:?}"));
    assert_eq!(
        format!("{:?}", q.columns),
        format!("{:?}", p.columns),
        "{qualified}"
    );
    assert_eq!(
        format!("{:?}", q.rows),
        format!("{:?}", p.rows),
        "{qualified}"
    );
}

#[test]
fn sql44_qualified_columns_are_accepted_in_every_position() {
    let (core, ctx, _g) = setup();
    let cases = [
        (
            "SELECT x.id, x.lang FROM docs AS x LIMIT 10",
            "SELECT id, lang FROM docs LIMIT 10",
        ),
        (
            "SELECT docs.id FROM docs WHERE docs.lang = 'en' LIMIT 10",
            "SELECT id FROM docs WHERE lang = 'en' LIMIT 10",
        ),
        (
            "SELECT x.id FROM docs x WHERE (x.lang = 'en' OR x.lang = 'ja') AND x.id IN (1, 2) LIMIT 10",
            "SELECT id FROM docs WHERE (lang = 'en' OR lang = 'ja') AND id IN (1, 2) LIMIT 10",
        ),
        (
            "SELECT x.id FROM docs x WHERE x.lang IS NOT NULL AND x.id BETWEEN 1 AND 2 LIMIT 10",
            "SELECT id FROM docs WHERE lang IS NOT NULL AND id BETWEEN 1 AND 2 LIMIT 10",
        ),
        (
            "SELECT x.id FROM docs x WHERE x.lang LIKE 'e%' LIMIT 10",
            "SELECT id FROM docs WHERE lang LIKE 'e%' LIMIT 10",
        ),
        (
            "SELECT x.id FROM docs x ORDER BY x.embedding <=> '[0.5,0.1]' LIMIT 10",
            "SELECT id FROM docs ORDER BY embedding <=> '[0.5,0.1]' LIMIT 10",
        ),
        (
            "SELECT x.id FROM docs x ORDER BY x.id LIMIT 10",
            "SELECT id FROM docs ORDER BY id LIMIT 10",
        ),
        (
            "SELECT x.lang, COUNT(*) FROM docs x GROUP BY x.lang",
            "SELECT lang, COUNT(*) FROM docs GROUP BY lang",
        ),
        (
            "SELECT x.lang, COUNT(*) AS n FROM docs x GROUP BY x.lang HAVING n >= 1",
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n >= 1",
        ),
        (
            "SELECT DISTINCT x.lang FROM docs x",
            "SELECT DISTINCT lang FROM docs",
        ),
        (
            "SELECT COUNT(x.lang) FROM docs x",
            "SELECT COUNT(lang) FROM docs",
        ),
        (
            "SELECT x.id FROM docs x WHERE x.lang IN (SELECT y.lang FROM langs y LIMIT 10) LIMIT 10",
            "SELECT id FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) LIMIT 10",
        ),
        (
            "WITH c AS (SELECT id, lang FROM docs WHERE lang = 'en') SELECT z.id FROM c z WHERE z.id = 1 LIMIT 10",
            "WITH c AS (SELECT id, lang FROM docs WHERE lang = 'en') SELECT id FROM c WHERE id = 1 LIMIT 10",
        ),
    ];
    for (q, p) in cases {
        assert_same(&core, &ctx, q, p);
    }
    core.execute_sql_in_session(
        &ctx,
        &mut SessionState::default(),
        "EXPLAIN SELECT x.id FROM docs x WHERE x.lang = 'en' LIMIT 5",
    )
    .expect("explain with qualified column");
}

#[test]
fn sql44_unmatched_qualifier_in_any_position_is_undefined_table() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT docs.id FROM docs AS x LIMIT 10",
        "SELECT zzz.id FROM docs LIMIT 10",
        "SELECT id FROM docs x ORDER BY langs.id LIMIT 10",
        "SELECT lang, COUNT(*) FROM docs x GROUP BY zzz.lang",
        "SELECT DISTINCT docs.lang FROM docs x",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42P01", "{sql}");
    }
}

#[test]
fn sql44_correlated_outer_qualifier_stays_unsupported() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id FROM docs WHERE lang IN (SELECT lang FROM langs WHERE docs.id = 1 LIMIT 10) LIMIT 10",
        "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM langs WHERE x.lang = lang LIMIT 10) LIMIT 10",
        "SELECT id FROM docs x WHERE lang IN (SELECT x.lang FROM langs LIMIT 10) LIMIT 10",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
    // 外側を別名にした後で元の表名を内側から使うのは未定義の表。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM langs WHERE zzz.id = 1 LIMIT 10) LIMIT 10"
        ),
        "42P01"
    );
}

#[test]
fn sql44_qualified_columns_out_of_scope_forms_stay_unsupported() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT x.id FROM docs x UNION SELECT id FROM docs",
        "SELECT x.lower(lang) FROM docs x LIMIT 10",
        "SELECT id AS x.y FROM docs x LIMIT 10",
        "SELECT x.id FROM s.docs LIMIT 10",
        "DELETE FROM docs WHERE docs.id = 1",
        "UPDATE docs SET lang = 'x' WHERE docs.lang = 'en'",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn sql44_qualified_hidden_column_matches_unqualified_error() {
    let (core, ctx, _g) = setup();
    let plain = code(&core, &ctx, "SELECT tenant_id FROM docs LIMIT 10");
    assert_eq!(
        code(&core, &ctx, "SELECT x.tenant_id FROM docs x LIMIT 10"),
        plain
    );
}

#[test]
fn sql44_qualified_columns_do_not_cross_tenant_boundary() {
    let (core, ctx, _g) = setup();
    let other = ctx_for("t2");
    assert!(ids(
        &core,
        &other,
        "SELECT x.id FROM docs x WHERE x.lang = 'en' LIMIT 10"
    )
    .is_empty());
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT x.id FROM docs x WHERE x.lang = 'en' LIMIT 10"
        ),
        vec![1]
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
