//! `ALTER TABLE ... ALTER COLUMN <列> SET DEFAULT <式>`／`DROP DEFAULT`（TABLE-39 (1)(2)(6)、
//! Issue #1628）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-39・
//! TABLE-5・TABLE-16・TABLE-20、`docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_alter_table_set_not_null.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として検証）。範囲は構文・型検査・
//! カタログ更新・永続化・補完値（`fill_value`）の不変までで、INSERT／UPSERT／COPY／NoSQL 等の
//! 各書き込み経路が新しい既定値を使うことの網羅的な検証は #1629 の範囲（ここでは省略 INSERT の
//! スモークのみ）。

use engine::catalog::ColumnDefault;
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

use crate::temp_db;
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
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("owner"), session, sql)
}

fn ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    exec(core, session, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match exec(core, session, sql) {
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
    ok(
        &core,
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, a TEXT, n INTEGER)",
    );
    (core, path, session)
}

fn insert(core: &EngineCore, session: &mut SessionState, id: u32, cols: &str, vals: &str) {
    let sql = format!(
        "INSERT INTO docs (id{cols}) VALUES ({id}{vals}) USING OPERATION_ID 'op-{id}-{}'",
        cols.len() + vals.len()
    );
    ok(core, session, &sql);
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

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn schema_column(path: &std::path::Path, table: &str, column: &str) -> engine::catalog::ColumnDef {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .into_iter()
        .find(|c| c.name == column)
        .expect("column")
}

/// 中核: ADD COLUMN 前に書かれた行の読み出し結果（補完値）は SET／DROP DEFAULT で変わらない。
/// 変わるのは `default_expr` だけで、再 open 後も同じ。
#[test]
fn fill_value_for_existing_rows_is_preserved() {
    let (core, path, mut session) = setup("set-default-fill");
    let _guard = CleanupGuard(path.clone());
    insert(&core, &mut session, 1, "", "");
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN c TEXT DEFAULT 'a'",
    );
    let outcome = exec(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'b'",
    )
    .expect("set default");
    match outcome {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AlterColumnSetDefault {
                column_name: "c".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(select_cells(&core, "SELECT c FROM docs"), vec![text("a")]);
    assert_eq!(
        select_cells(&core, "SELECT c FROM docs WHERE id = 1"),
        vec![text("a")]
    );
    // 以後の省略 INSERT は新しい既定値（詳細な経路の網羅は #1629）。
    insert(&core, &mut session, 2, "", "");
    assert_eq!(
        select_cells(&core, "SELECT c FROM docs WHERE id = 2"),
        vec![text("b")]
    );
    // DROP DEFAULT 後も既存行は補完値のまま。
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN c DROP DEFAULT",
    );
    assert_eq!(
        select_cells(&core, "SELECT c FROM docs WHERE id = 1"),
        vec![text("a")]
    );
    insert(&core, &mut session, 3, "", "");
    assert_eq!(
        select_cells(&core, "SELECT c FROM docs WHERE id = 3"),
        vec![Cell::Null]
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN c SET DEFAULT 'b'",
    );
    drop(session);
    drop(core);
    let col = schema_column(&path, "docs", "c");
    assert_eq!(col.default_expr, Some(ColumnDefault::Text("b".to_string())));
    assert_eq!(col.fill_value, Some(ColumnDefault::Text("a".to_string())));
}

/// `SET DEFAULT NULL` は `DROP DEFAULT` と同じ結果。既定値の無い列への DROP は成功する。
/// NOT NULL 列は DROP DEFAULT 後に列省略 INSERT が `23502`。
#[test]
fn null_and_drop_semantics() {
    let (core, path, mut session) = setup("set-default-null");
    let _guard = CleanupGuard(path.clone());
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN a DROP DEFAULT",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT 'x'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT NULL",
    );
    insert(&core, &mut session, 1, "", "");
    assert_eq!(
        select_cells(&core, "SELECT a FROM docs WHERE id = 1"),
        vec![Cell::Null]
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE strict (id BIGINT PRIMARY KEY, k TEXT NOT NULL DEFAULT 'd', v TEXT)",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE strict ALTER COLUMN k DROP DEFAULT",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "INSERT INTO strict (id, v) VALUES (1, 'x') USING OPERATION_ID 'op-s1'"
        ),
        "23502"
    );
    drop(session);
    drop(core);
    assert_eq!(schema_column(&path, "docs", "a").default_expr, None);
}

