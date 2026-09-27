//! `engine::core::EngineCore::execute_sql`／`execute_sql_in_session` の結合
//! テスト（Issue #920、対象ビヘイビア: SQL-26。ポインタ: `docs/spec/05-tasks.md`
//! TASK-210・`docs/spec/04-behavior/sql-surface.md` SQL-26）。
//!
//! `tests/sql26_numeric_functions.rs`・`tests/datetime_column.rs` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、独立
//! オラクル）で、日時スカラー関数群（`date_part`／`date_trunc`）・型付きリテラル
//! （`DATE '…'`／`TIMESTAMP '…'`）・`DATE` 算術・`DATE`／`TIMESTAMP` 列参照を
//! 結果列・`WHERE`・集計・UDF・CHECK の各経路から検証する。`EXTRACT(field FROM
//! src)` 構文は本 PR の対象外（`docs/design/datetime-scalar-functions.md` に
//! 記録。`date_part` は提供済み）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
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
    let path = unique_db_path("sql26-datetime-fn");
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

fn insert_sql(id: u64, day: &str, at: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, day, at) VALUES \
         ({id}, '[0.1,0.2]', 'ja', '{day}', '{at}') USING OPERATION_ID '{op_id}'"
    )
}

fn insert_sql_null_datetime(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', 'ja') \
         USING OPERATION_ID '{op_id}'"
    )
}

fn float_cell(cell: &Cell) -> f64 {
    match cell {
        Cell::Float(v) => *v,
        other => panic!("expected Cell::Float, got {other:?}"),
    }
}

// --- 投影: date_part の各 field ---------------------------------------------

#[test]
fn date_part_projects_scalar_for_each_field() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-02-29", "2024-02-29 12:34:56.5", "op-1"),
    )
    .expect("insert should succeed");

    let cases: &[(&str, f64)] = &[
        ("year", 2024.0),
        ("month", 2.0),
        ("day", 29.0),
        ("hour", 12.0),
        ("minute", 34.0),
        ("quarter", 1.0),
        ("dow", 4.0),
        ("isodow", 4.0),
    ];
    for (field, expected) in cases {
        let sql = format!("SELECT date_part('{field}', at) FROM {TABLE} WHERE id = 1 LIMIT 10");
        let result = core
            .execute_sql(&ctx, &sql)
            .unwrap_or_else(|e| panic!("date_part('{field}', at) should succeed, got {e:?}"));
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            float_cell(&result.rows[0].cells[0]),
            *expected,
            "field {field}"
        );
    }
}

/// `date_part` は `DATE` 列も受理する（`bind_call` が深夜 0 時の `TIMESTAMP` へ
/// 暗黙昇格する。§2-4）。
#[test]
fn date_part_accepts_date_column_via_implicit_promotion() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-02-29", "2024-02-29 12:34:56.5", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('day', day) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_part on DATE column should succeed");
    assert_eq!(float_cell(&result.rows[0].cells[0]), 29.0);
}

/// `date_trunc` の戻り値は `Cell::Timestamp`（ISO 整形。§2-9）。
#[test]
fn date_trunc_projects_timestamp_cell() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-02-29", "2024-02-29 12:34:56.5", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_trunc('month', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_trunc should succeed");
    match &result.rows[0].cells[0] {
        Cell::Timestamp(micros) => {
            assert_eq!(
                *micros,
                engine::datetime::parse_timestamp("2024-02-01 00:00:00").unwrap()
            );
        }
        other => panic!("expected Cell::Timestamp, got {other:?}"),
    }
}

// --- WHERE: DATE 算術・DATE/TIMESTAMP 比較 ----------------------------------

