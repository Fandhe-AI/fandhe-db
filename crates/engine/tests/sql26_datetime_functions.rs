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

// --- NULLIF over DATE/TIMESTAMP（Cursor Bugbot 指摘対応。PR #1120） ---------
//
// `=` 演算子は DATE/TIMESTAMP 同士の比較（DATE⋈TIMESTAMP は DATE 側を深夜
// 0 時の TIMESTAMP へ暗黙昇格）を受理するが、束縛時に NULLIF 側だけ
// Scalar/Text に限定されたままだと `NULLIF(a, b) ≡ CASE WHEN a = b THEN NULL
// ELSE a END`（対象ビヘイビア: SQL-26）という PostgreSQL 互換契約が崩れる。
// bind_nullif／eval_nullif に DATE/TIMESTAMP の分岐を追加した回帰を固定する。

#[test]
fn nullif_over_date_returns_null_when_equal_and_lhs_when_different() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let equal = core
        .execute_sql(
            &ctx,
            &format!("SELECT NULLIF(day, DATE '2024-06-15') FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("NULLIF over equal DATE operands should be accepted");
    assert_eq!(equal.rows[0].cells[0], Cell::Null);

    let different = core
        .execute_sql(
            &ctx,
            &format!("SELECT NULLIF(day, DATE '2024-01-01') FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("NULLIF over different DATE operands should be accepted");
    let expected_day = engine::datetime::parse_date("2024-06-15").expect("valid DATE literal");
    assert_eq!(different.rows[0].cells[0], Cell::Date(expected_day));
}

#[test]
fn nullif_over_timestamp_returns_null_when_equal_and_lhs_when_different() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 10:30:00", "op-1"),
    )
    .expect("insert should succeed");

    let equal = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT NULLIF(at, TIMESTAMP '2024-06-15 10:30:00') FROM {TABLE} WHERE id = 1 LIMIT 10"
            ),
        )
        .expect("NULLIF over equal TIMESTAMP operands should be accepted");
    assert_eq!(equal.rows[0].cells[0], Cell::Null);

    let different = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT NULLIF(at, TIMESTAMP '2000-01-01 00:00:00') FROM {TABLE} WHERE id = 1 LIMIT 10"
            ),
        )
        .expect("NULLIF over different TIMESTAMP operands should be accepted");
    match different.rows[0].cells[0] {
        Cell::Timestamp(_) => {}
        ref other => panic!("expected Cell::Timestamp, got {other:?}"),
    }
}

#[test]
fn nullif_over_date_and_timestamp_promotes_date_side_like_equality_operator() {
    // DATE⋈TIMESTAMP の暗黙昇格（`=` 演算子と同じ規則）。DATE 側が深夜 0 時の
    // TIMESTAMP へ昇格されるため、`day`（深夜 0 時）と `at`（深夜 0 時）が
    // 同一暦日なら NULL になる。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT NULLIF(day, at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("NULLIF over DATE/TIMESTAMP mismatch types should promote DATE side");
    assert_eq!(result.rows[0].cells[0], Cell::Null);
}

#[test]
fn nullif_rejects_mismatched_date_and_scalar() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT NULLIF(day, 1) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42804");
}

// --- date_part/date_trunc の裸の NULL 第 2 引数（codex P1 指摘対応。PR #1120） -
//
// `docs/design/datetime-scalar-functions.md` の契約「NULL 入力はすべて
// strict（結果は NULL）」を、列参照だけでなく裸の `NULL` リテラルでも満たす。
// `sql::allowlist::Parser::parse_call_expr` の `date_part`／`date_trunc` 分岐
// （第 2 引数のみ NULL リテラルを許可）と `bind_date_part_or_trunc` の
// `bind_null_aware` 対応の組み合わせで動作する回帰を固定する。

#[test]
fn date_part_and_date_trunc_accept_bare_null_second_argument() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part('year', NULL) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_part with a bare NULL second argument should be accepted");
    assert_eq!(result.rows[0].cells[0], Cell::Null);

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_trunc('day', NULL) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_trunc with a bare NULL second argument should be accepted");
    assert_eq!(result.rows[0].cells[0], Cell::Null);
}

#[test]
fn date_part_with_bare_null_second_argument_works_nested_in_coalesce() {
    // 第 2 引数の NULL 許可が `COALESCE` 等のネスト文脈でも正しく機能することを
    // 固定する（`position_argument_parsing_does_not_leak_null_literal_permission`
    // と対になる「許可が漏れない／欠けない」の両面確認）。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT COALESCE(date_part('year', NULL), 0) FROM {TABLE} WHERE id = 1 LIMIT 10"
            ),
        )
        .expect("date_part(..., NULL) nested in COALESCE should be accepted");
    assert_eq!(float_cell(&result.rows[0].cells[0]), 0.0);
}

