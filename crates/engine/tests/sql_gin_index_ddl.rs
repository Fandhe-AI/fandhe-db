//! `CREATE INDEX ... USING gin` の宣言（索引名・演算子クラス・JSONB 列）の結合テスト
//! （Issue #2206・INDEX-8。設計は `docs/design/gin-jsonb-index.md` D2・D12）。
//!
//! 本 Issue の範囲は「宣言の受理とカタログ永続化」までで、ポスティングの生成・維持・
//! プランナ接続は後続 Issue の担当。ここでは `sql_index_ddl.rs` と同じ流儀（実 `Storage` ＋
//! `EngineCore::execute_sql_in_session`）で、次の契約を固定する:
//!
//! - 受理形と省略時の演算子クラス（`jsonb_ops`）、再オープン後の復元
//! - `IF NOT EXISTS`・`DROP INDEX`・`DROP INDEX ... CASCADE`（既存の契約のまま）
//! - 表の `DROP TABLE`・`DROP COLUMN`・`RENAME`・`ALTER COLUMN TYPE` との整合
//! - 未対応形（JSONB 以外の列・複数列・式・`UNIQUE`・`WHERE`・`COLLATE`）の SQLSTATE
//! - 宣言の有無でクエリ結果集合が変わらないこと、DDL 権限ゲート

use engine::catalog::{ColumnDef, ColumnType, GinOpClass, IndexKind, TableSchema};
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

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("doc", ColumnType::Jsonb, true),
            ColumnDef::new("raw", ColumnType::Json, true),
            ColumnDef::new("body", ColumnType::Text, true),
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ],
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn open_fixture(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (path, guard)
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn run_ok(core: &EngineCore, session: &mut SessionState, sql: &str) {
    if let Err(e) = run(core, session, sql) {
        panic!("{sql} must succeed, got {e:?}");
    }
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e.wire_code(),
    }
}

/// 再オープンしたストレージから索引宣言（名前・種別・表・列）を列挙する。
fn declared(path: &std::path::Path) -> Vec<(String, IndexKind, String, Vec<String>)> {
    let mut v: Vec<_> = Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
        .into_iter()
        .map(|d| (d.name, d.kind, d.table, d.columns))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// 受理形・省略時の演算子クラス・再オープン後の復元（受け入れ条件）。
#[test]
fn gin_declarations_are_accepted_and_persist_across_reopen() {
    let (path, _guard) = open_fixture("gin-ddl-persist");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(
            &core,
            &mut s,
            "CREATE INDEX g_default ON docs USING gin (doc)",
        );
        run_ok(
            &core,
            &mut s,
            "create index g_ops on docs using GIN (doc jsonb_ops);",
        );
        run_ok(
            &core,
            &mut s,
            "CREATE INDEX g_path ON docs USING gin (doc JSONB_PATH_OPS)",
        );
    }
    let defs = declared(&path);
    let kinds: Vec<_> = defs.iter().map(|d| (d.0.as_str(), d.1)).collect();
    assert_eq!(
        kinds,
        vec![
            ("g_default", IndexKind::Gin(GinOpClass::JsonbOps)),
            ("g_ops", IndexKind::Gin(GinOpClass::JsonbOps)),
            ("g_path", IndexKind::Gin(GinOpClass::JsonbPathOps)),
        ]
    );
    for d in &defs {
        assert_eq!(d.2, TABLE);
        assert_eq!(d.3, vec!["doc".to_string()]);
    }
}

/// `IF NOT EXISTS`（TABLE-36）・名前衝突 `42P07`・`DROP INDEX`・`CASCADE` は既存の契約のまま。
#[test]
fn gin_if_not_exists_drop_and_cascade_follow_existing_index_contract() {
    let (path, _guard) = open_fixture("gin-ddl-lifecycle");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(&core, &mut s, "CREATE INDEX g ON docs USING gin (doc)");
        // 同名は定義を比較せず no-op（演算子クラスが違っても既存のまま）。
        run_ok(
            &core,
            &mut s,
            "CREATE INDEX IF NOT EXISTS g ON docs USING gin (doc jsonb_path_ops)",
        );
        assert_eq!(
            code(&core, &mut s, "CREATE INDEX g ON docs USING gin (doc)"),
            "42P07"
        );
        assert_eq!(
            code(&core, &mut s, "CREATE INDEX docs ON docs USING gin (doc)"),
            "42P07"
        );
        // CASCADE は全種別で未対応（`42601`）。GIN だけ特別扱いしない。
        assert_eq!(code(&core, &mut s, "DROP INDEX g CASCADE"), "42601");
    }
    assert_eq!(
        declared(&path)[0].1,
        IndexKind::Gin(GinOpClass::JsonbOps),
        "IF NOT EXISTS keeps the first declaration"
    );
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(&core, &mut s, "DROP INDEX g");
        run_ok(&core, &mut s, "DROP INDEX IF EXISTS g");
        assert_eq!(code(&core, &mut s, "DROP INDEX g"), "42704");
    }
    assert!(declared(&path).is_empty());
}

