//! ウィンドウ集計の `RANGE` フレーム評価（SQL-64 (2)・Issue #1890）の結合テスト。
//!
//! 足場は `sql64_window_rows_frame.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。期待値は PostgreSQL 17（17.11）で同じ SQL を流して確認した値で、
//! 数値・日時キーの値オフセット、NULL キー（`NULLS FIRST`／`LAST`）、`DESC`、`EXCLUDE`、
//! float の NaN・無限大、オフセット型の照合（`0A000`）、オフセットの検査時点（NULL＝実行開始時の
//! `22004`、負・NaN＝非 NULL キーの行で境界を求める時点の `22013`）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
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

const DAY: i64 = 86_400_000_000;
const HOUR: i64 = 3_600_000_000;
/// 2024-01-01 00:00:00 の 1970-01-01 起点マイクロ秒。
const T0: i64 = 1_704_067_200_000_000;

fn dec(unscaled: i128) -> Value {
    Value::Numeric(Decimal::from_parts(unscaled, 1).expect("scale"))
}

/// キー列は全型を持つ 5 行（id 1〜5。id 5 のキーは全列 NULL）。`s`・`b` は peer（id 2・3）を持つ。
fn key_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("s", ColumnType::Integer, true),
        ColumnDef::new("b", ColumnType::BigInt, true),
        ColumnDef::new("f", ColumnType::Double, true),
        ColumnDef::new(
            "n",
            ColumnType::Numeric {
                precision: 10,
                scale: 1,
            },
            true,
        ),
        ColumnDef::new("d", ColumnType::Date, true),
        ColumnDef::new("ts", ColumnType::Timestamp, true),
        ColumnDef::new("tz", ColumnType::TimestampTz, true),
        ColumnDef::new("tm", ColumnType::Time, true),
        ColumnDef::new("x", ColumnType::Text, false),
        ColumnDef::new("v", ColumnType::Integer, false),
    ]
}

fn key_row(id: u64) -> Vec<Value> {
    let nulls = |x: &str, v: i32| {
        let mut r = vec![Value::Null; 8];
        r.push(Value::Text(x.to_string()));
        r.push(Value::Integer(v));
        r
    };
    match id {
        1 => vec![
            Value::Integer(1),
            Value::BigInt(1),
            Value::Double(1.0),
            dec(15),
            Value::Date(19_723),
            Value::Timestamp(T0),
            Value::TimestampTz(T0),
            Value::Time(0),
            Value::Text("a".into()),
            Value::Integer(10),
        ],
        2 => vec![
            Value::Integer(2),
            Value::BigInt(2),
            Value::Double(2.0),
            dec(25),
            Value::Date(19_724),
            Value::Timestamp(T0 + DAY),
            Value::TimestampTz(T0 + DAY),
            Value::Time(HOUR),
            Value::Text("b".into()),
            Value::Integer(20),
        ],
        3 => vec![
            Value::Integer(2),
            Value::BigInt(2),
            Value::Double(2.0),
            dec(25),
            Value::Date(19_724),
            Value::Timestamp(T0 + DAY + 12 * HOUR),
            Value::TimestampTz(T0 + DAY + 12 * HOUR),
            Value::Time(HOUR),
            Value::Text("c".into()),
            Value::Integer(30),
        ],
        4 => vec![
            Value::Integer(4),
            Value::BigInt(4),
            Value::Double(4.0),
            dec(40),
            Value::Date(19_727),
            Value::Timestamp(T0 + 4 * DAY),
            Value::TimestampTz(T0 + 4 * DAY),
            Value::Time(5 * HOUR),
            Value::Text("d".into()),
            Value::Integer(40),
        ],
        _ => nulls("e", 50),
    }
}

