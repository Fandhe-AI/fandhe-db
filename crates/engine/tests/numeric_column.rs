//! `NUMERIC(p, s)` 列型（TABLE-13〔検討中〕・TASK-197、Issue #885）の結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-197・
//! `docs/spec/04-behavior/data-model.md` TABLE-13。
//!
//! `tests/boolean_column.rs`（Issue #883・TASK-196）と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として
//! 検証）。NUMERIC 列の往復・境界値・丸め規則・桁あふれ拒否・NULL 区別・
//! UPDATE/UPSERT/RETURNING・content_hash 再送判定・RLS 境界・COUNT 集計・
//! SUM/AVG/MIN/MAX・式・GROUP BY の拒否・負数リテラルを固定する。WHERE の
//! 文字列リテラル形等価・範囲比較（TABLE-13・TASK-199、Issue #891・レーン B）
//! は受理する（裸の数値リテラル形は対象外のまま。詳細は
//! `tests/scalar_types_predicates.rs` 参照）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::returning::DmlCommand;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("numeric-column");
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

fn insert_sql(id: u64, lang: &str, price_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, price) VALUES ({id}, '[0.1,0.2]', '{lang}', {price_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn count_star(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count(*) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn count_price(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(price) FROM {TABLE}"))
        .expect("count(price) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn expect_returning(outcome: SqlOutcome) -> engine::sql::exec::ReturningOutcome {
    match outcome {
        SqlOutcome::Returning(o) => o,
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

fn d(unscaled: i128, scale: u8) -> Decimal {
    Decimal::from_parts(unscaled, scale).expect("test scale must be within MAX_PRECISION")
}

// --- 受け入れ条件 1: 宣言 → INSERT → 再オープン → SELECT で値・型が往復する ---

#[test]
fn numeric_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("numeric-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1.50", 1),
        )
        .expect("insert should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "-1.50", 2),
        )
        .expect("insert negative should succeed");
        // NULL 行（price 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-3-3'"
            ),
        )
        .expect("insert without price should succeed");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(&alice, &format!("SELECT id, price FROM {TABLE} LIMIT 100"))
        .expect("select after reopen should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(by_id.get(&1), Some(&Cell::Numeric(d(150, 2))));
    assert_eq!(by_id.get(&2), Some(&Cell::Numeric(d(-150, 2))));
    assert_eq!(by_id.get(&3), Some(&Cell::Null));
}

// --- 受け入れ条件 2: 境界値（NUMERIC(38,0)・NUMERIC(38,38)・NUMERIC(5,2) 境界） ---

fn numeric_schema(precision: u8, scale: u8) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("price", ColumnType::Numeric { precision, scale }, true),
        ],
    )
}

fn new_core_with_precision(precision: u8, scale: u8) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("numeric-column-precision");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&numeric_schema(precision, scale))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

#[test]
fn boundary_numeric_38_0_roundtrips_max_and_min() {
    let (core, path) = new_core_with_precision(38, 0);
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let max = "9".repeat(38);
    let min = format!("-{max}");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &max, 1),
    )
    .expect("max value insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", &min, 2),
    )
    .expect("min value insert should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT id, price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(
        by_id.get(&1),
        Some(&Cell::Numeric(d(
            99_999_999_999_999_999_999_999_999_999_999_999_999i128,
            0
        )))
    );
    assert_eq!(
        by_id.get(&2),
        Some(&Cell::Numeric(d(
            -99_999_999_999_999_999_999_999_999_999_999_999_999i128,
            0
        )))
    );
}

#[test]
fn boundary_numeric_38_38_roundtrips_near_one() {
    let (core, path) = new_core_with_precision(38, 38);
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let text = format!("0.{}", "9".repeat(38));
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &text, 1),
    )
    .expect("scale-38 boundary insert should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    match &result.rows[0].cells[0] {
        Cell::Numeric(v) => assert_eq!(v.scale(), 38),
        other => panic!("expected Cell::Numeric, got {other:?}"),
    }
}

