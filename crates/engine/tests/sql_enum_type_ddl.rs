//! `CREATE TYPE ... AS ENUM` / `DROP TYPE`（TABLE-14・SQL-23・TASK-198、
//! Issue #1194）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md`
//! TABLE-14・`docs/spec/04-behavior/sql-surface.md` SQL-23・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_index_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//! 検証する契約（詳細は `docs/design/enum-type-ddl.md` 参照）:
//!
//! - DDL 実行権限ゲートが型の実在有無を問わず `42501` を返し、カタログを変更しない
//! - 構文の許可形状（`42601`／`54000`）は権限の有無に関わらずカタログを参照しない
//! - SQL だけで `2BP01`（依存列あり）に到達でき、依存が消えれば削除できる
//! - 明示トランザクション内の型 DDL は `0A000`
//! - 拡張クエリプロトコルの `$n` は `42601`、Describe は結果列なし

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// `t` テーブル（VECTOR 列のみ）を持つ DB とコアを開く。
fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (path, guard, core)
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// コアを閉じて（redb のファイルロックを解放して）カタログを読み直す。
fn reopen(core: EngineCore, path: &std::path::Path) -> Storage {
    drop(core);
    Storage::open(path).expect("reopen storage")
}

#[test]
fn ddl_without_permission_is_rejected_regardless_of_type_existence() {
    let (p, _g, core) = open_fixture("enum-ddl-perm");
    run(
        &core,
        &mut allowed_session(),
        "CREATE TYPE mood AS ENUM ('a')",
    )
    .expect("seed type");
    let mut denied = SessionState::default();
    for sql in [
        "CREATE TYPE mood AS ENUM ('x')",
        "CREATE TYPE fresh AS ENUM ('x')",
        "DROP TYPE mood",
        "DROP TYPE nothere",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
    let storage = reopen(core, &p);
    assert!(storage.get_enum_type("mood").is_ok());
    assert!(storage.get_enum_type("fresh").is_err());
}

#[test]
fn malformed_statements_are_syntax_errors_regardless_of_permission() {
    let (_p, _g, core) = open_fixture("enum-ddl-syntax");
    let mut denied = SessionState::default();
    let mut allowed = allowed_session();
    for sql in [
        "CREATE TYPE t1 AS ENUM ()",
        "CREATE TYPE t1 AS ENUM (a)",
        "CREATE TYPE t1 AS ENUM (1)",
        "CREATE TYPE t1 AS (a int)",
        "CREATE TYPE t1",
        "CREATE TYPE IF NOT EXISTS t1 AS ENUM ('a')",
        "CREATE TYPE s.t1 AS ENUM ('a')",
        "CREATE TYPE t1 AS ENUM ('a') junk",
        "DROP TYPE IF EXISTS t1",
        "DROP TYPE t1 CASCADE",
        "DROP TYPE a, b",
        "DROP TYPE",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42601", "denied: {sql}");
        assert_eq!(code(&core, &mut allowed, sql), "42601", "allowed: {sql}");
    }
}

#[test]
fn create_type_registers_labels_in_declaration_order() {
    let (p, _g, core) = open_fixture("enum-ddl-create");
    let mut s = allowed_session();
    assert!(matches!(
        run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy', 'sad');"),
        Ok(SqlOutcome::CreateType(_))
    ));
    assert_eq!(
        code(&core, &mut s, "CREATE TYPE mood AS ENUM ('x')"),
        "42P07"
    );
    let def = reopen(core, &p).get_enum_type("mood").expect("registered");
    assert_eq!(def.labels(), ["happy".to_string(), "sad".to_string()]);
}

#[test]
fn create_type_catalog_side_validation_and_label_limit() {
    let (p, _g, core) = open_fixture("enum-ddl-validate");
    let mut s = allowed_session();
    for sql in [
        "CREATE TYPE text AS ENUM ('a')",
        "CREATE TYPE Decimal AS ENUM ('a')",
        "CREATE TYPE d1 AS ENUM ('a', 'a')",
        "CREATE TYPE d2 AS ENUM ('')",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42601", "{sql}");
    }
    let long = "x".repeat(64);
    assert_eq!(
        code(&core, &mut s, &format!("CREATE TYPE d3 AS ENUM ('{long}')")),
        "42601"
    );
    let many = (0..257)
        .map(|i| format!("'l{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        code(&core, &mut s, &format!("CREATE TYPE d4 AS ENUM ({many})")),
        "54000"
    );
    let storage = reopen(core, &p);
    assert!(storage.get_enum_type("d1").is_err() && storage.get_enum_type("d4").is_err());
}

#[test]
fn drop_type_is_rejected_while_dependent_column_exists_via_sql_only() {
    let (_p, _g, core) = open_fixture("enum-ddl-2bp01");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy', 'sad')").expect("create type");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN m mood").expect("add enum column");
    assert_eq!(code(&core, &mut s, "DROP TYPE mood"), "2BP01");
    run(&core, &mut s, "ALTER TABLE t DROP COLUMN m").expect("drop column");
    assert!(matches!(
        run(&core, &mut s, "DROP TYPE mood"),
        Ok(SqlOutcome::DropType(_))
    ));
    assert_eq!(code(&core, &mut s, "DROP TYPE mood"), "42704");
}

#[test]
fn drop_type_succeeds_after_dependent_table_is_dropped() {
    let (_p, _g, core) = open_fixture("enum-ddl-drop-table");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy')").expect("create type");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN m mood").expect("add enum column");
    assert_eq!(code(&core, &mut s, "DROP TYPE mood"), "2BP01");
    run(&core, &mut s, "DROP TABLE t").expect("drop table");
    run(&core, &mut s, "DROP TYPE mood").expect("drop type");
}

#[test]
fn sql_created_type_enforces_vocabulary_on_insert() {
    let (_p, _g, core) = open_fixture("enum-ddl-insert");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy', 'sad')").expect("create type");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN m mood").expect("add enum column");
    run(
        &core,
        &mut SessionState::default(),
        "INSERT INTO t (id, embedding, m) VALUES (1, '[0.1,0.2]', 'happy') USING OPERATION_ID 'op-1'",
    )
    .expect("in-vocabulary label");
    assert_eq!(
        code(
            &core,
            &mut SessionState::default(),
            "INSERT INTO t (id, embedding, m) VALUES (2, '[0.1,0.2]', 'angry') USING OPERATION_ID 'op-2'"
        ),
        "22P02"
    );
}

#[test]
fn type_names_are_case_sensitive() {
    let (_p, _g, core) = open_fixture("enum-ddl-case");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE TYPE Mood AS ENUM ('a')").expect("create type");
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE t ADD COLUMN c mood"),
        "42601"
    );
}

#[test]
fn type_ddl_inside_explicit_transaction_is_rejected() {
    let (p, _g, core) = open_fixture("enum-ddl-txn");
    let mut s = allowed_session();
    for sql in ["CREATE TYPE mood AS ENUM ('a')", "DROP TYPE mood"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, sql)
            .expect_err("type DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    assert!(reopen(core, &p).get_enum_type("mood").is_err());
}

#[test]
fn extended_protocol_rejects_params_and_describe_has_no_columns() {
    let (_p, _g, core) = open_fixture("enum-ddl-extended");
    for sql in ["CREATE TYPE t1 AS ENUM ($1)", "DROP TYPE $1"] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("$n must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
    let s = allowed_session();
    for sql in ["CREATE TYPE t1 AS ENUM ('a')", "DROP TYPE t1"] {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let described = core
            .describe_prepared_in_session(&s, &prepared)
            .expect("describe");
        assert!(described.is_none(), "{sql}");
    }
}