/// 型不一致は `42804`。ADD COLUMN の `42601` は据え置き。
#[test]
fn type_mismatch_is_42804() {
    let (core, path, mut session) = setup("set-default-mismatch");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "ALTER TABLE docs ADD COLUMN b BOOLEAN");
    ok(&core, &mut session, "ALTER TABLE docs ADD COLUMN d DATE");
    for sql in [
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT 5",
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT true",
        "ALTER TABLE docs ALTER COLUMN n SET DEFAULT true",
        "ALTER TABLE docs ALTER COLUMN b SET DEFAULT 1",
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT now()",
        "ALTER TABLE docs ALTER COLUMN d SET DEFAULT now()",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42804", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN e BOOLEAN DEFAULT 1"
        ),
        "42601"
    );
}

/// 値の検査コードは CREATE TABLE／ADD COLUMN と同じ。
#[test]
fn value_validation_codes() {
    let (core, path, mut session) = setup("set-default-values");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "CREATE TABLE v (id BIGINT PRIMARY KEY, vc VARCHAR(3), si SMALLINT, bb BYTEA, emb VECTOR(3))",
    );
    for (sql, c) in [
        ("ALTER TABLE v ALTER COLUMN vc SET DEFAULT 'abcd'", "22001"),
        ("ALTER TABLE v ALTER COLUMN si SET DEFAULT 99999", "22003"),
        ("ALTER TABLE v ALTER COLUMN bb SET DEFAULT 'zz'", "22P02"),
        ("ALTER TABLE v ALTER COLUMN emb SET DEFAULT 'x'", "0A000"),
    ] {
        assert_eq!(code(&core, &mut session, sql), c, "{sql}");
    }
    let long = format!(
        "ALTER TABLE v ALTER COLUMN vc SET DEFAULT '{}'",
        "x".repeat(70_000)
    );
    assert_eq!(code(&core, &mut session, &long), "54000");
    ok(
        &core,
        &mut session,
        "ALTER TABLE v ALTER COLUMN vc SET DEFAULT 'abc'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE v ALTER COLUMN emb DROP DEFAULT",
    );
}

