//! ウィンドウ値取得関数 `first_value`／`last_value`／`nth_value`（SQL-64 (3)・Issue #1883、
//! 母集合の境界は RLS-10 のポインタ）の結合テスト。
//!
//! 足場は `sql64_window_groups_frame.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17 の意味論（既定フレームは
//! `RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW` で、`ORDER BY` があれば現在行の peer
//! グループ末尾まで・無ければパーティション全体。`EXCLUDE` はフレームから除いてから先頭・末尾・
//! n 番目を数える）から手で導いた値で、PostgreSQL への実測は未実施。
//!
//! フィクスチャは `s = [1:10, 2:20, 3:20, 4:NULL, 5:50]`、`x = [b, a, c, d, e]`、
//! `g = [a, a, b, b, b]`。`ORDER BY s` の並びは id 1, 2, 3, 5, 4（NULL は昇順で最後）で、
//! peer グループは {1} {2,3} {5} {4}。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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

/// 受け入れ条件の本体: 既定フレーム（`ORDER BY` あり）。`last_value` は現在行ではなく
/// 現在行の peer グループの最後の行の値になる。
#[test]
fn default_frame_with_order_by() {
    let (core, ctx, _g) = setup("vf-default", true);
    let o = "ORDER BY s";
    assert_eq!(
        win(&core, &ctx, "first_value(x)", o),
        strs(&["b", "b", "b", "b", "b"])
    );
    // id2 と id3 は peer（s = 20）で、どちらも peer グループ末尾の id3（"c"）になる。
    assert_eq!(
        win(&core, &ctx, "last_value(x)", o),
        strs(&["b", "c", "c", "d", "e"])
    );
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 2)", o),
        strs(&["NULL", "a", "a", "a", "a"])
    );
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 5)", o),
        strs(&["NULL", "NULL", "NULL", "d", "NULL"])
    );
    // 降順・NULL 位置の指定で並びが変わる（NULL が先頭: 4, 5, 3, 2, 1 → peer は {4} {5} {2,3} {1}）。
    assert_eq!(
        win(&core, &ctx, "first_value(x)", "ORDER BY s DESC"),
        strs(&["d", "d", "d", "d", "d"])
    );
    assert_eq!(
        win(&core, &ctx, "first_value(x)", "ORDER BY s NULLS FIRST"),
        strs(&["d", "d", "d", "d", "d"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", "ORDER BY s NULLS FIRST"),
        strs(&["b", "c", "c", "d", "e"])
    );
}

/// `ORDER BY` が無ければフレームはパーティション全体。`PARTITION BY` の境界をまたがない。
#[test]
fn default_frame_without_order_by_and_partition() {
    let (core, ctx, _g) = setup("vf-noorder", true);
    assert_eq!(
        win(&core, &ctx, "first_value(x)", ""),
        strs(&["b", "b", "b", "b", "b"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", ""),
        strs(&["e", "e", "e", "e", "e"])
    );
    assert_eq!(
        win(&core, &ctx, "first_value(x)", "PARTITION BY g ORDER BY id"),
        strs(&["b", "b", "c", "c", "c"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", "PARTITION BY g ORDER BY id"),
        strs(&["b", "a", "c", "d", "e"])
    );
}

#[test]
fn explicit_rows_frames() {
    let (core, ctx, _g) = setup("vf-rows", true);
    let w = "ORDER BY s ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "first_value(x)", w),
        strs(&["b", "b", "a", "e", "c"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", w),
        strs(&["a", "c", "e", "d", "d"])
    );
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 2)", w),
        strs(&["a", "a", "c", "d", "e"])
    );
    // `last_value` の定番の書き方（全体フレーム）。
    let all = "ORDER BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "last_value(x)", all),
        strs(&["d", "d", "d", "d", "d"])
    );
    // 空フレーム（パーティション末尾側）は NULL。
    let ahead = "ORDER BY s ROWS BETWEEN 2 FOLLOWING AND 3 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "first_value(x)", ahead),
        strs(&["c", "e", "d", "NULL", "NULL"])
    );
}

#[test]
fn explicit_range_and_groups_frames() {
    let (core, ctx, _g) = setup("vf-range-groups", true);
    let r = "ORDER BY s RANGE BETWEEN 10 PRECEDING AND CURRENT ROW";
    assert_eq!(
        win(&core, &ctx, "first_value(x)", r),
        strs(&["b", "b", "b", "d", "e"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", r),
        strs(&["b", "c", "c", "d", "e"])
    );
    let g = "ORDER BY s GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW";
    assert_eq!(
        win(&core, &ctx, "first_value(x)", g),
        strs(&["b", "b", "b", "e", "a"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(x)", g),
        strs(&["b", "c", "c", "d", "e"])
    );
}

#[test]
fn frame_exclusion() {
    let (core, ctx, _g) = setup("vf-exclude", true);
    let base = "ORDER BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    for excl in ["EXCLUDE CURRENT ROW", "EXCLUDE GROUP"] {
        let w = format!("{base} {excl}");
        assert_eq!(
            win(&core, &ctx, "first_value(x)", &w),
            strs(&["a", "b", "b", "b", "b"]),
            "{excl}"
        );
        assert_eq!(
            win(&core, &ctx, "last_value(x)", &w),
            strs(&["d", "d", "d", "e", "d"]),
            "{excl}"
        );
    }
    let ties = format!("{base} EXCLUDE TIES");
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 3)", &ties),
        strs(&["c", "e", "e", "c", "c"])
    );
    let none = format!("{base} EXCLUDE NO OTHERS");
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 3)", &none),
        strs(&["c", "c", "c", "c", "c"])
    );
}

/// フレームオフセットの検査は集計と同じ時点・同じ分類で、`XX000` に落ちない。
#[test]
fn frame_offset_errors() {
    let (core, ctx, _g) = setup("vf-frame-errors", true);
    for (over, code) in [
        ("ORDER BY s ROWS NULL PRECEDING", "22004"),
        ("ORDER BY s ROWS -1 PRECEDING", "22013"),
        (
            "ORDER BY s GROUPS BETWEEN -1 FOLLOWING AND UNBOUNDED FOLLOWING",
            "22013",
        ),
        ("ORDER BY s RANGE -1 PRECEDING", "22013"),
    ] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, first_value(x) OVER ({over}) FROM t LIMIT 100")
            ),
            code,
            "over={over}"
        );
    }
}

