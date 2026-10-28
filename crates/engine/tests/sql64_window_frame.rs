//! ウィンドウのフレーム句（`ROWS`／`RANGE`／`GROUPS`）の構文受理（SQL-64 (2)・Issue #1887）の
//! 結合テスト。
//!
//! 評価は兄弟 Issue の担当で、本 Issue（#1887）では正しいフレームが構文・束縛段を通ること、
//! 不正なフレームが PostgreSQL 17 と同じ SQLSTATE（`42601`／`42P20`／`42P10`／`42803`／`42804`）で
//! 拒否されることを検証する。`ROWS` は #1889 で評価まで通り（値は `sql_t40_up/sql64_window_rows_frame.rs`）、
//! `RANGE` は #1890 で評価まで通り（値は `sql_t40_up/sql64_window_range_frame.rs`）、`GROUPS` も
//! #1891 で評価まで通る（値は `sql_t40_up/sql64_window_groups_frame.rs`）。足場は
//! `tests/sql64_lag_lead.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API のみ）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::Integer, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str, rows: u64) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=rows {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("frame-op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Text(if id % 2 == 0 { "ja" } else { "en" }.to_string()),
                Value::Integer(id as i32 * 10),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

/// 成功なら `"OK"`、失敗なら SQLSTATE。
fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => "OK",
        Err(e) => e.wire_code(),
    }
}

fn check(core: &EngineCore, ctx: &PolicyContext, cases: &[(&str, &str)]) {
    for (over, want) in cases {
        let sql = format!("SELECT id, sum(score) OVER ({over}) FROM docs LIMIT 10");
        assert_eq!(outcome(core, ctx, &sql), *want, "{sql}");
    }
}

#[test]
fn accepted_frames_reach_evaluation() {
    for rows in [0u64, 3] {
        let (core, ctx, _g) = setup(&format!("frame-gate-{rows}"), rows);
        let cases = [
            "ORDER BY score ROWS UNBOUNDED PRECEDING",
            "ORDER BY score ROWS 1 PRECEDING",
            "ORDER BY score ROWS CURRENT ROW",
            "ORDER BY score rows between unbounded preceding and unbounded following",
            "PARTITION BY lang ORDER BY score DESC NULLS FIRST ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
            "ORDER BY score ROWS BETWEEN CURRENT ROW AND 2 FOLLOWING",
            "ORDER BY score ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING",
            "ORDER BY score ROWS BETWEEN 1 PRECEDING AND 3 PRECEDING",
            "ORDER BY score RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
            "ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING",
            "RANGE UNBOUNDED PRECEDING",
            "ORDER BY score RANGE BETWEEN 1 PRECEDING AND NULL FOLLOWING",
            "ORDER BY score GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW",
            "ORDER BY score GROUPS NULL PRECEDING",
            "ORDER BY score GROUPS -1 PRECEDING",
            "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW",
            "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE GROUP",
            "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE TIES",
            "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE NO OTHERS",
            "ORDER BY score ROWS NULL PRECEDING",
            "ORDER BY score ROWS -1 PRECEDING",
            "ORDER BY score ROWS 1 + 1 PRECEDING",
            "ORDER BY score ROWS (2) PRECEDING",
        ];
        for c in cases {
            let sql = format!("SELECT id, sum(score) OVER ({c}) FROM docs LIMIT 10");
            // `ROWS`・`RANGE`・`GROUPS` は評価まで通る（NULL のオフセットは行数に依らず 22004。
            // `ROWS`／`GROUPS` の負は 22013）。
            let want = if c.contains("NULL PRECEDING") || c.contains("NULL FOLLOWING") {
                "22004"
            } else if c.contains("-1 PRECEDING") {
                "22013"
            } else {
                "OK"
            };
            assert_eq!(outcome(&core, &ctx, &sql), want, "{sql}");
        }
        for f in [
            "row_number()",
            "rank()",
            "count(*)",
            "lag(score)",
            "dense_rank()",
        ] {
            let sql =
                format!("SELECT id, {f} OVER (ORDER BY score ROWS 1 PRECEDING) FROM docs LIMIT 10");
            assert_eq!(outcome(&core, &ctx, &sql), "OK", "{sql}");
            let sql = format!(
                "SELECT id, {f} OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM docs LIMIT 10"
            );
            assert_eq!(outcome(&core, &ctx, &sql), "OK", "{sql}");
            let sql = format!(
                "SELECT id, {f} OVER (ORDER BY score GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM docs LIMIT 10"
            );
            assert_eq!(outcome(&core, &ctx, &sql), "OK", "{sql}");
        }
    }
}

#[test]
fn other_tenant_sees_same_outcome() {
    let (core, _ctx, _g) = setup("frame-gate-tenant", 3);
    let other = ctx_for("tenant-b");
    let sql = "SELECT id, sum(score) OVER (ORDER BY score GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM docs LIMIT 10";
    assert_eq!(outcome(&core, &other, sql), "OK");
    let range = "SELECT id, sum(score) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM docs LIMIT 10";
    assert_eq!(outcome(&core, &other, range), "OK");
    let rows = "SELECT id, sum(score) OVER (ORDER BY score ROWS 1 PRECEDING) FROM docs LIMIT 10";
    assert_eq!(outcome(&core, &other, rows), "OK");
}

