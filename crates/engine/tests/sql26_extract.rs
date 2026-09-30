//! `EXTRACT(field FROM src)` 構文（Issue #1188、対象ビヘイビア: SQL-26。ポインタ:
//! `docs/spec/05-tasks.md` TASK-210・`docs/spec/04-behavior/sql-surface.md` SQL-26）の
//! 結合テスト。
//!
//! `EXTRACT` は構文段で `date_part('field', src)` へ脱糖されるため、同じ field・同じ入力に
//! 対して `date_part` と値・NULL 伝播・エラー分類が一致することを、同一行集合への 2 つの
//! クエリの突き合わせ（独立オラクル）で固定する。`tests/sql26_datetime_functions.rs` と同じ
//! 流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "events";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("at", ColumnType::Timestamp, true),
        ],
    )
}

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql26-extract");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 年境界（ISO 週・年内通算日が動く日付）を含む固定行。id=6 は日時が NULL。
const ROWS: [(u64, Option<(&str, &str)>); 6] = [
    (1, Some(("2024-02-29", "2024-02-29 12:34:56.5"))),
    (2, Some(("2020-12-31", "2020-12-31 23:59:59.999999"))),
    (3, Some(("2021-01-01", "2021-01-01 00:00:00"))),
    (4, Some(("2021-01-03", "2021-01-03 06:07:08.25"))),
    (5, Some(("1969-07-20", "1969-07-20 20:17:40"))),
    (6, None),
];

fn seed(core: &EngineCore, ctx: &PolicyContext) {
    for (id, dt) in ROWS {
        let sql = match dt {
            Some((day, at)) => format!(
                "INSERT INTO {TABLE} (id, embedding, lang, day, at) VALUES \
                 ({id}, '[0.1,0.2]', 'ja', '{day}', '{at}') USING OPERATION_ID 'op-{id}'"
            ),
            None => format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'op-{id}'"
            ),
        };
        core.execute_insert_sql(ctx, &sql).expect("insert");
    }
}

const FIELDS: [&str; 17] = [
    "microseconds",
    "milliseconds",
    "second",
    "minute",
    "hour",
    "day",
    "month",
    "quarter",
    "year",
    "dow",
    "isodow",
    "doy",
    "week",
    "isoyear",
    "decade",
    "century",
    "epoch",
];

fn column_values(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Cell> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql} should succeed, got {e:?}"));
    result
        .rows
        .iter()
        .map(|r| r.cells.first().cloned().expect("one projected column"))
        .collect()
}

/// 全 field・`TIMESTAMP` 列と `DATE` 列の両方で `EXTRACT` と `date_part` が一致する
/// （年境界・NULL 行を含む）。
#[test]
fn extract_matches_date_part_for_every_field_and_source() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    for field in FIELDS {
        for src in ["at", "day"] {
            let via_extract = column_values(
                &core,
                &ctx,
                &format!("SELECT EXTRACT({field} FROM {src}) FROM {TABLE} ORDER BY id LIMIT 10"),
            );
            let via_date_part = column_values(
                &core,
                &ctx,
                &format!("SELECT date_part('{field}', {src}) FROM {TABLE} ORDER BY id LIMIT 10"),
            );
            assert_eq!(via_extract.len(), ROWS.len());
            assert_eq!(via_extract, via_date_part, "field {field} on {src}");
            // NULL 入力（id=6）は NULL のまま伝播する。
            assert_eq!(via_extract.last(), Some(&Cell::Null), "{field} on {src}");
        }
    }
}

/// field は文字列リテラルでも書け、識別子・関数名・`FROM` は大文字小文字を区別しない。
#[test]
fn extract_accepts_string_field_and_is_case_insensitive() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    let expected = column_values(
        &core,
        &ctx,
        &format!("SELECT date_part('year', at) FROM {TABLE} ORDER BY id LIMIT 10"),
    );
    for sql in [
        format!("SELECT extract('year' FROM at) FROM {TABLE} ORDER BY id LIMIT 10"),
        format!("SELECT EXTRACT(YEAR FROM at) FROM {TABLE} ORDER BY id LIMIT 10"),
        format!("SELECT Extract(Year from at) FROM {TABLE} ORDER BY id LIMIT 10"),
    ] {
        assert_eq!(column_values(&core, &ctx, &sql), expected, "{sql}");
    }
}

