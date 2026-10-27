//! 相関参照の検出と外側スコープへの束縛（SQL-61 (1)。`docs/spec/04-behavior/` のポインタのみ。
//! Issue #1841、親 #1840）の結合テスト。実 `Storage`＋`CpuScalarProvider` を
//! `EngineCore::execute_sql` 経由で使い、名前解決のエラーコード（`42702`／`42703`／`42P01`）、
//! 相関の fail-closed（`42601`・文言 `correlated`）、内側優先の従来動作、テナント境界
//! （RLS-10 ポインタ。束縛はスキーマだけを使い行データに依存しない）を固定する。

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

fn docs_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("outer_only", ColumnType::Text, true),
        ],
    )
}

fn refs_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql61-correlated-binding");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&docs_schema("docs"))
        .expect("create docs");
    storage
        .create_table(&refs_schema("refs"))
        .expect("create refs");
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

/// `(wire_code, 文言)`。
fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql(ctx, sql) {
        Ok(_) => panic!("{sql:?} should be rejected"),
        Err(e) => (e.error_class().wire_code().to_string(), e.to_string()),
    }
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let (core, path) = new_core();
    let guard = CleanupGuard(path);
    let ctx = ctx_for("t1");
    seed(&core, &ctx, "docs", 1, "en");
    seed(&core, &ctx, "docs", 2, "ja");
    seed(&core, &ctx, "refs", 1, "en");
    (core, ctx, guard)
}

#[test]
fn sql61_qualified_outer_refs_reach_binding_and_fail_closed_as_correlated() {
    let (core, ctx, _g) = setup();
    for sql in [
        // 肯定形の相関 `EXISTS` は SQL-61（Issue #1842）で実行できる
        // （`tests/sql61_correlated_exists.rs`）。反結合（#1843）も同様。ここでは #1844 の範囲を固定する。
        "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM refs WHERE x.id = 1 LIMIT 10) LIMIT 10",
        "SELECT id FROM docs x WHERE lang NOT IN (SELECT lang FROM refs WHERE x.id = 1 LIMIT 10) LIMIT 10",
        "SELECT id FROM docs x WHERE lang = (SELECT lang FROM refs WHERE x.id = id LIMIT 1) LIMIT 10",
        "SELECT id FROM docs x WHERE lang = (SELECT COUNT(x.id) FROM refs) LIMIT 10",
        "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM refs WHERE id IN (SELECT id FROM refs WHERE lang = x.lang LIMIT 10) LIMIT 10) LIMIT 10",
        "SELECT id FROM docs WHERE lang IN (SELECT lang FROM refs WHERE docs.lang = lang LIMIT 10) LIMIT 10",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "{sql}: {msg}");
        assert!(msg.contains("correlated"), "{sql}: {msg}");
        assert!(!msg.contains('\u{2}'), "{sql}: {msg:?}");
    }
}

#[test]
fn sql61_qualified_outer_refs_in_not_exists_are_executed_as_anti_join() {
    // 否定形の相関 `EXISTS`（Issue #1843）は束縛段を通って反結合として実行される。
    // docs は {1: en, 2: ja}、refs は {1: en} なので ja の docs 2 だけが残る。
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id FROM docs x WHERE NOT EXISTS (SELECT id FROM refs WHERE lang = x.lang LIMIT 1) LIMIT 10",
        "SELECT id FROM docs WHERE NOT EXISTS (SELECT id FROM refs WHERE docs.lang = lang LIMIT 1) LIMIT 10",
    ] {
        assert_eq!(ids(&core, &ctx, sql), vec![2], "{sql}");
    }
}

#[test]
fn sql61_outer_ref_in_order_by_expr_is_correlated() {
    // Cursor Bugbot 指摘: 式 ORDER BY 内の外側参照も
    // 相関として検出され、内側表上の 42703 ではなく 42601 になる。
    // 集約 HAVING・集約 ORDER BY は内側が GROUP BY 集約を受理しない（グループキー項目で
    // 項目数が 2 以上になる）ため SQL 経由では到達不能で、収集は多層防御として置く。
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id FROM docs x WHERE EXISTS (SELECT id FROM refs ORDER BY lower(x.lang) LIMIT 1) LIMIT 10",
        "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM refs ORDER BY lower(x.lang) LIMIT 10) LIMIT 10",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "{sql}: {msg}");
        assert!(msg.contains("correlated"), "{sql}: {msg}");
    }
}

