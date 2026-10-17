//! `ALTER TABLE ... ALTER COLUMN <列> SET NOT NULL`（TABLE-39 (3)・TABLE-22 (e)、
//! Issue #1631）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-39・TABLE-22・`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_alter_table_primary_key.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証）。既存行の
//! NULL 検査は単一検査点 `constraint::table_first_null_in_columns` を再利用するため、
//! ここでは SQL 表層（構文・権限ゲート・エラー分類）と「全テナント走査・副作用ゼロ・
//! 他テナント情報の非漏えい」を検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn exec(
    core: &EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    exec(core, session, "owner", sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match exec(core, session, "owner", sql) {
        Ok(_) => panic!("statement must fail: {sql}"),
        Err(e) => e,
    }
    .wire_code()
    .to_string()
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = ddl_session();
    ok(&core, &mut session, "CREATE TABLE docs (a TEXT, b TEXT)");
    (core, path, session)
}

fn insert_null_a(core: &EngineCore, session: &mut SessionState, tenant: &str, id: u32) {
    let sql = format!(
        "INSERT INTO docs (id, b) VALUES ({id}, 'secret-b') USING OPERATION_ID 'op-{tenant}-{id}'"
    );
    exec(core, session, tenant, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const SET_NN: &str = "ALTER TABLE docs ALTER COLUMN a SET NOT NULL";

/// 他テナントの行にだけ NULL があっても `23502`。文言は列名以外（テナント・値・id）を含まず、
/// 拒否後もカタログは不変（NULL の INSERT が通る）。
#[test]
fn rejects_null_in_other_tenant_without_leaking_and_leaves_catalog() {
    let (core, path, mut session) = setup("set-nn-other-tenant");
    let _guard = CleanupGuard(path);
    insert_null_a(&core, &mut session, "tenant-zzz", 4242);
    let err = exec(&core, &mut session, "owner", SET_NN).expect_err("must fail");
    assert_eq!(err.wire_code(), "23502");
    let msg = err.to_string();
    for leak in ["tenant-zzz", "4242", "secret-b"] {
        assert!(!msg.contains(leak), "leaked {leak}: {msg}");
    }
    insert_null_a(&core, &mut session, "owner", 1);
}

/// Private のみの書き込み主体が書いた NULL 行も検出する（可視性で母集合を縮めない）。
#[test]
fn rejects_null_in_private_rows() {
    let (core, path, mut session) = setup("set-nn-private");
    let _guard = CleanupGuard(path);
    let private_writer =
        PolicyContext::with_visibilities("t1", [Visibility::Private]).expect("valid tenant");
    core.execute_sql_in_session(
        &private_writer,
        &mut session,
        "INSERT INTO docs (id, b) VALUES (1, 'x') USING OPERATION_ID 'op-p1'",
    )
    .expect("private insert");
    assert_eq!(code(&core, &mut session, SET_NN), "23502");
}

/// NULL 行なしなら成功し、以後の明示 NULL・列省略・UPDATE による NULL 化は `23502`。
#[test]
fn succeeds_without_null_and_enforces_afterwards() {
    let (core, path, mut session) = setup("set-nn-ok");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    );
    let outcome = exec(&core, &mut session, "owner", SET_NN).expect("set not null");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AlterColumnSetNotNull {
                column_name: "a".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    for sql in [
        "INSERT INTO docs (id, a, b) VALUES (2, NULL, 'y') USING OPERATION_ID 'op-2'",
        "INSERT INTO docs (id, b) VALUES (3, 'y') USING OPERATION_ID 'op-3'",
        "UPDATE docs SET a = NULL WHERE id = 1 USING OPERATION_ID 'op-4'",
    ] {
        assert_eq!(code(&core, &mut session, sql), "23502", "{sql}");
    }
}

/// 行 0 件（行ストア未作成）でも成功する。既に NOT NULL の列は冪等に成功する。
#[test]
fn empty_table_and_already_not_null_succeed() {
    let (core, path, mut session) = setup("set-nn-empty");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, SET_NN);
    ok(&core, &mut session, SET_NN);
    ok(
        &core,
        &mut session,
        "CREATE TABLE keyed (code TEXT PRIMARY KEY, v VECTOR(3))",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE keyed ALTER COLUMN code SET NOT NULL",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE keyed ALTER COLUMN v SET NOT NULL",
    );
}

/// `ADD COLUMN`（DEFAULT なし）より前に書かれた行は NULL として `23502`（`XX000` にならない）。
/// DEFAULT 付きで追加した列は既存行が既定値として読め、成功する。
#[test]
fn rows_written_before_add_column() {
    let (core, path, mut session) = setup("set-nn-add-column");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    );
    ok(&core, &mut session, "ALTER TABLE docs ADD COLUMN c INTEGER");
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN d INTEGER DEFAULT 5",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs ALTER COLUMN c SET NOT NULL"
        ),
        "23502"
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN d SET NOT NULL",
    );
}

