//! `CREATE SEQUENCE` / `DROP SEQUENCE`（TABLE-24・TABLE-27・SQL-23、Issue #1688）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24・TABLE-27・TABLE-36・TABLE-37、
//! `docs/spec/04-behavior/error-format.md` ERR-6。設計記録は
//! `docs/design/sequence-namespace-and-ledger.md`。
//!
//! `sql_enum_type_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。検証する契約:
//!
//! - 再起動（Storage の再オープン）後も定義が残り、DROP 後は同名を再作成できる
//! - 表・ビュー・索引・matview との名前衝突は両方向で `42P07`／`42809`
//! - DDL 実行権限ゲートが SEQUENCE の実在有無を問わず `42501` を返す
//! - 未対応の句は `0A000`、構文違反は `42601`、数値範囲は `22003`／`22023`
//! - 明示トランザクション内の SEQUENCE DDL は `0A000`
//! - 定義は全テナント共有で、未知の名前の `42P01` はテナントによらず同一

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn core_over(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `t` 表（VECTOR 列のみ）を持つ DB とコアを開く。
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
    (path, guard, core_over(storage))
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, "alice", session, sql)
}

fn ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    run(core, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// コアを閉じて（redb のファイルロックを解放して）開き直す。
fn reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    core_over(Storage::open(path).expect("reopen storage"))
}

#[test]
fn definition_survives_reopen_and_drop_frees_the_name() {
    let (p, _g, core) = open_fixture("seq-persist");
    let mut s = allowed_session();
    assert!(matches!(
        run(&core, &mut s, "CREATE SEQUENCE s1 START WITH 10;"),
        Ok(SqlOutcome::CreateSequence(_))
    ));
    let core = reopen(core, &p);
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE s1"), "42P07");
    assert!(matches!(
        run(&core, &mut s, "DROP SEQUENCE s1"),
        Ok(SqlOutcome::DropSequence(_))
    ));
    let core = reopen(core, &p);
    ok(&core, &mut s, "CREATE SEQUENCE s1");
}

#[test]
fn drop_of_unknown_name_is_undefined_table() {
    let (_p, _g, core) = open_fixture("seq-unknown");
    let mut s = allowed_session();
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE nothere"), "42P01");
    // RESTRICT は既定と同義で受理される。
    assert_eq!(
        code(&core, &mut s, "DROP SEQUENCE nothere RESTRICT"),
        "42P01"
    );
}

#[test]
fn relation_namespace_collisions_both_directions() {
    let (_p, _g, core) = open_fixture("seq-namespace");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE seq_a");
    ok(&core, &mut s, "CREATE VIEW v1 AS SELECT embedding FROM t");
    ok(
        &core,
        &mut s,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, lang TEXT)",
    );
    ok(&core, &mut s, "CREATE INDEX idx1 ON u (lang)");

    // SEQUENCE 作成が既存の表・ビュー・索引・SEQUENCE と衝突する。
    for name in ["t", "v1", "idx1", "seq_a"] {
        assert_eq!(
            code(&core, &mut s, &format!("CREATE SEQUENCE {name}")),
            "42P07",
            "{name}"
        );
    }
    // 既存 SEQUENCE の名前で表・ビュー・索引・改名を作ると衝突する。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE seq_a (id BIGINT PRIMARY KEY, x TEXT)"
        ),
        "42P07"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE VIEW seq_a AS SELECT embedding FROM t"
        ),
        "42P07"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX seq_a ON u (lang)"),
        "42P07"
    );
    assert_eq!(
        code(&core, &mut s, "ALTER TABLE t RENAME TO seq_a"),
        "42P07"
    );

    // 種別違い: DROP SEQUENCE に表・ビュー・索引、DROP TABLE/VIEW/INDEX に SEQUENCE。
    for name in ["t", "v1", "idx1"] {
        assert_eq!(
            code(&core, &mut s, &format!("DROP SEQUENCE {name}")),
            "42809",
            "{name}"
        );
    }
    assert_eq!(code(&core, &mut s, "DROP TABLE seq_a"), "42809");
    assert_eq!(code(&core, &mut s, "DROP VIEW seq_a"), "42809");
    assert_eq!(code(&core, &mut s, "DROP INDEX seq_a"), "42809");
    // SEQUENCE 上の索引は種別不一致。
    assert_eq!(
        code(&core, &mut s, "CREATE INDEX idx2 ON seq_a (lang)"),
        "42809"
    );
}