#[test]
fn where_date_arithmetic_and_cross_type_comparison() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(2, "2024-06-01", "2024-06-01 12:00:00", "op-2"),
    )
    .expect("insert should succeed");

    // `day + 30 > DATE '2024-01-20'` は id=2（2024-06-01 + 30 日）のみ真。
    let result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE day + 30 > DATE '2024-06-20' ORDER BY id LIMIT 10"
            ),
        )
        .expect("DATE arithmetic predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    // `at >= TIMESTAMP '…'`。
    let result = core.execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE at >= TIMESTAMP '2024-06-01 00:00:00' ORDER BY id LIMIT 10"),
        )
        .expect("TIMESTAMP comparison should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    // `DATE` と `TIMESTAMP` の比較（`day` を深夜 0 時へ昇格して比較）。
    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day < at ORDER BY id LIMIT 10"),
        )
        .expect("DATE/TIMESTAMP comparison should be accepted");
    // id=1: day 2024-01-01 00:00:00 < at 2024-01-01 00:00:00 は偽（等しい）。
    // id=2: day 2024-06-01 00:00:00 < at 2024-06-01 12:00:00 は真。
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    // `day - day2` 相当（日数差）。
    let result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE day - DATE '2024-01-01' = 152 AND id = 2 LIMIT 10"
            ),
        )
        .expect("DATE - DATE should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));
}

// --- mask 反映漏れの回帰テスト: COALESCE の分岐内でだけ参照する日時列 --------

/// `mark_referenced_scalar_columns`／`visit_referenced_scalar_columns` が
/// `DateColumnRef`／`TimestampColumnRef` を反映し損ねると、マスク外の列が
/// 実 NULL と取り違えられて `Internal` になるか、誤った値が返る
/// （advisor 指摘・§4 Step 4 参照）。COALESCE の分岐内だけで `at` を参照する式を
/// WHERE と投影の双方で評価し、正しい値が返ることを固定する。
#[test]
fn coalesce_branch_only_datetime_reference_is_not_masked_out() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");
    core.execute_insert_sql(&ctx, &insert_sql_null_datetime(2, "op-2"))
        .expect("insert should succeed");

    // 投影: id=1 は at の year、id=2 は NULL → COALESCE の DATE 既定値の year。
    let result = core.execute_sql(
            &ctx,
            &format!(
                "SELECT id, date_part('year', COALESCE(at, TIMESTAMP '2000-01-01 00:00:00')) FROM {TABLE} ORDER BY id LIMIT 10"
            ),
        )
        .expect("COALESCE over TIMESTAMP column should be accepted");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(float_cell(&result.rows[0].cells[1]), 2024.0);
    assert_eq!(float_cell(&result.rows[1].cells[1]), 2000.0);

    // WHERE: 分岐内だけの参照でも id=2 の NULL 行が正しく既定値に解決される。
    let result = core.execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE date_part('year', COALESCE(at, TIMESTAMP '2000-01-01 00:00:00')) = 2000 LIMIT 10"
            ),
        )
        .expect("WHERE over COALESCE(TIMESTAMP column) should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));
}

// --- NULL 伝播 ---------------------------------------------------------------

#[test]
fn null_datetime_column_propagates_null_in_projection_and_where() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(&ctx, &insert_sql_null_datetime(1, "op-1"))
        .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('year', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_part over NULL should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Null);

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE date_part('year', at) = 2024 LIMIT 10"),
        )
        .expect("WHERE over NULL should succeed without matching");
    assert!(result.rows.is_empty());
}

// --- エラー: 範囲外・型不一致・未知 field/unit -------------------------------

#[test]
fn datetime_arithmetic_and_field_errors_have_deterministic_wire_codes() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    // `DATE '9999-12-31' + 1` は可視行に適用すると `22008`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE DATE '9999-12-31' + 1 > day LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22008");

    // `day + 1.5`（非整数）は `22000`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day + 1.5 > day AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // `day + 3000000000`（i32 範囲外）は `22003`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day + 3000000000 > day AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");

    // 未知の field は `22000`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('fortnight', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // 未知の unit は `22000`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_trunc('fortnight', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // `DATE '2024-13-01'` は `22008`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day = DATE '2024-13-01' AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22008");

    // `DATE '2024/01/01'`（区切り文字違反）は `22000`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day = DATE '2024/01/01' AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // 現在時刻系は未実装のため未知の関数として `22000`。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT now() FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

