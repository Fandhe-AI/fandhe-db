//! ウィンドウ集計の `FILTER (WHERE ...)` 句の結合テスト（SQL-64・Issue #1894。関連: SQL-57・
//! RLS-10）。
//!
//! 期待値は PostgreSQL 17 の意味論（条件が偽・NULL の行はその項目の累積器の入力からだけ除き、
//! パーティション・peer・フレーム位置・出力行には残す）から手で導いた固定値で、PostgreSQL 実機での
//! 照合は未実施。`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証する。
//!
//! フィクスチャ（`sql57_aggregate_filter.rs` と同じ）: id 1..8 の `k = [3, 1, 2, 5, NULL, 1, 4, 9]`、
//! `g = [a, a, a, b, b, c, b, a]`。`k > 2` を満たす行は id 1・4・7・8。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("s", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, s, k)`。
type Seed = (u64, &'static str, Option<&'static str>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some("x"), Some(3)),
    (2, "a", None, Some(1)),
    (3, "a", Some("y"), Some(2)),
    (4, "b", Some("z"), Some(5)),
    (5, "b", Some("w"), None),
    (6, "c", None, Some(1)),
    (7, "b", Some("x"), Some(4)),
    (8, "a", Some("x"), Some(9)),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, s, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            s.map_or(Value::Null, |v| Value::Text(v.to_string())),
            k.map_or(Value::Null, Value::Integer),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない。FILTER 条件に一致する値を持つ）。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some("SECRET"), Some(1000)),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// `id` 昇順に並べた、`id` を除く列の文字列表現（1 行 1 要素。複数列は `,` 区切り）。
fn by_id(core: &EngineCore, sql: &str) -> Vec<String> {
    let r = core
        .execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {}", e.wire_code()));
    let mut rows: Vec<(u64, String)> = r
        .rows
        .iter()
        .map(|row| {
            let id = cell_str(&row.cells[0]).parse::<u64>().expect("numeric id");
            let rest: Vec<String> = row.cells.iter().skip(1).map(cell_str).collect();
            (id, rest.join(","))
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, v)| v).collect()
}

fn win(core: &EngineCore, expr: &str) -> Vec<String> {
    by_id(core, &format!("SELECT id, {expr} FROM t LIMIT 100"))
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl failed: {sql}: {}", e.wire_code()));
}

/// 既定フレーム（`ORDER BY` あり）の累積。FILTER が偽・NULL の行は入力にだけ入らない。
#[test]
fn cumulative_default_frame() {
    let (core, _g) = build_core("sql64-wf-cum");
    assert_eq!(
        win(&core, "count(*) FILTER (WHERE k > 2) OVER (ORDER BY id)"),
        strs(&["1", "1", "1", "2", "2", "2", "3", "4"])
    );
    assert_eq!(
        win(&core, "sum(k) FILTER (WHERE k > 2) OVER (ORDER BY id)"),
        strs(&["3", "3", "3", "8", "8", "8", "12", "21"])
    );
}

/// `PARTITION BY` のみ（フレームはパーティション全体）。
#[test]
fn partition_without_order_by() {
    let (core, _g) = build_core("sql64-wf-part");
    assert_eq!(
        win(&core, "count(*) FILTER (WHERE k > 2) OVER (PARTITION BY g)"),
        strs(&["2", "2", "2", "2", "2", "0", "2", "2"])
    );
}

/// 全行が除外されたら `count` は 0、他は NULL。条件が NULL になる行（id5 の `k`）も除外。
#[test]
fn empty_input_yields_zero_or_null() {
    let (core, _g) = build_core("sql64-wf-empty");
    for f in ["sum", "avg", "min", "max"] {
        assert_eq!(
            win(&core, &format!("{f}(k) FILTER (WHERE k > 100) OVER ()")),
            strs(&["NULL"; 8]),
            "{f}"
        );
    }
    assert_eq!(
        win(&core, "count(k) FILTER (WHERE k > 100) OVER ()"),
        strs(&["0"; 8])
    );
    // NULL 条件の行（id5）は除外される: `k < 100` を満たすのは k が NULL でない 7 行。
    assert_eq!(
        win(&core, "count(*) FILTER (WHERE k < 100) OVER ()"),
        strs(&["7"; 8])
    );
}

/// FILTER で除外された行もフレームの位置を占める（ROWS・EXCLUDE CURRENT ROW・cumulative 経路）。
#[test]
fn rows_frame_and_exclude_keep_frame_positions() {
    let (core, _g) = build_core("sql64-wf-rows");
    assert_eq!(
        win(
            &core,
            "count(*) FILTER (WHERE k > 2) OVER (ORDER BY id \
             ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)"
        ),
        strs(&["1", "1", "1", "1", "1", "1", "2", "2"])
    );
    assert_eq!(
        win(
            &core,
            "count(*) FILTER (WHERE k > 2) OVER (ORDER BY id \
             ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW)"
        ),
        strs(&["0", "1", "1", "0", "1", "1", "1", "1"])
    );
    assert_eq!(
        win(
            &core,
            "count(*) FILTER (WHERE k > 2) OVER (ORDER BY id \
             ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"
        ),
        strs(&["1", "1", "1", "2", "2", "2", "3", "4"])
    );
}

