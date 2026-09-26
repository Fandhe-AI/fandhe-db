//! ウィンドウ関数（SQL-30・TASK-214、Issue #930）の結合テスト。
//!
//! `tests/sql_scan.rs`（広域取得。Issue #454）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、公開 API（`EngineCore::execute_sql`）のみを経由する）で検証する。
//! テナント境界の一般化検証は `tests/rls_generalized.rs` が別途担うため、本ファイルは
//! ウィンドウ固有の契約（受理・値・決定性・拒否マトリクス・RLS 不変性・ビュー列
//! スコープ）に焦点を絞る。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("score", ColumnType::Integer, false),
        ],
    )
}

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, id: u64, lang: &str, score: i32) {
    insert_row_with_visibility(storage, ctx, id, lang, score, Visibility::Public);
}

/// `visibility` を明示指定できる版（RLS 不変性テスト用。`Visibility::Public` は
/// テナント越境で可視になる既存契約〔`PolicyContext::is_visible`〕のため、他
/// テナントから不可視であることを検証したい行は `Visibility::Private` で挿入する
/// 必要がある）。
fn insert_row_with_visibility(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    score: i32,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[Value::Text(lang.to_string()), Value::Integer(score)],
        &op_id,
    )
    .expect("insert row");
}

/// `id, lang, score` の 6 行（`ja` 3 行・`en` 3 行、`score` に重複あり）。
fn seed_basic(storage: &Storage, ctx: &PolicyContext) {
    storage.create_table(&schema()).expect("create table");
    insert_row(storage, ctx, 1, "ja", 10);
    insert_row(storage, ctx, 2, "ja", 20);
    insert_row(storage, ctx, 3, "ja", 20);
    insert_row(storage, ctx, 4, "en", 5);
    insert_row(storage, ctx, 5, "en", 15);
    insert_row(storage, ctx, 6, "en", 15);
}

fn expect_query(
    result: Result<engine::sql::exec::QueryResult, engine::sql::allowlist::SqlSurfaceError>,
) -> engine::sql::exec::QueryResult {
    result.expect("statement should succeed")
}

fn cell_int(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// `SUM`/`AVG`/`MIN`/`MAX(<INTEGER 列>)` の結果セル（`Cell::SignedInteger`。
/// `sql::aggregate::Accumulator::finish` の `IntSum`/`IntMin`/`IntMax` 契約と同じ）。
fn cell_signed(cell: &Cell) -> i64 {
    match cell {
        Cell::SignedInteger(v) => *v,
        other => panic!("expected Cell::SignedInteger, got {other:?}"),
    }
}

fn row_map(
    result: &engine::sql::exec::QueryResult,
    id_col: usize,
) -> std::collections::HashMap<u64, &[Cell]> {
    result
        .rows
        .iter()
        .map(|r| {
            let id = match &r.cells[id_col] {
                Cell::Integer(v) => *v,
                other => panic!("expected id column to be Cell::Integer, got {other:?}"),
            };
            (id, r.cells.as_slice())
        })
        .collect()
}

// ---------- 受理と値 ----------

#[test]
fn row_number_partitioned_and_ordered() {
    let path = unique_db_path("window-row-number");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score DESC) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja: id2(20) id3(20) id1(10) -> row_number は id 昇順でタイブレーク
    // (score DESC のみでは 2,3 が同点のため id 昇順で 1,2 を割り当てる)。
    assert_eq!(cell_int(&rows[&2][1]), 1);
    assert_eq!(cell_int(&rows[&3][1]), 2);
    assert_eq!(cell_int(&rows[&1][1]), 3);
    // en: id5(15) id6(15) id4(5)
    assert_eq!(cell_int(&rows[&5][1]), 1);
    assert_eq!(cell_int(&rows[&6][1]), 2);
    assert_eq!(cell_int(&rows[&4][1]), 3);
}

