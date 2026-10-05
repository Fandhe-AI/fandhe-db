//! `UUID` 列型（TABLE-13〔検討中〕・TASK-197、Issue #887）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-197・`docs/spec/04-behavior/data-model.md`
//! TABLE-13・TABLE-6・TABLE-7。
//!
//! `tests/bytea_column.rs`（Issue #886）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。`UUID` 列の
//! カタログ・行コーデック往復・厳密リテラル検証（大小文字・nil・全 1・拒否形）・
//! `NULL` との区別・`UPDATE`／UPSERT／`RETURNING`・拒否経路（`WHERE`・`SUM`/`AVG`/
//! `MIN`/`MAX`）・RLS 境界・`operation_id` 再送判定を固定する。

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
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const ALL_ONES_UUID: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("ext_id", ColumnType::Uuid, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("uuid-column");
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

fn insert_sql(id: u64, lang: &str, ext_id_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, ext_id) VALUES ({id}, '[0.1,0.2]', '{lang}', {ext_id_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn select_ext_id(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT ext_id FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

// --- 受け入れ条件 1: カタログ・行コーデックでの往復（NULL・nil・全 1 の区別） ---

#[test]
fn uuid_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("uuid-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'12345678-9abc-def0-1234-56789abcdef0'", 1),
        )
        .expect("insert with lowercase uuid should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "'12345678-9ABC-DEF0-1234-56789ABCDEF0'", 2),
        )
        .expect("insert with uppercase uuid should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(3, "ja", &format!("'{NIL_UUID}'"), 3),
        )
        .expect("insert with nil uuid should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(4, "ja", &format!("'{ALL_ONES_UUID}'"), 4),
        )
        .expect("insert with all-ones uuid should succeed");
        // NULL 行（ext_id 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (5, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-5-5'"
            ),
        )
        .expect("insert without ext_id should succeed");
    }

    // 再オープン後も値・型が一致する。大文字入力は小文字として保存される。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let expected = Cell::Text("12345678-9abc-def0-1234-56789abcdef0".to_string());
    match select_ext_id(&core, &alice, 1) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), "12345678-9abc-def0-1234-56789abcdef0"),
        other => {
            panic!("expected Cell::Uuid, got {other:?} (expected canonical text like {expected:?})")
        }
    }
    match select_ext_id(&core, &alice, 2) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), "12345678-9abc-def0-1234-56789abcdef0"),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
    match select_ext_id(&core, &alice, 3) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), NIL_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
    match select_ext_id(&core, &alice, 4) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), ALL_ONES_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
    assert_eq!(select_ext_id(&core, &alice, 5), Cell::Null);
}

// --- 受け入れ条件 2: 厳密文法の拒否形（22P02）・型不一致（22000） -----------------

#[test]
fn insert_rejects_malformed_uuid_literal_with_22p02() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (label, literal) in [
        (
            "35 chars (too short)",
            "'12345678-9abc-def0-1234-56789abcdef'",
        ),
        (
            "37 chars (too long)",
            "'12345678-9abc-def0-1234-56789abcdef00'",
        ),
        ("braces", "'{12345678-9abc-def0-1234-56789abcdef0}'"),
        (
            "urn prefix",
            "'urn:uuid:12345678-9abc-def0-1234-56789abcdef'",
        ),
        ("no hyphens", "'123456789abcdef0123456789abcdef0123'"),
        (
            "wrong hyphen position",
            "'1234567-89abc-def0-1234-56789abcdef0'",
        ),
        ("non hex digit", "'1234567g-9abc-def0-1234-56789abcdef0'"),
    ] {
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, "ja", literal, 1),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22P02", "case: {label}");
    }

    // 副作用なし（テーブル世代不変・行数不変）を確認する。
    let result = core
        .execute_sql(&alice, &format!("SELECT id FROM {TABLE} LIMIT 100"))
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn insert_rejects_number_or_boolean_literal_for_uuid_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "true", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 受け入れ条件 5: WHERE 等価・範囲比較述語（TABLE-13・TASK-199、Issue #891）は
// 受理・SUM/AVG/MIN/MAX への露出は拒否・COUNT は受理 ------------------------------

#[test]
fn where_equality_predicate_on_uuid_column_is_accepted() {
    // UUID 列は算術を持たない宣言的経路（レーン B。Issue #891）で `=` を
    // 受理する。式（算術・関数引数）中の参照は引き続き対象外のまま
    // （`vec_norm(ext_id)` 等は下の SUM/AVG/MIN/MAX と同じ 22000 経路）。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &format!("'{NIL_UUID}'"), 1),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", &format!("'{ALL_ONES_UUID}'"), 2),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE ext_id = '{NIL_UUID}' LIMIT 10"),
        )
        .expect("UUID equality predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

    // 範囲比較（`< > <= >=`）。UUID の `Ord` はバイト列の辞書順。
    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE ext_id > '{NIL_UUID}' LIMIT 10"),
        )
        .expect("UUID range predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    // 形式不正のリテラルは `22P02`（既存の INSERT リテラルと同じ写像）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE ext_id = 'not-a-uuid' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");

    // UUID 列と数値リテラルの比較（演算子が存在しない。Issue #1408）は `42883`
    // （TEXT 列の範囲比較は Issue #1183 で受理されたため、型不一致の例には使わない）。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE ext_id > 5 LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "42883");
}

