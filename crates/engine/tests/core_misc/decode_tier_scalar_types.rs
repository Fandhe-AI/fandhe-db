//! 新スカラー型（`INTEGER`/`BIGINT`/`REAL`/`DOUBLE PRECISION`/`BOOLEAN`/`DATE`/
//! `TIMESTAMP`/`NUMERIC`/`BYTEA`/`UUID`/`ARRAY`/`JSON`/`JSONB`/`ENUM`。TABLE-13・
//! TASK-199）が、集計・広域取得の必要列限定デコード（Issue #350・TASK-199・
//! `docs/design/aggregate-decode-skip.md`「#894 追記」節）の 3 段階
//! （`Fast`/`DimAndScalar`/`Embedding`）を跨いでも RLS 可視性・TABLE-12 の
//! テナント整合を維持したまま正しい結果を返すことを固定する（Issue #894）。
//!
//! `tests/aggregate_scalar_types.rs`・`tests/enum_column.rs` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として
//! 検証）。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-12・TABLE-13・
//! `docs/spec/05-tasks.md` TASK-199。

use engine::catalog::{ColumnDef, ColumnType, EnumTypeDef, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};
use std::sync::Arc;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ENUM_TYPE: &str = "mood";
const DIM: usize = 2;

fn schema_with(def: Arc<EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), true), // 0
            ColumnDef::new("lang", ColumnType::Text, false),                   // 1
            ColumnDef::new("i", ColumnType::Integer, true),                    // 2
            ColumnDef::new("u", ColumnType::Uuid, true),                       // 3
            ColumnDef::new("dt", ColumnType::Date, true),                      // 4
            ColumnDef::new(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ), // 5
            ColumnDef::new("mood", ColumnType::Enum(def), true),               // 6
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("decode-tier-scalar-types");
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(
            ENUM_TYPE,
            vec![
                "happy".to_string(),
                "sad".to_string(),
                "neutral".to_string(),
            ],
        )
        .expect("create enum type");
    storage
        .create_table(&schema_with(def))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

#[allow(clippy::too_many_arguments)]
fn insert_full_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    i: i32,
    uuid_literal: &str,
    date_literal: &str,
    numeric_literal: &str,
    mood_literal: &str,
    op_id: &str,
) {
    core.execute_sql_in_session(
        ctx,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, i, u, dt, n, mood) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', {i}, '{uuid_literal}', '{date_literal}', \
             {numeric_literal}, '{mood_literal}') USING OPERATION_ID '{op_id}'"
        ),
    )
    .expect("insert should succeed");
}

fn insert_null_optional_row(
    core: &EngineCore,
    ctx: &PolicyContext,
    id: u64,
    lang: &str,
    op_id: &str,
) {
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

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(n) => *n,
        other => panic!("expected Integer cell, got {other:?}"),
    }
}

// --- 受入条件 1・3: Fast/DimAndScalar/Embedding 3 段の正しさ -----------------

#[test]
fn count_star_count_uuid_min_date_sum_integer_match_visible_only_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    // alice の可視行（Public）: id=1..=3。
    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        10,
        "00000000-0000-0000-0000-000000000001",
        "2024-01-01",
        "1.10",
        "happy",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        20,
        "00000000-0000-0000-0000-000000000002",
        "2024-06-01",
        "2.20",
        "sad",
        "op-2",
    );
    // id=3 は新型列すべて NULL（TABLE-5 相当の未設定行と同じく NULL 規約に従う）。
    insert_null_optional_row(&core, &alice, 3, "en", "op-3");

    // bob の可視行（alice からは不可視）。
    insert_full_row(
        &core,
        &bob,
        100,
        "en",
        99,
        "00000000-0000-0000-0000-0000000000ff",
        "2020-01-01",
        "9.99",
        "neutral",
        "op-bob-1",
    );

    // Fast tier: COUNT(*)（新型列・VECTOR いずれも非参照）。
    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("COUNT(*) should succeed");
    assert_eq!(as_integer(&result.rows[0].cells[0]), 3);

    // DimAndScalar tier: COUNT(u)（新型列参照、embedding は不要）。
    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(u) FROM {TABLE}"))
        .expect("COUNT(u) should succeed");
    assert_eq!(
        as_integer(&result.rows[0].cells[0]),
        2,
        "id=3 has NULL u and must not be counted"
    );

    // DimAndScalar tier: MIN(dt)。
    let result = core
        .execute_sql(&alice, &format!("SELECT MIN(dt) FROM {TABLE}"))
        .expect("MIN(dt) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Date(days) => assert_eq!(
            *days,
            engine::datetime::parse_date("2024-01-01").expect("date")
        ),
        other => panic!("expected Date cell, got {other:?}"),
    }

    // DimAndScalar tier: SUM(i)。
    let result = core
        .execute_sql(&alice, &format!("SELECT SUM(i) FROM {TABLE}"))
        .expect("SUM(i) should succeed");
    match &result.rows[0].cells[0] {
        Cell::SignedInteger(n) => assert_eq!(*n, 30),
        other => panic!("expected SignedInteger cell, got {other:?}"),
    }

    // bob の可視集合は他テナントの値・件数を一切反映しない（RLS-7・RLS-8）。
    let result = core
        .execute_sql(&bob, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("COUNT(*) should succeed");
    assert_eq!(as_integer(&result.rows[0].cells[0]), 1);
}

// --- 受入条件 3: GROUP BY（TEXT キー）＋新型集計 -----------------------------

#[test]
fn group_by_lang_with_new_type_aggregate_matches_visible_only_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        10,
        "00000000-0000-0000-0000-000000000001",
        "2024-01-01",
        "1.10",
        "happy",
        "op-1",
    );
    insert_full_row(
        &core,
        &alice,
        2,
        "ja",
        20,
        "00000000-0000-0000-0000-000000000002",
        "2024-06-01",
        "2.20",
        "sad",
        "op-2",
    );
    insert_full_row(
        &core,
        &alice,
        3,
        "en",
        30,
        "00000000-0000-0000-0000-000000000003",
        "2023-01-01",
        "3.30",
        "neutral",
        "op-3",
    );

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT lang, COUNT(u) FROM {TABLE} GROUP BY lang"),
        )
        .expect("GROUP BY should succeed");

    let mut counts = std::collections::BTreeMap::new();
    for row in &result.rows {
        let lang = match &row.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("expected Text cell, got {other:?}"),
        };
        counts.insert(lang, as_integer(&row.cells[1]));
    }
    assert_eq!(counts.get("ja"), Some(&2));
    assert_eq!(counts.get("en"), Some(&1));
}

// --- 受入条件 2・3: 広域取得（scan の DimAndScalar）------------------------

#[test]
fn wide_retrieval_scan_projects_new_type_columns_without_leaking_other_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    insert_full_row(
        &core,
        &alice,
        1,
        "ja",
        10,
        "00000000-0000-0000-0000-000000000001",
        "2024-01-01",
        "1.10",
        "happy",
        "op-1",
    );
    insert_full_row(
        &core,
        &bob,
        2,
        "en",
        20,
        "00000000-0000-0000-0000-000000000002",
        "2024-06-01",
        "2.20",
        "sad",
        "op-2",
    );

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id, i, u, dt FROM {TABLE} LIMIT 10"),
        )
        .expect("wide retrieval should succeed");
    assert_eq!(
        result.rows.len(),
        1,
        "bob's row must not be visible to alice"
    );
    assert_eq!(as_integer(&result.rows[0].cells[0]), 1);
}