#[test]
fn boundary_numeric_5_2_accepts_max_and_min_magnitude() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "999.99", 1),
    )
    .expect("max magnitude should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "-999.99", 2),
    )
    .expect("min magnitude should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT id, price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(by_id.get(&1), Some(&Cell::Numeric(d(99_999, 2))));
    assert_eq!(by_id.get(&2), Some(&Cell::Numeric(d(-99_999, 2))));
}

// --- 受け入れ条件 3: 丸め表（half away from zero）とゼロ埋め -------------------

#[test]
fn rounds_half_away_from_zero_and_pads_short_fraction() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let cases: &[(&str, i128)] = &[
        ("1.005", 101),
        ("-1.005", -101),
        ("1.004", 100),
        ("1.5", 150),
        ("1", 100),
    ];
    for (idx, (literal, expected)) in cases.iter().enumerate() {
        let id = idx as u64 + 1;
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(id, "ja", literal, id),
        )
        .unwrap_or_else(|e| panic!("insert {literal:?} should succeed: {e:?}"));
        let result = core
            .execute_sql(
                &alice,
                &format!("SELECT price FROM {TABLE} WHERE id = {id} LIMIT 1"),
            )
            .expect("select should succeed");
        assert_eq!(
            result.rows[0].cells[0],
            Cell::Numeric(d(*expected, 2)),
            "literal {literal:?} should round to {expected}"
        );
    }
}

// --- 受け入れ条件 4: 桁あふれは 22003・書き込みの副作用ゼロ --------------------

#[test]
fn overflow_by_rounding_carry_is_rejected_with_no_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // NUMERIC(5,2) の最大は 999.99。丸め後 1000.00 になる入力は桁あふれ。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "999.995", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
    assert_eq!(count_star(&core, &alice), 0, "no row should be inserted");
}

#[test]
fn integer_part_overflow_is_rejected_with_no_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1000.00", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
    assert_eq!(count_star(&core, &alice), 0);
}

// --- 受け入れ条件 5: 形式不正・型不一致は 22P02（形式不正）・22000（型不一致） --------------------------------

#[test]
fn malformed_literals_are_rejected_as_invalid_input() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // Issue #1358: 指数表記 `'1e3'` は受理側へ契約改訂（拒否例から外した）。
    // 桁の無い指数・指数後の小数などの不正形は引き続き 22P02。
    for bad in ["'1e'", "'1e3.5'", "'abc'", "'1.2.3'", "' 1'", "'NaN'"] {
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, "ja", bad, 1),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22P02", "literal {bad:?} should be 22P02");
    }
}

#[test]
fn boolean_literal_for_numeric_column_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "true", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 受け入れ条件 6: NULL と 0 の区別、ADD COLUMN 後の既存行は NULL -----------

#[test]
fn null_and_zero_are_distinguished() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "0", 1),
    )
    .expect("zero insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2-2'"
        ),
    )
    .expect("null insert should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT id, price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(by_id.get(&1), Some(&Cell::Numeric(d(0, 2))));
    assert_eq!(by_id.get(&2), Some(&Cell::Null));
    assert_eq!(count_price(&core, &alice), 1);
}

#[test]
fn add_column_existing_rows_read_as_null() {
    let path = unique_db_path("numeric-column-add-column");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");
    let base_schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&base_schema).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-pre-add'"
            ),
        )
        .expect("insert before ADD COLUMN should succeed");
    }
    let storage = Storage::open(&path).expect("reopen storage for ADD COLUMN");
    storage
        .alter_table_add_column(
            TABLE,
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                true,
            ),
        )
        .expect("ADD COLUMN price should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(&alice, &format!("SELECT price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Null);

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "3.25", 2),
    )
    .expect("insert after ADD COLUMN should succeed");
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT price FROM {TABLE} WHERE id = 2 LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Numeric(d(325, 2)));
}

// --- 受け入れ条件 7: UPDATE SET・UPSERT DO UPDATE SET EXCLUDED.col・RETURNING ---