#[test]
fn rank_and_dense_rank_skip_or_not_skip_on_ties() {
    let path = unique_db_path("window-rank");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, RANK() OVER (PARTITION BY lang ORDER BY score DESC), DENSE_RANK() OVER (PARTITION BY lang ORDER BY score DESC) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja: 20,20,10 -> RANK 1,1,3 / DENSE_RANK 1,1,2
    assert_eq!(cell_int(&rows[&2][1]), 1);
    assert_eq!(cell_int(&rows[&3][1]), 1);
    assert_eq!(cell_int(&rows[&1][1]), 3);
    assert_eq!(cell_int(&rows[&2][2]), 1);
    assert_eq!(cell_int(&rows[&3][2]), 1);
    assert_eq!(cell_int(&rows[&1][2]), 2);
}

#[test]
fn count_star_over_partition_matches_partition_size() {
    let path = unique_db_path("window-count-star");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER (PARTITION BY lang) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3, 4, 5, 6] {
        assert_eq!(
            cell_int(&rows[&id][1]),
            3,
            "partition size mismatch for id {id}"
        );
    }
}

#[test]
fn sum_over_order_by_is_cumulative_per_peer_group() {
    let path = unique_db_path("window-sum-cumulative");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, SUM(score) OVER (PARTITION BY lang ORDER BY score) FROM docs LIMIT 10",
    ));
    let rows = row_map(&result, 0);
    // ja ascending: 10, 20, 20 -> cumulative: 10, 50, 50 (peer group 20,20 gets same cumulative sum)
    assert_eq!(cell_signed(&rows[&1][1]), 10);
    assert_eq!(cell_signed(&rows[&2][1]), 50);
    assert_eq!(cell_signed(&rows[&3][1]), 50);
}

#[test]
fn window_over_empty_parens_treats_whole_partition_as_one_peer_group() {
    let path = unique_db_path("window-over-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result =
        expect_query(core.execute_sql(&ctx, "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10"));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3, 4, 5, 6] {
        assert_eq!(cell_int(&rows[&id][1]), 6);
    }
}

#[test]
fn window_item_only_select_has_no_plain_columns() {
    let path = unique_db_path("window-only-select");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10",
    ));
    assert_eq!(result.columns.len(), 1);
    assert!(matches!(result.columns[0], ColumnMeta::Computed { .. }));
    assert_eq!(result.rows.len(), 6);
}

#[test]
fn window_alias_and_position_are_preserved_among_plain_columns() {
    let path = unique_db_path("window-alias-position");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT lang, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score) AS rn, score FROM docs LIMIT 10",
    ));
    assert_eq!(result.columns.len(), 3);
    assert!(matches!(
        result.columns[0],
        ColumnMeta::Scalar {
            ty: ColumnType::Text,
            ..
        }
    ));
    assert!(matches!(result.columns[1], ColumnMeta::Computed { ref name } if name == "rn"));
    assert!(matches!(
        result.columns[2],
        ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        }
    ));
}

#[test]
fn where_combines_with_window_and_filters_output_rows_but_not_window_population() {
    let path = unique_db_path("window-where");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    // WHERE lang = 'ja' で出力は ja の 3 行のみだが、COUNT(*) OVER () は
    // WHERE 適用後の母集合（ja の 3 行のみ）で計算される。
    let result = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER () FROM docs WHERE lang = 'ja' LIMIT 10",
    ));
    assert_eq!(result.rows.len(), 3);
    for row in &result.rows {
        assert_eq!(cell_int(&row.cells[1]), 3);
    }
}

#[test]
fn offset_skips_output_rows_but_window_values_reflect_full_population() {
    let path = unique_db_path("window-offset");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let full =
        expect_query(core.execute_sql(&ctx, "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10"));
    let offset = expect_query(core.execute_sql(
        &ctx,
        "SELECT id, COUNT(*) OVER () FROM docs LIMIT 10 OFFSET 3",
    ));
    assert_eq!(full.rows.len(), 6);
    assert_eq!(offset.rows.len(), 3);
    for row in &offset.rows {
        assert_eq!(cell_int(&row.cells[1]), 6);
    }
}

// ---------- NULL 契約・主要スカラー型のキー・集計引数 ----------
//
// レビュー指摘（Issue #930 最終レビュー 1）: `WindowKeyValue`／`cmp_window_key`／
// `partition_key_bytes`（`sql::window`）の NULL 契約（PARTITION BY で NULL 同値化・
// ORDER BY で NULL は ASC 末尾・DESC 先頭。peer 決定性の詳細は
// `docs/design/window-functions.md` 参照）と、DATE/TIMESTAMP/NUMERIC/UUID/
// BOOLEAN/REAL/DOUBLE/BIGINT をキー・集計引数に使うケースを固定する。

