//! 式索引の宣言側（parser と式の決定性検査）の結合テスト（INDEX-10、Issue #1647）。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7、
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_index_ddl.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session` を
//! production 経路として使う）。検証する契約（詳細は `docs/design/expression-index.md`）:
//!
//! - 非決定式は `42P17`、集計は `42803`、ウィンドウは `42P20`、部分クエリ・SRF・組み込み以外の
//!   関数は `0A000`。決定的な式は後続 Issue（#1648〜#1651）までの間 `0A000`（最終ゲート）
//! - 判定はカタログを参照しない（権限の有無・対象の有無に依存せず同じ分類。存在オラクルにならない）
//! - 拒否後にカタログ・世代・クエリ結果が変わらない
//! - 拒否文言は固定で、列名・値・テナント名を含まない
//! - 式のノード数・深さには上限がある（`54000`）
//! - `((col))` は通常の列索引として作成でき `DROP INDEX` で消える

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

/// INDEX-10: SQLSTATE の段階と判定順。権限なしのセッション・存在しない表でも同じ分類
/// （カタログ非参照の証跡）。
#[test]
fn expression_rejections_are_classified_before_the_catalog() {
    let (path, _guard) = open_fixture("expr-index-classify");
    let core = core_at(&path);
    let cases: &[(&str, &str)] = &[
        // 非決定式（42P17）。UNIQUE・部分索引の WHERE・複数要素でも同じ。
        ("CREATE INDEX i ON docs ((now()))", "42P17"),
        (
            "CREATE INDEX i ON docs ((lower(lang) || CURRENT_DATE::text))",
            "42P17",
        ),
        ("CREATE UNIQUE INDEX i ON docs ((now()))", "42P17"),
        ("CREATE INDEX i ON docs (lang, (random()))", "42P17"),
        (
            "CREATE INDEX i ON docs ((now())) WHERE lang = 'ja'",
            "42P17",
        ),
        // 集計（42803）・ウィンドウ（42P20）。
        ("CREATE INDEX i ON docs ((count(*)))", "42803"),
        ("CREATE INDEX i ON docs ((max(lang)))", "42803"),
        ("CREATE INDEX i ON docs ((count(*) OVER ()))", "42P20"),
        // 部分クエリ・SRF・組み込み以外の関数（0A000）。
        ("CREATE INDEX i ON docs ((SELECT 1))", "0A000"),
        ("CREATE INDEX i ON docs ((EXISTS (SELECT 1)))", "0A000"),
        ("CREATE INDEX i ON docs ((unnest(lang)))", "0A000"),
        ("CREATE INDEX i ON docs ((no_such_fn(lang)))", "0A000"),
        // 決定的な式は後続 Issue までの間、最終ゲートの 0A000。
        ("CREATE INDEX i ON docs (lower(lang))", "0A000"),
        ("CREATE INDEX i ON docs ((lower(lang) || body))", "0A000"),
        ("CREATE UNIQUE INDEX i ON docs ((lower(lang)))", "0A000"),
        // 構文。
        ("CREATE INDEX i ON docs ((lang) || body)", "42601"),
        ("CREATE INDEX i ON docs (lower(lang) || body)", "42601"),
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

/// 拒否後にカタログは変わらない（索引が作られず、`DROP INDEX` は不在の `42704`）。
#[test]
fn rejected_expression_index_leaves_no_declaration() {
    let (path, _guard) = open_fixture("expr-index-no-decl");
    let core = core_at(&path);
    let mut s = allowed_session();
    for sql in [
        "CREATE INDEX i ON docs ((now()))",
        "CREATE INDEX i ON docs (lower(lang))",
        "CREATE UNIQUE INDEX i ON docs ((lower(lang)))",
    ] {
        run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(code_of(&core, &mut s, "DROP INDEX i"), "42704", "{sql}");
    }
    // 拒否の後も同名で通常の索引を作れる。
    run(&core, &mut s, "CREATE INDEX i ON docs (lang)").expect("plain index");
}

/// 拒否文言は固定で、列名・テナント名を含まない（`&'static str` の固定文言）。
#[test]
fn rejection_messages_are_fixed_and_leak_nothing() {
    let (path, _guard) = open_fixture("expr-index-messages");
    let core = core_at(&path);
    let mut s = allowed_session();
    for (sql, message) in [
        (
            "CREATE INDEX i ON docs ((now()))",
            "functions in index expression must be marked IMMUTABLE",
        ),
        (
            "CREATE INDEX i ON docs ((sum(lang)))",
            "aggregate functions are not allowed in index expressions",
        ),
        (
            "CREATE INDEX i ON docs ((sum(lang) OVER ()))",
            "window functions are not allowed in index expressions",
        ),
        (
            "CREATE INDEX i ON docs ((SELECT lang FROM docs))",
            "feature not supported: cannot use subquery in index expression",
        ),
        (
            "CREATE INDEX i ON docs (lower(lang))",
            "feature not supported: expression indexes are not supported yet",
        ),
    ] {
        let err = run(&core, &mut s, sql).expect_err(sql);
        assert_eq!(err.to_string(), message, "{sql}");
    }
}

/// 式のノード数・深さの上限超過は `54000`（無制限確保なし）。
#[test]
fn expression_size_and_depth_are_bounded() {
    let (path, _guard) = open_fixture("expr-index-bounds");
    let core = core_at(&path);
    let mut s = allowed_session();
    let long = vec!["lang"; 5000].join(" || ");
    assert_eq!(
        code_of(&core, &mut s, &format!("CREATE INDEX i ON docs (({long}))")),
        "54000"
    );
    let deep = format!(
        "CREATE INDEX i ON docs (({}lang{}))",
        "(".repeat(2000),
        ")".repeat(2000)
    );
    assert_eq!(code_of(&core, &mut s, &deep), "54000");
}

/// `((col))` は PostgreSQL と同じく通常の列索引として作成でき、`DROP INDEX` で消える。
#[test]
fn parenthesized_plain_column_is_a_column_index() {
    let (path, _guard) = open_fixture("expr-index-plain-column");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX i ON docs ((lang), body)").expect("create");
    run(
        &core,
        &mut s,
        "CREATE INDEX j ON docs ((body COLLATE \"C\"))",
    )
    .expect("create collate");
    // 正規化後の重複列は 42601。
    assert_eq!(
        code_of(&core, &mut s, "CREATE INDEX k ON docs ((lang), lang)"),
        "42601"
    );
    run(&core, &mut s, "DROP INDEX i").expect("drop i");
    run(&core, &mut s, "DROP INDEX j").expect("drop j");
}
