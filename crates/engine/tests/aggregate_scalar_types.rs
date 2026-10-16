//! 新スカラー型（`INTEGER`/`BIGINT`/`REAL`/`DOUBLE PRECISION`/`NUMERIC`/`DATE`/
//! `TIMESTAMP`）の `SUM`/`AVG`/`MIN`/`MAX` 集計・`22003` オーバーフロー契約
//! （Issue #892）の結合テスト。ポインタ: `docs/spec/04-behavior/table-schema.md`
//! TABLE-13・`docs/spec/04-behavior/sql-surface.md` SQL-13・SQL-14・
//! `docs/spec/05-tasks.md` TASK-166・TASK-167・TASK-196・TASK-197。
//!
//! `EngineCore::execute_sql`（SQL 経由の集計実行経路。`sql::aggregate`・
//! `sql::group_by`）を通した既存の集計テスト（`tests/sql_aggregate.rs`・
//! `tests/sql_group_by.rs`）と同じ流儀（`unique_db_path`＋`CleanupGuard`）で
//! 検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
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
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("b", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("at", ColumnType::Timestamp, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("aggregate-scalar-types");
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

/// `n`/`b`/`r`/`d`/`price`/`day`/`at` のすべてに値を入れる行を挿入する
/// （NULL にしたい列は呼び出し元が別の SQL を組み立てる）。
#[allow(clippy::too_many_arguments)]
fn insert_full_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    n: i32,
    b: i64,
    r: f32,
    d: f64,
    price: &str,
    day: &str,
    at: &str,
    op_id: &str,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, n, b, r, d, price, day, at) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', {n}, {b}, {r}, {d}, {price}, '{day}', '{at}') \
             USING OPERATION_ID '{op_id}'"
        ),
    )
    .expect("insert should succeed");
}

fn insert_null_row(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, op_id: &str) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', '{lang}') \
             USING OPERATION_ID '{op_id}'"
        ),
    )
    .expect("insert with all-NULL optional columns should succeed");
}

// --- SUM(BIGINT) は numeric(38,0)（Issue #2271・TABLE-13・TASK-330） ---------

#[test]
fn sum_bigint_beyond_i64_returns_numeric_instead_of_22003() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        i64::MAX,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        1,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-2",
    );

    // i64::MAX + 1 は i64 を超えるが、numeric として厳密な値を返す。
    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(b) FROM {TABLE}"))
        .expect("SUM(BIGINT) beyond i64 must return numeric");
    assert_eq!(
        result.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(i128::from(i64::MAX) + 1, 0).unwrap()
        )]
    );
}

/// `SUM(INTEGER)` は `i32` の範囲を超えても、`BIGINT` 相当（`i64`）に収まれば
/// 成功する（D2: 部分和が `i64` を超えても最終値が収まれば成功する契約とは
/// 別に、まず `INTEGER` 単体の値自体が `i32` 範囲でも合計は `i64` として
/// 扱われることを固定する）。
#[test]
fn sum_integer_multi_row_widens_beyond_i32_range() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for i in 0..3u64 {
        insert_full_row(
            &core,
            &alice,
            i + 1,
            "ja",
            i32::MAX,
            1,
            1.0,
            1.0,
            "1.00",
            "2024-01-01",
            "2024-01-01 00:00:00",
            &format!("op-{i}"),
        );
    }

    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(n) FROM {TABLE}"))
        .expect("SUM(INTEGER) exceeding i32 range but within i64 must succeed");
    assert_eq!(
        result.rows[0].cells,
        vec![Cell::SignedInteger(3 * i64::from(i32::MAX))]
    );
}

/// `SUM(<BIGINT>)` の部分和が一時的に `i64` を超えても、最終値が `i64` に
/// 収まれば成功する（D2 の中核契約。累積は `i128` で行う）。
#[test]
fn sum_bigint_transient_overflow_of_partial_sum_still_succeeds_if_final_value_fits() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        i64::MAX,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        1,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-2",
    );
    insert_full_row(
        &core,
        &alice,
        3,
        "ja",
        1,
        -1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-3",
    );

    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(b) FROM {TABLE}"))
        .expect("transient overflow of the partial sum must not fail if the final value fits");
    assert_eq!(
        result.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(i128::from(i64::MAX), 0).unwrap()
        )]
    );
}

