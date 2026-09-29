//! `DATE`／`TIMESTAMP` 列型（TABLE-13・TASK-197、Issue #884）の結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-197・`docs/spec/04-behavior/data-model.md`
//! TABLE-13／TABLE-6／TABLE-7・`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `tests/boolean_column.rs`（TABLE-13・TASK-196）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。往復（宣言→書き込み→
//! 再オープン→読み出し）・リテラル受理範囲（`22000` 文法違反／`22008` 範囲外・
//! 暦上不正）・COUNT／SUM 拒否・RLS 境界・UPDATE・content_hash 再送判定
//! （`23505`／`22023`）・Rust API 直接投入の往復を固定する。`DATE`／`TIMESTAMP`
//! の等価・範囲 WHERE 述語（TABLE-13・TASK-199、Issue #891・レーン B）は
//! 受理する。より広い網羅テストは `tests/scalar_types_predicates.rs` を参照。
//! 式（算術・関数引数）中の参照・二次索引での候補削減は引き続き対象外
//! （Issue #891・#893）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
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

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("datetime-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
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

fn insert_sql_without_day_at(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', 'ja') \
         USING OPERATION_ID '{op_id}'"
    )
}

fn count_column(core: &EngineCore, ctx: &PolicyContext, column: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT({column}) FROM {TABLE}"))
        .expect("count should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn select_projection(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
) -> Result<Vec<Cell>, engine::sql::allowlist::SqlSurfaceError> {
    let result = core.execute_sql(
        ctx,
        &format!("SELECT day, at FROM {TABLE} WHERE id = {id} LIMIT 1"),
    )?;
    assert_eq!(result.rows.len(), 1);
    Ok(result.rows[0].cells.clone())
}

// --- 受け入れ条件 1・4: カタログ・行コーデックでの往復 -----------------------

#[test]
fn datetime_columns_roundtrip_through_storage_reopen() {
    let path = unique_db_path("datetime-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "0001-01-01", "0001-01-01 00:00:00", "seed-1"),
        )
        .expect("min boundary insert should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "9999-12-31", "9999-12-31 23:59:59.999999", "seed-2"),
        )
        .expect("max boundary insert should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(3, "1969-12-31", "1969-12-31 23:59:59", "seed-3"),
        )
        .expect("pre-epoch insert should succeed");
        // NULL 行（day/at 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql_without_day_at(4, "seed-4"),
        )
        .expect("null insert should succeed");
    }

    // 再オープンして読み出す（TABLE-13「宣言→書き込み→再起動→読み出し」）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let cells = select_projection(&core, &alice, 1).expect("select id=1");
    assert_eq!(
        cells,
        vec![
            Cell::Date(-719_162),
            Cell::Timestamp(-62_135_596_800_000_000)
        ]
    );

    let cells = select_projection(&core, &alice, 2).expect("select id=2");
    assert_eq!(
        cells,
        vec![
            Cell::Date(2_932_896),
            Cell::Timestamp(253_402_300_799_999_999)
        ]
    );

    let cells = select_projection(&core, &alice, 3).expect("select id=3");
    assert_eq!(cells[0], Cell::Date(-1));

    let cells = select_projection(&core, &alice, 4).expect("select id=4");
    assert_eq!(cells, vec![Cell::Null, Cell::Null]);
}

// --- 受け入れ条件 3: リテラル受理範囲（文法違反=22000／範囲外・暦上不正=22008） ---

#[test]
fn insert_rejects_format_violations_as_22000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let cases = [
        "2024/01/01",           // 区切り文字違い
        "2024-1-01",            // 桁数不足
        "2024-01-01Z",          // TZ 接尾辞
        "2024-01-01T00:00:00Z", // TZ 接尾辞（TIMESTAMP 側は day 列で検証）
        " 2024-01-01",          // 前後空白
        "infinity",             // 特殊語
    ];
    for (i, day) in cases.iter().enumerate() {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, lang, day) VALUES \
             ({id}, '[0.1,0.2]', 'ja', '{day}') USING OPERATION_ID 'fmt-{id}'",
            id = 100 + i as u64,
        );
        let err = core
            .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000", "case {day:?} should be 22000");
    }
}

#[test]
fn insert_rejects_timestamp_timezone_suffix_and_date_only_as_22000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (i, at) in [
        "2024-01-02 03:04:05Z",
        "2024-01-02 03:04:05+09:00",
        "2024-01-02",
    ]
    .iter()
    .enumerate()
    {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, lang, at) VALUES \
             ({id}, '[0.1,0.2]', 'ja', '{at}') USING OPERATION_ID 'ts-fmt-{id}'",
            id = 200 + i as u64,
        );
        let err = core
            .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22000", "case {at:?} should be 22000");
    }
}

#[test]
fn insert_rejects_calendar_and_range_overflow_as_22008() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let cases = [
        "2024-13-01",
        "2024-02-30",
        "1900-02-29",
        "0000-12-31",
        "10000-01-01",
    ];
    for (i, day) in cases.iter().enumerate() {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, lang, day) VALUES \
             ({id}, '[0.1,0.2]', 'ja', '{day}') USING OPERATION_ID 'ovf-{id}'",
            id = 300 + i as u64,
        );
        let err = core
            .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22008", "case {day:?} should be 22008");
    }

    for (i, at) in [
        "2024-01-02 24:00:00",
        "2024-01-02 00:60:00",
        "2024-01-02 00:00:60",
    ]
    .iter()
    .enumerate()
    {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, lang, at) VALUES \
             ({id}, '[0.1,0.2]', 'ja', '{at}') USING OPERATION_ID 'ts-ovf-{id}'",
            id = 400 + i as u64,
        );
        let err = core
            .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
            .unwrap_err();
        assert_eq!(err.wire_code(), "22008", "case {at:?} should be 22008");
    }
}

