//! ウィンドウ集計の `GROUPS` フレーム評価（SQL-64 (2)・Issue #1891）の結合テスト。
//!
//! 足場は `sql64_window_rows_frame.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17 に同じ SQL・同じフィクスチャを流して
//! 実測した値。フィクスチャは `s = [1:10, 2:20, 3:20, 4:NULL, 5:50]` で、`ORDER BY s` の peer グループは
//! {1} {2,3} {5} {4}（NULL は昇順で最後）。オフセットの検査時点（実行開始時・0 行でも）が
//! `ROWS` と同じであること、順位関数がフレームとオフセットを無視することも含む。

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

/// (id, s, f, x, g)。`s` は NULL を含み、`s = 20` の 2 行が peer になる。
type Row = (u64, Option<i32>, f64, &'static str, &'static str);

const ROWS: [Row; 5] = [
    (1, Some(10), 0.1, "b", "a"),
    (2, Some(20), 0.2, "a", "a"),
    (3, Some(20), 0.3, "c", "b"),
    (4, None, 1e20, "d", "b"),
    (5, Some(50), -1e20, "e", "b"),
];

fn insert(storage: &Storage, ctx: &PolicyContext, vis: Visibility, row: Row) {
    let (id, s, f, x, g) = row;
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "groupsframe-{}-{id}",
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
            Value::Double(f),
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
            ColumnDef::new("f", ColumnType::Double, false),
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
        Cell::Float(v) => format!("{v:?}"),
        Cell::Text(v) => v.clone(),
        Cell::Numeric(d) => d.to_string(),
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

#[test]
fn sliding_group_frames() {
    let (core, ctx, _g) = setup("groups-slide", true);
    let w = "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w),
        strs(&["50", "100", "100", "50", "90"])
    );
    assert_eq!(
        win(&core, &ctx, "count(*)", w),
        strs(&["3", "4", "4", "2", "4"])
    );
    assert_eq!(
        win(&core, &ctx, "count(s)", w),
        strs(&["3", "4", "4", "1", "3"])
    );
    assert_eq!(
        win(&core, &ctx, "min(s)", w),
        strs(&["10", "10", "10", "50", "20"])
    );
    assert_eq!(
        win(&core, &ctx, "max(s)", w),
        strs(&["20", "50", "50", "50", "50"])
    );
    let w = "ORDER BY s GROUPS BETWEEN 2 PRECEDING AND 1 PRECEDING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w),
        strs(&["NULL", "10", "10", "90", "50"])
    );
    assert_eq!(
        win(&core, &ctx, "count(*)", w),
        strs(&["0", "1", "1", "3", "3"])
    );
    let w = "ORDER BY s GROUPS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w),
        strs(&["90", "50", "50", "NULL", "NULL"])
    );
    assert_eq!(
        win(&core, &ctx, "count(*)", w),
        strs(&["4", "2", "2", "0", "1"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(s)", "ORDER BY s GROUPS CURRENT ROW"),
        strs(&["10", "40", "40", "NULL", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s GROUPS BETWEEN 2 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["50", "100", "100", "90", "100"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["50", "90", "90", "NULL", "50"])
    );
}

#[test]
fn null_keys_form_one_group_and_desc_nulls_first() {
    let (core, ctx, _g) = setup("groups-null", true);
    // DESC では NULL が先頭グループ。順序は {4} {5} {2,3} {1}。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s DESC GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["50", "100", "100", "50", "90"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s DESC GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW"
        ),
        strs(&["3", "3", "3", "1", "2"])
    );
}

#[test]
fn multi_key_order_by_defines_peers() {
    let (core, ctx, _g) = setup("groups-multi", true);
    // `ORDER BY g, s`: グループは {1} {2} {3} {5} {4}（RANGE のオフセット形と違い複数キーでも受理）。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY g, s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["30", "50", "90", "50", "70"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY g, s GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["2", "2", "2", "1", "2"])
    );
}

#[test]
fn partition_by_scopes_the_frame() {
    let (core, ctx, _g) = setup("groups-part", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "PARTITION BY g ORDER BY s GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW"
        ),
        strs(&["10", "30", "20", "50", "70"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "PARTITION BY g ORDER BY s GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["2", "1", "2", "1", "2"])
    );
}

#[test]
fn default_frame_equivalence() {
    let (core, ctx, _g) = setup("groups-default", true);
    for f in ["sum(s)", "count(*)", "min(s)", "max(s)"] {
        let default = win(&core, &ctx, f, "ORDER BY s");
        for over in [
            "ORDER BY s GROUPS UNBOUNDED PRECEDING",
            "ORDER BY s GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
            "ORDER BY s RANGE UNBOUNDED PRECEDING",
        ] {
            assert_eq!(win(&core, &ctx, f, over), default, "{f} {over}");
        }
    }
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s GROUPS UNBOUNDED PRECEDING"
        ),
        strs(&["10", "50", "50", "100", "100"])
    );
    // 既定フレームは peer 全体に同じ値を配る（g の peer）。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY g GROUPS UNBOUNDED PRECEDING"
        ),
        strs(&["30", "30", "100", "100", "100"])
    );
}

