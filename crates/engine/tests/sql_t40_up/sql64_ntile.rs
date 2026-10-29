//! ウィンドウ関数 `ntile`（SQL-64 (4)・Issue #1884、母集合の境界は RLS-10 のポインタ）の
//! 結合テスト。
//!
//! 足場は `sql64_value_functions.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17 の `window_ntile` の状態機械
//! （先頭の `行数 % n` 個のバケットが 1 行多い。バケット数は最初の行で確定し、NULL の行は
//! NULL を返して次の行で再評価する）から手で導いた値で、PostgreSQL への実測は未実施。
//!
//! フィクスチャは `s = [1:10, 2:20, 3:20, 4:NULL, 5:50]`、`x = [b, a, c, d, e]`、
//! `g = [a, a, b, b, b]`（`sql64_value_functions.rs` と同じ）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
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
        "valuefn-{}-{id}",
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

fn id_order(core: &EngineCore, ctx: &PolicyContext, func: &str) -> Vec<String> {
    win(core, ctx, func, "ORDER BY id")
}

/// 余りの配り方: 先頭のバケットが 1 行ずつ多い（5 行 × n = 1, 2, 3, 4, 5, 7）。
#[test]
fn remainder_goes_to_leading_buckets() {
    let (core, ctx, _g) = setup("nt-remainder", true);
    assert_eq!(
        id_order(&core, &ctx, "ntile(1)"),
        strs(&["1", "1", "1", "1", "1"])
    );
    assert_eq!(
        id_order(&core, &ctx, "ntile(2)"),
        strs(&["1", "1", "1", "2", "2"])
    );
    assert_eq!(
        id_order(&core, &ctx, "ntile(3)"),
        strs(&["1", "1", "2", "2", "3"])
    );
    assert_eq!(
        id_order(&core, &ctx, "ntile(4)"),
        strs(&["1", "1", "2", "3", "4"])
    );
    assert_eq!(
        id_order(&core, &ctx, "ntile(5)"),
        strs(&["1", "2", "3", "4", "5"])
    );
    // バケット数が行数より多いと各行が別バケットで、最大値は行数。
    assert_eq!(
        id_order(&core, &ctx, "ntile(7)"),
        strs(&["1", "2", "3", "4", "5"])
    );
    assert_eq!(
        id_order(&core, &ctx, "ntile(2147483647)"),
        strs(&["1", "2", "3", "4", "5"])
    );
}

/// 並び順（`DESC`・`NULLS FIRST`・`ORDER BY` 無し）と `PARTITION BY`。
#[test]
fn ordering_and_partitions() {
    let (core, ctx, _g) = setup("nt-order", true);
    // 降順は 5, 4, 3, 2, 1 の順に配る。
    assert_eq!(
        win(&core, &ctx, "ntile(2)", "ORDER BY id DESC"),
        strs(&["2", "2", "1", "1", "1"])
    );
    // `ORDER BY s`（NULL は最後）: 並びは id 1, 2, 3, 5, 4。
    assert_eq!(
        win(&core, &ctx, "ntile(2)", "ORDER BY s"),
        strs(&["1", "1", "1", "2", "2"])
    );
    // `ORDER BY s NULLS FIRST`: 並びは id 4, 1, 2, 3, 5。
    assert_eq!(
        win(&core, &ctx, "ntile(2)", "ORDER BY s NULLS FIRST"),
        strs(&["1", "1", "2", "1", "2"])
    );
    // `ORDER BY` 無しは id 昇順（行の並びは id→seq）。
    assert_eq!(
        win(&core, &ctx, "ntile(2)", ""),
        strs(&["1", "1", "1", "2", "2"])
    );
    // g = a は {1,2}、g = b は {3,4,5}。パーティションごとに独立に配る。
    assert_eq!(
        win(&core, &ctx, "ntile(2)", "PARTITION BY g ORDER BY id"),
        strs(&["1", "2", "1", "1", "2"])
    );
}

/// 引数が NULL なら全行 NULL。0 以下は `22014`。0 行ではエラーにならない。
#[test]
fn null_and_non_positive_arguments() {
    let (core, ctx, _g) = setup("nt-arg", true);
    assert_eq!(id_order(&core, &ctx, "ntile(NULL)"), strs(&["NULL"; 5]));
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, ntile(0) OVER (ORDER BY id) FROM t LIMIT 100"
        ),
        "22014"
    );
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, ntile(-1) OVER (ORDER BY id) FROM t LIMIT 100"
        ),
        "22014"
    );
    // 0 行のパーティションは引数を評価しない。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, ntile(0) OVER (ORDER BY id) FROM t WHERE id > 100 LIMIT 100"
        ),
        "OK"
    );
    // `integer` を超える値は `22003`。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, ntile(3000000000) OVER (ORDER BY id) FROM t LIMIT 100"
        ),
        "22003"
    );
}