// --- 22003: SUM/AVG(NUMERIC) の 38 桁上限 -----------------------------------

#[test]
fn sum_numeric_exceeding_38_digits_is_22003() {
    let path = unique_db_path("aggregate-numeric-overflow");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 38,
                    scale: 0,
                },
                true,
            ),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");

    let max38 = "9".repeat(38);
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, amount) VALUES (1, '[0.1,0.2]', {max38}) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("insert boundary value should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, amount) VALUES (2, '[0.1,0.2]', 1) \
             USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(amount) FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22003");
}

// --- AVG の丸め ---------------------------------------------------------

#[test]
fn avg_integer_divides_and_returns_numeric() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        2,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-2",
    );

    let result = core
        .execute_sql(&alice, &format!("SELECT AVG(n) FROM {TABLE}"))
        .expect("AVG(INTEGER) must succeed");
    // Issue #2270（TABLE-13）: 整数の AVG は numeric。PG 17 の `1.5000000000000000`（小数 16 桁）。
    assert_eq!(
        result.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(15_000_000_000_000_000, 16).expect("valid decimal")
        )]
    );
}

/// `AVG(NUMERIC(p,s))` の丸めは half away from zero（D6）。割り切れない値・
/// 負の値の丸め方向を固定する。
#[test]
fn avg_numeric_rounds_half_away_from_zero_including_negative_values() {
    let path = unique_db_path("aggregate-numeric-avg-rounding");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // `NUMERIC(38, 2)`: 整数部が 36 桁を占めるため、AVG の結果 scale は
    // `max(2, min(16, 38-36)) = 2`（列の scale のまま拡張されない）。丸めが
    // 実際に発生するケースを検証しやすくするため、あえてこの組み合わせを選ぶ
    // （`NUMERIC(p, s)` で `p - s` が大きいほど AVG の結果 scale は列の scale
    // に近づく。D6）。
    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 38,
                    scale: 2,
                },
                true,
            ),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");

    // 0.05 / 2 = 0.025 → half away from zero で 0.03。
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, amount) VALUES (1, '[0.1,0.2]', 0.05) \
             USING OPERATION_ID 'op-1'"
        ),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, amount) VALUES (2, '[0.1,0.2]', 0.00) \
             USING OPERATION_ID 'op-2'"
        ),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(&alice, &format!("SELECT AVG(amount) FROM {TABLE}"))
        .expect("AVG(NUMERIC) must succeed");
    let expected = Decimal::from_parts(3, 2).expect("valid decimal");
    assert_eq!(result.rows[0].cells, vec![Cell::Numeric(expected)]);

    // 負の値も絶対値が大きい方へ丸める: -0.05 / 2 = -0.025 → -0.03。
    let path2 = unique_db_path("aggregate-numeric-avg-rounding-negative");
    let _cleanup2 = CleanupGuard(path2.clone());
    let storage2 = Storage::open(&path2).expect("open storage");
    storage2.create_table(&schema).expect("create table");
    let core2 = EngineCore::from_storage(storage2, Box::new(CpuScalarProvider));
    core2
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, amount) VALUES (1, '[0.1,0.2]', -0.05) \
                 USING OPERATION_ID 'op-1'"
            ),
        )
        .expect("insert should succeed");
    core2
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, amount) VALUES (2, '[0.1,0.2]', 0.00) \
                 USING OPERATION_ID 'op-2'"
            ),
        )
        .expect("insert should succeed");

    let negative_result = core2
        .execute_sql(&alice, &format!("SELECT AVG(amount) FROM {TABLE}"))
        .expect("AVG(NUMERIC) must succeed");
    let expected_negative = Decimal::from_parts(-3, 2).expect("valid decimal");
    assert_eq!(
        negative_result.rows[0].cells,
        vec![Cell::Numeric(expected_negative)]
    );
}