#[test]
fn update_single_row_set_numeric_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET price = 2.50 WHERE id = 1 USING OPERATION_ID 'op-set-price'"),
    )
    .expect("UPDATE SET price should succeed");
    let result = core
        .execute_sql(&alice, &format!("SELECT price FROM {TABLE} LIMIT 1"))
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Numeric(d(250, 2)));
}

/// 述語つき UPDATE（SQL-19。#871・#1016 で単一行 UPDATE と
/// `merge_encode_scalar_columns` を共有する経路）が SET 対象でない
/// NUMERIC 列を再エンコードしても値を保つことを固定する（Issue #885 の
/// origin/main 取り込み後に追加された `merge_encode_scalar_columns` の
/// `Some(ScalarRef::Numeric(d))` 分岐の到達性検証）。
#[test]
fn predicate_update_on_other_column_preserves_numeric_value() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "3.14", 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-predicate-lang'"
        ),
    )
    .expect("predicate UPDATE on lang should succeed");
    let result = core
        .execute_sql(&alice, &format!("SELECT lang, price FROM {TABLE} LIMIT 1"))
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Text("en".to_string()));
    assert_eq!(
        result.rows[0].cells[1],
        Cell::Numeric(d(314, 2)),
        "NUMERIC column not targeted by SET must survive re-encode via the predicate UPDATE path"
    );
}

#[test]
fn upsert_do_update_set_excluded_numeric_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, price) VALUES (1, '[0.1,0.2]', 'ja', 9.99) \
             ON CONFLICT (id) DO UPDATE SET price = EXCLUDED.price \
             USING OPERATION_ID 'op-upsert-1'"
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    let result = core
        .execute_sql(&alice, &format!("SELECT price FROM {TABLE} LIMIT 1"))
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Numeric(d(999, 2)));
}

#[test]
fn insert_returning_numeric_column_uses_canonical_text_value() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, price) VALUES \
                 (1, '[0.1,0.2]', 'ja', 5) RETURNING price \
                 USING OPERATION_ID 'op-insert-returning'"
            ),
        )
        .expect("INSERT RETURNING should succeed"),
    );
    assert_eq!(outcome.command, DmlCommand::Insert);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows[0].cells[0], Cell::Numeric(d(500, 2)));
}

// --- 受け入れ条件 8: operation_id 再送判定（同一内容 23505 / 異なる内容 22023） ---

#[test]
fn resend_same_operation_id_with_same_numeric_value_is_23505_and_different_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", "1.10", 1).replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 正規化後は同一値になる再送（`1.10` と `1.1`）は 23505（同一内容）。
    let same_value = insert_sql(1, "ja", "1.1", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &same_value)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // 数値だけ異なる再送は 22023。
    let differing = insert_sql(1, "ja", "2.00", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- 受け入れ条件 9: RLS 境界 ---------------------------------------------------

#[test]
fn rls_isolates_numeric_rows_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("alice insert");
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "2.00", 2),
    )
    .expect("bob insert");

    let alice_result = core
        .execute_sql(&alice, &format!("SELECT id FROM {TABLE} LIMIT 100"))
        .expect("select should succeed");
    assert_eq!(
        alice_result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(count_star(&core, &alice), 1);
    assert_eq!(count_star(&core, &bob), 1);

    // 0 件 UPDATE（他テナント所有行）の応答は他テナント行の有無で変わらない
    // （RLS-9・RLS-10）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 2 USING OPERATION_ID 'op-cross'"),
        )
        .expect("cross-tenant UPDATE should succeed with 0 rows");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
}

// --- 受け入れ条件 10: COUNT(numeric) と WHERE・式・GROUP BY の拒否
// （別 Issue #891 の担当であることを固定するテスト。SUM/AVG/MIN/MAX の直接
// 参照は Issue #892 で受理されたため
// `sum_avg_min_max_on_numeric_column_succeed_after_issue_892` 参照）------------

#[test]
fn count_numeric_counts_non_null_rows_only() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("insert with price");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2'"
        ),
    )
    .expect("insert without price");

    assert_eq!(count_star(&core, &alice), 2);
    assert_eq!(count_price(&core, &alice), 1);
}

