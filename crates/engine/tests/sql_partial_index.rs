//! 部分索引の宣言側（述語の parser と決定性検査）の結合テスト（INDEX-11、Issue #1653）。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-11、
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_expression_index.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）。
//! 検証する契約（詳細は `docs/design/partial-index.md`）:
//!
//! - 非決定な述語は `42P17`、集計は `42803`、ウィンドウは `42P20`、部分クエリ・SRF・組み込み以外の
//!   関数は `0A000`、`visible()` と構文誤りは `42601`、葉数の上限超過は `54000`
//! - 決定的な述語も、格納が入る Issue #1654 までは最終ゲートの `0A000`（宣言だけ残らない）
//! - 構文段の判定はカタログを参照しない（権限・対象の有無に依存しない）
//! - 拒否後にカタログ・世代・クエリ結果が変わらず、拒否文言は固定で何も漏らさない

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

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
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

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    match run(core, session, sql) {
        Ok(outcome) => panic!("{sql} must fail, got {outcome:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 述語の SQLSTATE 分類。権限なしのセッション・存在しない表でも同じ（カタログ非参照の証跡）。
#[test]
fn predicate_rejections_are_classified_before_the_catalog() {
    let (path, _guard) = open_fixture("partial-index-classify");
    let core = core_at(&path);
    let cases: &[(&str, &str)] = &[
        // 非決定（42P17）。NOT・OR・複数要素の内側でも同じ。
        ("CREATE INDEX i ON docs (lang) WHERE body < now()", "42P17"),
        (
            "CREATE INDEX i ON docs (lang) WHERE body = CURRENT_DATE",
            "42P17",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE random() > 0.5",
            "42P17",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lower(lang) || CURRENT_DATE::text = 'x'",
            "42P17",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = 'a' OR body = now()::text",
            "42P17",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE NOT (body = now()::text)",
            "42P17",
        ),
        (
            "CREATE UNIQUE INDEX i ON docs (lang) WHERE random() > 0.5",
            "42P17",
        ),
        // 集計（42803）・ウィンドウ（42P20）。
        ("CREATE INDEX i ON docs (lang) WHERE count(*) > 0", "42803"),
        (
            "CREATE INDEX i ON docs (lang) WHERE sum(id) OVER () > 0",
            "42P20",
        ),
        // 部分クエリ・SRF・組み込み以外の関数（0A000）。
        (
            "CREATE INDEX i ON docs (lang) WHERE lang IN (SELECT lang FROM docs)",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE EXISTS (SELECT 1)",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = (SELECT lang FROM docs)",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE unnest(lang) = 'a'",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE no_such_fn(lang) = 'a'",
            "0A000",
        ),
        // RLS 述語・構文誤り（42601）。
        ("CREATE INDEX i ON docs (lang) WHERE visible()", "42601"),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = 'ja' AND visible()",
            "42601",
        ),
        ("CREATE INDEX i ON docs (lang) WHERE", "42601"),
        ("CREATE INDEX i ON docs (lang) WHERE lang ==", "42601"),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = 'ja' LIMIT 1",
            "42601",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE docs.lang = 'ja'",
            "42601",
        ),
        ("CREATE INDEX i ON docs (lang) WHERE lang = $1", "42601"),
        // 決定的な述語は格納が入るまで最終ゲートの 0A000。
        ("CREATE INDEX i ON docs (lang) WHERE lang = 'ja'", "0A000"),
        (
            "CREATE INDEX i ON docs (lang) WHERE lower(lang) = 'ja'",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE body IS NOT NULL AND lang IN ('a', 'b')",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = 'a' OR NOT (body = 'b')",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE id BETWEEN 1 AND 5",
            "0A000",
        ),
        (
            "CREATE UNIQUE INDEX i ON docs (lang) WHERE lang = 'ja'",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs ((lower(lang))) WHERE lang = 'ja'",
            "0A000",
        ),
        // 判定順: 式要素の判定が述語より先。
        (
            "CREATE INDEX i ON docs ((now())) WHERE count(*) > 0",
            "42P17",
        ),
    ];
    for (sql, code) in cases {
        for session in [&mut allowed_session(), &mut SessionState::default()] {
            assert_eq!(code_of(&core, session, sql), *code, "{sql}");
        }
        let ghost = sql.replace(" docs ", " ghost ");
        assert_eq!(
            code_of(&core, &mut allowed_session(), &ghost),
            *code,
            "{ghost}"
        );
    }
}

/// 述語の葉数の上限超過は `54000`（無制限確保なし）。
#[test]
fn predicate_size_is_bounded() {
    let (path, _guard) = open_fixture("partial-index-bounds");
    let core = core_at(&path);
    let mut s = allowed_session();
    let long = (0..300)
        .map(|i| format!("lang = 'v{i}'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    assert_eq!(
        code_of(
            &core,
            &mut s,
            &format!("CREATE INDEX i ON docs (lang) WHERE {long}")
        ),
        "54000"
    );
}

/// 拒否後に索引は作られず、同名で通常の索引を作れる。
#[test]
fn rejected_partial_index_leaves_no_declaration() {
    let (path, _guard) = open_fixture("partial-index-no-decl");
    let core = core_at(&path);
    let mut s = allowed_session();
    for sql in [
        "CREATE INDEX i ON docs (lang) WHERE lang = 'ja'",
        "CREATE INDEX i ON docs (lang) WHERE body < now()",
        "CREATE UNIQUE INDEX i ON docs (lang) WHERE lang = 'ja'",
    ] {
        run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(code_of(&core, &mut s, "DROP INDEX i"), "42704", "{sql}");
    }
    drop(core);
    assert!(list(&path).is_empty());
    let core = core_at(&path);
    run(&core, &mut s, "CREATE INDEX i ON docs (lang)").expect("plain index");
}

fn list(path: &std::path::Path) -> Vec<engine::catalog::IndexDef> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
}

/// 拒否文言は固定で、列名・リテラル値・テナント名を含まない。
#[test]
fn rejection_messages_are_fixed_and_leak_nothing() {
    let (path, _guard) = open_fixture("partial-index-messages");
    let core = core_at(&path);
    let mut s = allowed_session();
    for (sql, message) in [
        (
            "CREATE INDEX i ON docs (lang) WHERE body < now()",
            "functions in index predicate must be marked IMMUTABLE",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE sum(lang) > 0",
            "aggregate functions are not allowed in index predicates",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE sum(lang) OVER () > 0",
            "window functions are not allowed in index predicates",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang IN (SELECT lang FROM docs)",
            "feature not supported: cannot use subquery in index predicate",
        ),
        (
            "CREATE INDEX i ON docs (lang) WHERE lang = 'secret-alice'",
            "feature not supported: partial indexes are not supported yet",
        ),
    ] {
        let err = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(err.to_string(), message, "{sql}");
    }
}
