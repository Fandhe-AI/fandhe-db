//! `ARRAY` 列型（TABLE-14・TASK-198、Issue #888）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/data-model.md`
//! TABLE-14・`docs/spec/04-behavior/data-model.md` TABLE-1／TABLE-6／TABLE-7・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-13。
//!
//! `tests/boolean_column.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`、実
//! `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。配列列の往復・
//! リテラル受理・要素数上限・NULL 要素非対応・RLS 境界・検索経路
//! （KNN・`EXPLAIN`）への非影響・集計（`COUNT` のみ）を固定する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
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
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                true,
            ),
            ColumnDef::new(
                "flags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).expect("array ty")),
                true,
            ),
        ],
    )
}

/// 配列列を持たない、それ以外は [`schema`] と同一のテーブル定義（検索経路の
/// 非影響検証用の対照）。
fn baseline_schema() -> TableSchema {
    TableSchema::new(
        "docs_baseline",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("array-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
        .create_table(&baseline_schema())
        .expect("create baseline table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, tags_literal: &str, flags_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, tags, flags) \
         VALUES ({id}, '[0.1,0.2]', '{lang}', '{tags_literal}', '{flags_literal}') \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

// --- 往復（NULL・空配列・引用要素の区別を含む） -------------------------------

#[test]
fn array_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("array-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{a,\"b b\",\"\"}", "{t,f}", 1),
        )
        .expect("insert with array values should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "{}", "{}", 2),
        )
        .expect("insert with empty arrays should succeed");
        // tags/flags 未指定（nullable のため NULL 列として許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-3-3'"
            ),
        )
        .expect("insert without array columns should succeed");
    }

    // 再オープン後も値・型が一致する（NULL 列と空配列 `{}` が区別されること含む）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id, tags, flags FROM {TABLE} LIMIT 100"),
        )
        .expect("select after reopen should succeed");
    let mut by_id: std::collections::BTreeMap<u64, (Cell, Cell)> =
        std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, (row.cells[1].clone(), row.cells[2].clone()));
    }
    match by_id.get(&1) {
        Some((Cell::Array(tags), Cell::Array(flags))) => {
            assert_eq!(
                *tags,
                engine::row_codec::ArrayValue::Text(vec![
                    "a".to_string(),
                    "b b".to_string(),
                    "".to_string()
                ])
            );
            assert_eq!(
                *flags,
                engine::row_codec::ArrayValue::Bool(vec![true, false])
            );
        }
        other => panic!("expected Cell::Array pair for id=1, got {other:?}"),
    }
    match by_id.get(&2) {
        Some((Cell::Array(tags), Cell::Array(flags))) => {
            assert_eq!(*tags, engine::row_codec::ArrayValue::Text(vec![]));
            assert_eq!(*flags, engine::row_codec::ArrayValue::Bool(vec![]));
        }
        other => panic!("expected empty Cell::Array pair for id=2, got {other:?}"),
    }
    assert_eq!(by_id.get(&3), Some(&(Cell::Null, Cell::Null)));
}

// --- 要素数上限・リテラル形式違反・NULL 要素非対応 ---------------------------

#[test]
fn insert_rejects_element_count_exceeding_column_max_len() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    // tags は max_len=4。5 要素は超過。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{a,b,c,d,e}", "{}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "54000");

    // 拒否された INSERT は台帳・テーブル世代を進めない（write txn 開始前に拒否）。
    let count = core
        .execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count should succeed");
    match &count.rows[0].cells[0] {
        Cell::Integer(0) => {}
        other => panic!("expected 0 rows after rejected insert, got {other:?}"),
    }
}

#[test]
fn insert_rejects_null_element_and_malformed_literal() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{a,null,b}", "{}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "a,b,c", "{}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn insert_rejects_invalid_bool_word_for_bool_array_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "{}", "{t,maybe}", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

// --- UPDATE SET / UPSERT ------------------------------------------------------

#[test]
fn update_set_replaces_array_column_value() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{a}", "{t}", 1),
    )
    .expect("seed insert");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET tags = '{{x,y}}' WHERE id = 1 USING OPERATION_ID 'upd-1'"),
    )
    .expect("update should succeed");
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT tags FROM {TABLE} WHERE id = 1 LIMIT 1"),
        )
        .expect("select should succeed");
    match &result.rows[0].cells[0] {
        Cell::Array(v) => assert_eq!(
            *v,
            engine::row_codec::ArrayValue::Text(vec!["x".to_string(), "y".to_string()])
        ),
        other => panic!("expected Cell::Array, got {other:?}"),
    }
}

// --- RLS: 他テナントの配列行が漏えいしないこと --------------------------------

#[test]
fn array_rows_are_isolated_by_tenant() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{secret}", "{t}", 1),
    )
    .expect("alice insert");

    let result = core
        .execute_sql(&bob, &format!("SELECT id, tags FROM {TABLE} LIMIT 100"))
        .expect("bob select should succeed");
    assert!(result.rows.is_empty(), "bob must not see alice's array row");

    let count = core
        .execute_sql(&bob, &format!("SELECT COUNT(tags) FROM {TABLE}"))
        .expect("bob count should succeed");
    match &count.rows[0].cells[0] {
        Cell::Integer(0) => {}
        other => panic!("expected 0 for bob's COUNT(tags), got {other:?}"),
    }
}

// --- 検索経路（KNN・EXPLAIN）への非影響 ---------------------------------------

#[test]
fn vector_search_is_bit_identical_with_and_without_array_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (id, x, y, tags) in [
        (1u64, "0.10", "0.20", "{a,b}"),
        (2, "0.30", "0.10", "{}"),
        (3, "0.05", "0.05", "{c}"),
    ] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, tags) \
                 VALUES ({id}, '[{x},{y}]', 'ja', '{tags}') USING OPERATION_ID 'v-{id}'"
            ),
        )
        .expect("insert into array-column table");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO docs_baseline (id, embedding, lang) VALUES ({id}, '[{x},{y}]', 'ja') \
                 USING OPERATION_ID 'b-{id}'"
            ),
        )
        .expect("insert into baseline table");
    }

    let with_array = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3"),
        )
        .expect("knn on array-column table");
    let baseline = core
        .execute_sql(
            &alice,
            "SELECT id FROM docs_baseline ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3",
        )
        .expect("knn on baseline table");

    let with_array_ids: Vec<u64> = with_array.rows.iter().map(|r| r.id).collect();
    let baseline_ids: Vec<u64> = baseline.rows.iter().map(|r| r.id).collect();
    assert_eq!(
        with_array_ids, baseline_ids,
        "presence of an ARRAY column must not change KNN ordering"
    );

    for (a, b) in with_array.rows.iter().zip(baseline.rows.iter()) {
        assert_eq!(a.score, b.score, "KNN scores must be bit-identical");
    }
}

// --- WHERE 述語・集計の対象範囲（D-A8） ---------------------------------------

#[test]
fn where_referencing_array_column_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE tags = 'x' LIMIT 10"),
        )
        .unwrap_err();
    // 型不一致として拒否される（TEXT 前提の等価述語は ARRAY 列に対して
    // fail-closed に拒否する。D-A8）。
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn count_array_column_is_accepted_but_sum_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "{a}", "{t}", 1),
    )
    .expect("seed insert");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2'"
        ),
    )
    .expect("seed insert without tags");

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(tags) FROM {TABLE}"))
        .expect("COUNT(array column) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(1) => {}
        other => panic!("expected COUNT(tags) = 1, got {other:?}"),
    }

    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(tags) FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}