/// 任意のテーブル・列値で 1 行挿入する（`insert_row`／`insert_row_with_visibility`
/// は `TABLE`（`docs`）専用のため、本セクション専用のテーブルに使う汎用版）。
fn insert_values(storage: &Storage, ctx: &PolicyContext, table: &str, id: u64, values: &[Value]) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "test-op-{table}-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(storage, table, ctx, id, Visibility::Public, values, &op_id)
        .expect("insert row");
}

fn cell_float(cell: &Cell) -> f64 {
    match cell {
        Cell::Float(v) => *v,
        other => panic!("expected Cell::Float, got {other:?}"),
    }
}

fn cell_numeric(cell: &Cell) -> Decimal {
    match cell {
        Cell::Numeric(d) => *d,
        other => panic!("expected Cell::Numeric, got {other:?}"),
    }
}

const NULLCTL_TABLE: &str = "nullctl";

fn schema_null_contract() -> TableSchema {
    TableSchema::new(
        NULLCTL_TABLE,
        vec![
            ColumnDef::new("pgrp", ColumnType::Text, true),
            ColumnDef::new("ord", ColumnType::Integer, true),
        ],
    )
}

/// `pgrp`（PARTITION BY 対象）・`ord`（ORDER BY 対象）をそれぞれ独立に NULL 混在
/// させた 5 行（NULL 契約の検証専用。値の意図は各テストのコメント参照）。
fn seed_null_contract(storage: &Storage, ctx: &PolicyContext) {
    storage
        .create_table(&schema_null_contract())
        .expect("create table");
    insert_values(
        storage,
        ctx,
        NULLCTL_TABLE,
        1,
        &[Value::Text("a".to_string()), Value::Integer(10)],
    );
    insert_values(
        storage,
        ctx,
        NULLCTL_TABLE,
        2,
        &[Value::Text("a".to_string()), Value::Integer(20)],
    );
    insert_values(
        storage,
        ctx,
        NULLCTL_TABLE,
        3,
        &[Value::Text("a".to_string()), Value::Null],
    );
    insert_values(
        storage,
        ctx,
        NULLCTL_TABLE,
        4,
        &[Value::Null, Value::Integer(5)],
    );
    insert_values(
        storage,
        ctx,
        NULLCTL_TABLE,
        5,
        &[Value::Null, Value::Integer(15)],
    );
}

#[test]
fn partition_by_null_groups_together_and_order_by_null_sorts_last_asc_first_desc() {
    let path = unique_db_path("window-null-contract");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_null_contract(&storage, &ctx);
    let core = new_core(storage);

    // PARTITION BY NULL 同値化: pgrp="a" の 3 行 (id 1,2,3) は同一パーティション、
    // pgrp=NULL の 2 行 (id 4,5) も NULL 同士で同一パーティションにまとまる
    // （NULL 同士を別パーティションに分けない）。
    let by_partition = expect_query(core.execute_sql(
        &ctx,
        &format!("SELECT id, COUNT(*) OVER (PARTITION BY pgrp) FROM {NULLCTL_TABLE} LIMIT 10"),
    ));
    let rows = row_map(&by_partition, 0);
    for id in [1u64, 2, 3] {
        assert_eq!(cell_int(&rows[&id][1]), 3, "pgrp=a group size, id {id}");
    }
    for id in [4u64, 5] {
        assert_eq!(cell_int(&rows[&id][1]), 2, "pgrp=NULL group size, id {id}");
    }

    // ORDER BY ... ASC: NULL は末尾（PostgreSQL 既定）。非 NULL は昇順
    // 5(id4),10(id1),15(id5),20(id2) の後に NULL(id3) が続く。
    let asc = expect_query(core.execute_sql(
        &ctx,
        &format!("SELECT id, ROW_NUMBER() OVER (ORDER BY ord) FROM {NULLCTL_TABLE} LIMIT 10"),
    ));
    let asc_rows = row_map(&asc, 0);
    assert_eq!(cell_int(&asc_rows[&4][1]), 1);
    assert_eq!(cell_int(&asc_rows[&1][1]), 2);
    assert_eq!(cell_int(&asc_rows[&5][1]), 3);
    assert_eq!(cell_int(&asc_rows[&2][1]), 4);
    assert_eq!(
        cell_int(&asc_rows[&3][1]),
        5,
        "NULL ord must sort last under ASC"
    );

    // ORDER BY ... DESC: NULL は先頭（PostgreSQL 既定）。NULL(id3) の後に
    // 降順 20(id2),15(id5),10(id1),5(id4) が続く。
    let desc = expect_query(core.execute_sql(
        &ctx,
        &format!("SELECT id, ROW_NUMBER() OVER (ORDER BY ord DESC) FROM {NULLCTL_TABLE} LIMIT 10"),
    ));
    let desc_rows = row_map(&desc, 0);
    assert_eq!(
        cell_int(&desc_rows[&3][1]),
        1,
        "NULL ord must sort first under DESC"
    );
    assert_eq!(cell_int(&desc_rows[&2][1]), 2);
    assert_eq!(cell_int(&desc_rows[&5][1]), 3);
    assert_eq!(cell_int(&desc_rows[&1][1]), 4);
    assert_eq!(cell_int(&desc_rows[&4][1]), 5);
}

