//! `INSERT ... VALUES` の `NULL` リテラル受理（SQL-41・Issue #1523）の結合テスト。
//! ポインタ: `docs/design/not-null-default.md`・`docs/design/implementation-status.md`。
//!
//! 構文受理は `sql::allowlist`、束縛（nullable なら `Value::Null`・非 nullable なら
//! `23502`・`DEFAULT` 非適用）は `sql::parser::bind_insert_row` の既存契約で、本ファイルは
//! 実 `Storage` を通した端到端（NULLS DISTINCT の UNIQUE・省略列との読み出し一致を含む）
//! を固定する。`sql_not_null_default.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let (core, path) = new_core(label);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), note TEXT, req TEXT NOT NULL, \
         u TEXT UNIQUE, d TEXT DEFAULT 'x')",
    )
    .expect("create table");
    (core, path, session)
}

const VEC: &str = "'[0.1,0.2,0.3,0.4]'";

fn insert(core: &EngineCore, cols: &str, vals: &str, op: &str) -> Result<(), String> {
    core.execute_insert_sql(
        &ctx(),
        &format!("INSERT INTO docs ({cols}) VALUES {vals} USING OPERATION_ID '{op}'"),
    )
    .map(|_| ())
    .map_err(|e| e.wire_code().to_string())
}

fn select(core: &EngineCore, session: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    match core
        .execute_sql_in_session(&ctx(), session, sql)
        .expect("select ok")
    {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        _ => panic!("expected Query outcome"),
    }
}

#[test]
fn explicit_null_into_nullable_column_succeeds_and_reads_back_as_null() {
    let (core, path, mut session) = setup("insert-null-ok");
    let _guard = CleanupGuard(path);
    insert(
        &core,
        "id, embedding, note, req",
        &format!("(1, {VEC}, NULL, 'r')"),
        "op-1",
    )
    .expect("explicit NULL into nullable column");
    // 列を省略した行と読み出し結果が一致する（格納表現が同じ）。
    insert(
        &core,
        "id, embedding, req",
        &format!("(2, {VEC}, 'r')"),
        "op-2",
    )
    .expect("omitted nullable column");
    let rows = select(&core, &mut session, "SELECT note FROM docs LIMIT 10");
    assert_eq!(rows.len(), 2);
    assert!(matches!(rows[0][0], Cell::Null), "{:?}", rows[0]);
    assert!(matches!(rows[1][0], Cell::Null), "{:?}", rows[1]);
    let hits = select(
        &core,
        &mut session,
        "SELECT id FROM docs WHERE note IS NULL LIMIT 10",
    );
    assert_eq!(hits.len(), 2);
}

#[test]
fn explicit_null_into_not_null_column_is_23502() {
    let (core, path, _session) = setup("insert-null-not-null");
    let _guard = CleanupGuard(path);
    let err = insert(
        &core,
        "id, embedding, req",
        &format!("(1, {VEC}, NULL)"),
        "op-1",
    )
    .unwrap_err();
    assert_eq!(err, "23502");
}

#[test]
fn explicit_null_into_declared_primary_key_column_is_23502() {
    let (core, path) = new_core("insert-null-pk");
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), k TEXT PRIMARY KEY)",
    )
    .expect("create pk table");
    let err = core
        .execute_insert_sql(
            &ctx(),
            &format!(
                "INSERT INTO docs (id, embedding, k) VALUES (1, {VEC}, NULL) USING OPERATION_ID 'op-pk'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn null_in_unique_column_is_nulls_distinct_but_unique_still_enforced() {
    let (core, path, _session) = setup("insert-null-unique");
    let _guard = CleanupGuard(path);
    insert(
        &core,
        "id, embedding, req, u",
        &format!("(1, {VEC}, 'r', NULL)"),
        "op-1",
    )
    .expect("first NULL");
    insert(
        &core,
        "id, embedding, req, u",
        &format!("(2, {VEC}, 'r', NULL)"),
        "op-2",
    )
    .expect("second NULL must not conflict");
    insert(
        &core,
        "id, embedding, req, u",
        &format!("(3, {VEC}, 'r', 'dup')"),
        "op-3",
    )
    .expect("first non-NULL");
    let err = insert(
        &core,
        "id, embedding, req, u",
        &format!("(4, {VEC}, 'r', 'dup')"),
        "op-4",
    )
    .unwrap_err();
    assert_eq!(err, "23505");
}

#[test]
fn explicit_null_does_not_apply_default() {
    let (core, path, mut session) = setup("insert-null-default");
    let _guard = CleanupGuard(path);
    insert(
        &core,
        "id, embedding, req, d",
        &format!("(1, {VEC}, 'r', NULL)"),
        "op-1",
    )
    .expect("explicit NULL on DEFAULT column");
    let rows = select(&core, &mut session, "SELECT d FROM docs LIMIT 10");
    assert!(matches!(rows[0][0], Cell::Null), "{:?}", rows[0]);
}

#[test]
fn multi_row_values_may_mix_null_in_some_rows() {
    let (core, path, mut session) = setup("insert-null-multirow");
    let _guard = CleanupGuard(path);
    insert(
        &core,
        "id, embedding, note, req",
        &format!("(1, {VEC}, NULL, 'a'), (2, {VEC}, 'n', 'b'), (3, {VEC}, null, 'c')"),
        "op-1",
    )
    .expect("multi-row with NULLs");
    let hits = select(
        &core,
        &mut session,
        "SELECT id FROM docs WHERE note IS NULL LIMIT 10",
    );
    assert_eq!(hits.len(), 2);
}
