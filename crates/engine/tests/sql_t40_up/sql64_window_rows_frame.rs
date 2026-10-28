//! ウィンドウ集計の `ROWS` フレーム評価（SQL-64 (2)・Issue #1889）の結合テスト。
//!
//! 足場は `sql64_lag_lead.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17（17.11）で同じ SQL を流して確認した値で、
//! 順位関数・`LAG`／`LEAD` がフレームを無視すること、オフセット（NULL＝`22004`・負＝`22013`）の
//! 検査が関数ごとに異なることも含む。`RANGE`／`GROUPS` は #1890／#1891 の担当で本テストの対象外。

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
        "rowsframe-{}-{id}",
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

fn setup(label: &str, populate: bool) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("s", ColumnType::Integer, true),
                ColumnDef::new("f", ColumnType::Double, false),
                ColumnDef::new("x", ColumnType::Text, false),
                ColumnDef::new("g", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
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
fn sliding_frames_by_physical_position() {
    let (core, ctx, _g) = setup("rows-slide", true);
    let w = "ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w),
        strs(&["30", "50", "40", "70", "50"])
    );
    assert_eq!(
        win(&core, &ctx, "count(s)", w),
        strs(&["2", "3", "2", "2", "1"])
    );
    assert_eq!(
        win(&core, &ctx, "count(*)", w),
        strs(&["2", "3", "3", "3", "2"])
    );
    assert_eq!(
        win(&core, &ctx, "min(s)", w),
        strs(&["10", "10", "20", "20", "50"])
    );
    assert_eq!(
        win(&core, &ctx, "max(s)", w),
        strs(&["20", "20", "20", "50", "50"])
    );
    let w2 = "ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w2),
        strs(&["10", "30", "40", "20", "50"])
    );
    // 単一境界形は `BETWEEN <境界> AND CURRENT ROW` と同じ。
    assert_eq!(
        win(&core, &ctx, "sum(s)", "ORDER BY id ROWS 1 PRECEDING"),
        win(&core, &ctx, "sum(s)", w2)
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["30", "40", "20", "50", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY id ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING"
        ),
        strs(&["4", "3", "2", "1", "0"])
    );
    // smallint のオフセットも受理される。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, sum(s) OVER (ROWS 1::smallint PRECEDING) FROM t"
        ),
        "OK"
    );
}

#[test]
fn partition_by_scopes_the_frame() {
    let (core, ctx, _g) = setup("rows-part", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["30", "30", "20", "70", "50"])
    );
}

#[test]
fn rows_frame_differs_from_default_peer_frame() {
    let (core, ctx, _g) = setup("rows-vs-default", true);
    // 既定フレームは peer（同じ g）全体に同じ値を配る。
    assert_eq!(
        win(&core, &ctx, "sum(s)", "ORDER BY g"),
        strs(&["30", "30", "100", "100", "100"])
    );
    // ROWS は物理位置で行ごとに値が変わる（同値キー内は id のタイブレーク順）。
    assert_eq!(
        win(&core, &ctx, "sum(s)", "ORDER BY g ROWS UNBOUNDED PRECEDING"),
        strs(&["10", "30", "50", "50", "100"])
    );
}

#[test]
fn empty_and_huge_offsets() {
    let (core, ctx, _g) = setup("rows-empty-huge", true);
    let empty = "ORDER BY id ROWS BETWEEN 2 PRECEDING AND 3 PRECEDING";
    assert_eq!(win(&core, &ctx, "sum(s)", empty), strs(&["NULL"; 5]));
    assert_eq!(win(&core, &ctx, "count(s)", empty), strs(&["0"; 5]));
    assert_eq!(win(&core, &ctx, "count(*)", empty), strs(&["0"; 5]));
    for big in [
        "ORDER BY id ROWS BETWEEN 2147483648 PRECEDING AND 2147483648 FOLLOWING",
        "ORDER BY id ROWS BETWEEN 9007199254740992 PRECEDING AND 9007199254740992 FOLLOWING",
    ] {
        assert_eq!(win(&core, &ctx, "sum(s)", big), strs(&["100"; 5]), "{big}");
    }
}