const TYPED_TABLE: &str = "typed_keys";

fn schema_typed_keys() -> TableSchema {
    TableSchema::new(
        TYPED_TABLE,
        vec![
            ColumnDef::new("grp", ColumnType::Text, false),
            ColumnDef::new("c_big", ColumnType::BigInt, true),
            ColumnDef::new("c_real", ColumnType::Real, true),
            ColumnDef::new("c_double", ColumnType::Double, true),
            ColumnDef::new("c_bool", ColumnType::Boolean, true),
            ColumnDef::new("c_date", ColumnType::Date, true),
            ColumnDef::new("c_ts", ColumnType::Timestamp, true),
            ColumnDef::new(
                "c_num",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("c_uuid", ColumnType::Uuid, true),
        ],
    )
}

fn decimal(unscaled: i128, scale: u8) -> Decimal {
    Decimal::from_parts(unscaled, scale).expect("valid decimal for test")
}

fn uuid_n(n: u8) -> Uuid {
    Uuid::from_bytes([n; 16])
}

/// `grp="a"` の 3 行は各型付き列にすべて異なる非 NULL 値を持ち（SUM で正しく
/// 合算されることの検証・キー列としては行ごとに異なるパーティションになる
/// ことの検証を兼ねる。ただし `c_bool` は取りうる値が 2 通りしかないため
/// 3 行とも同一値にする）。`grp="b"` の 2 行はすべての型付き列が NULL
/// （NULL 同士が 1 パーティションにまとまることの検証）。
fn seed_typed_keys(storage: &Storage, ctx: &PolicyContext) {
    storage
        .create_table(&schema_typed_keys())
        .expect("create table");
    insert_values(
        storage,
        ctx,
        TYPED_TABLE,
        1,
        &[
            Value::Text("a".to_string()),
            Value::BigInt(100),
            Value::Real(1.5),
            Value::Double(10.5),
            Value::Bool(true),
            Value::Date(0),
            Value::Timestamp(0),
            Value::Numeric(decimal(1000, 2)),
            Value::Uuid(uuid_n(1)),
        ],
    );
    insert_values(
        storage,
        ctx,
        TYPED_TABLE,
        2,
        &[
            Value::Text("a".to_string()),
            Value::BigInt(200),
            Value::Real(2.5),
            Value::Double(20.5),
            Value::Bool(true),
            Value::Date(1),
            Value::Timestamp(1_000_000),
            Value::Numeric(decimal(2000, 2)),
            Value::Uuid(uuid_n(2)),
        ],
    );
    insert_values(
        storage,
        ctx,
        TYPED_TABLE,
        3,
        &[
            Value::Text("a".to_string()),
            Value::BigInt(300),
            Value::Real(3.5),
            Value::Double(30.5),
            Value::Bool(true),
            Value::Date(2),
            Value::Timestamp(2_000_000),
            Value::Numeric(decimal(3000, 2)),
            Value::Uuid(uuid_n(3)),
        ],
    );
    for id in [4u64, 5] {
        insert_values(
            storage,
            ctx,
            TYPED_TABLE,
            id,
            &[
                Value::Text("b".to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        );
    }
}

#[test]
fn partition_by_accepts_all_scalar_key_types_and_groups_nulls_together() {
    let path = unique_db_path("window-typed-keys");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_typed_keys(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        &format!(
            "SELECT id, \
             COUNT(*) OVER (PARTITION BY c_big), \
             COUNT(*) OVER (PARTITION BY c_real), \
             COUNT(*) OVER (PARTITION BY c_double), \
             COUNT(*) OVER (PARTITION BY c_bool), \
             COUNT(*) OVER (PARTITION BY c_date), \
             COUNT(*) OVER (PARTITION BY c_ts), \
             COUNT(*) OVER (PARTITION BY c_num), \
             COUNT(*) OVER (PARTITION BY c_uuid) \
             FROM {TYPED_TABLE} LIMIT 10"
        ),
    ));
    let rows = row_map(&result, 0);
    // BIGINT/REAL/DOUBLE/DATE/TIMESTAMP/NUMERIC/UUID 列は行ごとに異なる値
    // なので、id 1..3 はそれぞれ単独パーティション (count=1) になる。
    for id in [1u64, 2, 3] {
        for col in [1usize, 2, 3, 5, 6, 7, 8] {
            assert_eq!(cell_int(&rows[&id][col]), 1, "col {col} id {id}");
        }
        // BOOLEAN は 3 行とも同一値 (true) のため 1 パーティションにまとまる。
        assert_eq!(cell_int(&rows[&id][4]), 3, "c_bool id {id}");
    }
    // NULL の 2 行 (id 4,5) は列を問わず NULL 同士で 1 パーティションにまとまる。
    for id in [4u64, 5] {
        for (col, cell) in rows[&id][1..=8].iter().enumerate() {
            assert_eq!(cell_int(cell), 2, "col {} id {id}", col + 1);
        }
    }
}

#[test]
fn sum_over_numeric_like_types_computes_correct_totals_and_null_group_is_null() {
    let path = unique_db_path("window-typed-sum");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_typed_keys(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        &format!(
            "SELECT id, \
             SUM(c_big) OVER (PARTITION BY grp), \
             SUM(c_real) OVER (PARTITION BY grp), \
             SUM(c_double) OVER (PARTITION BY grp), \
             SUM(c_num) OVER (PARTITION BY grp) \
             FROM {TYPED_TABLE} LIMIT 10"
        ),
    ));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3] {
        assert_eq!(cell_signed(&rows[&id][1]), 600, "SUM(c_big) id {id}");
        assert_eq!(cell_float(&rows[&id][2]), 7.5, "SUM(c_real) id {id}");
        assert_eq!(cell_float(&rows[&id][3]), 61.5, "SUM(c_double) id {id}");
        assert_eq!(
            cell_numeric(&rows[&id][4]),
            decimal(6000, 2),
            "SUM(c_num) id {id}"
        );
    }
    // すべて NULL の grp="b" グループは合算対象が無いため SUM は NULL。
    for id in [4u64, 5] {
        assert!(matches!(rows[&id][1], Cell::Null), "SUM(c_big) id {id}");
        assert!(matches!(rows[&id][2], Cell::Null), "SUM(c_real) id {id}");
        assert!(matches!(rows[&id][3], Cell::Null), "SUM(c_double) id {id}");
        assert!(matches!(rows[&id][4], Cell::Null), "SUM(c_num) id {id}");
    }
}

