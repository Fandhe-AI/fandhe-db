//! `ALTER TABLE ... ALTER COLUMN <列> DROP NOT NULL`（TABLE-39 (4)、Issue #1632）の結合
//! テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-39・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_alter_table_set_not_null.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証）。制約を緩める変更の
//! ため既存行は走査せず、主キー構成列・VECTOR 列・FK 参照先側の拒否と、成功後の書き込み
//! 経路（INSERT・UPDATE・UPSERT）が NULL を受け付けること、補完値・既定値が不変なことを検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::exec::Cell;
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

fn err(core: &EngineCore, session: &mut SessionState, sql: &str) -> SqlSurfaceError {
    match exec(core, session, "owner", sql) {
        Ok(_) => panic!("statement must fail: {sql}"),
        Err(e) => e,
    }
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    err(core, session, sql).wire_code().to_string()
}

fn select_cells(core: &EngineCore, sql: &str) -> Vec<Cell> {
    let result = core
        .execute_sql(&ctx("owner"), &format!("{sql} LIMIT 10"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    result
        .rows
        .into_iter()
        .map(|r| r.cells.into_iter().next().expect("cell"))
        .collect()
}

fn nullable(path: &std::path::Path, table: &str, column: &str) -> bool {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .into_iter()
        .find(|c| c.name == column)
        .expect("column")
        .nullable
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = ddl_session();
    ok(
        &core,
        &mut session,
        "CREATE TABLE docs (a TEXT NOT NULL, b TEXT)",
    );
    (core, path, session)
}

const DROP_NN: &str = "ALTER TABLE docs ALTER COLUMN a DROP NOT NULL";

/// 成功後は INSERT（明示 NULL・列省略）・UPDATE・UPSERT が NULL を受け付ける。
#[test]
fn drop_not_null_accepts_nulls_afterwards() {
    let (core, path, mut session) = setup("drop-nn-basic");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "INSERT INTO docs (id, a, b) VALUES (2, NULL, 'y') USING OPERATION_ID 'op-2'"
        ),
        "23502"
    );
    let outcome = exec(&core, &mut session, "owner", DROP_NN).expect("drop not null");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AlterColumnDropNotNull {
                column_name: "a".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (2, NULL, 'y') USING OPERATION_ID 'op-3'",
    );
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, b) VALUES (3, 'y') USING OPERATION_ID 'op-4'",
    );
    ok(
        &core,
        &mut session,
        "UPDATE docs SET a = NULL WHERE id = 1 USING OPERATION_ID 'op-5'",
    );
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (1, 'z', 'y') \
         ON CONFLICT (id) DO UPDATE SET a = NULL USING OPERATION_ID 'op-6'",
    );
    for id in [1, 2, 3] {
        assert_eq!(
            select_cells(&core, &format!("SELECT a FROM docs WHERE id = {id}")),
            vec![Cell::Null],
            "id={id}"
        );
    }
}

/// 主キー構成列（単一・複合）は `42P16`。文言は列名のみで、拒否後もカタログは不変。
#[test]
fn rejects_primary_key_columns() {
    let (core, path, mut session) = setup("drop-nn-pk");
    let _guard = CleanupGuard(path.clone());
    ok(
        &core,
        &mut session,
        "CREATE TABLE pk1 (k TEXT PRIMARY KEY, v TEXT)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE pk2 (x TEXT, y TEXT, PRIMARY KEY (x, y))",
    );
    for (sql, col) in [
        ("ALTER TABLE pk1 ALTER COLUMN k DROP NOT NULL", "k"),
        ("ALTER TABLE pk2 ALTER COLUMN y DROP NOT NULL", "y"),
    ] {
        let e = err(&core, &mut session, sql);
        assert_eq!(e.wire_code(), "42P16", "{sql}");
        assert_eq!(
            e.to_string(),
            format!("column \"{col}\" is in a primary key")
        );
    }
    drop(session);
    drop(core);
    assert!(!nullable(&path, "pk1", "k"));
    assert!(!nullable(&path, "pk2", "y"));
}

/// 主キー削除後は構成列だった列の DROP NOT NULL が成功する。
#[test]
fn succeeds_after_primary_key_dropped() {
    let (core, path, mut session) = setup("drop-nn-after-pk");
    let _guard = CleanupGuard(path.clone());
    ok(&core, &mut session, "CREATE TABLE pk1 (k TEXT PRIMARY KEY)");
    ok(
        &core,
        &mut session,
        "ALTER TABLE pk1 DROP CONSTRAINT pk1_pkey",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE pk1 ALTER COLUMN k DROP NOT NULL",
    );
    drop(session);
    drop(core);
    assert!(nullable(&path, "pk1", "k"));
}

/// 予約列・未知の列・不在の表・ビュー。
#[test]
fn existence_and_reserved_column_errors() {
    let (core, path, mut session) = setup("drop-nn-existence");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "CREATE VIEW v1 AS SELECT a FROM docs");
    for (sql, c) in [
        ("ALTER TABLE nope ALTER COLUMN a DROP NOT NULL", "42P01"),
        ("ALTER TABLE v1 ALTER COLUMN a DROP NOT NULL", "42809"),
        ("ALTER TABLE docs ALTER COLUMN zzz DROP NOT NULL", "42703"),
        ("ALTER TABLE docs ALTER COLUMN id DROP NOT NULL", "42601"),
        (
            "ALTER TABLE docs ALTER COLUMN tenant_id DROP NOT NULL",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN visibility DROP NOT NULL",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, &mut session, sql), c, "{sql}");
    }
}