#[test]
fn exclusion_clauses() {
    let (core, ctx, _g) = setup("rows-exclude", true);
    // ORDER BY に id を含むので peer は自分だけ（GROUP は CURRENT ROW と同じ）。
    let w = "ORDER BY s, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP";
    assert_eq!(
        win(&core, &ctx, "sum(s)", w),
        strs(&["20", "30", "70", "50", "20"])
    );
    assert_eq!(
        win(&core, &ctx, "count(s)", w),
        strs(&["1", "2", "2", "1", "1"])
    );
    assert_eq!(
        win(&core, &ctx, "min(s)", w),
        strs(&["20", "10", "20", "50", "20"])
    );
    // peer を含むフレーム全体に対する EXCLUDE（peer 内の並びに依存しない形）。
    let all = "ORDER BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{all} EXCLUDE TIES")),
        strs(&["100", "80", "80", "100", "100"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{all} EXCLUDE GROUP")),
        strs(&["90", "60", "60", "100", "50"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(s)", &format!("{all} EXCLUDE CURRENT ROW")),
        strs(&["90", "80", "80", "100", "50"])
    );
    // ORDER BY なしでは全行が peer。
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(s)",
            "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES"
        ),
        // `s` が NULL の行は自分以外の peer を除くと空になり count(s) = 0。
        strs(&["1", "1", "1", "0", "1"])
    );
    // 累積形でも EXCLUDE があれば除外が効く。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(s)",
            "ORDER BY id ROWS UNBOUNDED PRECEDING EXCLUDE CURRENT ROW"
        ),
        strs(&["NULL", "10", "30", "50", "50"])
    );
}

#[test]
fn float_sum_follows_forward_accumulation_order() {
    let (core, ctx, _g) = setup("rows-float", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(f)",
            "ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&[
            "0.30000000000000004",
            "0.6000000000000001",
            "1e20",
            "0.0",
            "0.0"
        ])
    );
}

#[test]
fn text_min_max_with_frames() {
    let (core, ctx, _g) = setup("rows-text", true);
    assert_eq!(
        win(
            &core,
            &ctx,
            "min(x)",
            "ORDER BY id ROWS BETWEEN CURRENT ROW AND 1 FOLLOWING EXCLUDE CURRENT ROW"
        ),
        strs(&["a", "c", "d", "e", "NULL"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "max(x)",
            "ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW"
        ),
        strs(&["b", "b", "c", "d", "e"])
    );
}

#[test]
fn ranking_functions_ignore_the_frame_and_its_offsets() {
    let (core, ctx, _g) = setup("rows-rank", true);
    for f in ["row_number()", "rank()", "dense_rank()"] {
        let plain = win(&core, &ctx, f, "ORDER BY id");
        for frame in [
            "ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE GROUP",
            "ROWS -1 PRECEDING",
            "ROWS NULL PRECEDING",
        ] {
            assert_eq!(
                win(&core, &ctx, f, &format!("ORDER BY id {frame}")),
                plain,
                "{f} {frame}"
            );
        }
        // 評価エラーはプラン時の定数畳み込みで返る。
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, {f} OVER (ORDER BY id ROWS 1/0 PRECEDING) FROM t")
            ),
            "22012"
        );
    }
}