#[test]
fn min_max_over_date_and_timestamp_computes_correct_bounds() {
    let path = unique_db_path("window-typed-minmax");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_typed_keys(&storage, &ctx);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx,
        &format!(
            "SELECT id, \
             MIN(c_date) OVER (PARTITION BY grp), \
             MAX(c_date) OVER (PARTITION BY grp), \
             MIN(c_ts) OVER (PARTITION BY grp), \
             MAX(c_ts) OVER (PARTITION BY grp) \
             FROM {TYPED_TABLE} LIMIT 10"
        ),
    ));
    let rows = row_map(&result, 0);
    for id in [1u64, 2, 3] {
        assert_eq!(rows[&id][1], Cell::Date(0), "MIN(c_date) id {id}");
        assert_eq!(rows[&id][2], Cell::Date(2), "MAX(c_date) id {id}");
        assert_eq!(rows[&id][3], Cell::Timestamp(0), "MIN(c_ts) id {id}");
        assert_eq!(
            rows[&id][4],
            Cell::Timestamp(2_000_000),
            "MAX(c_ts) id {id}"
        );
    }
    for id in [4u64, 5] {
        assert_eq!(rows[&id][1], Cell::Null, "MIN(c_date) id {id}");
        assert_eq!(rows[&id][2], Cell::Null, "MAX(c_date) id {id}");
        assert_eq!(rows[&id][3], Cell::Null, "MIN(c_ts) id {id}");
        assert_eq!(rows[&id][4], Cell::Null, "MAX(c_ts) id {id}");
    }
}