/// 未対応形の SQLSTATE（構文段は `0A000`／`42704`、カタログ段は `0A000`／`42703`／`42P01`）。
#[test]
fn gin_unsupported_forms_are_classified() {
    let (path, _guard) = open_fixture("gin-ddl-rejections");
    let core = core_at(&path);
    let mut s = allowed_session();
    for (sql, expected) in [
        // カタログ段: JSONB 以外の列・疑似列 id。
        ("CREATE INDEX g ON docs USING gin (raw)", "0A000"),
        ("CREATE INDEX g ON docs USING gin (body)", "0A000"),
        ("CREATE INDEX g ON docs USING gin (embedding)", "0A000"),
        ("CREATE INDEX g ON docs USING gin (id)", "0A000"),
        ("CREATE INDEX g ON docs USING gin (nope)", "42703"),
        ("CREATE INDEX g ON nope USING gin (doc)", "42P01"),
        // 構文段。
        ("CREATE INDEX g ON docs USING gin (doc, body)", "0A000"),
        ("CREATE INDEX g ON docs USING gin ((doc -> 'a'))", "0A000"),
        ("CREATE UNIQUE INDEX g ON docs USING gin (doc)", "0A000"),
        (
            "CREATE INDEX g ON docs USING gin (doc) WHERE body = 'x'",
            "0A000",
        ),
        (
            "CREATE INDEX g ON docs USING gin (doc COLLATE \"C\")",
            "0A000",
        ),
        ("CREATE INDEX g ON docs USING gin (doc array_ops)", "42704"),
        (
            "CREATE INDEX g ON docs USING btree (doc jsonb_ops)",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
    drop(core);
    assert!(
        declared(&path).is_empty(),
        "rejections leave no declaration"
    );
}

/// `DROP TABLE`・`DROP COLUMN`・`RENAME` の追随、`ALTER COLUMN TYPE` の拒否。
#[test]
fn gin_declaration_follows_table_and_column_ddl() {
    let (path, _guard) = open_fixture("gin-ddl-follow");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(
            &core,
            &mut s,
            "CREATE INDEX g ON docs USING gin (doc jsonb_path_ops)",
        );
        // 列の型変更は索引を効かなくするため `0A000`（宣言は残る）。
        assert_eq!(
            code(&core, &mut s, "ALTER TABLE docs ALTER COLUMN doc TYPE text"),
            "0A000"
        );
        run_ok(
            &core,
            &mut s,
            "ALTER TABLE docs RENAME COLUMN doc TO payload",
        );
        run_ok(&core, &mut s, "ALTER TABLE docs RENAME TO items");
    }
    let defs = declared(&path);
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].1, IndexKind::Gin(GinOpClass::JsonbPathOps));
    assert_eq!(defs[0].2, "items");
    assert_eq!(defs[0].3, vec!["payload".to_string()]);
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(&core, &mut s, "ALTER TABLE items DROP COLUMN payload");
    }
    assert!(
        declared(&path).is_empty(),
        "DROP COLUMN removes the declaration"
    );

    let (path2, _guard2) = open_fixture("gin-ddl-drop-table");
    {
        let core = core_at(&path2);
        let mut s = allowed_session();
        run_ok(&core, &mut s, "CREATE INDEX g ON docs USING gin (doc)");
        run_ok(&core, &mut s, "DROP TABLE docs");
    }
    assert!(
        declared(&path2).is_empty(),
        "DROP TABLE removes the declaration"
    );
    {
        let core = core_at(&path2);
        let mut s = allowed_session();
        run_ok(
            &core,
            &mut s,
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, doc JSONB)",
        );
        run_ok(&core, &mut s, "CREATE INDEX g ON docs USING gin (doc)");
        run_ok(&core, &mut s, "DROP TABLE docs CASCADE");
    }
    assert!(declared(&path2).is_empty());
}

/// 権限の無いセッションは `42501` で、カタログは変わらない。明示トランザクション内は `0A000`。
#[test]
fn gin_ddl_is_gated_by_permission_and_transaction_state() {
    let (path, _guard) = open_fixture("gin-ddl-gates");
    {
        let core = core_at(&path);
        let mut denied = SessionState::default();
        assert_eq!(
            code(&core, &mut denied, "CREATE INDEX g ON docs USING gin (doc)"),
            "42501"
        );
        let mut s = allowed_session();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(
                &ctx(),
                &mut s,
                &mut txn,
                "CREATE INDEX g ON docs USING gin (doc)",
            )
            .expect_err("index DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000");
    }
    assert!(declared(&path).is_empty());
}

/// 宣言の有無でクエリの結果集合が変わらない（本 Issue では索引を使わない。後続の回帰基準）。
#[test]
fn gin_declaration_does_not_change_query_results() {
    let (path, _guard) = open_fixture("gin-ddl-results");
    let core = core_at(&path);
    let mut s = allowed_session();
    run_ok(
        &core,
        &mut s,
        "INSERT INTO docs (id, doc, body, embedding) VALUES (1, '{\"a\": 1}', 'x', '[1,0]')",
    );
    let query = "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";
    let before = format!("{:?}", run(&core, &mut s, query).expect("query before"));
    run_ok(&core, &mut s, "CREATE INDEX g ON docs USING gin (doc)");
    let after = format!("{:?}", run(&core, &mut s, query).expect("query after"));
    assert_eq!(before, after);
}