// --- MIN/MAX: 型ごとの順序・NULL 無視・空集合 --------------------------------

#[test]
fn min_max_skip_null_rows_and_empty_set_is_null_for_all_new_types() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        -5,
        -50,
        -1.5,
        -2.5,
        "-1.50",
        "2020-01-01",
        "2020-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        10,
        100,
        3.5,
        4.5,
        "3.50",
        "2025-06-15",
        "2025-06-15 12:00:00",
        "op-2",
    );
    // NULL 行（すべての新型列が NULL）は MIN/MAX に一切影響しない。
    insert_null_row(&core, &alice, 3, "ja", "op-3");

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT MIN(n), MAX(n), MIN(b), MAX(b), MIN(r), MAX(r), MIN(d), MAX(d), \
                 MIN(price), MAX(price), MIN(day), MAX(day), MIN(at), MAX(at) FROM {TABLE}"
            ),
        )
        .expect("MIN/MAX over all new types must succeed");
    assert_eq!(
        result.rows[0].cells,
        vec![
            Cell::SignedInteger(-5),
            Cell::SignedInteger(10),
            Cell::SignedInteger(-50),
            Cell::SignedInteger(100),
            Cell::Float(-1.5),
            Cell::Float(3.5),
            Cell::Float(-2.5),
            Cell::Float(4.5),
            Cell::Numeric(Decimal::from_parts(-150, 2).expect("valid decimal")),
            Cell::Numeric(Decimal::from_parts(350, 2).expect("valid decimal")),
            Cell::Date(engine::datetime::parse_date("2020-01-01").expect("valid date")),
            Cell::Date(engine::datetime::parse_date("2025-06-15").expect("valid date")),
            Cell::Timestamp(
                engine::datetime::parse_timestamp("2020-01-01 00:00:00").expect("valid ts")
            ),
            Cell::Timestamp(
                engine::datetime::parse_timestamp("2025-06-15 12:00:00").expect("valid ts")
            ),
        ]
    );

    // 空集合（可視行が 0 件）はすべて NULL。
    let empty = core
        .execute_sql(
            &alice,
            &format!("SELECT SUM(n), MIN(price), MAX(at) FROM {TABLE} WHERE id = 999"),
        )
        .expect("aggregates over an empty result set must still succeed");
    assert_eq!(
        empty.rows[0].cells,
        vec![Cell::Null, Cell::Null, Cell::Null]
    );
}

#[test]
fn count_new_scalar_types_ignores_null_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_null_row(&core, &alice, 2, "ja", "op-2");

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(n) FROM {TABLE}"))
        .expect("COUNT(INTEGER) must succeed");
    assert_eq!(result.rows[0].cells, vec![Cell::Integer(1)]);
}

// --- RLS: 他テナント行が集計へ影響しない -------------------------------------

#[test]
fn rls_isolation_holds_for_sum_min_max_on_new_scalar_types() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        1,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &bob,
        2,
        "ja",
        1000,
        1000,
        1000.0,
        1000.0,
        "1000.00",
        "2030-01-01",
        "2030-01-01 00:00:00",
        "op-2",
    );

    let alice_sum = core
        .execute_sql(&alice, &format!("SELECT SUM(n) FROM {TABLE}"))
        .expect("alice SUM must succeed");
    assert_eq!(alice_sum.rows[0].cells, vec![Cell::SignedInteger(1)]);

    let alice_max_price = core
        .execute_sql(&alice, &format!("SELECT MAX(price) FROM {TABLE}"))
        .expect("alice MAX(price) must succeed");
    assert_eq!(
        alice_max_price.rows[0].cells,
        vec![Cell::Numeric(
            Decimal::from_parts(100, 2).expect("valid decimal")
        )]
    );

    // 他テナント行を挿入する前後で、bob 不可視のクエリ応答は変わらない
    // （空集合の応答が他テナント行の有無で変わらない。RLS-9・RLS-10）。
    let carol = ctx_for("carol");
    let carol_sum = core
        .execute_sql(&carol, &format!("SELECT SUM(n) FROM {TABLE}"))
        .expect("carol SUM over an empty visible set must succeed");
    assert_eq!(carol_sum.rows[0].cells, vec![Cell::Null]);
}