#[test]
fn rejects_sum_of_non_numeric_column_types() {
    let path = unique_db_path("window-reject-sum-types");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_typed_keys(&storage, &ctx);
    let core = new_core(storage);

    for column in ["c_bool", "c_date", "c_ts", "c_uuid"] {
        expect_rejected(
            &core,
            &ctx,
            &format!("SELECT id, SUM({column}) OVER () FROM {TYPED_TABLE} LIMIT 10"),
            "22000",
        );
    }
}

// ---------- パーティション数上限 ----------

#[test]
fn partition_count_over_max_window_partitions_is_rejected_as_payload_too_large() {
    let path = unique_db_path("window-max-partitions");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let schema = TableSchema::new(
        "wide_partitions",
        vec![ColumnDef::new("k", ColumnType::Text, false)],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = ctx_for("tenant-a");

    // `MAX_WINDOW_PARTITIONS`（`sql::group_by::MAX_GROUPS` = 10,000）を明らかに
    // 超える規模を投入し、fail-closed に `54000` へ落ちることを実データで確認する
    // （`sql_group_by.rs::group_count_over_max_groups_is_rejected_as_payload_too_large`
    // と同方針。境界値ちょうどの検証は `sql::window::limit_tests` が単体テストで
    // 別途担う）。
    const OVER: u64 = 10_001;
    for i in 0..OVER {
        insert_values(
            &storage,
            &ctx,
            "wide_partitions",
            i,
            &[Value::Text(format!("k{i}"))],
        );
    }

    let core = new_core(storage);
    let err = core
        .execute_sql(
            &ctx,
            "SELECT id, COUNT(*) OVER (PARTITION BY k) FROM wide_partitions LIMIT 10",
        )
        .expect_err("exceeding MAX_WINDOW_PARTITIONS must be rejected");
    assert_eq!(err.wire_code(), "54000");
}

// ---------- 決定性 ----------

#[test]
fn row_number_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("window-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);
    let sql = "SELECT id, ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let first = expect_query(core.execute_sql(&ctx, sql));
    let second = expect_query(core.execute_sql(&ctx, sql));
    let first_vals: Vec<(u64, u64)> = first
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    let second_vals: Vec<(u64, u64)> = second
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    assert_eq!(first_vals, second_vals);
}

// ---------- 拒否マトリクス ----------

fn expect_rejected(core: &EngineCore, ctx: &PolicyContext, sql: &str, wire_code: &str) {
    let err = core
        .execute_sql(ctx, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"));
    assert_eq!(err.wire_code(), wire_code, "sql={sql}");
}

#[test]
fn rejects_group_by_combined_with_window() {
    let path = unique_db_path("window-reject-group-by");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT lang, COUNT(*) OVER () FROM docs GROUP BY lang",
        "42601",
    );
}

#[test]
fn rejects_order_by_combined_with_window() {
    let path = unique_db_path("window-reject-order-by");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    // 非 window の検索 SELECT は VECTOR 列を要求するため、ここでは構文段で
    // ウィンドウ項目＋通常の ORDER BY 併用が `42601` になることのみを固定する
    // （テーブルに VECTOR 列が無くても構文検証はテーブル存在確認の前に走る）。
    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER () FROM docs ORDER BY score LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_frame_clause() {
    let path = unique_db_path("window-reject-frame");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY score ROWS UNBOUNDED PRECEDING) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_named_window() {
    let path = unique_db_path("window-reject-named");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER w FROM docs WINDOW w AS (ORDER BY score) LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_distinct_inside_window_aggregate() {
    let path = unique_db_path("window-reject-distinct");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, COUNT(DISTINCT lang) OVER () FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_star_mixed_with_window_item() {
    let path = unique_db_path("window-reject-star-mix");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT *, ROW_NUMBER() OVER () FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_unknown_window_function_name() {
    let path = unique_db_path("window-reject-unknown-func");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, LAG(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_ranking_function_with_argument() {
    let path = unique_db_path("window-reject-ranking-arg");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER(score) OVER (ORDER BY score) FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_where_referencing_window_alias() {
    let path = unique_db_path("window-reject-where-alias");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY score) AS rn FROM docs WHERE rn = 1 LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_using_plan_combined_with_window() {
    let path = unique_db_path("window-reject-using-plan");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER () FROM docs USING PLAN('find docs') LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_explain_combined_with_window() {
    // Issue #922（EXPLAIN の対象文拡大）取り込み時の再配線: `EXPLAIN` は
    // ウィンドウ関数に未対応のまま維持する（`sql::explain` 側に対応する記述が
    // ないため）。`validate_select_statement` が EXPLAIN・非 EXPLAIN 双方から
    // 共有されるようになったことで、`Statement::Scan` の `window_items` が
    // 非空なら EXPLAIN 側で明示的に拒否することを固定する。
    let path = unique_db_path("window-reject-explain");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "EXPLAIN SELECT id, ROW_NUMBER() OVER () FROM docs LIMIT 10",
        "42601",
    );
}

#[test]
fn rejects_sum_of_text_column_type_mismatch() {
    let path = unique_db_path("window-reject-sum-text");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    expect_rejected(
        &core,
        &ctx,
        "SELECT id, SUM(lang) OVER () FROM docs LIMIT 10",
        "22000",
    );
}

#[test]
fn rejects_too_many_partition_by_columns() {
    let path = unique_db_path("window-reject-too-many-keys");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);

    let cols = "lang, ".repeat(9);
    let sql = format!(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY {}score) FROM docs LIMIT 10",
        cols
    );
    expect_rejected(&core, &ctx, &sql, "54000");
}

// ---------- RLS ----------

#[test]
fn window_values_are_invariant_to_other_tenants_rows() {
    // EngineCore は Storage の所有権を取るため、「後から他テナント行を追加する」
    // ことを直接は表現できない。代わりに、同一テナント A のコーパスへ (1)
    // 追加行なし・(2) 大量の他テナント B 行を追加、の 2 通りの DB を独立に構築し、
    // テナント A から見た結果が完全に一致することを検証する（RLS-7・RLS-8 の
    // ウィンドウ版）。
    let ctx_a = ctx_for("tenant-a");
    let sql = "SELECT id, COUNT(*) OVER (), ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let path1 = unique_db_path("window-rls-invariance-a-only");
    let _guard1 = CleanupGuard(path1.clone());
    let storage1 = open_storage(&path1);
    seed_basic(&storage1, &ctx_a);
    let core1 = new_core(storage1);
    let a_only = expect_query(core1.execute_sql(&ctx_a, sql));

    let path2 = unique_db_path("window-rls-invariance-a-and-b");
    let _guard2 = CleanupGuard(path2.clone());
    let storage2 = open_storage(&path2);
    seed_basic(&storage2, &ctx_a);
    let ctx_b = ctx_for("tenant-b");
    for id in 1..=50u64 {
        insert_row_with_visibility(
            &storage2,
            &ctx_b,
            id,
            "ja",
            (id % 7) as i32,
            Visibility::Private,
        );
    }
    let core2 = new_core(storage2);
    let a_and_b = expect_query(core2.execute_sql(&ctx_a, sql));

    let a_only_vals: Vec<(u64, u64, u64)> = a_only
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1]), cell_int(&r.cells[2])))
        .collect();
    let a_and_b_vals: Vec<(u64, u64, u64)> = a_and_b
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1]), cell_int(&r.cells[2])))
        .collect();
    assert_eq!(a_only_vals, a_and_b_vals);
}

