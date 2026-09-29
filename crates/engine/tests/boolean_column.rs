//! `BOOLEAN` 列型（TABLE-13・TASK-196、Issue #883）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-196・`docs/spec/04-behavior/data-model.md`
//! TABLE-13・`docs/spec/04-behavior/data-model.md` TABLE-6／TABLE-7・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-13・
//! `docs/spec/04-behavior/nosql.md` NOSQL-17。
//!
//! `tests/sql_update_single_row.rs`・`tests/sql_predicate_dml_exec.rs` と同じ
//! 流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として
//! 検証）。BOOLEAN 列の往復・リテラル受理・WHERE 述語評価・NULL と false の
//! 区別・RLS 境界・UPDATE/DELETE・content_hash 再送判定・スカラー二次索引の
//! 非索引化（plain scan 固定）を固定する。

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

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("boolean-column");
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

fn insert_sql(id: u64, lang: &str, flag_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, flag) VALUES ({id}, '[0.1,0.2]', '{lang}', {flag_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn count_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> u64 {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {predicate}"),
        )
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

/// 広域取得（scan、SQL-15）経由で `WHERE` を満たす行の `id` を取得する。
/// `ORDER BY` を伴わない `SELECT` は scan 形（`LIMIT` 必須）としてのみ受理
/// されるため、常に十分大きい `LIMIT` を付ける。
fn select_ids_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100"),
        )
        .expect("select should succeed");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

// --- 受け入れ条件 1・4: カタログ・行コーデックでの往復（NULL/false/true の区別） ---

#[test]
fn boolean_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("boolean-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        for (id, lit) in [(1u64, "true"), (2, "false")] {
            core.execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(id, "ja", lit, id),
            )
            .expect("insert should succeed");
        }
        // NULL 行（flag 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-3-3'"
            ),
        )
        .expect("insert without flag should succeed");
    }

    // 再オープン後も値・型が一致する。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let result = core
        .execute_sql(&alice, &format!("SELECT id, flag FROM {TABLE} LIMIT 100"))
        .expect("select after reopen should succeed");
    let mut by_id: std::collections::BTreeMap<u64, Cell> = std::collections::BTreeMap::new();
    for row in &result.rows {
        by_id.insert(row.id, row.cells[1].clone());
    }
    assert_eq!(by_id.get(&1), Some(&Cell::Bool(true)));
    assert_eq!(by_id.get(&2), Some(&Cell::Bool(false)));
    assert_eq!(by_id.get(&3), Some(&Cell::Null));
}

// --- 受け入れ条件 2: リテラル受理・型不一致の拒否 -----------------------------

#[test]
fn insert_accepts_true_false_case_insensitively() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    for (id, lit) in [(1u64, "TRUE"), (2, "False"), (3, "true"), (4, "FALSE")] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(id, "ja", lit, id),
        )
        .expect("case-insensitive boolean literal should be accepted");
    }
    assert_eq!(count_where(&core, &alice, "flag"), 2); // id=1,3
    assert_eq!(count_where(&core, &alice, "flag = false"), 2); // id=2,4
}

#[test]
fn insert_rejects_string_or_number_literal_for_boolean_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'true'", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn insert_rejects_boolean_literal_for_text_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', true) \
                 USING OPERATION_ID 'op-1'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 受け入れ条件 3・4: WHERE 述語（裸参照・明示等価）・NULL の非包含 ---------

#[test]
fn where_bare_and_explicit_boolean_predicates_exclude_null_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "true", 1),
    )
    .expect("insert true row");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "false", 2),
    )
    .expect("insert false row");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-3'"
        ),
    )
    .expect("insert null-flag row");

    // 裸参照は true 行だけ。
    assert_eq!(select_ids_where(&core, &alice, "flag"), vec![1]);
    // 明示 false は false 行だけ。NULL 行はどちらにも含まれない。
    assert_eq!(select_ids_where(&core, &alice, "flag = false"), vec![2]);
    assert_eq!(select_ids_where(&core, &alice, "flag = true"), vec![1]);

    // AND と TEXT 等価条件の組み合わせ。
    assert_eq!(
        select_ids_where(&core, &alice, "flag AND lang = 'ja'"),
        vec![1]
    );

    // COUNT(*) と COUNT(flag)（非 NULL 行数）。
    assert_eq!(count_where(&core, &alice, "flag = true"), 1);
    let total = core
        .execute_sql(&alice, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count(*) should succeed");
    match &total.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 3),
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
    let count_flag = core
        .execute_sql(&alice, &format!("SELECT COUNT(flag) FROM {TABLE}"))
        .expect("count(flag) should succeed");
    match &count_flag.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 2), // id=1,2 のみ非 NULL
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