#[test]
fn malformed_frames_are_42601() {
    let (core, ctx, _g) = setup("frame-syntax", 2);
    check(
        &core,
        &ctx,
        &[
            ("ORDER BY score ROWS", "42601"),
            ("ORDER BY score ROWS UNBOUNDED", "42601"),
            (
                "ORDER BY score ROWS BETWEEN UNBOUNDED PRECEDING CURRENT ROW",
                "42601",
            ),
            ("ORDER BY score ROWS BETWEEN 1 PRECEDING AND", "42601"),
            ("ORDER BY score ROWS CURRENT", "42601"),
            ("ORDER BY score ROWS 1", "42601"),
            ("ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE", "42601"),
            (
                "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE NO",
                "42601",
            ),
            (
                "ORDER BY score ROWS UNBOUNDED PRECEDING EXCLUDE FOO",
                "42601",
            ),
            ("ORDER BY score ROWS UNBOUNDED PRECEDING extra", "42601"),
            ("ROWS UNBOUNDED PRECEDING ORDER BY score", "42601"),
            ("ORDER BY score ROWS $1 PRECEDING", "42601"),
            ("ORDER BY score FOO UNBOUNDED PRECEDING", "42601"),
        ],
    );
}

#[test]
fn bound_combinations_are_42p20() {
    let (core, ctx, _g) = setup("frame-combo", 2);
    check(
        &core,
        &ctx,
        &[
            ("ORDER BY score ROWS UNBOUNDED FOLLOWING", "42P20"),
            ("ORDER BY score ROWS 1 FOLLOWING", "42P20"),
            (
                "ORDER BY score ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW",
                "42P20",
            ),
            (
                "ORDER BY score ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING",
                "42P20",
            ),
            (
                "ORDER BY score ROWS BETWEEN CURRENT ROW AND 1 PRECEDING",
                "42P20",
            ),
            (
                "ORDER BY score ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING",
                "42P20",
            ),
            (
                "ORDER BY score ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW",
                "42P20",
            ),
        ],
    );
}

#[test]
fn analysis_stage_checks() {
    let (core, ctx, _g) = setup("frame-analysis", 2);
    check(
        &core,
        &ctx,
        &[
            ("RANGE 1 PRECEDING", "42P20"),
            ("ORDER BY score, lang RANGE 1 PRECEDING", "42P20"),
            ("RANGE UNBOUNDED PRECEDING", "OK"),
            // `RANGE` のオフセット型はキー型と照合する（PostgreSQL 17 の in_range 照合。不一致は 0A000）。
            ("ORDER BY score RANGE 1.5 PRECEDING", "0A000"),
            ("ORDER BY score RANGE INTERVAL '1 day' PRECEDING", "0A000"),
            ("ORDER BY score RANGE 1::float8 PRECEDING", "0A000"),
            ("ORDER BY lang RANGE 1 PRECEDING", "0A000"),
            ("ORDER BY lang RANGE NULL PRECEDING", "0A000"),
            ("ORDER BY score RANGE '1' PRECEDING", "OK"),
            ("ORDER BY score RANGE 'abc' PRECEDING", "22P02"),
            ("ORDER BY score RANGE 1::bigint PRECEDING", "OK"),
            ("GROUPS CURRENT ROW", "42P20"),
            ("ORDER BY score GROUPS CURRENT ROW", "OK"),
            ("ORDER BY score ROWS score PRECEDING", "42P10"),
            ("ORDER BY score ROWS nope PRECEDING", "42P10"),
            ("ORDER BY score RANGE nope PRECEDING", "42P10"),
            ("ORDER BY score ROWS sum(score) PRECEDING", "42803"),
            ("ORDER BY score RANGE sum(score) PRECEDING", "42803"),
            ("ORDER BY score GROUPS sum(score) PRECEDING", "42803"),
            (
                "ORDER BY score ROWS row_number() OVER () PRECEDING",
                "42P20",
            ),
            ("ORDER BY score ROWS 1.5 PRECEDING", "42804"),
            ("ORDER BY score GROUPS 'a' PRECEDING", "42804"),
        ],
    );
}

#[test]
fn existing_prechecks_are_unaffected() {
    let (core, ctx, _g) = setup("frame-precheck", 2);
    let sql =
        "SELECT count(*) FILTER (WHERE sum(score) OVER (ROWS UNBOUNDED PRECEDING) > 1) FROM docs";
    assert_eq!(outcome(&core, &ctx, sql), "42P20");
}

#[test]
fn cte_and_derived_bodies_reach_evaluation() {
    let (core, ctx, _g) = setup("frame-cte", 2);
    let cte = "WITH c AS (SELECT id, sum(score) OVER (ORDER BY score ROWS 1 PRECEDING) AS s FROM docs) SELECT * FROM c";
    assert_eq!(outcome(&core, &ctx, cte), "OK");
    let derived = "SELECT * FROM (SELECT id, sum(score) OVER (ORDER BY score ROWS 1 PRECEDING) AS s FROM docs) t";
    assert_eq!(outcome(&core, &ctx, derived), "OK");
    let cte = "WITH c AS (SELECT id, sum(score) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS s FROM docs) SELECT * FROM c";
    assert_eq!(outcome(&core, &ctx, cte), "OK");
    let derived = "SELECT * FROM (SELECT id, sum(score) OVER (ORDER BY score RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS s FROM docs) t";
    assert_eq!(outcome(&core, &ctx, derived), "OK");
    let cte = "WITH c AS (SELECT id, sum(score) OVER (ORDER BY score GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS s FROM docs) SELECT * FROM c";
    assert_eq!(outcome(&core, &ctx, cte), "OK");
    let derived = "SELECT * FROM (SELECT id, sum(score) OVER (ORDER BY score GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS s FROM docs) t";
    assert_eq!(outcome(&core, &ctx, derived), "OK");
}