#[test]
fn window_values_stay_correct_when_multiple_tenants_share_the_same_row_id() {
    // レビュー指摘（P1・テナント境界）: 行の物理キーは `(tenant_id, id)` だが、
    // 以前の実装はウィンドウ値を疑似列 `id` だけをキーにした `HashMap` に
    // 保存・取得していた。`PolicyContext::is_visible` は他テナントの
    // `Public` 行も可視にするため、異なるテナントに同じ `id` の可視行が
    // あると値が上書きされ、順位・集計値が誤った行に付いてしまう。
    //
    // テナント A・B にそれぞれ `id=1` の `Public` 行を、score を違えて置く
    // （テナント A から見るとどちらも可視で `id` が重複した 2 行になる）。
    // 修正前は評価段の `HashMap<u64, Cell>` が `id=1` の 1 エントリしか
    // 持てず、`ORDER BY score` で最後に処理された行の row_number で
    // 上書きされ、2 行とも同じ row_number（2）になっていた。
    let path = unique_db_path("window-tenant-id-collision");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    insert_row(&storage, &ctx_a, 1, "ja", 100);
    insert_row(&storage, &ctx_b, 1, "ja", 999);
    // テナント B の Private 行（id=2）は引き続き不可視のままであることも
    // あわせて確認する（他テナントの存在情報を漏らさない）。
    insert_row_with_visibility(&storage, &ctx_b, 2, "ja", 1, Visibility::Private);
    let core = new_core(storage);

    let result = expect_query(core.execute_sql(
        &ctx_a,
        "SELECT id, score, ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10",
    ));
    // テナント B の Private 行は不可視のまま（2 行のみ可視）。
    assert_eq!(result.rows.len(), 2);
    // `id` が重複していても row_map（`id` キー）に頼らず、score で行を識別する
    // （row_map 自体が同じ衝突を起こしうるため、本テストでは意図的に使わない）。
    let mut by_score: Vec<(i64, u64)> = result
        .rows
        .iter()
        .map(|r| (cell_signed(&r.cells[1]), cell_int(&r.cells[2])))
        .collect();
    by_score.sort_by_key(|(score, _)| *score);
    assert_eq!(
        by_score,
        vec![(100, 1), (999, 2)],
        "each id=1 row must keep its own row_number instead of being overwritten"
    );
}

