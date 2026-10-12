//! 修飾ワイルドカード `t.*` の受理と、修飾 `id` 疑似列の非修飾との等価性
//! （SQL-44。`docs/spec/04-behavior/` のポインタのみ。Issue #1536、親 #1533）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、`SELECT *` との
//! 列メタ・行の一致、不一致修飾子の `42P01`、位置・対象外形の `42601`、テナント境界を固定する。

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
    let path = unique_db_path("sql44-qualified-star");
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
fn sql44_qualified_star_equals_plain_star() {
    let (core, ctx, _g) = setup();
    for q in [
        "SELECT x.* FROM docs x LIMIT 5",
        "SELECT x.* FROM docs AS x LIMIT 5",
        "SELECT docs.* FROM docs LIMIT 5",
    ] {
        assert_same(&core, &ctx, q, "SELECT * FROM docs LIMIT 5");
    }
    let r = core
        .execute_sql(&ctx, "SELECT x.* FROM docs x LIMIT 5")
        .expect("ok");
    assert_eq!(r.rows.len(), 2);
    let cols = format!("{:?}", r.columns);
    assert!(cols.starts_with("[Id,"), "{cols}");
    assert_same(
        &core,
        &ctx,
        "SELECT x.* FROM docs x WHERE x.lang = 'en' ORDER BY x.id LIMIT 5",
        "SELECT * FROM docs WHERE lang = 'en' ORDER BY id LIMIT 5",
    );
    assert_eq!(
        code(&core, &ctx, "SELECT DISTINCT x.* FROM docs x"),
        code(&core, &ctx, "SELECT DISTINCT * FROM docs")
    );
}

#[test]
fn sql44_qualified_star_mismatch_is_42p01_without_existence_oracle() {
    let (core, ctx, _g) = setup();
    assert_eq!(code(&core, &ctx, "SELECT zzz.* FROM docs LIMIT 5"), "42P01");
    assert_eq!(
        code(&core, &ctx, "SELECT docs.* FROM docs AS x LIMIT 5"),
        "42P01"
    );
    assert_eq!(
        code(&core, &ctx, "SELECT langs.* FROM docs LIMIT 5"),
        "42P01"
    );
}

#[test]
fn sql44_qualified_star_in_explain_and_cte() {
    let (core, ctx, _g) = setup();
    let run = |sql: &str| {
        core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .map(|r| format!("{r:?}"))
            .map_err(|e| e.error_class().wire_code().to_string())
    };
    assert_eq!(
        run("EXPLAIN SELECT x.* FROM docs x LIMIT 5"),
        run("EXPLAIN SELECT * FROM docs LIMIT 5")
    );
    assert_same(
        &core,
        &ctx,
        "WITH c AS (SELECT id, lang FROM docs) SELECT c.* FROM c LIMIT 5",
        "WITH c AS (SELECT id, lang FROM docs) SELECT * FROM c LIMIT 5",
    );
}

#[test]
fn sql44_qualified_star_rejected_positions_and_unsupported_forms() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT x.*, x.lang FROM docs x"),
        code(&core, &ctx, "SELECT *, lang FROM docs")
    );
    for sql in [
        "SELECT x.id FROM docs x WHERE x.* = 1",
        "SELECT x.id FROM docs x ORDER BY x.*",
        "SELECT COUNT(x.*) FROM docs x",
        "SELECT x.* FROM docs x UNION SELECT * FROM docs",
        "SELECT x.* FROM docs x JOIN langs y ON x.id = y.id LIMIT 5",
        "UPDATE docs SET lang = docs.* WHERE id = 1",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn sql44_qualified_id_pseudo_column_matches_plain() {
    let (core, ctx, _g) = setup();
    assert_same(
        &core,
        &ctx,
        "SELECT x.id FROM docs x LIMIT 5",
        "SELECT id FROM docs LIMIT 5",
    );
    // Issue #1540（SQL-42）: 疑似列 `id` への AS 別名は修飾の有無で同じ結果になる。
    assert_same(
        &core,
        &ctx,
        "SELECT x.id AS i FROM docs x LIMIT 5",
        "SELECT id AS i FROM docs LIMIT 5",
    );
}

#[test]
fn sql44_qualified_star_respects_tenant_boundary() {
    let (core, ctx, _g) = setup();
    let other = ctx_for("t2");
    let r = core
        .execute_sql(&other, "SELECT x.* FROM docs x LIMIT 10")
        .expect("ok");
    assert!(r.rows.is_empty());
    assert_eq!(
        ids(&core, &ctx, "SELECT x.* FROM docs x LIMIT 10"),
        vec![1, 2]
    );
}
