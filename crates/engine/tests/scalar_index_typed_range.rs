//! `sql::scalar_index::ScalarIndex` の `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`
//! 列二次索引（`declarative_filter::FilterOp::TypedCompare`。Issue #891・
//! TASK-199 で production 結線済みの WHERE 述語）への候補削減結線
//! （Issue #893）の SQL 表層結合テスト。
//!
//! `tests/scalar_index_prune.rs`（TEXT 等価・前方一致・`id` 範囲。Issue #474）
//! と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋
//! `CpuScalarProvider`、`engine::tenant::insert_typed_row` による投入、
//! `EngineCore::scalar_index_cache_stats()` の `index_scans` カウンタ突合）で、
//! レーン B（`DATE`／`NUMERIC`／`UUID`）の候補削減が下記の契約を満たすことを
//! 固定する:
//!
//! 1. `DATE`／`NUMERIC`／`UUID` 列の範囲比較の cold（索引構築のみ）/hot
//!    （索引消費）結果が完全一致し、hot 実行後は `index_scans` が増える
//! 2. `NUMERIC` はリテラルが列の `scale` より細かい桁を持つ（列の格子に
//!    乗らない）場合でも `numeric::cmp_exact` と一致する候補を返す
//! 3. `TEXT` 等価と `TypedCompare` の複合述語（`IndexConjunction`）でも
//!    正しく交差する
//! 4. RLS: 他テナントの private 行は typed 索引経路でも一切露出しない
//! 5. テーブル世代の進行後、typed 索引は再構築され結果は一貫し続ける
//! 6. `BYTEA` 列の範囲比較も二次索引経路を使い（Issue #1257）、cold／hot の
//!    結果が一致する

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "typed_docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("kind", ColumnType::Text, false),
            ColumnDef::new("dt", ColumnType::Date, false),
            ColumnDef::new(
                "num",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                false,
            ),
            ColumnDef::new("uid", ColumnType::Uuid, false),
            ColumnDef::new("blob", ColumnType::Bytea, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn uuid_for(id: u64) -> engine::uuid::Uuid {
    let mut bytes = [0u8; 16];
    bytes[8..16].copy_from_slice(&id.to_be_bytes());
    engine::uuid::Uuid::from_bytes(bytes)
}

#[allow(clippy::too_many_arguments)]
fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    dt: i32,
    num_unscaled: i128,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
            Value::Date(dt),
            Value::Numeric(
                engine::numeric::Decimal::from_parts(num_unscaled, 2)
                    .expect("valid decimal for scale 2"),
            ),
            Value::Uuid(uuid_for(id)),
            Value::Bytes(vec![id as u8]),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

/// 10 行（`id` 1..=10）。偶数 `id` は `kind = 'a'`、奇数 `id` は `kind = 'b'`。
/// `dt` は `id` 日目（エポック起点）、`num` は `id.00`（`NUMERIC(10,2)`）。
/// 距離は `id` に単調なベクトルにし、`ORDER BY ... LIMIT 20`（全件超）で常に
/// 全一致行を取得できるようにする。
fn seed_ten_rows(storage: &Storage, tenant: &str) {
    let tenant_ctx = ctx(tenant);
    for id in 1..=10u64 {
        let kind = if id % 2 == 0 { "a" } else { "b" };
        insert_row(
            storage,
            &tenant_ctx,
            id,
            [id as f32, 0.0],
            kind,
            id as i32,
            (id as i128) * 100,
            Visibility::Public,
        );
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

/// クエリを 2 回実行し（cold: 索引構築のみ・hot: 索引消費）、結果が完全一致
/// することを固定したうえで、hot 実行後の `index_scans` 増分を返す
/// （`tests/scalar_index_prune.rs::assert_cold_hot_equivalent` と同型）。
fn assert_cold_hot_equivalent(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, tenant, sql);
    let hot = run(core, tenant, sql);
    assert_eq!(
        result_ids(&cold),
        result_ids(&hot),
        "cold/hot results must match exactly for: {sql}"
    );
    let after = core.scalar_index_cache_stats().index_scans;
    after.saturating_sub(before)
}

// --- 契約 1: DATE 範囲述語の cold/hot 等価性 --------------------------------

#[test]
fn date_range_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-typed-range-date");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // エポック（1970-01-01）は day 0 のため 1970-01-08 は day 7。
    // dt >= 7 は id=7..10 に一致する。
    let sql = "SELECT id FROM typed_docs WHERE dt >= '1970-01-08' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a DATE range predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![7, 8, 9, 10]);
}

// --- 契約 1・2: NUMERIC 範囲述語（列より細かいリテラル scale を含む） -------

#[test]
fn numeric_range_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-typed-range-numeric");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM typed_docs WHERE num >= '7.00' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a NUMERIC range predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![7, 8, 9, 10]);
}