/// 未知 field は `date_part` と同じ分類（`22000`）で拒否する。
#[test]
fn extract_unknown_field_is_rejected_like_date_part() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    let via_date_part = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('nosuch', at) FROM {TABLE} LIMIT 10"),
        )
        .expect_err("unknown date_part field must be rejected");
    let via_extract = core
        .execute_sql(
            &ctx,
            &format!("SELECT EXTRACT(nosuch FROM at) FROM {TABLE} LIMIT 10"),
        )
        .expect_err("unknown EXTRACT field must be rejected");
    assert_eq!(via_extract.wire_code(), via_date_part.wire_code());
}

/// 構文の不備（`FROM` の欠落・余剰トークン）は `42601`。先読みに一致しない `extract(...)` は
/// 通常の関数呼び出し（未知関数）として扱われ、`EXTRACT` を予約名にはしない。
#[test]
fn extract_syntax_errors_are_reported_as_42601() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    for sql in [
        format!("SELECT EXTRACT(year at) FROM {TABLE} LIMIT 10"),
        format!("SELECT EXTRACT(year FROM at extra) FROM {TABLE} LIMIT 10"),
        format!("SELECT EXTRACT(year FROM) FROM {TABLE} LIMIT 10"),
    ] {
        let err = core
            .execute_sql(&ctx, &sql)
            .expect_err("malformed EXTRACT must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
    // `extract(x)` は関数呼び出し形（未知関数）として束縛段で拒否される（構文エラーではない）。
    let err = core
        .execute_sql(&ctx, &format!("SELECT extract(at) FROM {TABLE} LIMIT 10"))
        .expect_err("extract(x) is an unknown function call");
    assert_ne!(err.wire_code(), "42601");
}

/// 既定の結果列名は PostgreSQL 互換で `extract`（明示の `date_part` は `date_part`）。
#[test]
fn extract_default_column_name_is_extract() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    let name_of = |sql: &str| -> String {
        let result = core.execute_sql(&ctx, sql).expect("query should succeed");
        match result.columns.first() {
            Some(ColumnMeta::Computed { name, .. }) => name.clone(),
            other => panic!("expected a computed column, got {other:?}"),
        }
    };
    assert_eq!(
        name_of(&format!(
            "SELECT EXTRACT(year FROM at) FROM {TABLE} LIMIT 1"
        )),
        "extract"
    );
    assert_eq!(
        name_of(&format!(
            "SELECT EXTRACT(year FROM at) AS y FROM {TABLE} LIMIT 1"
        )),
        "y"
    );
    assert_eq!(
        name_of(&format!(
            "SELECT date_part('year', at) FROM {TABLE} LIMIT 1"
        )),
        "date_part"
    );
}

/// 式文法の一部として、WHERE・CASE の内側・関数引数・集計関数の引数で使える。
#[test]
fn extract_works_in_where_case_call_args_and_aggregates() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE EXTRACT(year FROM at) = 2021 LIMIT 10"),
        )
        .expect("WHERE with EXTRACT");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![3, 4]);

    let cells = column_values(
        &core,
        &ctx,
        &format!(
            "SELECT CASE WHEN EXTRACT(month FROM at) > 6 THEN 1 ELSE 0 END FROM {TABLE} \
             ORDER BY id LIMIT 10"
        ),
    );
    let flags: Vec<Option<f64>> = cells
        .iter()
        .map(|c| match c {
            Cell::Float(v) => Some(*v),
            Cell::Null => None,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    // 2024-02 → 0、2020-12 → 1、2021-01 → 0、2021-01 → 0、1969-07 → 1、NULL 入力 → CASE の ELSE。
    assert_eq!(
        flags,
        vec![
            Some(0.0),
            Some(1.0),
            Some(0.0),
            Some(0.0),
            Some(1.0),
            Some(0.0)
        ]
    );

    let abs_cells = column_values(
        &core,
        &ctx,
        &format!("SELECT abs(EXTRACT(year FROM at) - 2000) FROM {TABLE} ORDER BY id LIMIT 10"),
    );
    assert_eq!(abs_cells.first(), Some(&Cell::Float(24.0)));

    let sum = column_values(
        &core,
        &ctx,
        &format!("SELECT SUM(EXTRACT(year FROM at)) FROM {TABLE}"),
    );
    // 2024 + 2020 + 2021 + 2021 + 1969（NULL 行は集計から除外）。
    assert_eq!(sum, vec![Cell::Float(10055.0)]);
}