#[test]
fn date_part_and_date_trunc_accept_bare_null_first_argument() {
    // codex 指摘対応（PR #1120）: 第 1 引数（field/unit）も ADR の
    // 「NULL 入力はすべて strict」契約に従い、裸の NULL は NULL を返す
    // （field が定まらないため `bind_date_part_or_trunc` は field 名解決を
    // 行わず、無条件で NULL を返す）。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part(NULL, at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_part with a bare NULL first argument should be accepted");
    assert_eq!(result.rows[0].cells[0], Cell::Null);

    let result = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_trunc(NULL, at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("date_trunc with a bare NULL first argument should be accepted");
    assert_eq!(result.rows[0].cells[0], Cell::Null);
}

#[test]
fn date_part_rejects_non_literal_first_argument_when_not_null() {
    // 第 1 引数が NULL でない場合は従来どおり「文字列リテラルであること」を
    // 要求する契約を維持する回帰（`bind_date_part_or_trunc` の
    // `field_ty == Some(Text)` 分岐で `BoundExpr::Text` 以外を拒否するパス）。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part(lang, at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT date_part(1, at) FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- DATE 減算の i32::MIN 境界値（codex P1 指摘対応。PR #1120） -------------
//
// `DATE - n` を `date_add_days(d, -n)` として実装すると、`n == i32::MIN`
// （`-2147483648`。それ自体は妥当な `i32` 値）の符号反転が `i32` の範囲を
// オーバーフローし、妥当な `n` が「`i32` 範囲外」という誤った `22003` で
// 拒否されていた。`date_sub_days` が符号反転前に `n` を検証する回帰を固定する。
//
// `sql::allowlist::Parser::parse_primary_expr` は一般の式位置で単項マイナスの
// 数値リテラルを受理しない（`HAVING`／`INSERT` の値のみ特例で受理する構造的
// 制約）ため、`n = i32::MIN` は `(0 - 2147483648)`（Scalar 同士の減算）として
// 表現し、`DATE - Scalar` の右辺に渡す。

#[test]
fn date_subtraction_with_i32_min_operand_is_accepted_not_rejected_as_out_of_range() {
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    // `DATE_MIN_DAYS`（`0001-01-01`）を基準日にする: `n = i32::MIN` の減算
    // 結果（`days - i32::MIN` = `days + 2147483648`）はこの基準日でなら
    // ちょうど `i32` の範囲には収まるが `DATE` の受理範囲（`0001-01-01`〜
    // `9999-12-31`）を大きく超えるため、修正後は「計算結果が範囲外」
    // （`22008`）になる。旧実装（符号反転後に範囲検査）は `days` に関わらず
    // 符号反転自体で `22003` になっていたため、`days` を変えても常に同じ
    // （誤った）エラーコードだった。
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "0001-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE day - (0 - 2147483648) > day AND id = 1 LIMIT 10"
            ),
        )
        .unwrap_err();
    assert_eq!(
        err.wire_code(),
        "22008",
        "a valid i32::MIN operand must not be rejected as operand-out-of-range (22003); \
         the result is genuinely outside DATE's representable range"
    );

    // `n` が非整数か `i32` 範囲外の場合の通常の拒否（`22000`／`22003`）は
    // 従来どおり機能する。
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day - 1.5 < day AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE day - 3000000000 < day AND id = 1 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
}

#[test]
fn date_subtraction_within_range_still_computes_correctly() {
    // `date_sub_days` への切り替えが通常範囲の計算結果を変えないことを固定する
    // （`SELECT` 頂点の非関数式は受理しない既存の構造的制約のため、`WHERE` の
    // 等価比較で計算結果を検証する。`where_date_arithmetic_and_cross_type_
    // comparison` と同じ流儀）。
    let (core, _guard) = new_core();
    let ctx = ctx_for("tenant-a");
    core.execute_insert_sql(
        &ctx,
        &insert_sql(1, "2024-06-15", "2024-06-15 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &ctx,
            &format!(
                "SELECT id FROM {TABLE} WHERE day - 10 = DATE '2024-06-05' AND id = 1 LIMIT 10"
            ),
        )
        .expect("DATE - n within range should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));
}