#[test]
fn sum_avg_min_max_where_and_expr_reject_numeric_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("insert should succeed");

    // WHERE 述語の裸の数値リテラル形（引用符なし `price = 1.00`）は Issue #1430 で
    // 受理され、文字列リテラル形 `price = '1.00'` と同じ行集合を返す
    // （詳細は `tests/sql24_numeric_literal_compare.rs`）。
    let bare = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE price = 1.00 LIMIT 10"),
        )
        .expect("bare numeric literal compare should be accepted");
    let quoted = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE price = '1.00' LIMIT 10"),
        )
        .expect("quoted literal compare should be accepted");
    assert_eq!(bare.rows.len(), 1);
    assert_eq!(bare.rows.len(), quoted.rows.len());

    // 式中の NUMERIC 列参照は対象外（Issue #891）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT vec_norm(price) FROM {TABLE} LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // Issue #1185・SQL-25 (d): NUMERIC 列の `GROUP BY` キーは受理側へ契約改訂した
    // （結果は `tests/sql25_aggregate_order_by_scalar_keys.rs` で固定）。
    core.execute_sql(
        &alice,
        &format!("SELECT price, COUNT(*) FROM {TABLE} GROUP BY price"),
    )
    .expect("NUMERIC GROUP BY key is accepted since Issue #1185");
}

/// `SUM`/`AVG`/`MIN`/`MAX(price)` は Issue #892（D5・D6）で受理された。
/// `SUM`/`MIN`/`MAX` は列の scale（`NUMERIC(5,2)` なら scale=2）のまま、
/// `AVG` は `max(s, min(16, 38-(p-s)))`（本ケースでは 16）まで拡張した scale
/// で返る。
#[test]
fn sum_avg_min_max_on_numeric_column_succeed_after_issue_892() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "1.00", 1),
    )
    .expect("insert row 1 should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "2.50", 2),
    )
    .expect("insert row 2 should succeed");

    let sum = core
        .execute_sql(&alice, &format!("SELECT SUM(price) FROM {TABLE}"))
        .expect("SUM(NUMERIC) must succeed after Issue #892");
    assert_eq!(
        sum.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(350, 2).expect("valid decimal")
        )]
    );

    let avg = core
        .execute_sql(&alice, &format!("SELECT AVG(price) FROM {TABLE}"))
        .expect("AVG(NUMERIC) must succeed after Issue #892");
    assert_eq!(
        avg.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(17_500_000_000_000_000, 16).expect("valid decimal")
        )]
    );

    let min_max = core
        .execute_sql(
            &alice,
            &format!("SELECT MIN(price), MAX(price) FROM {TABLE}"),
        )
        .expect("MIN/MAX(NUMERIC) must succeed after Issue #892");
    assert_eq!(
        min_max.rows[0].cells,
        vec![
            Cell::Numeric(Decimal::from_parts(100, 2).expect("valid decimal")),
            Cell::Numeric(Decimal::from_parts(250, 2).expect("valid decimal")),
        ]
    );

    // 空集合は NULL。
    let empty = core
        .execute_sql(
            &alice,
            &format!("SELECT SUM(price) FROM {TABLE} WHERE id = 999"),
        )
        .expect("SUM over an empty result set must still succeed");
    assert_eq!(empty.rows[0].cells, vec![Cell::Null]);
}

// --- 受け入れ条件 11: 負数リテラル（数値・文字列）が同じ値に束縛される --------

#[test]
fn negative_number_and_string_literal_bind_to_same_value() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "-1.50", 1),
    )
    .expect("negative number literal should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "'-1.50'", 2),
    )
    .expect("negative string literal should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT id, price FROM {TABLE} LIMIT 10"))
        .expect("select should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(by_id.get(&1), Some(&Cell::Numeric(d(-150, 2))));
    assert_eq!(by_id.get(&2), Some(&Cell::Numeric(d(-150, 2))));
}