#[test]
fn lag_lead_ignore_the_frame_but_check_offsets() {
    let (core, ctx, _g) = setup("rows-lag", true);
    let plain = win(&core, &ctx, "lag(s)", "ORDER BY id");
    assert_eq!(
        win(
            &core,
            &ctx,
            "lag(s)",
            "ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 1 FOLLOWING EXCLUDE GROUP"
        ),
        plain
    );
    for f in ["lag(s)", "lead(s)"] {
        for (frame, want) in [
            ("ROWS -1 PRECEDING", "22013"),
            ("ROWS NULL PRECEDING", "22004"),
            ("ROWS BETWEEN 1 PRECEDING AND -1 FOLLOWING", "22013"),
            ("ROWS 1/0 PRECEDING", "22012"),
        ] {
            let sql = format!("SELECT id, {f} OVER (ORDER BY id {frame}) FROM t");
            assert_eq!(outcome(&core, &ctx, &sql), want, "{sql}");
        }
    }
    // 空テーブルでも検査される。
    let (empty, ctx2, _g2) = setup("rows-lag-empty", false);
    assert_eq!(
        outcome(
            &empty,
            &ctx2,
            "SELECT id, lag(s) OVER (ORDER BY id ROWS -1 PRECEDING) FROM t"
        ),
        "22013"
    );
}

#[test]
fn aggregate_offset_checks_run_even_without_rows() {
    let (core, ctx, _g) = setup("rows-agg-checks", true);
    let (empty, ctx2, _g2) = setup("rows-agg-checks-empty", false);
    let cases = [
        ("ROWS -1 PRECEDING", "22013"),
        ("ROWS NULL PRECEDING", "22004"),
        ("ROWS BETWEEN NULL PRECEDING AND -1 FOLLOWING", "22004"),
        ("ROWS BETWEEN 1 PRECEDING AND -1 FOLLOWING", "22013"),
        ("ROWS BETWEEN 1 PRECEDING AND NULL FOLLOWING", "22004"),
        ("ROWS 1/0 PRECEDING", "22012"),
        ("ROWS 9223372036854775807 + 1 PRECEDING", "22003"),
        ("ROWS 1 PRECEDING", "OK"),
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
    let (core, ctx, _g) = setup("rows-paths", true);
    let want = strs(&["10", "30", "40", "20", "50"]);
    let body =
        "SELECT id, sum(s) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS w FROM t";
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
            "SELECT g, sum(n) OVER (ORDER BY g ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM tv",
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
            "SELECT g, sum(n) OVER (ORDER BY g ROWS -1 PRECEDING) FROM tv"
        ),
        "22013"
    );
}

#[test]
fn other_tenants_private_rows_never_enter_the_frame() {
    let path = unique_db_path("rows-rls");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("s", ColumnType::Integer, true),
                ColumnDef::new("f", ColumnType::Double, false),
                ColumnDef::new("x", ColumnType::Text, false),
                ColumnDef::new("g", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    for row in ROWS {
        insert(&storage, &a, Visibility::Private, row);
    }
    // 別テナントの Private 行（同じ id・巨大な値）。
    for (id, ..) in ROWS {
        insert(
            &storage,
            &b,
            Visibility::Private,
            (id + 100, Some(1000), 0.0, "z", "a"),
        );
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let w = "ORDER BY s, id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    let got = col(
        &core,
        &a,
        &format!("SELECT id, sum(s) OVER ({w}) FROM t LIMIT 100"),
    );
    assert_eq!(got, strs(&["100"; 5]));
    let w1 = "ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        col(
            &core,
            &a,
            &format!("SELECT id, sum(s) OVER ({w1}) FROM t LIMIT 100")
        ),
        strs(&["30", "50", "40", "70", "50"])
    );
    assert_eq!(
        col(
            &core,
            &b,
            &format!("SELECT id, count(*) OVER ({w}) FROM t LIMIT 100")
        ),
        strs(&["5"; 5])
    );
}

#[test]
fn describe_does_not_evaluate_offsets() {
    let (core, _ctx, _g) = setup("rows-describe", true);
    // PostgreSQL も Parse／Describe ではオフセットを評価しない（実行時に 22013）。
    assert!(core
        .parse_sql_prepared("SELECT id, sum(s) OVER (ORDER BY id ROWS -1 PRECEDING) FROM t")
        .is_ok());
}