// --- GROUP BY: 新スカラー型の集計・HAVING・ORDER BY -------------------------

fn group_by_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_group_by_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("aggregate-scalar-types-group-by");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&group_by_schema())
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn insert_group_by_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    n: i32,
    price: &str,
    op_id: &str,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, n, price) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', {n}, {price}) USING OPERATION_ID '{op_id}'"
        ),
    )
    .expect("insert should succeed");
}

#[test]
fn group_by_sum_avg_min_per_group_matches_oracle() {
    let (core, path) = new_group_by_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_group_by_row(&core, &alice, 1, "ja", 1, "1.00", "op-1");
    insert_group_by_row(&core, &alice, 2, "ja", 3, "3.00", "op-2");
    insert_group_by_row(&core, &alice, 3, "en", 10, "10.00", "op-3");

    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT lang, SUM(n) AS s, MIN(n) AS mn FROM {TABLE} GROUP BY lang \
                 ORDER BY lang"
            ),
        )
        .expect("GROUP BY with SUM/MIN(INTEGER) must succeed");
    assert_eq!(result.rows.len(), 2);
    // `ORDER BY lang` 昇順: "en" < "ja"。
    assert_eq!(
        result.rows[0].cells,
        vec![
            Cell::Text("en".to_string()),
            Cell::SignedInteger(10),
            Cell::SignedInteger(10),
        ]
    );
    assert_eq!(
        result.rows[1].cells,
        vec![
            Cell::Text("ja".to_string()),
            Cell::SignedInteger(4),
            Cell::SignedInteger(1),
        ]
    );
}

/// `HAVING SUM(n) > 10` がグループを実際に絞ることを固定する（0 件への退行を
/// 検出する目的の非 vacuous アサーション）。
#[test]
fn group_by_having_on_integer_sum_actually_filters_groups() {
    let (core, path) = new_group_by_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_group_by_row(&core, &alice, 1, "ja", 1, "1.00", "op-1");
    insert_group_by_row(&core, &alice, 2, "en", 100, "100.00", "op-2");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, SUM(n) AS s FROM {TABLE} GROUP BY lang HAVING s > 10"),
        )
        .expect("HAVING on SignedInteger sum must succeed");
    assert_eq!(result.rows.len(), 1, "HAVING must filter out the ja group");
    assert_eq!(
        result.rows[0].cells,
        vec![Cell::Text("en".to_string()), Cell::SignedInteger(100)]
    );
}

/// `HAVING` は `Cell::Numeric`（`NUMERIC` 型集計結果）を対象にできない
/// （D8。`f64` リテラルとの厳密な数値比較に意味論が無いため fail-closed に
/// `22000`）。
#[test]
fn having_on_numeric_aggregate_is_rejected_with_22000() {
    let (core, path) = new_group_by_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_group_by_row(&core, &alice, 1, "ja", 1, "1.00", "op-1");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, SUM(price) AS s FROM {TABLE} GROUP BY lang HAVING s > 1"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

/// `ORDER BY SUM(n)` が実際に並び替えることを固定する（D9。追加した
/// `Cell::SignedInteger` の `cmp_cell_values` 対応が欠けていると常に `Equal`
/// へ落ちて並び替えが黙って効かなくなる）。
#[test]
fn group_by_order_by_signed_integer_sum_actually_sorts() {
    let (core, path) = new_group_by_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_group_by_row(&core, &alice, 1, "a", 5, "1.00", "op-1");
    insert_group_by_row(&core, &alice, 2, "b", 100, "1.00", "op-2");
    insert_group_by_row(&core, &alice, 3, "c", 1, "1.00", "op-3");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, SUM(n) AS s FROM {TABLE} GROUP BY lang ORDER BY s DESC"),
        )
        .expect("ORDER BY on SignedInteger sum must succeed");
    let langs: Vec<&Cell> = result.rows.iter().map(|r| &r.cells[0]).collect();
    assert_eq!(
        langs,
        vec![
            &Cell::Text("b".to_string()),
            &Cell::Text("a".to_string()),
            &Cell::Text("c".to_string()),
        ]
    );
}

