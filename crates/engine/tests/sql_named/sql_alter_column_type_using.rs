//! `ALTER TABLE <t> ALTER COLUMN <c> [SET DATA] TYPE <型> [COLLATE <n>] [USING <式>]`
//! （TABLE-40・TABLE-19、Issue #1635）の構文・束縛・事前判定の SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session` を production 経路として検証する。型検査に成功した
//! USING 付きの文は、全行の書き換え（Issue #1636。結合テストは `sql_alter_column_type_rewrite.rs`）
//! で受理される。本ファイルは構文・束縛エラーと、従来どおりの拒否（複数アクション内・
//! 明示トランザクション内・権限ゲート先行）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-alter-type-using");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("note", ColumnType::Text, true),
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new("big", ColumnType::BigInt, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("owner", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn err_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> SqlSurfaceError {
    run(core, session, sql).expect_err(&format!("expected {sql:?} to be rejected"))
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    err_of(core, session, sql).wire_code()
}

#[test]
fn set_data_type_behaves_like_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = ddl_session();
    run(
        &core,
        &mut s,
        "ALTER TABLE docs ALTER COLUMN qty SET DATA TYPE BIGINT",
    )
    .expect("widening via SET DATA TYPE");
    run(
        &core,
        &mut s,
        "alter table docs alter column amount set data type numeric(9,2)",
    )
    .expect("case-insensitive keywords");
    // `SET DATA` の後ろが `TYPE` でなければ構文エラー。
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "ALTER TABLE docs ALTER COLUMN qty SET DATA BIGINT"
        ),
        "42601"
    );
}

#[test]
fn using_binding_errors() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = ddl_session();
    for (sql, code) in [
        // 未定義列。
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING missing::integer",
            "42703",
        ),
        // 結果型が新しい型へ代入変換できない（TEXT → INTEGER は明示 CAST が必要）。
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING note",
            "42804",
        ),
        // 組み込み以外の関数・集計。
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING my_udf(note)",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING sum(qty)",
            "42601",
        ),
        // 部分クエリは構文段で拒否。
        (
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING (SELECT 1)",
            "42601",
        ),
        // VECTOR 列。
        (
            "ALTER TABLE docs ALTER COLUMN embedding TYPE TEXT USING embedding",
            "42804",
        ),
        // 構文: USING だけで式が無い・COLLATE より前・OPERATION_ID との混同。
        (
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE TEXT USING note COLLATE \"C\" x",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE TEXT USING note OPERATION_ID 'x'",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING OPERATION_ID 'x'",
            "42601",
        ),
    ] {
        assert_eq!(code_of(&core, &mut s, sql), code, "{sql}");
    }
}

#[test]
fn type_checked_using_is_accepted_on_empty_table() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = ddl_session();
    // 行が無い表でも、型検査に成功した USING 付きの文は受理される（Issue #1636）。
    for sql in [
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING note::integer",
        "ALTER TABLE docs ALTER COLUMN big TYPE INTEGER USING big::integer",
        "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING qty + 1",
        "ALTER TABLE docs ALTER COLUMN note TYPE VARCHAR(8) USING qty",
        "ALTER TABLE docs ALTER COLUMN note TYPE TEXT COLLATE \"C\" USING upper(note)",
    ] {
        run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn omitted_using_requires_assignment_cast() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = ddl_session();
    for sql in [
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER",
        "ALTER TABLE docs ALTER COLUMN flag TYPE INTEGER",
        "ALTER TABLE docs ALTER COLUMN qty TYPE BOOLEAN",
    ] {
        let err = err_of(&core, &mut s, sql);
        assert_eq!(err.wire_code(), "42804", "{sql}");
        assert!(err.to_string().contains("USING"), "{err}");
    }
    // 代入変換がある縮小は、USING 省略でも全行を検査・変換して受理する（Issue #1636）。
    run(
        &core,
        &mut s,
        "ALTER TABLE docs ALTER COLUMN big TYPE INTEGER",
    )
    .expect("assignment-castable narrowing");
    // 既存の拡大変換は成功する。
    run(
        &core,
        &mut s,
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2)",
    )
    .expect("numeric widening");
}

#[test]
fn multiple_actions_with_using_are_not_supported() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = ddl_session();
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING qty, DROP COLUMN note"
        ),
        "0A000"
    );
}

#[test]
fn permission_gate_precedes_using_binding() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut s = SessionState::default();
    let mut messages = Vec::new();
    for sql in [
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING note::integer",
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING missing",
        "ALTER TABLE nope ALTER COLUMN x TYPE INTEGER USING y",
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER",
    ] {
        let err = err_of(&core, &mut s, sql);
        assert_eq!(err.wire_code(), "42501", "{sql}");
        messages.push(err.to_string());
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn using_inside_explicit_transaction_is_rejected_with_0a000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let owner = ctx();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT USING qty",
        )
        .expect_err("DDL inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
}