/// TIMESTAMPTZ の既定値は UTC 表記へ正規化されて `default_expr` にだけ保存される。
#[test]
fn timestamptz_default_is_normalized_in_default_expr_only() {
    let (core, path, mut session) = setup("set-default-tstz");
    let _guard = CleanupGuard(path.clone());
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN t TIMESTAMPTZ DEFAULT '2020-01-01 00:00:00+00'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN t SET DEFAULT '2020-01-01 09:00:00+09'",
    );
    drop(session);
    drop(core);
    let col = schema_column(&path, "docs", "t");
    match (&col.default_expr, &col.fill_value) {
        (Some(ColumnDefault::Text(d)), Some(ColumnDefault::Text(f))) => {
            assert!(d.starts_with("2020-01-01 00:00:00"), "{d}");
            assert!(f.starts_with("2020-01-01 00:00:00"), "{f}");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// 存在系のエラーと権限・明示トランザクションのゲート。
#[test]
fn existence_permission_and_transaction_gates() {
    let (core, path, mut session) = setup("set-default-gates");
    let _guard = CleanupGuard(path);
    ok(&core, &mut session, "CREATE VIEW v1 AS SELECT a FROM docs");
    for (sql, c) in [
        ("ALTER TABLE nope ALTER COLUMN a SET DEFAULT 'x'", "42P01"),
        ("ALTER TABLE v1 ALTER COLUMN a SET DEFAULT 'x'", "42809"),
        ("ALTER TABLE docs ALTER COLUMN zzz SET DEFAULT 'x'", "42703"),
        ("ALTER TABLE nope ALTER COLUMN a DROP DEFAULT", "42P01"),
        ("ALTER TABLE v1 ALTER COLUMN a DROP DEFAULT", "42809"),
        ("ALTER TABLE docs ALTER COLUMN zzz DROP DEFAULT", "42703"),
        ("ALTER TABLE docs ALTER COLUMN id SET DEFAULT 1", "42601"),
    ] {
        assert_eq!(code(&core, &mut session, sql), c, "{sql}");
    }
    let mut plain = SessionState::default();
    for sql in [
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT 'x'",
        "ALTER TABLE nope ALTER COLUMN a DROP DEFAULT",
    ] {
        assert_eq!(code(&core, &mut plain, sql), "42501", "{sql}");
    }
    let owner = ctx("owner");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs ALTER COLUMN a SET DEFAULT 'x'",
        )
        .expect_err("explicit transaction must reject DDL");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    insert(&core, &mut session, 1, "", "");
    assert_eq!(
        select_cells(&core, "SELECT a FROM docs WHERE id = 1"),
        vec![Cell::Null]
    );
}

/// FK の `ON DELETE SET DEFAULT` で参照される NOT NULL 列: DROP DEFAULT は `42830`、
/// 時刻値関数の SET DEFAULT は `0A000`。拒否後も既定値は変わらない。
#[test]
fn foreign_key_set_default_interaction() {
    let (core, path, mut session) = setup("set-default-fk");
    let _guard = CleanupGuard(path);
    ok(
        &core,
        &mut session,
        "CREATE TABLE parent (id BIGINT PRIMARY KEY, k TEXT UNIQUE)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE child (id BIGINT PRIMARY KEY, r TEXT NOT NULL DEFAULT 'x', \
         FOREIGN KEY (r) REFERENCES parent (k) ON DELETE SET DEFAULT)",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE child ALTER COLUMN r DROP DEFAULT"
        ),
        "42830"
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE child ALTER COLUMN r SET DEFAULT now()"
        ),
        "42804"
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE child ALTER COLUMN r SET DEFAULT 'y'",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE parent2 (id BIGINT PRIMARY KEY, ts TIMESTAMP UNIQUE)",
    );
    ok(
        &core,
        &mut session,
        "CREATE TABLE child2 (id BIGINT PRIMARY KEY, at TIMESTAMP NOT NULL DEFAULT '2020-01-01 00:00:00', \
         FOREIGN KEY (at) REFERENCES parent2 (ts) ON DELETE SET DEFAULT)",
    );
    assert_eq!(
        code(
            &core,
            &mut session,
            "ALTER TABLE child2 ALTER COLUMN at SET DEFAULT now()"
        ),
        "0A000"
    );
}

/// 時刻値関数: TIMESTAMP 列へ SET でき、CREATE TABLE の `DEFAULT now()` 列にも SET／DROP できる
/// （補完値のミラーは落ちる）。再 open 後も読める。
#[test]
fn time_function_defaults() {
    let (core, path, mut session) = setup("set-default-time");
    let _guard = CleanupGuard(path.clone());
    ok(
        &core,
        &mut session,
        "CREATE TABLE ev (id BIGINT PRIMARY KEY, at TIMESTAMP DEFAULT now(), note TEXT)",
    );
    ok(
        &core,
        &mut session,
        "INSERT INTO ev (id, note) VALUES (1, 'n') USING OPERATION_ID 'op-ev-1'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE ev ALTER COLUMN at SET DEFAULT '2020-01-01 00:00:00'",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE ev ALTER COLUMN at SET DEFAULT now()",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE ev ALTER COLUMN at DROP DEFAULT",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ADD COLUMN ts TIMESTAMP",
    );
    ok(
        &core,
        &mut session,
        "ALTER TABLE docs ALTER COLUMN ts SET DEFAULT CURRENT_TIMESTAMP",
    );
    assert_eq!(
        select_cells(&core, "SELECT note FROM ev WHERE id = 1"),
        vec![text("n")]
    );
    drop(session);
    drop(core);
    let col = schema_column(&path, "ev", "at");
    assert_eq!((col.default_expr, col.fill_value), (None, None));
    let storage = Storage::open(&path).expect("reopen");
    assert!(storage.get_table_schema("docs").is_ok());
}

/// 未対応の形は `42601` のまま（fail-closed）。
#[test]
fn unsupported_shapes_are_syntax_errors() {
    let (core, path, mut session) = setup("set-default-shapes");
    let _guard = CleanupGuard(path);
    for sql in [
        "ALTER TABLE docs ALTER a SET DEFAULT 'x'",
        "ALTER TABLE docs ALTER COLUMN a SET DEFAULT",
        "ALTER TABLE docs ALTER COLUMN a DROP DEFAULT extra",
    ] {
        assert_eq!(code(&core, &mut session, sql), "42601", "{sql}");
    }
}