#[test]
fn nth_value_n_argument() {
    let (core, ctx, _g) = setup("vf-nth", true);
    let all = "ORDER BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "nth_value(x, NULL)", all),
        strs(&["NULL"; 5])
    );
    for n in ["0", "-1"] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, nth_value(x, {n}) OVER ({all}) FROM t LIMIT 100")
            ),
            "22016",
            "n={n}"
        );
    }
    // 行ごとの式（`id` を n にする）。並びは b, a, c, e, d。
    assert_eq!(
        win(&core, &ctx, "nth_value(x, id)", all),
        strs(&["b", "a", "c", "e", "d"])
    );
    // 行ごとに NULL（id4 の s）・フレームの行数を超える値（20, 50）は NULL。
    assert_eq!(win(&core, &ctx, "nth_value(x, s)", all), strs(&["NULL"; 5]));
    // 巨大な n（int4 の最大値）もフレーム超過として NULL。
    assert_eq!(
        win(&core, &ctx, "nth_value(x, 2147483647)", all),
        strs(&["NULL"; 5])
    );
    for (n, code) in [("1.5", "42804"), ("'a'", "42804"), ("3000000000", "22003")] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, nth_value(x, {n}) OVER ({all}) FROM t LIMIT 100")
            ),
            code,
            "n={n}"
        );
    }
    // 0 行の表では行の評価が起きないため `n = -1` でもエラーにならない（PostgreSQL と同じ）。
    let (core0, ctx0, _g0) = setup("vf-nth-empty", false);
    assert_eq!(
        outcome(
            &core0,
            &ctx0,
            "SELECT id, nth_value(x, -1) OVER (ORDER BY s) FROM t LIMIT 100"
        ),
        "OK"
    );
}

#[test]
fn argument_shapes() {
    let (core, ctx, _g) = setup("vf-args", true);
    for sql in [
        "SELECT id, first_value() OVER () FROM t LIMIT 10",
        "SELECT id, first_value(x, g) OVER () FROM t LIMIT 10",
        "SELECT id, last_value(x, 1) OVER () FROM t LIMIT 10",
        "SELECT id, nth_value(x) OVER () FROM t LIMIT 10",
        "SELECT id, nth_value() OVER () FROM t LIMIT 10",
        "SELECT id, nth_value(x, 1, 2) OVER () FROM t LIMIT 10",
    ] {
        assert_eq!(outcome(&core, &ctx, sql), "42883", "sql={sql}");
    }
    for sql in [
        "SELECT id, first_value(*) OVER () FROM t LIMIT 10",
        "SELECT id, first_value(DISTINCT x) OVER () FROM t LIMIT 10",
        "SELECT id, nth_value(x, $1) OVER () FROM t LIMIT 10",
        "SELECT id, last_value($1) OVER () FROM t LIMIT 10",
    ] {
        assert_eq!(outcome(&core, &ctx, sql), "42601", "sql={sql}");
    }
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, first_value(row_number() OVER ()) OVER () FROM t LIMIT 10"
        ),
        "42P20"
    );
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, first_value(count(*)) OVER () FROM t LIMIT 10"
        ),
        "42803"
    );
    // 式・数値・疑似列 `id`。
    assert_eq!(
        win(&core, &ctx, "first_value(s * 2)", "ORDER BY s"),
        strs(&["20"; 5])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(s)", "ORDER BY s"),
        strs(&["10", "20", "20", "NULL", "50"])
    );
    assert_eq!(
        win(&core, &ctx, "last_value(id)", "ORDER BY id"),
        strs(&["1", "2", "3", "4", "5"])
    );
}