// --- スカラー列二次索引の候補削減経路（Issue #475）との整合 ------------------

/// `WHERE lang = 'ja'` 付きの `SUM(n)` を 2 回実行し（cold＝索引未構築・
/// hot＝索引ヒット）、全行走査のオラクルと一致することを固定する
/// （Issue #475 の索引経路が集計値を変えないことの回帰検出）。
#[test]
fn scalar_index_warm_path_sum_matches_full_scan_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for i in 0..5u64 {
        let lang = if i % 2 == 0 { "ja" } else { "en" };
        insert_full_row(
            &core,
            &alice,
            i + 1,
            lang,
            i as i32 + 1,
            1,
            1.0,
            1.0,
            "1.00",
            "2024-01-01",
            "2024-01-01 00:00:00",
            &format!("op-{i}"),
        );
    }

    // オラクル: ja 行は id=1(n=1),3(n=3),5(n=5) → sum=9。
    for _ in 0..2 {
        let result = core
            .execute_sql(
                &alice,
                &format!("SELECT SUM(n) FROM {TABLE} WHERE lang = 'ja'"),
            )
            .expect("WHERE lang = 'ja' with SUM(n) must succeed on both cold and hot runs");
        assert_eq!(result.rows[0].cells, vec![Cell::SignedInteger(9)]);
    }
}
// --- Issue #2271: SUM(BIGINT) → numeric(38,0)（TABLE-13・SQL-13・SQL-30・TASK-330）---

fn dec0(v: i128) -> Cell {
    Cell::Numeric(Decimal::from_parts(v, 0).unwrap())
}

fn insert_b(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, n: i32, b: i64) {
    insert_full_row(
        core,
        ctx,
        id,
        lang,
        n,
        b,
        1.0,
        1.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        &format!("op-b-{id}"),
    );
}

/// 結果列の静的型は `numeric(38,0)`、`SUM(INTEGER)` は `bigint` のまま。
/// 負方向（`i64::MIN` 2 行）も i64 を超えて厳密に返す。
#[test]
fn sum_bigint_column_type_is_numeric_and_integer_stays_bigint() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_b(&core, &alice, 1, "ja", 1, i64::MIN);
    insert_b(&core, &alice, 2, "ja", 2, i64::MIN);

    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(b), SUM(n) FROM {TABLE}"))
        .expect("SUM(BIGINT) below i64::MIN must return numeric");
    assert_eq!(
        result.rows[0].cells,
        vec![dec0(2 * i128::from(i64::MIN)), Cell::SignedInteger(3)]
    );
    let tys: Vec<_> = result
        .columns
        .iter()
        .map(|c| match c {
            engine::sql::exec::ColumnMeta::Computed { ty, .. } => ty.clone(),
            other => panic!("unexpected column meta {other:?}"),
        })
        .collect();
    assert_eq!(
        tys,
        vec![
            Some(ColumnType::Numeric {
                precision: 38,
                scale: 0
            }),
            Some(ColumnType::BigInt)
        ]
    );
}

/// 空集合と NULL のみは NULL。
#[test]
fn sum_bigint_of_empty_and_null_only_is_null() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let empty = core
        .execute_sql(&alice, &format!("SELECT SUM(b) FROM {TABLE}"))
        .unwrap();
    assert_eq!(empty.rows[0].cells, vec![Cell::Null]);
    insert_null_row(&core, &alice, 1, "ja", "op-null-1");
    let nulls = core
        .execute_sql(&alice, &format!("SELECT SUM(b) FROM {TABLE}"))
        .unwrap();
    assert_eq!(nulls.rows[0].cells, vec![Cell::Null]);
}