#[test]
fn order_by_distance_and_scan_and_predicate_dml_respect_boolean_predicate() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "true", 1),
    )
    .expect("insert true row");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "false", 2),
    )
    .expect("insert false row");

    // ORDER BY 距離付き検索。
    let result = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT id FROM {TABLE} WHERE flag ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"
            ),
        )
        .expect("distance search with boolean predicate should succeed");
    assert_eq!(
        result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );

    // 広域取得（scan、LIMIT）。
    let scan_result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE flag LIMIT 10"),
        )
        .expect("scan with boolean predicate should succeed");
    assert_eq!(
        scan_result.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![1]
    );

    // 述語 UPDATE ... WHERE flag USING OPERATION_ID。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE flag USING OPERATION_ID 'op-pred-update'"
            ),
        )
        .expect("predicate UPDATE should succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    assert_eq!(count_where(&core, &alice, "lang = 'en'"), 1);

    // 述語 DELETE ... WHERE flag = false USING OPERATION_ID。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE flag = false USING OPERATION_ID 'op-pred-delete'"),
        )
        .expect("predicate DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_where(&core, &alice, "lang = 'ja'"), 0);
}

// --- TEXT 列への BOOLEAN 述語・BOOLEAN 列への TEXT 述語は拒否 ------------------

#[test]
fn where_rejects_type_mismatched_boolean_and_text_predicates() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    // TEXT 列への裸参照（`WHERE lang`）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE lang LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // TEXT 列への `= true`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE lang = true LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // BOOLEAN 列への文字列等価。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE flag = 'x' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // BOOLEAN 列への LIKE。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE flag LIKE 'x%' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    // SUM(flag) は拒否。
    let err = core
        .execute_sql(&alice, &format!("SELECT SUM(flag) FROM {TABLE}"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- UPDATE（単一行）・UPSERT の往復 ------------------------------------------

#[test]
fn update_single_row_set_boolean_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "true", 1),
    )
    .expect("insert true row");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET flag = false WHERE id = 1 USING OPERATION_ID 'op-set-false'"),
    )
    .expect("UPDATE SET flag = false should succeed");
    assert_eq!(select_ids_where(&core, &alice, "flag = false"), vec![1]);
    assert_eq!(
        select_ids_where(&core, &alice, "flag = true"),
        Vec::<u64>::new()
    );
}

#[test]
fn upsert_do_update_set_excluded_boolean_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "true", 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, flag) VALUES (1, '[0.1,0.2]', 'ja', false) \
             ON CONFLICT (id) DO UPDATE SET flag = EXCLUDED.flag \
             USING OPERATION_ID 'op-upsert-1'"
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    assert_eq!(select_ids_where(&core, &alice, "flag = false"), vec![1]);
}

// --- RLS: 2 テナントの混在で他テナント行が混入しない ---------------------------

#[test]
fn rls_isolates_boolean_predicate_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "true", 1),
    )
    .expect("alice insert");
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "true", 2),
    )
    .expect("bob insert");

    assert_eq!(select_ids_where(&core, &alice, "flag"), vec![1]);
    assert_eq!(select_ids_where(&core, &bob, "flag"), vec![2]);
    assert_eq!(count_where(&core, &alice, "flag"), 1);
    assert_eq!(count_where(&core, &bob, "flag"), 1);

    // 0 件 UPDATE/DELETE（他テナント所有・未存在行）の応答は他テナント行の
    // 有無で変わらない（RLS-9・RLS-10）。
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
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-missing'"),
        )
        .expect("nonexistent-row DELETE should succeed with 0 rows");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
}

// --- content_hash: operation_id 再送判定（同一内容 23505 / 異なる内容 22023） ---

#[test]
fn resend_same_operation_id_with_same_boolean_value_is_23505_and_different_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", "true", 1).replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 同一内容（bool 値も同じ）の再送は 23505。
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // bool 値だけ異なる再送は 22023。
    let differing = insert_sql(1, "ja", "false", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- スカラー二次索引: BOOLEAN 述語は plain scan のまま（誤って索引被覆済みと
// 信頼しない）。索引ウォーム後でも結果が索引なし相当と一致することを固定する。

#[test]
fn boolean_predicate_result_is_unaffected_by_scalar_index_warmup() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    for (id, lang, flag) in [(1u64, "ja", "true"), (2, "en", "false"), (3, "ja", "true")] {
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(id, lang, flag, id),
        )
        .expect("insert should succeed");
    }

    // TEXT 列（lang）の等価述語で二次索引をウォームする。
    let _ = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE lang = 'ja' LIMIT 100"),
        )
        .expect("warm scalar index via TEXT predicate");

    // ウォーム後も BOOLEAN 述語の結果は変わらない（索引なしの場合と一致）。
    assert_eq!(select_ids_where(&core, &alice, "flag = true"), vec![1, 3]);
    assert_eq!(count_where(&core, &alice, "flag = true"), 2);
}

// --- Rust API 直接投入（`RowInput`／`tenant::insert_typed_row` 経路）での往復 ---

#[test]
fn typed_row_insert_roundtrips_boolean_value() {
    let path = unique_db_path("boolean-column-typed-insert");
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
            engine::row_codec::Value::Bool(true),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(select_ids_where(&core, &alice, "flag"), vec![1]);
}