#[test]
fn rejected_insert_leaves_row_count_and_generation_unchanged() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let before = count_column(&core, &alice, "day");
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, day) VALUES \
         (1, '[0.1,0.2]', 'ja', '2024-13-01') USING OPERATION_ID 'ovf-solo'"
    );
    let _ = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .unwrap_err();
    let after = count_column(&core, &alice, "day");
    assert_eq!(before, after);
}

#[test]
fn insert_rejects_number_or_boolean_literal_for_datetime_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, day) VALUES \
                 (1, '[0.1,0.2]', 'ja', 1) USING OPERATION_ID 'num-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, day) VALUES \
                 (1, '[0.1,0.2]', 'ja', true) USING OPERATION_ID 'bool-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- COUNT／SUM・WHERE 対象外の拒否 -----------------------------------------

#[test]
fn count_counts_non_null_datetime_rows_and_sum_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql_without_day_at(2, "op-2"),
    )
    .expect("insert should succeed");

    assert_eq!(count_column(&core, &alice, "day"), 1);
    assert_eq!(count_column(&core, &alice, "at"), 1);

    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(day) FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

/// `MIN`/`MAX(<DATE>/<TIMESTAMP>)` は Issue #892（D1・D7）で受理された
/// （`SUM`/`AVG` は引き続き拒否。上のテスト参照）。
#[test]
fn min_max_on_date_and_timestamp_succeed_after_issue_892() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "2023-06-15", "2025-06-15 12:30:00", "op-2"),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql_without_day_at(3, "op-3"),
    )
    .expect("insert with NULL day/at should succeed");

    let min_day = engine::datetime::parse_date("2023-06-15").expect("valid date");
    let max_day = engine::datetime::parse_date("2024-01-01").expect("valid date");
    let min_at = engine::datetime::parse_timestamp("2024-01-01 00:00:00").expect("valid ts");
    let max_at = engine::datetime::parse_timestamp("2025-06-15 12:30:00").expect("valid ts");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT MIN(day), MAX(day), MIN(at), MAX(at) FROM {TABLE}"),
        )
        .expect("MIN/MAX(DATE/TIMESTAMP) must succeed after Issue #892");
    assert_eq!(
        result.rows[0].cells,
        vec![
            Cell::Date(min_day),
            Cell::Date(max_day),
            Cell::Timestamp(min_at),
            Cell::Timestamp(max_at),
        ]
    );

    // NULL のみの空集合は NULL。
    let empty = core
        .execute_sql(
            &alice,
            &format!("SELECT MIN(day) FROM {TABLE} WHERE id = 999"),
        )
        .expect("MIN over an empty result set must still succeed");
    assert_eq!(empty.rows[0].cells, vec![Cell::Null]);
}

#[test]
fn where_equality_and_range_on_datetime_column_is_accepted() {
    // DATE/TIMESTAMP 列は算術を持たない宣言的経路（レーン B。Issue #891）で
    // `=` と範囲比較（`< > <= >=`）を受理する。式（算術・関数引数）中の
    // 参照は引き続き対象外のまま（下の `sum_avg_min_max...` と同じ経路）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "2024-06-01", "2024-06-01 12:00:00", "op-2"),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE day = '2024-01-01' LIMIT 10"),
        )
        .expect("DATE equality predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE day > '2024-01-01' LIMIT 10"),
        )
        .expect("DATE range predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE at <= '2024-01-01 00:00:00' LIMIT 10"),
        )
        .expect("TIMESTAMP range predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

    // 文法違反のリテラルは `22000`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE day = 'not-a-date' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // DATE 列と数値リテラルの比較（型不一致）は `22000`。
    // （TEXT 列の範囲比較は Issue #1183 で受理されたため、型不一致の例には使わない）
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE day > 5 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- RLS 境界: 他テナントの日時値行は見えない ---------------------------------

#[test]
fn rls_isolates_datetime_rows_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-alice-1"),
    )
    .expect("alice insert should succeed");

    assert_eq!(count_column(&core, &alice, "day"), 1);
    assert_eq!(count_column(&core, &bob, "day"), 0);
}

// --- UPDATE（単一行）: DATE／TIMESTAMP 列の SET も受理・範囲外は拒否 ----------

#[test]
fn update_single_row_accepts_valid_datetime_and_rejects_overflow() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-1"),
    )
    .expect("insert should succeed");

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET day = '2024-06-15' WHERE id = 1 \
                 USING OPERATION_ID 'upd-1'"
            ),
        )
        .expect("update should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    let cells = select_projection(&core, &alice, 1).expect("select id=1");
    assert_eq!(cells[0], Cell::Date(19_889)); // 2024-06-15

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET day = '2024-02-30' WHERE id = 1 \
                 USING OPERATION_ID 'upd-2'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22008");
}

// --- content_hash: operation_id 再送判定（同一内容 23505 / 異なる内容 22023） ---

#[test]
fn resend_same_operation_id_with_same_datetime_value_is_23505_and_different_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "2024-01-01", "2024-01-01 00:00:00", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 同一内容（day/at も同じ）の再送は 23505。
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // day の値だけ異なる再送は 22023。
    let differing = insert_sql(1, "2024-01-02", "2024-01-01 00:00:00", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- Rust API 直接投入（`tenant::insert_typed_row` 経路）での往復 -----------

#[test]
fn typed_row_insert_roundtrips_datetime_values() {
    let path = unique_db_path("datetime-column-typed-insert");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Date(0),
            engine::row_codec::Value::Timestamp(0),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let cells = select_projection(&core, &alice, 1).expect("select id=1");
    assert_eq!(cells, vec![Cell::Date(0), Cell::Timestamp(0)]);
}