#[test]
fn sum_avg_min_max_on_uuid_column_are_rejected_but_count_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &format!("'{NIL_UUID}'"), 1),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2-2'"
        ),
    )
    .expect("insert without ext_id should succeed");

    for func in ["SUM", "AVG", "MIN", "MAX"] {
        let err = core
            .execute_sql(&alice, &format!("SELECT {func}(ext_id) FROM {TABLE}"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42883", "func: {func}");
    }

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(ext_id) FROM {TABLE}"))
        .expect("COUNT(ext_id) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1), // id=1 のみ非 NULL
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- UPDATE（単一行）・UPSERT・RETURNING の往復 --------------------------------

#[test]
fn update_single_row_set_uuid_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &format!("'{NIL_UUID}'"), 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET ext_id = '{ALL_ONES_UUID}' WHERE id = 1 \
             USING OPERATION_ID 'op-set-ext-id'"
        ),
    )
    .expect("UPDATE SET ext_id should succeed");
    match select_ext_id(&core, &alice, 1) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), ALL_ONES_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
}

#[test]
fn upsert_do_update_set_excluded_uuid_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &format!("'{NIL_UUID}'"), 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, ext_id) VALUES (1, '[0.1,0.2]', 'ja', '{ALL_ONES_UUID}') \
             ON CONFLICT (id) DO UPDATE SET ext_id = EXCLUDED.ext_id \
             USING OPERATION_ID 'op-upsert-1'"
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    match select_ext_id(&core, &alice, 1) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), ALL_ONES_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
}

#[test]
fn insert_returning_uuid_column_matches_select() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let result = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, ext_id) VALUES (1, '[0.1,0.2]', 'ja', '{NIL_UUID}') \
                 RETURNING ext_id USING OPERATION_ID 'op-insert-returning'"
            ),
        )
        .expect("INSERT RETURNING should succeed");
    match result {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.result.rows.len(), 1);
            match &o.result.rows[0].cells[0] {
                Cell::Uuid(u) => assert_eq!(u.to_string(), NIL_UUID),
                other => panic!("expected Cell::Uuid, got {other:?}"),
            }
        }
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
    match select_ext_id(&core, &alice, 1) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), NIL_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
}

// --- RLS: 2 テナントの混在で他テナント行が混入しない ---------------------------

#[test]
fn rls_isolates_uuid_rows_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", &format!("'{NIL_UUID}'"), 1),
    )
    .expect("alice insert");
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(2, "ja", &format!("'{ALL_ONES_UUID}'"), 2),
    )
    .expect("bob insert");

    let alice_result = core
        .execute_sql(&alice, &format!("SELECT id, ext_id FROM {TABLE} LIMIT 100"))
        .expect("alice select should succeed");
    assert_eq!(alice_result.rows.len(), 1);
    assert_eq!(alice_result.rows[0].id, 1);

    // 0 件 UPDATE（他テナント所有行）の応答は行の有無・値を漏らさない
    // （RLS-9・RLS-10。エラーメッセージに UUID 値を含まない設計と同じ判断）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET ext_id = '{NIL_UUID}' WHERE id = 2 \
                 USING OPERATION_ID 'op-cross'"
            ),
        )
        .expect("cross-tenant UPDATE should succeed with 0 rows");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    // bob 側の値は変更されていない。
    match select_ext_id(&core, &bob, 2) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), ALL_ONES_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
}

// --- content_hash: operation_id 再送判定（同一内容 23505 / 異なる内容 22023） ---

#[test]
fn resend_same_operation_id_with_same_uuid_value_is_23505_and_different_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", "'12345678-9abc-def0-1234-56789abcdef0'", 1)
        .replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 同一内容（大文字 hex でも同じバイト列）の再送は 23505。
    let same_value_upper = insert_sql(1, "ja", "'12345678-9ABC-DEF0-1234-56789ABCDEF0'", 1)
        .replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &same_value_upper)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // 値が異なる再送は 22023。
    let differing =
        insert_sql(1, "ja", &format!("'{ALL_ONES_UUID}'"), 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- Rust API 直接投入（`RowInput`／`tenant::insert_typed_row` 経路）での往復 ---

#[test]
fn typed_row_insert_roundtrips_uuid_value() {
    let path = unique_db_path("uuid-column-typed-insert");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-1").expect("valid operation_id");
    let uuid_value = engine::uuid::parse_uuid_text(NIL_UUID).expect("valid uuid literal");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Uuid(uuid_value),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match select_ext_id(&core, &alice, 1) {
        Cell::Uuid(u) => assert_eq!(u.to_string(), NIL_UUID),
        other => panic!("expected Cell::Uuid, got {other:?}"),
    }
}