/// GROUP BY・単純形 HAVING（i64 超の閾値を厳密比較）・ORDER BY が numeric で動く。
#[test]
fn sum_bigint_group_by_having_and_order_by_use_exact_numeric() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_b(&core, &alice, 1, "big", 1, i64::MAX);
    insert_b(&core, &alice, 2, "big", 1, i64::MAX);
    insert_b(&core, &alice, 3, "small", 1, 5);

    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, SUM(b) AS s FROM {TABLE} GROUP BY lang ORDER BY s DESC"),
        )
        .unwrap();
    assert_eq!(
        r.rows.iter().map(|x| x.cells.clone()).collect::<Vec<_>>(),
        vec![
            vec![
                Cell::Text("big".to_string()),
                dec0(2 * i128::from(i64::MAX))
            ],
            vec![Cell::Text("small".to_string()), dec0(5)],
        ]
    );

    // 閾値を超えた（i64 超の和を持つ）グループだけが残る。
    let r = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT lang, SUM(b) AS s FROM {TABLE} GROUP BY lang \
                 HAVING s > 9000000000000000"
            ),
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[0], Cell::Text("big".to_string()));

    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, SUM(b) AS s FROM {TABLE} GROUP BY lang HAVING s <= 5"),
        )
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[0], Cell::Text("small".to_string()));
}

/// 式内での SUM(BIGINT) は numeric 算術未対応のため fail-closed（22000）。
#[test]
fn sum_bigint_inside_expression_is_rejected_fail_closed() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_b(&core, &alice, 1, "ja", 1, 1);
    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(b) + 1 FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

/// ウィンドウ関数の SUM(BIGINT) も numeric（i64 超の累積も値を返す）。
#[test]
fn sum_bigint_window_returns_numeric_beyond_i64() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_b(&core, &alice, 1, "ja", 1, i64::MAX);
    insert_b(&core, &alice, 2, "ja", 1, i64::MAX);
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT id, SUM(b) OVER () FROM {TABLE} LIMIT 10"),
        )
        .unwrap();
    assert_eq!(r.rows.len(), 2);
    for row in &r.rows {
        assert_eq!(row.cells[1], dec0(2 * i128::from(i64::MAX)));
    }
}

/// RLS: 他テナントの行は和に入らない。
#[test]
fn sum_bigint_excludes_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    insert_b(&core, &alice, 1, "ja", 1, 10);
    insert_b(&core, &bob, 2, "ja", 1, i64::MAX);
    let r = core
        .execute_sql(&alice, &format!("SELECT SUM(b) FROM {TABLE}"))
        .unwrap();
    assert_eq!(r.rows[0].cells, vec![dec0(10)]);
}
/// 浮動小数列（REAL/DOUBLE）と `SUM(BIGINT)`（scale 0 numeric）のスカラー
/// サブクエリ比較は、変更前と同じく数値族間比較として受理される（Issue #2271）。
#[test]
fn float_column_compares_with_sum_bigint_scalar_subquery() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        1,
        10,
        5.0,
        5.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        1,
        20,
        50.0,
        50.0,
        "1.00",
        "2024-01-01",
        "2024-01-01 00:00:00",
        "op-2",
    );
    // SUM(b) = 30
    for col in ["r", "d"] {
        let r = core
            .execute_sql(
                &alice,
                &format!(
                    "SELECT id FROM {TABLE} WHERE {col} < (SELECT SUM(b) FROM {TABLE}) LIMIT 10"
                ),
            )
            .expect("float column vs SUM(BIGINT) must be accepted");
        assert_eq!(r.rows.len(), 1, "{col}");
    }
}