fn setup_with(
    label: &str,
    columns: Vec<ColumnDef>,
    rows: &[(u64, Vec<Value>)],
) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage
        .create_table(&TableSchema::new("t", columns))
        .expect("create table");
    for (id, values) in rows {
        insert(&storage, &ctx, Visibility::Public, *id, values);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

fn insert(storage: &Storage, ctx: &PolicyContext, vis: Visibility, id: u64, values: &[Value]) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "rangeframe-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(storage, "t", ctx, id, vis, values, &op_id)
        .expect("insert row");
}

fn setup(label: &str) -> (EngineCore, PolicyContext, CleanupGuard) {
    let rows: Vec<(u64, Vec<Value>)> = (1..=5).map(|id| (id, key_row(id))).collect();
    setup_with(label, key_columns(), &rows)
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

fn check_code(core: &EngineCore, ctx: &PolicyContext, over: &str, func: &str, want: &str) {
    let sql = format!("SELECT id, {func} OVER ({over}) FROM t LIMIT 100");
    assert_eq!(outcome(core, ctx, &sql), want, "{sql}");
}

#[test]
fn integer_keys_with_peers_and_null_keys() {
    let (core, ctx, _g) = setup("range-int");
    for key in ["s", "b"] {
        assert_eq!(
            win(
                &core,
                &ctx,
                "sum(v)",
                &format!("ORDER BY {key} RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING")
            ),
            strs(&["60", "60", "60", "40", "50"]),
            "{key}"
        );
    }
    let w = "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "count(*)", w),
        strs(&["3", "3", "3", "1", "1"])
    );
    assert_eq!(
        win(&core, &ctx, "min(v)", w),
        strs(&["10", "10", "10", "40", "50"])
    );
    // 型の異なる整数オフセット（bigint・smallint・型なし文字列リテラル）も同じ結果。
    for off in ["1::bigint", "1::smallint", "'1'"] {
        assert_eq!(
            win(
                &core,
                &ctx,
                "sum(v)",
                &format!("ORDER BY s RANGE BETWEEN {off} PRECEDING AND 1 FOLLOWING")
            ),
            strs(&["60", "60", "60", "40", "50"]),
            "{off}"
        );
    }
    // 疑似列 `id` もキーにできる。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY id RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["30", "60", "90", "120", "90"])
    );
    // 単一境界形は `BETWEEN <境界> AND CURRENT ROW`（現在行は peer グループの終端）と同じ。
    assert_eq!(
        win(&core, &ctx, "sum(v)", "ORDER BY s RANGE 1 PRECEDING"),
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND CURRENT ROW"
        )
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND CURRENT ROW"
        ),
        strs(&["10", "60", "60", "40", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN CURRENT ROW AND 1 FOLLOWING"
        ),
        strs(&["60", "50", "50", "40", "50"])
    );
}

