//! ウィンドウ関数 `percent_rank`／`cume_dist`（SQL-64・Issue #1885、母集合の境界は RLS-10 の
//! ポインタ）の結合テスト。
//!
//! 足場は `sql64_ntile.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17 の `window_percent_rank`／
//! `window_cume_dist` の定義式（`(rank-1)/(n-1)`、`peer 末尾までの行数/n`）から手で導いた値で、
//! PostgreSQL への実測は未実施。テキスト表現は float8 出力形式（`format_double`）。
//!
//! フィクスチャは `s = [1:10, 2:20, 3:20, 4:NULL, 5:50]`、`x = [b, a, c, d, e]`、
//! `g = [a, a, b, b, b]`（`sql64_value_functions.rs` と同じ）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// (id, s, x, g)。`s` は NULL を含み、`s = 20` の 2 行が peer になる。
type Row = (u64, Option<i32>, &'static str, &'static str);

const ROWS: [Row; 5] = [
    (1, Some(10), "b", "a"),
    (2, Some(20), "a", "a"),
    (3, Some(20), "c", "b"),
    (4, None, "d", "b"),
    (5, Some(50), "e", "b"),
];

fn insert(storage: &Storage, ctx: &PolicyContext, vis: Visibility, row: Row) {
    let (id, s, x, g) = row;
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "pctrank-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        "t",
        ctx,
        id,
        vis,
        &[
            s.map_or(Value::Null, Value::Integer),
            Value::Text(x.to_string()),
            Value::Text(g.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("s", ColumnType::Integer, true),
            ColumnDef::new("x", ColumnType::Text, false),
            ColumnDef::new("g", ColumnType::Text, false),
        ],
    )
}

fn setup(label: &str, populate: bool) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage.create_table(&schema()).expect("create table");
    if populate {
        for row in ROWS {
            insert(&storage, &ctx, Visibility::Public, row);
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => "OK",
        Err(e) => e.wire_code(),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Text(v) => v.clone(),
        Cell::Float(v) => engine::scalar_float::format_double(*v),
        other => format!("{other:?}"),
    }
}

/// 2 列目（`id` の次）を `id` 昇順の文字列で返す。
fn col(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql}: {}", e.wire_code()));
    let mut rows: Vec<(u64, String)> = result
        .rows
        .iter()
        .map(|r| {
            let id = match &r.cells[0] {
                Cell::Integer(v) => *v,
                other => panic!("expected id, got {other:?}"),
            };
            (id, show(&r.cells[1]))
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, v)| v).collect()
}

fn win(core: &EngineCore, ctx: &PolicyContext, func: &str, over: &str) -> Vec<String> {
    col(
        core,
        ctx,
        &format!("SELECT id, {func} OVER ({over}) FROM t LIMIT 100"),
    )
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// `ORDER BY s`（NULL は末尾）。rank = 1,2,2,4,5（id 1,2,3,5,4 の順）。
#[test]
fn ordering_with_peers_and_nulls() {
    let (core, ctx, _g) = setup("pr-order", true);
    assert_eq!(
        win(&core, &ctx, "percent_rank()", "ORDER BY s"),
        strs(&["0", "0.25", "0.25", "1", "0.75"])
    );
    assert_eq!(
        win(&core, &ctx, "cume_dist()", "ORDER BY s"),
        strs(&["0.2", "0.6", "0.6", "1", "0.8"])
    );
    // DESC は NULL が先頭。
    assert_eq!(
        win(&core, &ctx, "percent_rank()", "ORDER BY s DESC"),
        strs(&["1", "0.5", "0.5", "0", "0.25"])
    );
    assert_eq!(
        win(&core, &ctx, "cume_dist()", "ORDER BY s DESC"),
        strs(&["1", "0.8", "0.8", "0.2", "0.4"])
    );
}

/// パーティションごとに分母が変わる。1/3・2/3 は float8 の最短表現で出力される。
#[test]
fn partitions_and_float8_text() {
    let (core, ctx, _g) = setup("pr-part", true);
    assert_eq!(
        win(&core, &ctx, "percent_rank()", "PARTITION BY g ORDER BY s"),
        strs(&["0", "1", "0", "1", "0.5"])
    );
    assert_eq!(
        win(&core, &ctx, "cume_dist()", "PARTITION BY g ORDER BY s"),
        strs(&["0.5", "1", "0.3333333333333333", "1", "0.6666666666666666"])
    );
}

/// `ORDER BY` なしは全行が peer。1 行パーティションは 0 除算せず percent_rank = 0。
#[test]
fn all_peers_and_single_row_partition() {
    let (core, ctx, _g) = setup("pr-peers", true);
    assert_eq!(
        win(&core, &ctx, "percent_rank()", ""),
        strs(&["0", "0", "0", "0", "0"])
    );
    assert_eq!(
        win(&core, &ctx, "cume_dist()", ""),
        strs(&["1", "1", "1", "1", "1"])
    );
    assert_eq!(
        win(&core, &ctx, "percent_rank()", "PARTITION BY id ORDER BY s"),
        strs(&["0", "0", "0", "0", "0"])
    );
    assert_eq!(
        win(&core, &ctx, "cume_dist()", "PARTITION BY id ORDER BY s"),
        strs(&["1", "1", "1", "1", "1"])
    );
}

/// 空テーブルは 0 行でエラーにならない。
#[test]
fn empty_table() {
    let (core, ctx, _g) = setup("pr-empty", false);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, percent_rank() OVER (ORDER BY s), cume_dist() OVER (ORDER BY s) \
             FROM t LIMIT 100",
        )
        .expect("empty select");
    assert!(result.rows.is_empty());
}