/// `IN`／`NOT IN` の内側に集計（`SUM(BIGINT)`）は元々置けない（42601）。SUM の戻り値型
/// 変更（Issue #2271）の前後で挙動が変わらないことを固定する。
#[test]
fn in_subquery_with_sum_bigint_stays_unsupported() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    for kw in ["IN", "NOT IN"] {
        let err = core
            .execute_sql(
                &alice,
                &format!("SELECT id FROM {TABLE} WHERE b {kw} (SELECT SUM(b) FROM {TABLE} LIMIT 10) LIMIT 10"),
            )
            .expect_err("aggregate inside IN subquery is unsupported");
        assert_eq!(err.wire_code(), "42601", "{kw}");
    }
}

/// 整数列・浮動小数列の `IN`／`NOT IN` は、内側が scale 0 の numeric（`SUM(BIGINT)` と
/// 同じ型）でも受理され、numeric 比較の意味論で照合する（Issue #2271）。
#[test]
fn in_and_not_in_accept_scale0_numeric_inner_column() {
    let path = unique_db_path("aggregate-scale0-in");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new(
                    "q",
                    ColumnType::Numeric {
                        precision: 38,
                        scale: 0,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");
    // (id, b, d, q): q は b と一致する行（id 1）・しない行（id 2）・NULL（id 3）・
    // i64 を超える numeric（id 4。どの b・d とも一致しない）。
    for (id, b, d, q) in [
        (1, "10", "10.0", "10"),
        (2, "20", "25.0", "11"),
        (3, "30", "30.0", "NULL"),
        (4, "40", "40.0", "99999999999999999999"),
    ] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO t (id, embedding, b, d, q) VALUES \
                 ({id}, '[0.1,0.2]', {b}, {d}, {q}) USING OPERATION_ID 'op-{id}'"
            ),
        )
        .expect("insert");
    }
    let ids = |sql: &str| -> Vec<u64> {
        let r = core.execute_sql(&alice, sql).expect("must be accepted");
        let mut v: Vec<u64> = r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Integer(n) => *n,
                other => panic!("unexpected cell {other:?}"),
            })
            .collect();
        v.sort_unstable();
        v
    };
    let nn = "SELECT q FROM t WHERE q IS NOT NULL LIMIT 10";
    // 整数列 IN: q の集合は {10, 11, 99999999999999999999}
    assert_eq!(
        ids(&format!("SELECT id FROM t WHERE b IN ({nn}) LIMIT 10")),
        vec![1]
    );
    assert_eq!(
        ids(&format!("SELECT id FROM t WHERE b NOT IN ({nn}) LIMIT 10")),
        vec![2, 3, 4]
    );
    // 浮動小数列 IN／NOT IN
    assert_eq!(
        ids(&format!("SELECT id FROM t WHERE d IN ({nn}) LIMIT 10")),
        vec![1]
    );
    assert_eq!(
        ids(&format!("SELECT id FROM t WHERE d NOT IN ({nn}) LIMIT 10")),
        vec![2, 3, 4]
    );
    // 内側に NULL を含む NOT IN は、真になる行が無い（IN は一致行のみ）。
    let with_null = "SELECT q FROM t LIMIT 10";
    assert_eq!(
        ids(&format!(
            "SELECT id FROM t WHERE b NOT IN ({with_null}) LIMIT 10"
        )),
        Vec::<u64>::new()
    );
    assert_eq!(
        ids(&format!(
            "SELECT id FROM t WHERE d NOT IN ({with_null}) LIMIT 10"
        )),
        Vec::<u64>::new()
    );
    assert_eq!(
        ids(&format!(
            "SELECT id FROM t WHERE b IN ({with_null}) LIMIT 10"
        )),
        vec![1]
    );
    // スカラー比較: i64 を超える numeric との厳密比較（int_compare の範囲外写像）。
    assert_eq!(
        ids("SELECT id FROM t WHERE b < (SELECT q FROM t WHERE id = 4 LIMIT 1) LIMIT 10"),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE b = (SELECT q FROM t WHERE id = 4 LIMIT 1) LIMIT 10"),
        Vec::<u64>::new()
    );
}