#[test]
fn matview_namespace_is_shared() {
    let (_p, _g, core) = open_fixture("seq-matview");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE seq_a");
    ok(
        &core,
        &mut s,
        "CREATE MATERIALIZED VIEW mv1 AS SELECT embedding FROM t",
    );
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE mv1"), "42P07");
    assert_eq!(code(&core, &mut s, "DROP SEQUENCE mv1"), "42809");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE MATERIALIZED VIEW seq_a AS SELECT embedding FROM t"
        ),
        "42P07"
    );
    assert_eq!(code(&core, &mut s, "DROP MATERIALIZED VIEW seq_a"), "42809");
}

#[test]
fn ddl_without_permission_is_rejected_regardless_of_existence() {
    let (p, _g, core) = open_fixture("seq-perm");
    ok(&core, &mut allowed_session(), "CREATE SEQUENCE seeded");
    let mut denied = SessionState::default();
    for sql in [
        "CREATE SEQUENCE seeded",
        "CREATE SEQUENCE fresh",
        "DROP SEQUENCE seeded",
        "DROP SEQUENCE nothere",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
    // 拒否された文はカタログを変えない。
    let core = reopen(core, &p);
    let mut s = allowed_session();
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE seeded"), "42P07");
    ok(&core, &mut s, "CREATE SEQUENCE fresh");
}

#[test]
fn syntax_table() {
    let (_p, _g, core) = open_fixture("seq-syntax");
    let mut s = allowed_session();
    let cases: &[(&str, &str)] = &[
        // 未対応の句は 0A000（黙って無視しない）。`public.x` の修飾は Issue #1717 で裸の名前と
        // 同じ扱いになった（`table27_schema_objects.rs`）ため、ここには含めない。
        ("CREATE SEQUENCE IF NOT EXISTS x", "0A000"),
        ("CREATE SEQUENCE x CACHE 10", "0A000"),
        ("CREATE SEQUENCE x OWNED BY t.embedding", "0A000"),
        ("CREATE SEQUENCE x AS integer", "0A000"),
        ("CREATE TEMP SEQUENCE x", "0A000"),
        ("CREATE TEMPORARY SEQUENCE x", "0A000"),
        ("CREATE UNLOGGED SEQUENCE x", "0A000"),
        ("DROP SEQUENCE IF EXISTS x", "0A000"),
        ("DROP SEQUENCE x CASCADE", "0A000"),
        ("DROP SEQUENCE a, b", "0A000"),
        // 構文違反は 42601。
        ("CREATE SEQUENCE", "42601"),
        ("CREATE SEQUENCE x START 1 START 2", "42601"),
        ("CREATE SEQUENCE x MINVALUE 1 NO MINVALUE", "42601"),
        ("CREATE SEQUENCE x CYCLE NO CYCLE", "42601"),
        ("CREATE SEQUENCE x START 1.5", "42601"),
        ("CREATE SEQUENCE x INCREMENT 1e3", "42601"),
        ("CREATE SEQUENCE x START 'a'", "42601"),
        ("CREATE SEQUENCE x bogus", "42601"),
        ("CREATE SEQUENCE x NO bogus", "42601"),
        ("DROP SEQUENCE", "42601"),
        ("DROP SEQUENCE x y", "42601"),
        // 数値範囲。
        ("CREATE SEQUENCE x START 9223372036854775808", "22003"),
        ("CREATE SEQUENCE x MINVALUE -9223372036854775809", "22003"),
        // 意味検証。
        ("CREATE SEQUENCE x INCREMENT 0", "22023"),
        ("CREATE SEQUENCE x MINVALUE 5 MAXVALUE 5", "22023"),
        ("CREATE SEQUENCE x MINVALUE 5 MAXVALUE 1", "22023"),
        ("CREATE SEQUENCE x START 0", "22023"),
        ("CREATE SEQUENCE x MAXVALUE 10 START 11", "22023"),
        ("CREATE SEQUENCE x INCREMENT -1 START 1", "22023"),
    ];
    for (sql, want) in cases {
        assert_eq!(code(&core, &mut s, sql), *want, "{sql}");
    }
    // 構文段は権限ゲートより前に判定される（既存 DDL と同じ契約）。
    let mut denied = SessionState::default();
    assert_eq!(
        code(&core, &mut denied, "CREATE SEQUENCE x CACHE 10"),
        "0A000"
    );
    // 失敗した文はどれも定義を残さない。
    ok(&core, &mut s, "CREATE SEQUENCE x");
}

#[test]
fn accepted_forms() {
    let (_p, _g, core) = open_fixture("seq-accept");
    let mut s = allowed_session();
    for sql in [
        "CREATE SEQUENCE a1",
        "create sequence a2 start with 10 increment by -2 minvalue -100 maxvalue 10 cycle",
        "CREATE SEQUENCE a3 NO MINVALUE NO MAXVALUE NO CYCLE",
        "CREATE SEQUENCE a4 MAXVALUE 100 INCREMENT BY -1",
        "CREATE SEQUENCE a5 START +5",
        "CREATE SEQUENCE a6 INCREMENT BY -1 MINVALUE -9223372036854775808 START -9223372036854775808",
        "CREATE SEQUENCE a7 MINVALUE 1 MAXVALUE 9223372036854775807 START 9223372036854775807",
    ] {
        ok(&core, &mut s, sql);
    }
    for name in ["a1", "a2", "a3", "a4", "a5", "a6", "a7"] {
        ok(&core, &mut s, &format!("DROP SEQUENCE {name}"));
    }
}

#[test]
fn sequence_ddl_inside_explicit_transaction_is_rejected() {
    let (p, _g, core) = open_fixture("seq-txn");
    let mut s = allowed_session();
    ok(&core, &mut s, "CREATE SEQUENCE existing");
    for sql in ["CREATE SEQUENCE s9", "DROP SEQUENCE existing"] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, sql)
            .expect_err("sequence DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
        core.execute_sql_in_txn(&ctx("alice"), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
    let core = reopen(core, &p);
    assert_eq!(code(&core, &mut s, "CREATE SEQUENCE existing"), "42P07");
    ok(&core, &mut s, "CREATE SEQUENCE s9");
}

#[test]
fn definitions_are_shared_across_tenants_and_errors_do_not_leak() {
    let (_p, _g, core) = open_fixture("seq-tenant");
    let mut s = allowed_session();
    let err_a = run_as(&core, "alice", &mut s, "DROP SEQUENCE ghost").expect_err("unknown");
    let err_b = run_as(&core, "bob", &mut s, "DROP SEQUENCE ghost").expect_err("unknown");
    assert_eq!(err_a.wire_code(), "42P01");
    assert_eq!(err_b.wire_code(), "42P01");
    assert_eq!(err_a.client_message(), err_b.client_message());
    assert!(!err_a.client_message().contains("alice"));

    run_as(&core, "alice", &mut s, "CREATE SEQUENCE shared").expect("create as alice");
    // 全テナント共有カタログ: 他テナントの文脈からも同じ名前が衝突として見える。
    let dup = run_as(&core, "bob", &mut s, "CREATE SEQUENCE shared").expect_err("dup");
    assert_eq!(dup.wire_code(), "42P07");
    run_as(&core, "bob", &mut s, "DROP SEQUENCE shared").expect("drop as bob");
}

#[test]
fn extended_protocol_rejects_params() {
    let (_p, _g, core) = open_fixture("seq-extended");
    for sql in ["CREATE SEQUENCE x START $1", "DROP SEQUENCE $1"] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("$n must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}