/// 既定列名と別名。
#[test]
fn default_and_explicit_aliases() {
    let (core, ctx, _g) = setup("pr-alias", true);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, percent_rank() OVER (ORDER BY s), cume_dist() OVER (ORDER BY s) AS cd \
             FROM t LIMIT 100",
        )
        .expect("alias select");
    let names: Vec<String> = result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, ty } => {
                assert_eq!(*ty, Some(ColumnType::Double));
                name.clone()
            }
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(names[1..], ["percent_rank".to_string(), "cd".to_string()]);
}

/// 引数・`DISTINCT`・`*` は順位関数と同じ SQLSTATE で拒否する。
#[test]
fn rejected_forms() {
    let (core, ctx, _g) = setup("pr-reject", true);
    let run = |func: &str| {
        outcome(
            &core,
            &ctx,
            &format!("SELECT id, {func} OVER (ORDER BY id) FROM t LIMIT 100"),
        )
    };
    assert_eq!(run("percent_rank(s)"), run("rank(s)"));
    assert_eq!(run("percent_rank(s)"), "42601");
    assert_eq!(run("cume_dist(1)"), "42601");
    assert_eq!(run("percent_rank(DISTINCT s)"), "42601");
    assert_eq!(run("cume_dist(*)"), "42601");
}

/// フレーム句は無視される。オフセット式の扱いは `rank` と同じ。
#[test]
fn frame_clause_is_ignored() {
    let (core, ctx, _g) = setup("pr-frame", true);
    for over in [
        "ORDER BY s ROWS BETWEEN 1 PRECEDING AND CURRENT ROW",
        "ORDER BY s GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING",
        "ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW",
    ] {
        assert_eq!(
            win(&core, &ctx, "percent_rank()", over),
            win(&core, &ctx, "percent_rank()", "ORDER BY s"),
            "over={over}"
        );
        assert_eq!(
            win(&core, &ctx, "cume_dist()", over),
            win(&core, &ctx, "cume_dist()", "ORDER BY s"),
            "over={over}"
        );
    }
    let offset = "ORDER BY id ROWS BETWEEN (1 / 0) PRECEDING AND CURRENT ROW";
    for f in ["percent_rank()", "cume_dist()"] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, {f} OVER ({offset}) FROM t LIMIT 100")
            ),
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, rank() OVER ({offset}) FROM t LIMIT 100")
            )
        );
    }
}

/// 他のウィンドウ関数と同じ SELECT に混在させても独立に評価される。
#[test]
fn mixed_with_other_window_functions() {
    let (core, ctx, _g) = setup("pr-mixed", true);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, rank() OVER (ORDER BY s), percent_rank() OVER (ORDER BY s), \
             cume_dist() OVER (ORDER BY s), row_number() OVER (ORDER BY id) FROM t LIMIT 100",
        )
        .expect("mixed select");
    let mut rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(show).collect())
        .collect();
    rows.sort();
    assert_eq!(rows[0], strs(&["1", "1", "0", "0.2", "1"]),);
    assert_eq!(rows[3], strs(&["4", "5", "1", "1", "4"]));
}

/// 他テナントの行・Private で不可視の行・`WHERE` で除いた行は分母にも分子にも入らない。
#[test]
fn other_tenants_and_invisible_rows_never_shift_distribution() {
    let path = unique_db_path("pr-rls");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    storage.create_table(&schema()).expect("create table");
    for row in ROWS {
        insert(&storage, &a, Visibility::Private, row);
    }
    for (id, ..) in ROWS {
        insert(
            &storage,
            &b,
            Visibility::Private,
            (id + 100, Some(1000), "z", "a"),
        );
    }
    // 自テナントの Private を Public のみのコンテキストからは見せない。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        win(&core, &a, "percent_rank()", "ORDER BY s"),
        strs(&["0", "0.25", "0.25", "1", "0.75"])
    );
    assert_eq!(
        win(&core, &a, "cume_dist()", "ORDER BY s"),
        strs(&["0.2", "0.6", "0.6", "1", "0.8"])
    );
    assert_eq!(win(&core, &b, "cume_dist()", "ORDER BY s").len(), 5);
    // `WHERE` で除いた id4 は母集合に入らない（4 行: 10, 20, 20, 50）。
    assert_eq!(
        col(
            &core,
            &a,
            "SELECT id, cume_dist() OVER (ORDER BY s) FROM t WHERE id <> 4 LIMIT 100"
        ),
        strs(&["0.25", "0.75", "0.75", "1"])
    );
}