/// VECTOR 列は NULL 許容 VECTOR が未実装のため `42601`。カタログは不変。
#[test]
fn rejects_vector_column() {
    let (core, path, mut session) = setup("drop-nn-vector");
    let _guard = CleanupGuard(path.clone());
    ok(&core, &mut session, "CREATE TABLE vt (embedding VECTOR(3))");
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE vt ALTER COLUMN embedding DROP NOT NULL"
        ),
        "42601"
    );
    drop(session);
    drop(core);
    assert!(!nullable(&path, "vt", "embedding"));
}

/// 既に nullable な列は冪等に成功する。
#[test]
fn already_nullable_is_noop() {
    let (core, path, mut session) = setup("drop-nn-noop");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN b DROP NOT NULL",
    );
    ok(&core, &mut session, DROP_NN);
    ok(&core, &mut session, DROP_NN);
}

/// FK 参照先側: 参照元が NOT NULL かつ `ON UPDATE CASCADE` なら `42830`・副作用なし。
/// 参照元が nullable なら成功する。参照元側の DROP NOT NULL も成功する。
#[test]
fn foreign_key_interaction() {
    let (core, path, mut session) = setup("drop-nn-fk");
    let _guard = CleanupGuard(path.clone());
    ok(
        &core,
        &mut session,
        "CREATE TABLE p (k TEXT NOT NULL UNIQUE)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE c (pk TEXT NOT NULL REFERENCES p (k) ON UPDATE CASCADE)",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE p ALTER COLUMN k DROP NOT NULL"
        ),
        "42830"
    );

    ok(
        &core,
        &mut session,
        "CREATE TABLE p2 (k TEXT NOT NULL UNIQUE)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE c2 (pk TEXT REFERENCES p2 (k) ON UPDATE CASCADE)",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE p2 ALTER COLUMN k DROP NOT NULL",
    );

    ok(&core, &mut session, "CREATE TABLE p3 (k TEXT UNIQUE)");
    ok(
        &core,
        &mut session,
        "CREATE TABLE c3 (r TEXT NOT NULL, FOREIGN KEY (r) REFERENCES p3 (k))",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE c3 ALTER COLUMN r DROP NOT NULL",
    );
    drop(session);
    drop(core);
    assert!(!nullable(&path, "p", "k"));
    assert!(nullable(&path, "p2", "k"));
    assert!(nullable(&path, "c3", "r"));
}

/// 補完値と既定値は不変: ADD COLUMN 前の行は補完値で読め、SET DEFAULT 済みの既定値も残る。
#[test]
fn fill_value_and_default_are_preserved() {
    let (core, path, mut session) = setup("drop-nn-fill");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN n INTEGER NOT NULL DEFAULT 5",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN n SET DEFAULT 7",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN n DROP NOT NULL",
    );
    assert_eq!(
        select_cells(&core, "SELECT n FROM docs WHERE id = 1"),
        vec![Cell::SignedInteger(5)]
    );
    ok(
        &core,
        &mut session,
        "INSERT INTO docs (id, a, b) VALUES (2, 'x', 'y') USING OPERATION_ID 'op-2'",
    );
    assert_eq!(
        select_cells(&core, "SELECT n FROM docs WHERE id = 2"),
        vec![Cell::SignedInteger(7)]
    );
}

/// 未対応の形は `42601` のまま（fail-closed）。
#[test]
fn unsupported_shapes_are_syntax_errors() {
    let (core, path, mut session) = setup("drop-nn-shapes");
    let _guard = CleanupGuard(path);
    for sql in [
        "ALTER TABLE docs ALTER a DROP NOT NULL",
        "ALTER TABLE docs ALTER COLUMN a DROP NOT NULL extra",
        "ALTER TABLE docs ALTER COLUMN a DROP NULL",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
}

/// 権限なしは表の有無によらず `42501`。明示トランザクション内は `0A000` で `Failed` へ遷移し、
/// ROLLBACK 後も列は NOT NULL のまま。
#[test]
fn permission_and_explicit_transaction_gates() {
    let (core, path, mut session) = setup("drop-nn-gates");
    let _guard = CleanupGuard(path.clone());
    let mut plain = SessionState::default();
    assert_eq!(code(&core, &mut plain, DROP_NN), "42501");
    assert_eq!(
        code(
            &core,
            &mut plain,
            "ALTER TABLE nope ALTER COLUMN a DROP NOT NULL"
        ),
        "42501"
    );
    let owner = ctx("owner");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&owner, &mut session, &mut txn, DROP_NN)
        .expect_err("explicit transaction must reject DDL");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    drop(txn);
    drop(session);
    drop(core);
    assert!(!nullable(&path, "docs", "a"));
}

/// 成功後に開き直しても nullable が維持され、別テナントも NULL を書ける（共有の表定義）。
#[test]
fn survives_reopen_and_applies_to_all_tenants() {
    let (core, path, mut session) = setup("drop-nn-reopen");
    let _guard = CleanupGuard(path.clone());
    ok(&core, &mut session, DROP_NN);
    exec(
        &core,
        &mut session,
        "tenant-b",
        "INSERT INTO docs (id, a, b) VALUES (1, NULL, 'y') USING OPERATION_ID 'op-tb'",
    )
    .expect("other tenant null insert");
    drop(session);
    drop(core);
    assert!(nullable(&path, "docs", "a"));
}