#[test]
fn sql61_unqualified_outer_only_column_is_correlated() {
    let (core, ctx, _g) = setup();
    // 非修飾の外側専用列も相関として束縛される。`outer_only` は全行 NULL で `=` は真にならない
    // ため、反結合（Issue #1843）は全外側行を返す。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs WHERE NOT EXISTS (SELECT id FROM refs WHERE lang = outer_only LIMIT 1) LIMIT 10"
        ),
        vec![1, 2]
    );
    // 未対応の相関 `IN`（#1844）は引き続き 42601。
    let (code, msg) = err(
        &core,
        &ctx,
        "SELECT id FROM docs WHERE lang IN (SELECT lang FROM refs WHERE lang = outer_only LIMIT 10) LIMIT 10",
    );
    assert_eq!(code, "42601");
    assert!(msg.contains("correlated"), "{msg}");
}

#[test]
fn sql61_name_resolution_errors_use_postgres_codes() {
    let (core, ctx, _g) = setup();
    // 外側表で修飾した存在しない列は 42703（旧: 構文段の 42601）。
    let (code, msg) = err(
        &core,
        &ctx,
        "SELECT id FROM docs x WHERE EXISTS (SELECT id FROM refs WHERE lang = x.zz LIMIT 1) LIMIT 10",
    );
    assert_eq!(code, "42703", "{msg}");
    assert!(msg.contains("zz") && !msg.contains('\u{2}'), "{msg:?}");
    // どこにも無い非修飾名は 42703。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM docs WHERE EXISTS (SELECT id FROM refs WHERE lang = nowhere LIMIT 1) LIMIT 10",
    );
    assert_eq!(code, "42703");
    // 未定義の修飾子は 42P01。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM docs x WHERE EXISTS (SELECT id FROM refs WHERE zzz.lang = lang LIMIT 1) LIMIT 10",
    );
    assert_eq!(code, "42P01");
}

#[test]
fn sql61_inner_scope_wins_and_output_alias_is_not_correlated() {
    let (core, ctx, _g) = setup();
    // 内外で同名の列（`lang`）・同名の公開名は内側が優先され、従来どおり受理・実行される。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs WHERE EXISTS (SELECT id FROM refs WHERE lang = 'en' LIMIT 1) LIMIT 10"
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM refs x WHERE x.lang = 'en' LIMIT 10) LIMIT 10"
        ),
        vec![1]
    );
    // 内側の ORDER BY の出力別名は、外側の同名列（outer_only）へ束縛されない。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM docs WHERE lang IN (SELECT lang AS outer_only FROM refs ORDER BY outer_only LIMIT 10) LIMIT 10"
        ),
        vec![1]
    );
}

#[test]
fn sql61_errors_do_not_depend_on_other_tenant_rows() {
    let (core, path) = new_core();
    let _g = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    seed(&core, &a, "docs", 1, "en");
    seed(&core, &a, "refs", 1, "en");
    seed(&core, &b, "docs", 7, "fr");
    seed(&core, &b, "refs", 8, "de");
    seed(&core, &b, "refs", 9, "it");
    for sql in [
        "SELECT id FROM docs x WHERE lang IN (SELECT lang FROM refs WHERE lang = x.lang LIMIT 10) LIMIT 10",
        "SELECT id FROM docs x WHERE EXISTS (SELECT id FROM refs WHERE lang = x.zz LIMIT 1) LIMIT 10",
        "SELECT id FROM docs x WHERE EXISTS (SELECT id FROM refs WHERE zzz.lang = lang LIMIT 1) LIMIT 10",
    ] {
        assert_eq!(err(&core, &a, sql), err(&core, &b, sql), "{sql}");
    }
}
