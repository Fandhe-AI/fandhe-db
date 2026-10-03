//! `BYTEA` 列型（TABLE-13・TASK-197、Issue #886）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-197・`docs/spec/04-behavior/data-model.md`
//! TABLE-13・TABLE-6・TABLE-7・`docs/spec/04-behavior/wire-protocol.md`
//! WIRE-13・`docs/spec/04-behavior/nosql.md` NOSQL-17。
//!
//! `tests/boolean_column.rs`（Issue #883）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。`BYTEA` 列の
//! カタログ・行コーデック往復・hex リテラル受理（大小文字・空・拒否形）・
//! `NULL` と空バイト列の区別・`UPDATE`／UPSERT／`RETURNING`・拒否経路
//! （述語・集計・式評価）・RLS 境界・`operation_id` 再送判定を固定する。

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
            ColumnDef::new("blob", ColumnType::Bytea, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("bytea-column");
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

fn insert_sql(id: u64, lang: &str, blob_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, blob) VALUES ({id}, '[0.1,0.2]', '{lang}', {blob_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn select_blob(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT blob FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

// --- 受け入れ条件 1: カタログ・行コーデックでの往復（NULL/空/大小文字の区別） ---

#[test]
fn bytea_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("bytea-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'\\xDEADBEEF'", 1),
        )
        .expect("insert with uppercase hex should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "'\\xdeadbeef'", 2),
        )
        .expect("insert with lowercase hex should succeed");
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(3, "ja", "'\\x'", 3),
        )
        .expect("insert with empty bytea should succeed");
        // NULL 行（blob 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (4, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-4-4'"
            ),
        )
        .expect("insert without blob should succeed");
    }

    // 再オープン後も値・型が一致する。大文字・小文字 hex は同じバイト列になる。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        select_blob(&core, &alice, 1),
        Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );
    assert_eq!(
        select_blob(&core, &alice, 2),
        Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );
    // 空バイト列と NULL は区別される。
    assert_eq!(select_blob(&core, &alice, 3), Cell::Bytes(Vec::new()));
    assert_eq!(select_blob(&core, &alice, 4), Cell::Null);
}

// --- 受け入れ条件 2: hex リテラルの拒否形・型不一致 ---------------------------

#[test]
fn insert_rejects_malformed_hex_literal() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (label, literal) in [
        ("missing prefix", "'deadbeef'"),
        ("odd digit count", "'\\xabc'"),
        ("non hex digit", "'\\xzz'"),
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
}

#[test]
fn insert_rejects_number_or_boolean_literal_for_bytea_column() {
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
// 受理・集計・式評価への露出は拒否 --------------------------------------------

#[test]
fn where_equality_predicate_on_bytea_column_is_accepted() {
    // BYTEA 列は算術を持たない宣言的経路（レーン B。Issue #891）で `=` と
    // 範囲比較（辞書順）を受理する。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'\\xdead'", 1),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "'\\xff'", 2),
    )
    .expect("insert should succeed");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE blob = '\\xdead' LIMIT 10"),
        )
        .expect("BYTEA equality predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE blob > '\\xdead' LIMIT 10"),
        )
        .expect("BYTEA range predicate should be accepted");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(2));

    // 形式不正のリテラル（接頭辞なし）は `22P02`。
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE blob = 'deadbeef' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn sum_avg_min_max_on_bytea_column_are_rejected_but_count_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'\\xdead'", 1),
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
    .expect("insert without blob should succeed");

    for func in ["SUM", "AVG", "MIN", "MAX"] {
        let err = core
            .execute_sql(&alice, &format!("SELECT {func}(blob) FROM {TABLE}"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42883", "func: {func}");
    }

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(blob) FROM {TABLE}"))
        .expect("COUNT(blob) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1), // id=1 のみ非 NULL
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- UPDATE（単一行）・UPSERT・RETURNING の往復 --------------------------------

#[test]
fn update_single_row_set_bytea_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'\\x00'", 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "UPDATE {TABLE} SET blob = '\\x00ff' WHERE id = 1 USING OPERATION_ID 'op-set-blob'"
        ),
    )
    .expect("UPDATE SET blob should succeed");
    assert_eq!(select_blob(&core, &alice, 1), Cell::Bytes(vec![0x00, 0xff]));
}

#[test]
fn upsert_do_update_set_excluded_bytea_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'\\x00'", 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, blob) VALUES (1, '[0.1,0.2]', 'ja', '\\xff') \
             ON CONFLICT (id) DO UPDATE SET blob = EXCLUDED.blob \
             USING OPERATION_ID 'op-upsert-1'"
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    assert_eq!(select_blob(&core, &alice, 1), Cell::Bytes(vec![0xff]));
}

#[test]
fn insert_returning_bytea_column_matches_select() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let result = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, blob) VALUES (1, '[0.1,0.2]', 'ja', '\\xdeadbeef') \
                 RETURNING blob USING OPERATION_ID 'op-insert-returning'"
            ),
        )
        .expect("INSERT RETURNING should succeed");
    match result {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.result.rows.len(), 1);
            assert_eq!(
                o.result.rows[0].cells[0],
                Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
            );
        }
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
    assert_eq!(
        select_blob(&core, &alice, 1),
        Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
    );
}

// --- RLS: 2 テナントの混在で他テナント行が混入しない ---------------------------

#[test]
fn rls_isolates_bytea_rows_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'\\xaa'", 1),
    )
    .expect("alice insert");
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "'\\xbb'", 2),
    )
    .expect("bob insert");

    let alice_result = core
        .execute_sql(&alice, &format!("SELECT id, blob FROM {TABLE} LIMIT 100"))
        .expect("alice select should succeed");
    assert_eq!(alice_result.rows.len(), 1);
    assert_eq!(alice_result.rows[0].id, 1);

    // 0 件 UPDATE（他テナント所有行）の応答は行の有無・値を漏らさない
    // （RLS-9・RLS-10。エラーメッセージにバイト列を含まない設計と同じ判断）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET blob = '\\x00' WHERE id = 2 USING OPERATION_ID 'op-cross'"
            ),
        )
        .expect("cross-tenant UPDATE should succeed with 0 rows");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    // bob 側の値は変更されていない。
    assert_eq!(select_blob(&core, &bob, 2), Cell::Bytes(vec![0xbb]));
}

// --- content_hash: operation_id 再送判定（同一内容 23505 / 異なる内容 22023） ---

#[test]
fn resend_same_operation_id_with_same_bytea_value_is_23505_and_different_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", "'\\xdead'", 1).replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 同一内容（大文字 hex でも同じバイト列）の再送は 23505。
    let same_bytes_upper = insert_sql(1, "ja", "'\\xDEAD'", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &same_bytes_upper)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // バイト列が異なる再送は 22023。
    let differing = insert_sql(1, "ja", "'\\xbeef'", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- Rust API 直接投入（`RowInput`／`tenant::insert_typed_row` 経路）での往復 ---

#[test]
fn typed_row_insert_roundtrips_bytea_value() {
    let path = unique_db_path("bytea-column-typed-insert");
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
            engine::row_codec::Value::Bytes(vec![0x01, 0x02, 0x03]),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        select_blob(&core, &alice, 1),
        Cell::Bytes(vec![0x01, 0x02, 0x03])
    );
}