// ---------- cursor / COPY TO ----------

#[test]
fn declare_cursor_over_window_select_matches_direct_execution() {
    let path = unique_db_path("window-cursor");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    let ctx = ctx_for("tenant-a");
    seed_basic(&storage, &ctx);
    let core = new_core(storage);
    let window_sql = "SELECT id, ROW_NUMBER() OVER (ORDER BY score) FROM docs LIMIT 10";

    let direct = expect_query(core.execute_sql(&ctx, window_sql));

    let mut session = engine::sql::mode::SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("BEGIN should succeed");
    let declare_sql = format!("DECLARE c CURSOR FOR {window_sql}");
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, &declare_sql)
        .expect("DECLARE CURSOR over window select should succeed");
    let fetched = match core
        .execute_sql_in_txn(&ctx, &mut session, &mut txn, "FETCH 10 FROM c")
        .expect("FETCH should succeed")
    {
        engine::sql::SqlOutcome::Fetch(result) => result,
        other => panic!("expected SqlOutcome::Fetch, got {other:?}"),
    };
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("COMMIT should succeed");

    let direct_vals: Vec<(u64, u64)> = direct
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    let fetched_vals: Vec<(u64, u64)> = fetched
        .rows
        .iter()
        .map(|r| (r.id, cell_int(&r.cells[1])))
        .collect();
    assert_eq!(direct_vals, fetched_vals);
}