/// 表・列の存在系エラーと予約列名。
#[test]
fn existence_and_reserved_column_errors() {
    let (core, path, mut session) = setup("set-nn-existence");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "CREATE VIEW v1 AS SELECT a FROM docs");
    for (sql, c) in [
        ("ALTER TABLE nope ALTER COLUMN a SET NOT NULL", "42P01"),
        ("ALTER TABLE v1 ALTER COLUMN a SET NOT NULL", "42809"),
        ("ALTER TABLE docs ALTER COLUMN zzz SET NOT NULL", "42703"),
        ("ALTER TABLE docs ALTER COLUMN id SET NOT NULL", "42601"),
        (
            "ALTER TABLE docs ALTER COLUMN tenant_id SET NOT NULL",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, &mut session, sql), c, "{sql}");
    }
}

/// 未対応の形は `42601` のまま（fail-closed）。
#[test]
fn unsupported_shapes_are_syntax_errors() {
    let (core, path, mut session) = setup("set-nn-shapes");
    let _guard = CleanupGuard(path);
    for sql in [
        "ALTER TABLE docs ALTER COLUMN a DROP NOT NULL",
        "ALTER TABLE docs ALTER COLUMN a SET DATA TYPE TEXT",
        "ALTER TABLE docs ALTER a SET NOT NULL",
        "ALTER TABLE docs ALTER COLUMN a SET NOT NULL, ALTER COLUMN b SET NOT NULL",
        "ALTER TABLE docs ALTER COLUMN a SET NOT NULL extra",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
}

/// 権限なしは表の有無によらず `42501`。明示トランザクション内は `0A000` で `Failed` へ遷移し、
/// ROLLBACK 後も列は nullable のまま。
#[test]
fn permission_and_explicit_transaction_gates() {
    let (core, path, mut session) = setup("set-nn-gates");
    let _guard = CleanupGuard(path);
    let mut plain = SessionState::default();
    assert_eq!(code(&core, &mut plain, SET_NN), "42501");
    assert_eq!(
        code(
            &core,
            &mut plain,
            "ALTER TABLE nope ALTER COLUMN a SET NOT NULL"
        ),
        "42501"
    );
    let owner = ctx("owner");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&owner, &mut session, &mut txn, SET_NN)
        .expect_err("explicit transaction must reject DDL");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    insert_null_a(&core, &mut session, "owner", 1);
}

/// FK 構成列: `ON DELETE SET NULL` は常に失敗する宣言になるため `42830`・副作用なし。
/// `ON UPDATE CASCADE` で nullable な UNIQUE 親列を参照する子列も `42830`。NO ACTION は成功。
#[test]
fn foreign_key_interaction() {
    let (core, path, mut session) = setup("set-nn-fk");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "CREATE TABLE parent (k TEXT UNIQUE)");
    ok(
        &core,
        &mut session,
        "CREATE TABLE c1 (r TEXT, FOREIGN KEY (r) REFERENCES parent (k) ON DELETE SET NULL)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE c2 (r TEXT, FOREIGN KEY (r) REFERENCES parent (k) ON UPDATE CASCADE)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE c3 (r TEXT, FOREIGN KEY (r) REFERENCES parent (k))",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE c1 ALTER COLUMN r SET NOT NULL"
        ),
        "42830"
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE c2 ALTER COLUMN r SET NOT NULL"
        ),
        "42830"
    );
    // 副作用なし: 列は nullable のまま。
    ok(
        &core,
        &mut session,
        "INSERT INTO c1 (id, r) VALUES (1, NULL) USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE c3 ALTER COLUMN r SET NOT NULL",
    );
}

/// 成功後に開き直しても NOT NULL が維持される。
#[test]
fn survives_reopen() {
    let (core, path, mut session) = setup("set-nn-reopen");
    let _guard = CleanupGuard(path.clone());
    ok(&core, &mut session, SET_NN);
    drop(session);
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("docs").expect("schema");
    let col = schema
        .columns
        .iter()
        .find(|c| c.name == "a")
        .expect("col a");
    assert!(!col.nullable);
}