/// RANGE（値オフセット）と GROUPS フレーム。
#[test]
fn range_and_groups_frames() {
    let (core, _g) = build_core("sql64-wf-rg");
    assert_eq!(
        win(
            &core,
            "sum(k) FILTER (WHERE k > 2) OVER (ORDER BY k \
             RANGE BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        strs(&["3", "NULL", "NULL", "9", "NULL", "NULL", "7", "9"])
    );
    assert_eq!(
        win(
            &core,
            "count(*) FILTER (WHERE k > 2) OVER (ORDER BY k \
             GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        strs(&["1", "0", "0", "2", "1", "0", "2", "2"])
    );
}

/// FILTER つき・なしの項目と順位関数が混在しても互いに影響しない。
#[test]
fn mixed_items_are_independent() {
    let (core, _g) = build_core("sql64-wf-mixed");
    assert_eq!(
        win(
            &core,
            "row_number() OVER (ORDER BY id), \
             count(*) FILTER (WHERE k > 2) OVER (ORDER BY id), \
             count(*) OVER (ORDER BY id)"
        ),
        strs(&["1,1,1", "2,1,2", "3,1,3", "4,2,4", "5,2,5", "6,2,6", "7,3,7", "8,4,8"])
    );
}

/// 文の WHERE・文全体の ORDER BY／LIMIT との併用。
#[test]
fn combined_with_where_order_by_and_limit() {
    let (core, _g) = build_core("sql64-wf-comb");
    assert_eq!(
        by_id(
            &core,
            "SELECT id, count(*) FILTER (WHERE k > 2) OVER (ORDER BY id) FROM t \
             WHERE g = 'a' LIMIT 100"
        ),
        strs(&["1", "1", "1", "2"])
    );
    let r = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id, count(*) FILTER (WHERE k > 2) OVER (ORDER BY id) FROM t \
             ORDER BY id DESC LIMIT 3",
        )
        .expect("order by + limit");
    let got: Vec<String> = r
        .rows
        .iter()
        .map(|row| format!("{}:{}", cell_str(&row.cells[0]), cell_str(&row.cells[1])))
        .collect();
    assert_eq!(got, strs(&["8:4", "7:3", "6:2"]));
}

/// RLS: 他テナントの Private 行（FILTER 条件に一致する値）は結果へ影響しない。
#[test]
fn other_tenant_rows_do_not_affect_results() {
    let (core, _g) = build_core("sql64-wf-rls");
    assert_eq!(
        win(&core, "count(*) FILTER (WHERE k > 2) OVER ()"),
        strs(&["4"; 8])
    );
    assert_eq!(
        win(&core, "sum(k) FILTER (WHERE s = 'SECRET') OVER ()"),
        strs(&["NULL"; 8])
    );
}

/// SQLSTATE: 非集計は 0A000、FILTER 内の集計は 42803・ウィンドウは 42P20（0A000 より先）、
/// FILTER 内の部分クエリは対象外（fail-closed の 42601）。
#[test]
fn error_contract() {
    let (core, _g) = build_core("sql64-wf-err");
    for f in [
        "row_number()",
        "rank()",
        "dense_rank()",
        "percent_rank()",
        "cume_dist()",
        "lag(k)",
        "lead(k)",
        "first_value(k)",
        "last_value(k)",
        "nth_value(k, 2)",
        "ntile(2)",
    ] {
        assert_eq!(
            code_of(
                &core,
                &format!("SELECT id, {f} FILTER (WHERE k > 2) OVER (ORDER BY id) FROM t LIMIT 100")
            ),
            "0A000",
            "{f}"
        );
    }
    assert_eq!(
        code_of(
            &core,
            "SELECT id, row_number() FILTER (WHERE count(*) > 1) OVER () FROM t LIMIT 100"
        ),
        "42803"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT id, count(*) FILTER (WHERE row_number() OVER () > 1) OVER () FROM t LIMIT 100"
        ),
        "42P20"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT id, count(*) FILTER (WHERE k IN (SELECT k FROM t LIMIT 2)) OVER () \
             FROM t LIMIT 100"
        ),
        "42601"
    );
}

/// ビュー越し: 非公開列を FILTER で参照すると 42703、公開列の FILTER は動く。
#[test]
fn view_exposes_only_public_columns() {
    let (core, _g) = build_core("sql64-wf-view");
    ddl(&core, "CREATE VIEW pv AS SELECT id, g, k FROM t");
    assert_eq!(
        code_of(
            &core,
            "SELECT id, count(*) FILTER (WHERE s = 'x') OVER () FROM pv LIMIT 100"
        ),
        "42703"
    );
    assert_eq!(
        by_id(
            &core,
            "SELECT id, count(*) FILTER (WHERE k > 2) OVER (PARTITION BY g) FROM pv LIMIT 100"
        ),
        strs(&["2", "2", "2", "2", "2", "0", "2", "2"])
    );
}

/// 評価後射影形ビュー（`LIMIT` 本文）越しでも、FILTER にしか現れない列が NULL へ落ちない。
#[test]
fn filter_column_survives_buffered_view() {
    let (core, _g) = build_core("sql64-wf-buffered");
    ddl(
        &core,
        "CREATE VIEW top4 AS SELECT id, k FROM t ORDER BY id LIMIT 4",
    );
    assert_eq!(
        by_id(
            &core,
            "SELECT id, count(*) FILTER (WHERE k > 2) OVER (ORDER BY id) FROM top4 LIMIT 100"
        ),
        strs(&["1", "1", "1", "2"])
    );
}