#[test]
fn null_keys_and_unbounded_bounds_combine_like_postgres() {
    let (core, ctx, _g) = setup("range-null-unbounded");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    // 非 NULL キーの行は NULL 行をオフセット境界に含めないが、UNBOUNDED には含まれる。
    assert_eq!(
        sum("ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND 1 FOLLOWING"),
        strs(&["60", "60", "60", "100", "150"])
    );
    assert_eq!(
        sum("ORDER BY s RANGE BETWEEN 1 PRECEDING AND UNBOUNDED FOLLOWING"),
        strs(&["150", "150", "150", "90", "50"])
    );
    assert_eq!(
        sum("ORDER BY s NULLS FIRST RANGE BETWEEN UNBOUNDED PRECEDING AND 1 FOLLOWING"),
        strs(&["110", "110", "110", "150", "50"])
    );
    assert_eq!(
        sum("ORDER BY s NULLS FIRST RANGE BETWEEN 1 PRECEDING AND UNBOUNDED FOLLOWING"),
        strs(&["100", "100", "100", "40", "150"])
    );
    assert_eq!(
        sum("ORDER BY s NULLS FIRST RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
        strs(&["60", "60", "60", "40", "50"])
    );
}

#[test]
fn desc_and_empty_frames() {
    let (core, ctx, _g) = setup("range-desc");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    assert_eq!(
        sum("ORDER BY s DESC NULLS FIRST RANGE BETWEEN 1 PRECEDING AND 0 FOLLOWING"),
        strs(&["60", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY s DESC NULLS LAST RANGE BETWEEN 1 FOLLOWING AND 2 FOLLOWING"),
        strs(&["NULL", "10", "10", "50", "50"])
    );
    assert_eq!(
        sum("ORDER BY s RANGE BETWEEN 3 PRECEDING AND 1 PRECEDING"),
        strs(&["NULL", "10", "10", "60", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s RANGE BETWEEN 1 FOLLOWING AND 5 FOLLOWING"
        ),
        strs(&["3", "1", "1", "0", "1"])
    );
    // 空フレーム: COUNT は 0、他は NULL。
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "ORDER BY s RANGE BETWEEN 1 FOLLOWING AND 0 FOLLOWING"
        ),
        strs(&["0", "0", "0", "0", "1"])
    );
    assert_eq!(
        sum("ORDER BY s RANGE BETWEEN 1 FOLLOWING AND 0 FOLLOWING"),
        strs(&["NULL", "NULL", "NULL", "NULL", "50"])
    );
    assert_eq!(
        sum("ORDER BY s DESC RANGE BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING"),
        strs(&["140", "90", "90", "50", "50"])
    );
}

#[test]
fn exclusion_clauses_use_peer_groups() {
    let (core, ctx, _g) = setup("range-exclude");
    let w = |ex: &str| format!("ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE {ex}");
    assert_eq!(
        win(&core, &ctx, "sum(v)", &w("CURRENT ROW")),
        strs(&["50", "40", "30", "NULL", "NULL"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(v)", &w("GROUP")),
        strs(&["50", "10", "10", "NULL", "NULL"])
    );
    assert_eq!(
        win(&core, &ctx, "sum(v)", &w("TIES")),
        strs(&["60", "30", "40", "40", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW EXCLUDE TIES"
        ),
        strs(&["10", "30", "40", "100", "150"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE GROUP"
        ),
        strs(&["140", "100", "100", "110", "100"])
    );
}

#[test]
fn offsetless_range_frames_work_without_or_with_many_order_keys() {
    let (core, ctx, _g) = setup("range-offsetless");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    assert_eq!(
        sum("RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
        strs(&["150"; 5])
    );
    assert_eq!(sum("RANGE CURRENT ROW"), strs(&["150"; 5]));
    assert_eq!(
        sum("RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"),
        strs(&["150"; 5])
    );
    assert_eq!(
        sum("ORDER BY s, b RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"),
        strs(&["150", "140", "140", "90", "50"])
    );
    // 明示の `UNBOUNDED PRECEDING AND CURRENT ROW` は既定のフレームと全関数で同じ値。
    for f in [
        "sum(v)", "count(*)", "count(v)", "min(v)", "max(v)", "avg(v)", "min(x)",
    ] {
        for order in [
            "ORDER BY s",
            "ORDER BY s DESC",
            "ORDER BY s, b",
            "ORDER BY x",
        ] {
            assert_eq!(
                win(
                    &core,
                    &ctx,
                    f,
                    &format!("{order} RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW")
                ),
                win(&core, &ctx, f, order),
                "{f} {order}"
            );
        }
    }
    // ORDER BY が複数でもオフセット無しなら peer グループ単位。
    assert_eq!(
        win(
            &core,
            &ctx,
            "count(*)",
            "PARTITION BY x ORDER BY s, b RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"
        ),
        strs(&["1"; 5])
    );
}

#[test]
fn float_keys_with_huge_offsets() {
    // 列に NaN・無限大は格納できず、`'inf'`／`'NaN'` の float8 への変換も未対応（`0A000`）のため、
    // 無限大の代わりに巨大な有限オフセットで「全域」を確かめる。キー側・オフセット側の NaN・無限大の
    // 規則は `sql::window` の単体テストが担う。
    let (core, ctx, _g) = setup("range-float-huge");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    assert_eq!(
        sum("ORDER BY f RANGE BETWEEN 100000000000 PRECEDING AND 0 FOLLOWING"),
        strs(&["10", "60", "60", "100", "50"])
    );
    assert_eq!(
        sum("ORDER BY f RANGE BETWEEN 0 PRECEDING AND 100000000000 FOLLOWING"),
        strs(&["100", "90", "90", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY f DESC RANGE BETWEEN 100000000000 PRECEDING AND 100000000000 FOLLOWING"),
        strs(&["100", "100", "100", "100", "50"])
    );
}

#[test]
fn numeric_keys_use_exact_decimal_offsets() {
    let (core, ctx, _g) = setup("range-numeric");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    assert_eq!(
        sum("ORDER BY n RANGE BETWEEN 1 PRECEDING AND 1.5 FOLLOWING"),
        strs(&["60", "100", "100", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY n RANGE BETWEEN 0.5 PRECEDING AND 1.0 FOLLOWING"),
        strs(&["60", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY n RANGE BETWEEN 0.50 PRECEDING AND 0.5 FOLLOWING"),
        strs(&["10", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY n RANGE '0.5' PRECEDING"),
        strs(&["10", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY n RANGE BETWEEN 1::numeric PRECEDING AND 1 FOLLOWING"),
        strs(&["60", "60", "60", "40", "50"])
    );
    // 負の NUMERIC は 22013。float 型のオフセットは 0A000（numeric ← float は暗黙変換でない）。
    check_code(
        &core,
        &ctx,
        "ORDER BY n RANGE BETWEEN -1 PRECEDING AND 1 FOLLOWING",
        "sum(v)",
        "22013",
    );
    check_code(
        &core,
        &ctx,
        "ORDER BY n RANGE 1::float8 PRECEDING",
        "sum(v)",
        "0A000",
    );
}

#[test]
fn float_key_accepts_integer_and_decimal_offsets() {
    let (core, ctx, _g) = setup("range-float-offsets");
    for off in ["1", "1.0", "1::float8", "1::numeric", "'1'"] {
        assert_eq!(
            win(
                &core,
                &ctx,
                "sum(v)",
                &format!("ORDER BY f RANGE BETWEEN {off} PRECEDING AND 1.5 FOLLOWING")
            ),
            strs(&["60", "60", "60", "40", "50"]),
            "{off}"
        );
    }
}

#[test]
fn date_and_datetime_keys_with_interval_offsets() {
    let (core, ctx, _g) = setup("range-datetime");
    let sum = |over: &str| win(&core, &ctx, "sum(v)", over);
    // date
    assert_eq!(
        sum("ORDER BY d RANGE BETWEEN '1 day' PRECEDING AND '1 day' FOLLOWING"),
        strs(&["60", "60", "60", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY d RANGE BETWEEN INTERVAL '1 day' PRECEDING AND INTERVAL '1 day' FOLLOWING"),
        strs(&["60", "60", "60", "40", "50"])
    );
    // date は timestamp へ昇格して比較するため日未満のオフセットも効く。
    assert_eq!(
        sum("ORDER BY d RANGE BETWEEN '12 hours' PRECEDING AND '12 hours' FOLLOWING"),
        strs(&["10", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY d RANGE BETWEEN '36 hours' PRECEDING AND '1 hour' PRECEDING"),
        strs(&["NULL", "10", "10", "NULL", "50"])
    );
    assert_eq!(
        sum("ORDER BY d RANGE BETWEEN '1 day 12 hours' PRECEDING AND CURRENT ROW"),
        strs(&["10", "60", "60", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY d RANGE '1 month' PRECEDING"),
        strs(&["10", "60", "60", "100", "50"])
    );
    // timestamp
    assert_eq!(
        sum("ORDER BY ts RANGE BETWEEN '12 hours' PRECEDING AND '1 month' FOLLOWING"),
        strs(&["100", "90", "90", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY ts RANGE INTERVAL '12 hours' PRECEDING"),
        strs(&["10", "20", "50", "40", "50"])
    );
    // timestamptz（UTC セッションの暦で加減算する）
    assert_eq!(
        sum("ORDER BY tz RANGE BETWEEN INTERVAL '1 day' PRECEDING AND INTERVAL '1 hour' FOLLOWING"),
        strs(&["10", "30", "50", "40", "50"])
    );
    // time（日・月成分は無視し、時間成分だけで日またぎの折り返しなし）
    assert_eq!(
        sum("ORDER BY tm RANGE BETWEEN '1 hour' PRECEDING AND '1 hour' FOLLOWING"),
        strs(&["60", "60", "60", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY tm RANGE BETWEEN '2 hour' PRECEDING AND '23 hours' FOLLOWING"),
        strs(&["100", "100", "100", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY tm RANGE BETWEEN INTERVAL '1 month 1 hour' PRECEDING AND CURRENT ROW"),
        strs(&["10", "60", "60", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY tm RANGE BETWEEN '1 day' PRECEDING AND CURRENT ROW"),
        strs(&["10", "50", "50", "40", "50"])
    );
    assert_eq!(
        sum("ORDER BY tm RANGE BETWEEN INTERVAL '-1 month' PRECEDING AND CURRENT ROW"),
        strs(&["10", "50", "50", "40", "50"])
    );
}

#[test]
fn timestamp_bounds_may_pass_year_9999() {
    let columns = vec![
        ColumnDef::new("ts", ColumnType::Timestamp, true),
        ColumnDef::new("v", ColumnType::Integer, false),
    ];
    // 9999-12-30 と 9999-12-31（1970-01-01 起点のマイクロ秒）。
    let d31: i64 = 253_402_214_400_000_000 - DAY;
    let rows = vec![
        (1, vec![Value::Timestamp(d31), Value::Integer(1)]),
        (2, vec![Value::Timestamp(d31 - DAY), Value::Integer(2)]),
    ];
    let (core, ctx, _g) = setup_with("range-ts-9999", columns, &rows);
    // 境界値が 9999 年を超えても PostgreSQL の timestamp 範囲内ならエラーにならない。
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY ts RANGE BETWEEN '1 day' PRECEDING AND '1 day' FOLLOWING"
        ),
        strs(&["3", "3"])
    );
    // PostgreSQL の範囲（294276 年まで）を超えると 22008。
    check_code(
        &core,
        &ctx,
        "ORDER BY ts RANGE BETWEEN '1 day' PRECEDING AND '300000 years' FOLLOWING",
        "sum(v)",
        "22008",
    );
}

#[test]
fn integer_extremes_do_not_overflow() {
    let columns = vec![
        ColumnDef::new("b", ColumnType::BigInt, true),
        ColumnDef::new("v", ColumnType::Integer, false),
    ];
    let rows = vec![
        (1, vec![Value::BigInt(i64::MIN), Value::Integer(1)]),
        (2, vec![Value::BigInt(0), Value::Integer(2)]),
        (3, vec![Value::BigInt(i64::MAX), Value::Integer(3)]),
    ];
    let (core, ctx, _g) = setup_with("range-extremes", columns, &rows);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY b RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["1", "2", "3"])
    );
    let columns = vec![
        ColumnDef::new("s", ColumnType::Integer, true),
        ColumnDef::new("v", ColumnType::Integer, false),
    ];
    let rows = vec![
        (1, vec![Value::Integer(i32::MIN), Value::Integer(1)]),
        (2, vec![Value::Integer(i32::MAX), Value::Integer(2)]),
        (3, vec![Value::Integer(i32::MAX - 1), Value::Integer(3)]),
    ];
    let (core, ctx, _g2) = setup_with("range-extremes-int", columns, &rows);
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["1", "5", "5"])
    );
}

#[test]
fn offset_checks_have_postgres_timing() {
    let (core, ctx, _g) = setup("range-offset-checks");
    let over =
        |lo: &str, hi: &str| format!("ORDER BY s RANGE BETWEEN {lo} PRECEDING AND {hi} FOLLOWING");
    // NULL は実行開始時に 22004（集計・LAG／LEAD）。順位関数は検査しない。
    for f in ["sum(v)", "count(*)", "lag(v)", "lead(v)"] {
        check_code(&core, &ctx, &over("1", "NULL"), f, "22004");
        check_code(&core, &ctx, &over("NULL", "1"), f, "22004");
    }
    for f in ["row_number()", "rank()", "dense_rank()"] {
        check_code(&core, &ctx, &over("NULL", "1"), f, "OK");
        check_code(&core, &ctx, &over("-1", "1"), f, "OK");
    }
    // 負のオフセットは非 NULL キーの行で境界を求める時点で 22013（集計・LAG／LEAD）。
    for f in ["sum(v)", "count(*)", "lag(v)", "lead(v)"] {
        check_code(&core, &ctx, &over("-1", "1"), f, "22013");
        check_code(&core, &ctx, &over("1", "-1"), f, "22013");
    }
    check_code(
        &core,
        &ctx,
        "ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND -1 FOLLOWING",
        "sum(v)",
        "22013",
    );
    // 0 行・NULL キーの行だけなら 22013 は出ない。
    for f in ["sum(v)", "count(*)", "lag(v)"] {
        let sql = format!(
            "SELECT id, {f} OVER ({}) FROM t WHERE v > 1000 LIMIT 100",
            over("-1", "1")
        );
        assert_eq!(outcome(&core, &ctx, &sql), "OK", "{sql}");
        let sql = format!(
            "SELECT id, {f} OVER ({}) FROM t WHERE id = 5 LIMIT 100",
            over("-1", "1")
        );
        assert_eq!(outcome(&core, &ctx, &sql), "OK", "{sql}");
    }
    // NULL は 0 行でも 22004。
    let sql = format!(
        "SELECT id, sum(v) OVER ({}) FROM t WHERE v > 1000 LIMIT 100",
        over("1", "NULL")
    );
    assert_eq!(outcome(&core, &ctx, &sql), "22004", "{sql}");
    // 他の型の負のオフセット（date は月 = 30 日の正規化値、time は時間成分のみ）。
    check_code(
        &core,
        &ctx,
        "ORDER BY d RANGE '-1 day' PRECEDING",
        "sum(v)",
        "22013",
    );
    check_code(
        &core,
        &ctx,
        "ORDER BY d RANGE '1 month -31 days' PRECEDING",
        "sum(v)",
        "22013",
    );
    check_code(
        &core,
        &ctx,
        "ORDER BY tm RANGE '-1 hour' PRECEDING",
        "sum(v)",
        "22013",
    );
    check_code(
        &core,
        &ctx,
        "ORDER BY f RANGE -1.5 PRECEDING",
        "sum(v)",
        "22013",
    );
    // 非 NULL キーが無い（全行 NULL）なら date の負でも出ない。
    let sql = "SELECT id, sum(v) OVER (ORDER BY d RANGE '-1 day' PRECEDING) FROM t WHERE id = 5 LIMIT 100";
    assert_eq!(outcome(&core, &ctx, sql), "OK", "{sql}");
}

#[test]
fn offset_type_must_match_the_key_type() {
    let (core, ctx, _g) = setup("range-type-match");
    let cases = [
        ("s", "1.5", "0A000"),
        ("s", "1::float8", "0A000"),
        ("s", "INTERVAL '1 day'", "0A000"),
        ("b", "1.5", "0A000"),
        ("b", "INTERVAL '1 day'", "0A000"),
        ("f", "INTERVAL '1 day'", "0A000"),
        ("n", "INTERVAL '1 day'", "0A000"),
        ("d", "1", "0A000"),
        ("ts", "1", "0A000"),
        ("tz", "1", "0A000"),
        ("tm", "1", "0A000"),
        ("d", "1.5", "0A000"),
        ("x", "1", "0A000"),
        ("x", "NULL", "0A000"),
        // 型なし文字列リテラルはキー型へ強制変換され、解析エラーはその型のコードになる。
        ("s", "'abc'", "22P02"),
        ("d", "'abc'", "22007"),
        ("s", "'1'", "OK"),
        ("d", "'1 day'", "OK"),
    ];
    for (key, off, want) in cases {
        check_code(
            &core,
            &ctx,
            &format!("ORDER BY {key} RANGE {off} PRECEDING"),
            "sum(v)",
            want,
        );
    }
    // オフセットなしならキー型は問わない（`TEXT` キーでも peer グループ単位で評価される）。
    check_code(
        &core,
        &ctx,
        "ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        "sum(v)",
        "OK",
    );
    // 解析段の拒否は従来どおり（`ORDER BY` なし・複数キーは 42P20）。
    check_code(&core, &ctx, "RANGE 1 PRECEDING", "sum(v)", "42P20");
    check_code(
        &core,
        &ctx,
        "ORDER BY s, b RANGE 1 PRECEDING",
        "sum(v)",
        "42P20",
    );
}

#[test]
fn huge_offsets_cover_the_whole_non_null_range() {
    let (core, ctx, _g) = setup("range-huge");
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 2147483648 FOLLOWING"
        ),
        strs(&["100", "100", "100", "40", "50"])
    );
    assert_eq!(
        win(
            &core,
            &ctx,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 9007199254740991 FOLLOWING"
        ),
        strs(&["100", "100", "100", "40", "50"])
    );
}

#[test]
fn text_min_max_with_range_frames() {
    let (core, ctx, _g) = setup("range-text-minmax");
    let w = "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    assert_eq!(
        win(&core, &ctx, "min(x)", w),
        strs(&["a", "a", "a", "d", "e"])
    );
    assert_eq!(
        win(&core, &ctx, "max(x)", w),
        strs(&["c", "c", "c", "d", "e"])
    );
}

#[test]
fn query_shapes_share_the_range_evaluation() {
    let (core, ctx, _g) = setup("range-shapes");
    let w = "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING";
    let direct = win(&core, &ctx, "sum(v)", w);
    let cte = col(
        &core,
        &ctx,
        &format!(
            "WITH c AS (SELECT id, sum(v) OVER ({w}) AS r FROM t) SELECT id, r FROM c LIMIT 100"
        ),
    );
    assert_eq!(cte, direct);
    let derived = col(
        &core,
        &ctx,
        &format!("SELECT id, r FROM (SELECT id, sum(v) OVER ({w}) AS r FROM t) q LIMIT 100"),
    );
    assert_eq!(derived, direct);
}

#[test]
fn other_tenants_private_rows_never_enter_the_frame() {
    let path = unique_db_path("range-rls");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    storage
        .create_table(&TableSchema::new("t", key_columns()))
        .expect("create table");
    for id in 1..=5 {
        insert(&storage, &a, Visibility::Private, id, &key_row(id));
    }
    // 別テナントの Private 行（キーが近接・値が巨大）。
    for id in 1..=5u64 {
        let mut row = key_row(id);
        if let Some(Value::Integer(v)) = row.last_mut() {
            *v = 100_000;
        }
        insert(&storage, &b, Visibility::Private, id + 100, &row);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for over in [
        "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        "ORDER BY d RANGE BETWEEN '1 day' PRECEDING AND '1 day' FOLLOWING",
        "ORDER BY s RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW",
    ] {
        let got = win(&core, &a, "sum(v)", over);
        assert!(
            got.iter()
                .all(|v| v.parse::<i64>().map_or(true, |n| n < 1000)),
            "{over}: {got:?}"
        );
    }
    assert_eq!(
        win(
            &core,
            &a,
            "sum(v)",
            "ORDER BY s RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"
        ),
        strs(&["60", "60", "60", "40", "50"])
    );
}