#[test]
fn numeric_range_predicate_respects_finer_literal_scale_than_column() {
    let path = unique_db_path("scalar-index-typed-range-numeric-fine-literal");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `num` は `NUMERIC(10,2)`（列の格子は 0.01 単位）。リテラル `7.005`（scale=3）
    // は列の格子に乗らない。`num > 7.005` は 7.00（id=7）を含まず 8.00 以上のみ
    // に一致する（`numeric::cmp_exact` と一致する床方向の判定。
    // `numeric::tests::rescale_bounds_for_column_matches_cmp_exact_oracle` の
    // 単体レベル検証を SQL 表層から裏付ける）。
    let sql = "SELECT id FROM typed_docs WHERE num > '7.005' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(index_scans > 0);
    let result = run(&core, "tenant-a", sql);
    assert_eq!(
        result_ids(&result),
        vec![8, 9, 10],
        "num > 7.005 must exclude 7.00 (the literal's finer scale must not round up)"
    );

    let sql_le = "SELECT id FROM typed_docs WHERE num <= '7.005' \
                  ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let result_le = run(&core, "tenant-a", sql_le);
    assert_eq!(
        result_ids(&result_le),
        vec![1, 2, 3, 4, 5, 6, 7],
        "num <= 7.005 must include 7.00"
    );
}

// --- 契約 1: UUID 等価述語の cold/hot 等価性 --------------------------------

#[test]
fn uuid_equality_predicate_cold_hot_equivalence_and_index_consumption() {
    let path = unique_db_path("scalar-index-typed-range-uuid");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let target = uuid_for(5);
    let sql = format!(
        "SELECT id FROM typed_docs WHERE uid = '{target}' \
         ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20"
    );
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", &sql);
    assert!(
        index_scans > 0,
        "a UUID equality predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", &sql);
    assert_eq!(result_ids(&result), vec![5]);
}

// --- 契約 3: TEXT 等価 + TypedCompare の複合述語 ----------------------------

#[test]
fn conjunction_of_text_equality_and_typed_compare_cold_hot_equivalence() {
    let path = unique_db_path("scalar-index-typed-range-conjunction");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // kind='a'（偶数 id）∧ dt >= 6（id >= 6）。積集合は偶数かつ 6 以上。
    let sql = "SELECT id FROM typed_docs WHERE kind = 'a' AND dt >= '1970-01-07' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a conjunction of TEXT equality and TypedCompare must be consumed by the index"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![6, 8, 10]);
}

// --- 契約 4: RLS ------------------------------------------------------------

#[test]
fn other_tenant_private_rows_never_leak_through_typed_index_path() {
    let path = unique_db_path("scalar-index-typed-range-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    // 別テナントの private 行（同じ `dt` 範囲に一致するはずの id）。
    insert_row(
        &storage,
        &ctx("tenant-b"),
        999,
        [1.0, 0.0],
        "a",
        999,
        99_900,
        Visibility::Private,
    );
    let core = new_core(storage);

    let sql = "SELECT id FROM typed_docs WHERE dt >= '1970-01-08' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let cold = run(&core, "tenant-a", sql);
    let hot = run(&core, "tenant-a", sql);
    assert!(!result_ids(&cold).contains(&999));
    assert!(!result_ids(&hot).contains(&999));
    assert_eq!(result_ids(&hot), vec![7, 8, 9, 10]);
}

// --- 契約 5: テーブル世代の進行 ---------------------------------------------

#[test]
fn typed_index_rebuilds_after_generation_bump_and_results_stay_consistent() {
    let path = unique_db_path("scalar-index-typed-range-generation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    let sql = "SELECT id FROM typed_docs WHERE dt >= '1970-01-08' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    assert_cold_hot_equivalent(&core, "tenant-a", sql);
    let before_builds = core.scalar_index_cache_stats().builds;

    core.execute_insert_sql(
        &ctx("tenant-a"),
        "INSERT INTO typed_docs (id, embedding, kind, dt, num, uid, blob) \
         VALUES (12, '[12.0,0.0]', 'a', '1970-01-13', '12.00', \
         '00000000-0000-0000-0000-00000000000c', '\\x0c') USING OPERATION_ID 'seed-12'",
    )
    .expect("insert row after generation bump");

    let after_write = run(&core, "tenant-a", sql);
    assert!(
        result_ids(&after_write).contains(&12),
        "the newly written row must be visible after the generation bump"
    );
    let after_builds = core.scalar_index_cache_stats().builds;
    assert!(
        after_builds > before_builds,
        "a write must invalidate the stale typed index and trigger a rebuild"
    );

    assert_cold_hot_equivalent(&core, "tenant-a", sql);
}

// --- 契約 6: BYTEA も二次索引経路を使う（Issue #1257） ----------------------

#[test]
fn bytea_typed_compare_uses_index_and_result_matches() {
    let path = unique_db_path("scalar-index-typed-range-bytea-index");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_ten_rows(&storage, "tenant-a");
    let core = new_core(storage);

    // `blob` は `id as u8` の 1 バイト。`blob > '\x07'` は id 8・9・10 に一致
    // する（`OrderedColumnIndex::Bytes` 経由の索引候補）。
    let sql = "SELECT id FROM typed_docs WHERE blob > '\\x07' \
               ORDER BY embedding <=> '[10.0,0.0]' LIMIT 20";
    let index_scans = assert_cold_hot_equivalent(&core, "tenant-a", sql);
    assert!(
        index_scans > 0,
        "a BYTEA range predicate must be consumed by the index on the hot path"
    );
    let result = run(&core, "tenant-a", sql);
    assert_eq!(result_ids(&result), vec![8, 9, 10]);
}