/// 整数列と `SUM(BIGINT)` のスカラー比較は、`i64` を超える合計でも厳密に比べる。
#[test]
fn integer_column_compares_with_huge_sum_bigint_scalar_subquery() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 列値は式評価器の正確表現範囲（2^53 未満）に収め、合計だけ i64 を超えさせる
    // ことはできないため 2^54 近傍（2^53 を超える numeric）で厳密比較を確認する。
    insert_b(&core, &alice, 1, "ja", 1, 9_007_199_254_740_991);
    insert_b(&core, &alice, 2, "ja", 1, 9_007_199_254_740_991);
    let lt = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE b < (SELECT SUM(b) FROM {TABLE}) LIMIT 10"),
        )
        .expect("accepted");
    assert_eq!(lt.rows.len(), 2);
    let eq = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE b = (SELECT SUM(b) FROM {TABLE}) LIMIT 10"),
        )
        .expect("accepted");
    assert_eq!(eq.rows.len(), 0);
}

/// 2^53 を超える `SUM(BIGINT)` と REAL／DOUBLE 列のスカラー比較は、float8 と numeric の
/// 比較として受理される（`22003` にならない。Issue #2271。Cursor Bugbot 指摘の回帰）。
#[test]
fn float_column_compares_with_sum_bigint_beyond_2_pow_53() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // 列値は 2^53 未満。3 行の合計 27_000_000_000_000_000 は 2^53 を超える。
    for (id, r, d) in [(1, 1.0, 1.0), (2, 3.0e16, 3.0e16), (3, 2.0e16, 2.0e16)] {
        insert_full_row(
            &core,
            &alice,
            id,
            "ja",
            1,
            9_000_000_000_000_000,
            r,
            d,
            "1.00",
            "2024-01-01",
            "2024-01-01 00:00:00",
            &format!("op-{id}"),
        );
    }
    let count = |sql: String| core.execute_sql(&alice, &sql).expect("accepted").rows.len();
    for col in ["r", "d"] {
        let q = |op: &str| {
            format!("SELECT id FROM {TABLE} WHERE {col} {op} (SELECT SUM(b) FROM {TABLE}) LIMIT 10")
        };
        assert_eq!(count(q("<")), 2, "{col} <");
        assert_eq!(count(q(">")), 1, "{col} >");
        assert_eq!(count(q("<>")), 3, "{col} <>");
    }
}

/// scale 0 の numeric 列が 2^53 を超える値を持つとき、REAL／DOUBLE 列との
/// IN／NOT IN も受理される（float8 へ丸めて比較する）。
#[test]
fn float_column_in_and_not_in_with_scale0_numeric_beyond_2_pow_53() {
    let path = unique_db_path("aggregate-scale0-big-in");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new(
                    "q",
                    ColumnType::Numeric {
                        precision: 38,
                        scale: 0,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");
    for (id, d, r, q) in [
        (1, "1.0e16", "1.0", "10000000000000000"),
        (2, "2.0", "2.0", "10000000000000001"),
    ] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO t (id, embedding, d, r, q) VALUES \
                 ({id}, '[0.1,0.2]', {d}, {r}, {q}) USING OPERATION_ID 'op-{id}'"
            ),
        )
        .expect("insert");
    }
    let count = |sql: &str| core.execute_sql(&alice, sql).expect("accepted").rows.len();
    let inner = "SELECT q FROM t LIMIT 10";
    for col in ["d", "r"] {
        let hits = count(&format!(
            "SELECT id FROM t WHERE {col} IN ({inner}) LIMIT 10"
        ));
        let miss = count(&format!(
            "SELECT id FROM t WHERE {col} NOT IN ({inner}) LIMIT 10"
        ));
        assert_eq!(hits + miss, 2, "{col}");
    }
    // 1.0e16 は numeric 10000000000000000 と float8 として等しい。
    assert_eq!(
        count(&format!("SELECT id FROM t WHERE d IN ({inner}) LIMIT 10")),
        1
    );
    assert_eq!(
        count("SELECT id FROM t WHERE d < (SELECT q FROM t WHERE id = 2 LIMIT 1) LIMIT 10"),
        1
    );
}