#[test]
fn unsupported_value_types_are_0a000() {
    let path = unique_db_path("vf-vector");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "vecs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    for f in ["first_value", "last_value"] {
        assert_eq!(
            outcome(
                &core,
                &ctx,
                &format!("SELECT id, {f}(embedding) OVER (ORDER BY id) FROM vecs LIMIT 10")
            ),
            "0A000",
            "f={f}"
        );
    }
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, nth_value(embedding, 1) OVER (ORDER BY id) FROM vecs LIMIT 10"
        ),
        "0A000"
    );
}

/// 値の評価エラーは参照された行のときだけ返る（参照されない行の 0 除算は出ない）。
#[test]
fn value_errors_are_deferred_until_referenced() {
    let (core, ctx, _g) = setup("vf-lazy", true);
    // `s = 20` の行（id2・id3）で 0 除算になる式。`first_value` の参照先は id1（s = 10）だけ。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, first_value(10 / (s - 20)) OVER (ORDER BY s) FROM t LIMIT 100"
        ),
        "OK"
    );
    // `last_value` は id2・id3 の行で id3 を参照する。
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, last_value(10 / (s - 20)) OVER (ORDER BY s) FROM t LIMIT 100"
        ),
        "22012"
    );
}

#[test]
fn mixed_with_other_window_functions_and_limit() {
    let (core, ctx, _g) = setup("vf-mixed", true);
    let result = core
        .execute_sql(
            &ctx,
            "SELECT id, first_value(x) OVER (ORDER BY s), row_number() OVER (ORDER BY id), \
             lag(x) OVER (ORDER BY id) FROM t ORDER BY id LIMIT 3",
        )
        .expect("mixed window functions");
    let got: Vec<(String, String, String)> = result
        .rows
        .iter()
        .map(|r| (show(&r.cells[1]), show(&r.cells[2]), show(&r.cells[3])))
        .collect();
    assert_eq!(
        got,
        vec![
            ("b".to_string(), "1".to_string(), "NULL".to_string()),
            ("b".to_string(), "2".to_string(), "b".to_string()),
            ("b".to_string(), "3".to_string(), "a".to_string()),
        ]
    );
}

/// 他テナントの行・Private で不可視の行・`WHERE` で除いた行は first／last／nth の対象にならない。
#[test]
fn other_tenants_and_invisible_rows_never_become_targets() {
    let path = unique_db_path("vf-rls");
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
    let all = "ORDER BY s ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING";
    assert_eq!(
        win(&core, &a, "last_value(x)", all),
        strs(&["d", "d", "d", "d", "d"])
    );
    assert_eq!(
        win(&core, &a, "nth_value(x, 6)", all),
        strs(&["NULL"; 5]),
        "tenant-b rows must not extend the frame"
    );
    assert_eq!(win(&core, &b, "first_value(x)", all), strs(&["z"; 5]));
    // `WHERE` で除いた id4 は対象にならない。
    assert_eq!(
        col(
            &core,
            &a,
            &format!("SELECT id, last_value(x) OVER ({all}) FROM t WHERE id <> 4 LIMIT 100")
        ),
        strs(&["e"; 4])
    );
}

/// ビュー越しでは、非公開列を引数にすると `42703`（存在情報を漏らさない）。公開列だけなら評価まで進む。
#[test]
fn view_column_scope_and_buffered_view() {
    let (core, ctx, _g) = setup("vf-view", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE VIEW v AS SELECT id, x FROM t")
        .expect("create view");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW top3 AS SELECT id, x FROM t ORDER BY id LIMIT 3",
    )
    .expect("create buffered view");
    let run = |sql: &str| {
        let mut s = SessionState::default();
        core.execute_sql_in_session(&ctx, &mut s, sql)
    };
    for sql in [
        "SELECT id, first_value(s) OVER (ORDER BY id) FROM v LIMIT 10",
        "SELECT id, nth_value(x, s) OVER (ORDER BY id) FROM v LIMIT 10",
    ] {
        assert_eq!(
            run(sql).expect_err("rejected").wire_code(),
            "42703",
            "sql={sql}"
        );
    }
    let SqlOutcome::Query(ok) =
        run("SELECT id, last_value(x) OVER (ORDER BY id) FROM top3 LIMIT 10")
            .expect("buffered view")
    else {
        panic!("expected a query result");
    };
    let mut got: Vec<(u64, String)> = ok
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Integer(id) => (*id, show(&r.cells[1])),
            other => panic!("expected id, got {other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            (1, "b".to_string()),
            (2, "a".to_string()),
            (3, "c".to_string())
        ]
    );
}

/// `GROUP BY`・`FILTER` 内のウィンドウ関数は誤用として分類される。
#[test]
fn misuse_is_rejected() {
    let (core, ctx, _g) = setup("vf-misuse", true);
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT count(*) FILTER (WHERE first_value(s) OVER () > 1) FROM t"
        ),
        "42P20"
    );
    assert_ne!(
        outcome(&core, &ctx, "SELECT id, first_value(x) FROM t LIMIT 10"),
        "OK"
    );
}