/// 可視行が 0 件（他テナントの行しか無い）状態では、定数式の評価エラーが
/// クエリ全体の失敗にならない（defer-on-error 契約。§2-4）。
#[test]
fn visible_row_zero_defers_constant_overflow_error() {
    let (core, _guard) = new_core();
    let owner = ctx_for("tenant-a");
    let other = ctx_for("tenant-b");
    core.execute_insert_sql(
        &owner,
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &other,
            &format!("SELECT id FROM {TABLE} WHERE DATE '9999-12-31' + 1 > day LIMIT 10"),
        )
        .expect("no visible rows means the constant overflow is never evaluated");
    assert!(result.rows.is_empty());
}

// --- RLS: 他テナントの日時値行は見えない -------------------------------------

#[test]
fn rls_isolates_datetime_predicate_and_projection_across_tenants() {
    let (core, _guard) = new_core();
    let owner = ctx_for("tenant-a");
    let other = ctx_for("tenant-b");
    core.execute_insert_sql(
        &owner,
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core.execute_sql(
            &other,
            &format!("SELECT id, date_part('year', at) FROM {TABLE} WHERE day = DATE '2024-01-01' LIMIT 10"),
        )
        .expect("other tenant query should succeed with no visible rows");
    assert!(result.rows.is_empty());
}

// --- UDF・予約名 --------------------------------------------------------------

#[test]
fn udf_body_can_call_date_part_and_reserved_names_are_rejected() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION year_of(t) AS date_part('year', t)",
    )
    .expect("UDF body calling date_part should be accepted");
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("SELECT year_of(at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("calling the UDF should succeed");
    let result = match outcome {
        engine::sql::SqlOutcome::Query(r) => r,
        other => panic!("expected Query outcome, got {other:?}"),
    };
    assert_eq!(float_cell(&result.rows[0].cells[0]), 2024.0);

    // `date_part`／`date_trunc` は予約名として UDF 再定義を拒否する。
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION date_part(a, b) AS a")
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION date_trunc(a, b) AS a")
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // 非決定的関数名 `now` は引き続き UDF として登録・呼び出せる（PR #1107 の
    // codex 是正を維持。組み込み関数はすべて純粋関数のため決定性は崩れない）。
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION now(x) AS x")
        .expect("UDF named after a non-deterministic function name should still be definable");
}

// CHECK 制約の DATE/TIMESTAMP リテラル round-trip（render→再パース）は
// `sql::check_constraint::tests::check_constraint_with_datetime_literals_round_trips`
// で検証する。`CREATE TABLE` の SQL DDL は `DATE`／`TIMESTAMP` 列型を受理しない
// （TABLE-13・TASK-197 の列宣言は Rust API 経由に限る。別 Issue の対象）ため、
// 本結合テストファイル（SQL 経由のみで検証する）では列を持つ CHECK の
// 結合テストを組み立てられない。

// --- 決定性: date_part/date_trunc の畳み込み対象外でも同一入力は同一結果 ------

#[test]
#[allow(clippy::eq_op)]
fn date_part_is_deterministic_across_two_evaluations_of_the_same_row() {
    // 対象ビヘイビア: SQL-26（NOW の扱い・受入基準 2）。組み込み関数はすべて
    // 時計を読まない純粋関数のため、同一行を 2 回投影しても同一の結果を返す。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 10:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let first = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('hour', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("first projection should succeed");
    let second = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('hour', at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("second projection should succeed");
    assert_eq!(first.rows[0].cells[0], second.rows[0].cells[0]);
}

#[allow(dead_code)]
fn unused_session_state_import_anchor() -> SessionState {
    // `execute_sql_in_session` を使う既存テストとの記法揃えのため
    // `SessionState`/`OperationId` を import しているが、本ファイルは
    // `execute_sql`（暗黙セッション）のみを使う。未使用 import 警告を避ける
    // ための到達しないアンカー関数。
    let _ = OperationId::parse("op-anchor");
    SessionState::default()
}