/// 列を引数にした場合: バケット数は最初の行（NULL を除く）で固定され、後続行の値は無視される。
/// 先頭が NULL の行は NULL を返し、次の行で再評価される（PostgreSQL の挙動）。
#[test]
fn column_argument_is_read_once() {
    let (core, ctx, _g) = setup("nt-col", true);
    // 先頭の s = 10 > 5 行なので各行が別バケット。後続の s = 20, 20, NULL, 50 は無視される。
    assert_eq!(
        id_order(&core, &ctx, "ntile(s)"),
        strs(&["1", "2", "3", "4", "5"])
    );
    // `ORDER BY s DESC NULLS FIRST` は id 4(NULL), 5(50), 2, 3, 1。id4 は NULL で、
    // 次の id5（s = 50）で確定して 1 から数える。
    assert_eq!(
        win(&core, &ctx, "ntile(s)", "ORDER BY s DESC NULLS FIRST"),
        strs(&["4", "2", "3", "NULL", "1"])
    );
}

/// 型・個数・構文の誤りは固定コードで拒否する。
#[test]
fn rejected_forms() {
    let (core, ctx, _g) = setup("nt-reject", true);
    let run = |func: &str| {
        outcome(
            &core,
            &ctx,
            &format!("SELECT id, {func} OVER (ORDER BY id) FROM t LIMIT 100"),
        )
    };
    assert_eq!(run("ntile(x)"), "42804");
    assert_eq!(run("ntile(1.5)"), "42804");
    assert_eq!(run("ntile()"), "42883");
    assert_eq!(run("ntile(1, 2)"), "42883");
    assert_eq!(run("ntile(*)"), "42601");
    assert_eq!(run("ntile(DISTINCT 1)"), "42601");
    assert_eq!(run("ntile(row_number() OVER ())"), "42P20");
}

/// フレーム句は無視される（順位関数と同じ）。
#[test]
fn frame_clause_is_ignored() {
    let (core, ctx, _g) = setup("nt-frame", true);
    let expected = strs(&["1", "1", "1", "2", "2"]);
    for over in [
        "ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW",
        "ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
        "ORDER BY id GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING",
        "ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE CURRENT ROW",
    ] {
        assert_eq!(win(&core, &ctx, "ntile(2)", over), expected, "over={over}");
    }
}

/// 他のウィンドウ関数と同じ SELECT に混在させても独立に評価される。
#[test]
fn mixed_with_other_window_functions() {
    let (core, ctx, _g) = setup("nt-mixed", true);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, ntile(2) OVER (ORDER BY id), row_number() OVER (ORDER BY id DESC) \
             FROM t LIMIT 100",
        )
        .expect("mixed select");
    let mut rows: Vec<(String, String, String)> = result
        .rows
        .iter()
        .map(|r| (show(&r.cells[0]), show(&r.cells[1]), show(&r.cells[2])))
        .collect();
    rows.sort();
    let got: Vec<(&str, &str)> = rows
        .iter()
        .map(|(_, a, b)| (a.as_str(), b.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![("1", "5"), ("1", "4"), ("1", "3"), ("2", "2"), ("2", "1")]
    );
}

/// 他テナントの行・Private で不可視の行・`WHERE` で除いた行はバケット境界に影響しない。
#[test]
fn other_tenants_and_invisible_rows_never_shift_buckets() {
    let path = unique_db_path("nt-rls");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        win(&core, &a, "ntile(5)", "ORDER BY id"),
        strs(&["1", "2", "3", "4", "5"])
    );
    assert_eq!(
        win(&core, &a, "ntile(2)", "ORDER BY id"),
        strs(&["1", "1", "1", "2", "2"])
    );
    assert_eq!(win(&core, &b, "ntile(5)", "ORDER BY id").len(), 5);
    // `WHERE` で除いた id4 は母集合に入らない（4 行 ÷ 2 バケット = 2 行ずつ）。
    assert_eq!(
        col(
            &core,
            &a,
            "SELECT id, ntile(2) OVER (ORDER BY id) FROM t WHERE id <> 4 LIMIT 100"
        ),
        strs(&["1", "1", "2", "2"])
    );
}
