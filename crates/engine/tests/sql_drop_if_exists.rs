//! `DROP TABLE|INDEX|VIEW|TYPE IF EXISTS <name>`（Issue #1614・親 #1612、対象
//! ビヘイビア: TABLE-36・SQL-23）の結合テスト。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-36・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → DDL 権限ゲート →
//! `sql::ddl::execute_drop_*`）を production 経路として検証する。契約:
//!
//! - 対象不在は副作用ゼロの no-op 成功（`skipped() == true`）
//! - 対象が存在すれば従来どおり削除、依存（`2BP01`）・種類違い（`42809`）は不変
//! - 権限の無いセッションは存否に関わらず `42501`（no-op を存在オラクルにしない）
//! - 明示トランザクション内 DDL は `0A000`、対象外の形（複数名・`CASCADE` 等）は `42601`

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// テーブル `docs`（embedding・lang）を持つ DB を開く。
fn open_fixture(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// `skipped()` を取り出す（4 種の outcome を一括で扱う）。
/// `core` を手放して同じ DB を開き直す（カタログの永続状態を直接観測する）。
fn reopen(core: EngineCore, guard: &CleanupGuard) -> Storage {
    drop(core);
    Storage::open(&guard.0).expect("reopen storage")
}

fn skipped(outcome: SqlOutcome) -> bool {
    match outcome {
        SqlOutcome::DropTable(o) => o.skipped(),
        SqlOutcome::DropView(o) => o.skipped(),
        SqlOutcome::DropIndex(o) => o.skipped(),
        SqlOutcome::DropType(o) => o.skipped(),
        other => panic!("not a DROP outcome: {other:?}"),
    }
}

const KINDS: [&str; 4] = ["TABLE", "VIEW", "INDEX", "TYPE"];

#[test]
fn missing_target_is_a_noop_success_for_all_four_kinds() {
    let (core, g) = open_fixture("drop-ie-missing");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX keep_idx ON docs (lang)").expect("index");
    run(
        &core,
        &mut s,
        "CREATE VIEW keep_v AS SELECT id FROM docs LIMIT 5",
    )
    .expect("view");
    run(&core, &mut s, "CREATE TYPE keep_t AS ENUM ('a')").expect("type");

    for kind in KINDS {
        for clause in ["IF EXISTS", "if exists", "If Exists"] {
            let sql = format!("DROP {kind} {clause} ghost");
            let out = run(&core, &mut s, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            assert!(skipped(out), "{sql}");
        }
        let out = run(&core, &mut s, &format!("DROP {kind} IF EXISTS ghost;")).expect("semicolon");
        assert!(skipped(out));
    }

    // 世代の不変は `sql::ddl` の単体テストが担う。ここでは再オープン後も
    // 既存オブジェクトが無傷であることを確認する。
    let storage = reopen(core, &g);
    assert_eq!(
        storage.list_tables().expect("tables"),
        vec!["docs".to_string()]
    );
    assert_eq!(storage.list_indexes().expect("indexes").len(), 1);
    assert!(storage.get_enum_type("keep_t").is_ok());
}

#[test]
fn existing_target_is_dropped_then_repeat_is_noop() {
    let (core, _g) = open_fixture("drop-ie-existing");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX idx1 ON docs (lang)").expect("index");
    run(
        &core,
        &mut s,
        "CREATE VIEW v1 AS SELECT id FROM docs LIMIT 5",
    )
    .expect("view");
    run(&core, &mut s, "CREATE TYPE ty1 AS ENUM ('a')").expect("type");

    for (kind, name, missing) in [
        ("INDEX", "idx1", "42704"),
        ("VIEW", "v1", "42P01"),
        ("TYPE", "ty1", "42704"),
    ] {
        let sql = format!("DROP {kind} IF EXISTS {name}");
        let first = run(&core, &mut s, &sql).expect("first drop");
        assert!(!skipped(first), "{sql} must really drop");
        assert_eq!(code(&core, &mut s, &format!("DROP {kind} {name}")), missing);
        let second = run(&core, &mut s, &sql).expect("second drop");
        assert!(skipped(second), "{sql} must be idempotent");
    }
    let first = run(&core, &mut s, "DROP TABLE IF EXISTS docs").expect("drop table");
    assert!(!skipped(first));
    assert_eq!(code(&core, &mut s, "DROP TABLE docs"), "42P01");
    assert!(skipped(
        run(&core, &mut s, "DROP TABLE IF EXISTS docs").expect("repeat")
    ));
}

#[test]
fn dependents_are_still_rejected_with_if_exists() {
    let (core, g) = open_fixture("drop-ie-dependent");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE VIEW base AS SELECT * FROM docs").expect("base");
    run(&core, &mut s, "CREATE VIEW top AS SELECT * FROM base").expect("top");
    assert_eq!(code(&core, &mut s, "DROP TABLE IF EXISTS docs"), "2BP01");
    assert_eq!(code(&core, &mut s, "DROP VIEW IF EXISTS base"), "2BP01");

    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy')").expect("type");
    run(&core, &mut s, "ALTER TABLE docs ADD COLUMN m mood").expect("enum column");
    assert_eq!(code(&core, &mut s, "DROP TYPE IF EXISTS mood"), "2BP01");

    // 拒否後も対象は無傷。
    assert!(!skipped(
        run(&core, &mut s, "DROP VIEW IF EXISTS top").expect("top is droppable")
    ));
    let storage = reopen(core, &g);
    assert!(storage
        .list_tables()
        .expect("tables")
        .contains(&"docs".to_string()));
    assert!(storage.get_enum_type("mood").is_ok());
}

/// 種類違いは `IF EXISTS` でも `42809`（TYPE は名前空間が別のため対象外）。
#[test]
fn wrong_object_kind_is_still_rejected_with_if_exists() {
    let (core, _g) = open_fixture("drop-ie-kind");
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX idx1 ON docs (lang)").expect("index");
    run(
        &core,
        &mut s,
        "CREATE VIEW v1 AS SELECT id FROM docs LIMIT 5",
    )
    .expect("view");
    for sql in [
        "DROP TABLE IF EXISTS v1",
        "DROP TABLE IF EXISTS idx1",
        "DROP VIEW IF EXISTS docs",
        "DROP VIEW IF EXISTS idx1",
        "DROP INDEX IF EXISTS docs",
        "DROP INDEX IF EXISTS v1",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42809", "{sql}");
    }
}

/// 権限の無いセッションは対象の存否に関わらず `42501`（存在オラクルにしない）。
#[test]
fn permission_denied_regardless_of_existence() {
    let (core, _g) = open_fixture("drop-ie-perm");
    let mut allowed = allowed_session();
    run(&core, &mut allowed, "CREATE INDEX idx1 ON docs (lang)").expect("index");
    let mut denied = SessionState::default();
    for sql in [
        "DROP TABLE IF EXISTS docs",
        "DROP TABLE IF EXISTS ghost",
        "DROP INDEX IF EXISTS idx1",
        "DROP INDEX IF EXISTS ghost",
        "DROP VIEW IF EXISTS ghost",
        "DROP TYPE IF EXISTS ghost",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
}

#[test]
fn explicit_transaction_rejects_if_exists_ddl() {
    let (core, _g) = open_fixture("drop-ie-txn");
    let mut s = allowed_session();
    for kind in KINDS {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(
                &ctx(),
                &mut s,
                &mut txn,
                &format!("DROP {kind} IF EXISTS ghost"),
            )
            .expect_err("DDL in txn");
        assert_eq!(err.wire_code(), "0A000", "{kind}");
        core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

#[test]
fn unsupported_shapes_stay_42601() {
    let (core, _g) = open_fixture("drop-ie-shapes");
    let mut allowed = allowed_session();
    let mut denied = SessionState::default();
    for kind in KINDS {
        for sql in [
            format!("DROP {kind} IF EXISTS"),
            format!("DROP {kind} IF EXISTS a, b"),
            format!("DROP {kind} IF EXISTS a CASCADE"),
            format!("DROP {kind} IF a"),
            format!("DROP {kind} EXISTS a"),
        ] {
            assert_eq!(code(&core, &mut allowed, &sql), "42601", "allowed: {sql}");
            assert_eq!(code(&core, &mut denied, &sql), "42601", "denied: {sql}");
        }
    }
}

/// `if` という名前のオブジェクトを指す従来の解釈は 2 トークン先読みで保たれる。
#[test]
fn bare_if_name_is_still_treated_as_a_name() {
    let (core, _g) = open_fixture("drop-ie-bare-if");
    let mut s = allowed_session();
    assert_eq!(code(&core, &mut s, "DROP INDEX if"), "42704");
    assert_eq!(code(&core, &mut s, "DROP VIEW if"), "42P01");
}