#[test]
fn exclusion_clauses() {
    let (core, ctx, _g) = setup("groups-exclude", true);
    let base = "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{base} EXCLUDE GROUP")),
        strs(&["40", "60", "60", "50", "40"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{base} EXCLUDE TIES")),
        strs(&["50", "80", "80", "50", "90"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            &format!("{base} EXCLUDE CURRENT ROW")
        ),
        strs(&["40", "80", "80", "50", "40"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{base} EXCLUDE NO OTHERS")),
        strs(&["50", "100", "100", "50", "90"])
    );
    // 累積形でも EXCLUDE があれば除外が効く。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s GROUPS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW"
        ),
        strs(&["NULL", "30", "30", "100", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(s)",
            "ORDER BY s GROUPS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES"
        ),
        strs(&["4", "3", "3", "4", "4"])
    );
}

#[test]
fn empty_and_huge_offsets() {
    let (core, ctx, _g) = setup("groups-empty-huge", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY s GROUPS BETWEEN 9007199254740991 PRECEDING AND 9007199254740991 FOLLOWING"
        ),
        strs(&["100"; 5])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s GROUPS BETWEEN 0 PRECEDING AND 0 FOLLOWING"
        ),
        strs(&["1", "2", "2", "1", "1"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s GROUPS BETWEEN 9007199254740991 FOLLOWING AND UNBOUNDED FOLLOWING"
        ),
        strs(&["0"; 5])
    );
    // 先頭グループを越える PRECEDING は空フレーム（id 4 だけ 3 グループ前が先頭グループで 1 行）。
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s GROUPS BETWEEN 3 PRECEDING AND 3 PRECEDING"
        ),
        strs(&["0", "0", "0", "1", "0"])
    );
}

#[test]
fn float_sum_follows_forward_accumulation_order() {
    let (core, ctx, _g) = setup("groups-float", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(f)",
            "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["0.6000000000000001", "-1e20", "-1e20", "0.0", "0.0"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(f)",
            "ORDER BY id GROUPS UNBOUNDED PRECEDING"
        ),
        strs(&[
            "0.1",
            "0.30000000000000004",
            "0.6000000000000001",
            "1e20",
            "0.0"
        ])
    );
}

#[test]
fn text_min_max_with_frames() {
    let (core, ctx, _g) = setup("groups-text", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "min(x)",
            "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["a", "a", "a", "d", "a"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "max(x)",
            "ORDER BY s GROUPS BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["c", "e", "e", "d", "e"])
    );
}