/// PR #1020 codex-review 指摘対応: 設計 doc（`docs/design/column-type-extension.md`
/// 「#885 追記」節）が受理すると明記した数値リテラル文法
/// `[+-]?(digits)?(\.digits?)?` のうち、字句解析器（`sql::sql::lexer`）が整数から
/// しか開始できず・符号は `sql::allowlist::Parser::expect_literal` が `-` しか
/// 連結しなかったため `+1.5`・`.5`・`5.`（および符号付きの同形）が SQL 経路では
/// `42601`（構文エラー）で拒否されていた不整合を固定する（数値トークンとしての
/// 受理。文字列リテラル形は元々 `numeric::parse_for_column` が直接受理していた）。
#[test]
fn numeric_literal_grammar_accepts_leading_trailing_dot_and_plus_sign() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let cases: &[(u64, &str, i128)] = &[
        (1, "+1.5", 150),
        (2, ".5", 50),
        (3, "5.", 500),
        (4, "-.5", -50),
        (5, "-5.", -500),
        (6, "+.5", 50),
        (7, "+5.", 500),
    ];
    for (id, literal, expected_unscaled) in cases {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(*id, "ja", literal, *id),
        )
        .unwrap_or_else(|err| panic!("literal {literal:?} should be accepted: {err:?}"));
        let result = core
            .execute_sql(
                &alice,
                &format!("SELECT id, price FROM {TABLE} WHERE id = {id} LIMIT 1"),
            )
            .expect("select should succeed");
        assert_eq!(
            result.rows[0].cells[1],
            Cell::Numeric(d(*expected_unscaled, 2)),
            "literal {literal:?} should bind to unscaled {expected_unscaled}"
        );
    }
}

/// 負数リテラル `-5` を非 NUMERIC 列（TEXT）へ与えた場合、従来の構文エラー
/// （`42601`）から束縛時の型不一致（`22000`）へ変わる（D6・#881 と同じ意図の
/// 差分。拒否されること自体は変わらない）。
#[test]
fn negative_literal_on_text_column_is_bind_time_type_mismatch() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', -5) \
                 USING OPERATION_ID 'op-neg-text'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- Rust API 直接投入（`RowInput`／`tenant::insert_typed_row` 経路）での往復 ---

#[test]
fn typed_row_insert_roundtrips_numeric_value() {
    let path = unique_db_path("numeric-column-typed-insert");
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
            engine::row_codec::Value::Numeric(d(1234, 2)),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(&alice, &format!("SELECT price FROM {TABLE} LIMIT 1"))
        .expect("select should succeed");
    assert_eq!(result.rows[0].cells[0], Cell::Numeric(d(1234, 2)));
}

// --- Issue #1358: 指数表記の数値リテラル（数値トークン・文字列リテラル） ----------------

fn price_of(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT price FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    result.rows[0].cells[0].clone()
}

#[test]
fn exponent_literals_bind_to_numeric_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let cases: &[(&str, i128)] = &[
        ("1.5e2", 15000),
        ("-5e-3", -1),
        ("1E+1", 1000),
        ("'1.5e2'", 15000),
    ];
    for (idx, (literal, expected)) in cases.iter().enumerate() {
        let id = idx as u64 + 1;
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(id, "ja", literal, id),
        )
        .unwrap_or_else(|e| panic!("insert {literal:?} should succeed: {e:?}"));
        assert_eq!(
            price_of(&core, &alice, id),
            Cell::Numeric(d(*expected, 2)),
            "literal {literal:?}"
        );
    }
    // UPDATE も同じ束縛経路を共有する。
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET price = 2.5e1 WHERE id = 1 USING OPERATION_ID 'upd-1358-1'"),
    )
    .expect("update with exponent literal should succeed");
    assert_eq!(price_of(&core, &alice, 1), Cell::Numeric(d(2500, 2)));
}

#[test]
fn exponent_literal_overflow_is_22003_with_no_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    for bad in ["1e3", "'1e3'", "9.99995e2"] {
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, "ja", bad, 1),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22003", "literal {bad:?}");
    }
    assert_eq!(count_star(&core, &alice), 0);
}