#[test]
fn ranking_functions_ignore_the_frame_and_its_offsets() {
    let (core, ctx, _g) = setup("groups-rank", true);
    for f in ["rank()", "dense_rank()"] {
        let plain = win(&core, &ctx, f, "ORDER BY s");
        for frame in [
            "GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP",
            "GROUPS -1 PRECEDING",
            "GROUPS NULL PRECEDING",
        ] {
            assert_eq!(
                win(&core, &ctx, f, &format!("ORDER BY s {frame}")),
                plain,
                "{f} {frame}"
            );
        }
    }
    // 評価エラーはプラン時の定数畳み込みで返る。
    for f in ["row_number()", "rank()", "dense_rank()"] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, {f} OVER (ORDER BY s GROUPS 1/0 PRECEDING) FROM t")
            ),
            "22012"
        );
    }
    assert_eq!(
        win(
            &core,
            &ctx,
            "rank()",
            "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP"
        ),
        strs(&["1", "2", "2", "5", "4"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "dense_rank()",
            "ORDER BY s GROUPS NULL PRECEDING"
        ),
        strs(&["1", "2", "2", "4", "3"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "row_number()",
            "ORDER BY id GROUPS -1 PRECEDING"
        ),
        strs(&["1", "2", "3", "4", "5"])
    );
}

#[test]
fn lag_lead_ignore_the_frame_but_check_offsets() {
    let (core, ctx, _g) = setup("groups-lag", true);
    let plain = win(&core, &ctx, "lag(s)", "ORDER BY id");
    assert_eq!(
        win(
            &core,
            &ctx,
            "lag(s)",
            "ORDER BY id GROUPS BETWEEN 1 FOLLOWING AND 1 FOLLOWING EXCLUDE GROUP"
        ),
        plain
    );
    for f in ["lag(s)", "lead(s)"] {
        for (frame, want) in [
            ("GROUPS -1 PRECEDING", "22013"),
            ("GROUPS NULL PRECEDING", "22004"),
            ("GROUPS BETWEEN 1 PRECEDING AND -1 FOLLOWING", "22013"),
            ("GROUPS 1/0 PRECEDING", "22012"),
        ] {
            let sql = format!("SELECT id, {f} OVER (ORDER BY id {frame}) FROM t");
            assert_eq!(outcome(&core, &ctx, &sql), want, "{sql}");
        }
    }
    // 空テーブルでも検査される。
    let (empty, ctx2, _g2) = setup("groups-lag-empty", false);
    assert_eq!(
        outcome(
            &empty,
            &ctx2,
            "SELECT id, lag(s) OVER (ORDER BY id GROUPS -1 PRECEDING) FROM t"
        ),
        "22013"
    );
}

#[test]
fn aggregate_offset_checks_run_even_without_rows() {
    let (core, ctx, _g) = setup("groups-agg-checks", true);
    let (empty, ctx2, _g2) = setup("groups-agg-checks-empty", false);
    let cases = [
        ("GROUPS -1 PRECEDING", "22013"),
        ("GROUPS NULL PRECEDING", "22004"),
        ("GROUPS BETWEEN NULL PRECEDING AND -1 FOLLOWING", "22004"),
        ("GROUPS BETWEEN 1 PRECEDING AND -1 FOLLOWING", "22013"),
        ("GROUPS BETWEEN 1 PRECEDING AND NULL FOLLOWING", "22004"),
        ("GROUPS 1/0 PRECEDING", "22012"),
        ("GROUPS 9223372036854775807 + 1 PRECEDING", "22003"),
        ("GROUPS 1 PRECEDING", "OK"),
    ];
    for (frame, want) in cases {
        let sql = format!("SELECT id, sum(s) OVER (ORDER BY id {frame}) FROM t");
        assert_eq!(outcome(&core, &ctx, &sql), want, "{sql}");
        assert_eq!(outcome(&empty, &ctx2, &sql), want, "empty: {sql}");
        let filtered =
            format!("SELECT id, sum(s) OVER (ORDER BY id {frame}) FROM t WHERE id > 100");
        assert_eq!(outcome(&core, &ctx, &filtered), want, "{filtered}");
    }
}

#[test]
fn cte_derived_and_view_paths_agree() {
    let (core, ctx, _g) = setup("groups-paths", true);
    let want = strs(&["50", "100", "100", "50", "90"]);
    let body =
        "SELECT id, sum(s) OVER (ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS w FROM t";
    assert_eq!(
        col(
            &core,
            &ctx,
            &format!("WITH c AS ({body}) SELECT id, w FROM c")
        ),
        want
    );
    assert_eq!(
        col(&core, &ctx, &format!("SELECT id, w FROM ({body}) d")),
        want
    );
    // 評価後射影形ビュー（本文の結果列へ外側でウィンドウを評価する経路。`CellWindowEvaluator`）。
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW tv AS SELECT g, sum(s) AS n FROM t GROUP BY g",
    )
    .expect("create view");
    let result = core
        .execute_sql(
            &ctx,
            "SELECT g, sum(n) OVER (ORDER BY g GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM tv",
        )
        .expect("view window");
    let mut got: Vec<(String, String)> = result
        .rows
        .iter()
        .map(|r| (show(&r.cells[0]), show(&r.cells[1])))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), "30".to_string()),
            ("b".to_string(), "100".to_string())
        ]
    );
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT g, sum(n) OVER (ORDER BY g GROUPS -1 PRECEDING) FROM tv"
        ),
        "22013"
    );
}

#[test]
fn other_tenants_private_rows_never_enter_the_frame() {
    let path = unique_db_path("groups-rls");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    storage.create_table(&schema()).expect("create table");
    for row in ROWS {
        insert(&storage, &a, Visibility::Private, row);
    }
    // 別テナントの Private 行（キーも値も異なる。グループ数・境界に影響してはならない）。
    for (id, ..) in ROWS {
        insert(
            &storage,
            &b,
            Visibility::Private,
            (id + 100, Some(1000), 0.0, "z", "a"),
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let w = "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        col(
            &core,
            &a,
            &format!("SELECT id, sum(s) OVER ({w}) FROM t LIMIT 100")
        ),
        strs(&["50", "100", "100", "50", "90"])
    );
    assert_eq!(
        col(
            &core,
            &a,
            &format!("SELECT id, count(*) OVER ({w}) FROM t LIMIT 100")
        ),
        strs(&["3", "4", "4", "2", "4"])
    );
    // 別テナントから見ても自テナントの Private 行は現れない（自分の 5 行だけが 1 グループ）。
    let all = "ORDER BY s GROUPS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        col(
            &core,
            &b,
            &format!("SELECT id, count(*) OVER ({all}) FROM t LIMIT 100")
        ),
        strs(&["5"; 5])
    );
}

#[test]
fn describe_does_not_evaluate_offsets() {
    let (core, _ctx, _g) = setup("groups-describe", true);
    // PostgreSQL も Parse／Describe ではオフセットを評価しない（実行時に 22013）。
    assert!(core
        .parse_sql_prepared("SELECT id, sum(s) OVER (ORDER BY s GROUPS -1 PRECEDING) FROM t")
        .is_ok());
}
