//! 式索引の宣言側（parser と式の決定性検査）と格納側（束縛・永続化・キー計算）の結合テスト
//! （INDEX-10、Issue #1647・#1648）。
//! ポインタ: `docs/spec/04-behavior/indexing.md` INDEX-10・INDEX-7、
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_index_ddl.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session` を
//! production 経路として使う）。検証する契約（詳細は `docs/design/expression-index.md`）:
//!
//! - 非決定式は `42P17`、集計は `42803`、ウィンドウは `42P20`、部分クエリ・SRF・組み込み以外の
//!   関数は `0A000`。UNIQUE・hnsw・`COLLATE` と式の組み合わせは後続 Issue までの間 `0A000`
//! - 構文段の判定はカタログを参照しない（権限の有無・対象の有無に依存せず同じ分類。存在オラクルにならない）
//! - 決定的な式は書き込みトランザクション内で束縛して永続化される（未知列 `42703`・型不整合・
//!   型による決定性の再判定 `42P17`・索引化できない結果型 `0A000`）。参照列の `DROP COLUMN` は
//!   連鎖削除、`RENAME COLUMN`／`ALTER COLUMN TYPE` は `0A000` で無変化
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
        // UNIQUE・式への COLLATE は後続 Issue までの間、最終ゲートの 0A000。
        ("CREATE UNIQUE INDEX i ON docs ((lower(lang)))", "0A000"),
        (
            "CREATE INDEX i ON docs ((lower(lang)) COLLATE \"C\")",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs ((lower(lang) COLLATE \"C\"))",
            "0A000",
        ),
        // 式を含む索引では素の列の COLLATE も拒否する（v4 は照合を持たず、宣言を黙って落とさない）。
        (
            "CREATE INDEX i ON docs ((lower(lang)), body COLLATE \"C\")",
            "0A000",
        ),
        (
            "CREATE INDEX i ON docs (lang COLLATE \"C\", (lower(body)))",
            "0A000",
        ),
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
        "CREATE INDEX i ON docs ((lower(no_such_column)))",
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
            "CREATE UNIQUE INDEX i ON docs ((lower(lang)))",
            "feature not supported: CREATE UNIQUE INDEX does not support expression indexes yet",
        ),
        (
            "CREATE INDEX i ON docs ((CAST(lang AS uuid)))",
            "feature not supported: index expression result type is not supported yet",
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

// --- 格納側（Issue #1648） ----------------------------------------------------------------

fn list(path: &std::path::Path) -> Vec<engine::catalog::IndexDef> {
    Storage::open(path)
        .expect("reopen storage")
        .list_indexes()
        .expect("list indexes")
}

/// 決定的な式索引は作成でき、正規化テキストと参照列が永続化され、再オープン後も復元できる。
/// `DROP INDEX` で消え、`IF NOT EXISTS` は同名があれば何もしない。
#[test]
fn deterministic_expression_index_is_created_persisted_and_dropped() {
    let (path, _guard) = open_fixture("expr-index-create");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        for sql in [
            "CREATE INDEX i1 ON docs (lower(lang))",
            "CREATE INDEX i2 ON docs ((COALESCE(lower(lang), body)), lang)",
            "CREATE INDEX i3 ON docs ((length(body) + 1))",
            "CREATE INDEX i4 ON docs ((COALESCE(lang, 'it''s')))",
        ] {
            let outcome = run(&core, &mut s, sql);
            assert!(
                matches!(outcome, Ok(SqlOutcome::CreateIndex(_))),
                "{sql}: {outcome:?}"
            );
        }
        // 同名: IF NOT EXISTS は成功（何も変えない）、無しは 42P07。
        run(
            &core,
            &mut s,
            "CREATE INDEX IF NOT EXISTS i1 ON docs ((upper(lang)))",
        )
        .expect("if not exists");
        assert_eq!(
            code_of(&core, &mut s, "CREATE INDEX i1 ON docs ((upper(lang)))"),
            "42P07"
        );
    }
    let defs = list(&path);
    let get = |n: &str| defs.iter().find(|d| d.name == n).expect(n).clone();
    let i1 = get("i1");
    assert_eq!(i1.columns, vec![String::new()]);
    let e1 = i1.expressions[0].as_ref().expect("expression element");
    assert_eq!(e1.sql, "lower(lang)");
    assert_eq!(e1.referenced_columns, vec!["lang".to_string()]);
    let i2 = get("i2");
    assert_eq!(i2.columns, vec![String::new(), "lang".to_string()]);
    assert_eq!(
        i2.expressions[0].as_ref().expect("expr").referenced_columns,
        vec!["lang".to_string(), "body".to_string()]
    );
    assert!(i2.expressions[1].is_none());
    // 引用符を含む文字列リテラルは `''` で往復する。
    assert_eq!(
        get("i4").expressions[0].as_ref().expect("expr").sql,
        "COALESCE(lang, 'it''s')"
    );
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        for n in ["i1", "i2", "i3", "i4"] {
            run(&core, &mut s, &format!("DROP INDEX {n}")).expect(n);
        }
    }
    assert!(list(&path).is_empty());
}

/// 束縛エラーは束縛側の SQLSTATE のまま届き、何も作られない。
#[test]
fn expression_binding_errors_are_transparent_and_leave_no_declaration() {
    let (path, _guard) = open_fixture("expr-index-bind-errors");
    let core = core_at(&path);
    let mut s = allowed_session();
    // 未知列: 42703。型不整合（TEXT に数値演算）: 42883 または 42804。
    assert_eq!(
        code_of(&core, &mut s, "CREATE INDEX i ON docs ((lower(nope)))"),
        "42703"
    );
    let code = code_of(&core, &mut s, "CREATE INDEX i ON docs ((lang * 2))");
    assert!(code == "42883" || code == "42804", "{code}");
    // 索引化できない結果型（真偽値）は 0A000。
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "CREATE INDEX i ON docs ((CAST(lang AS uuid)))"
        ),
        "0A000"
    );
    // 存在しない表・ビュー名は従来どおり。
    assert_eq!(
        code_of(&core, &mut s, "CREATE INDEX i ON ghost ((lower(lang)))"),
        "42P01"
    );
    drop(core);
    assert!(list(&path).is_empty());
}

fn open_fixture_ts(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "ev",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("at_tz", ColumnType::TimestampTz, false),
                ColumnDef::new("at", ColumnType::Timestamp, false),
                ColumnDef::new("day", ColumnType::Date, false),
                ColumnDef::new("label", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (path, guard)
}

/// 型によって決まる決定性の再判定: `TIMESTAMPTZ` 引数の `date_part`・`TIMESTAMPTZ` への変換は
/// `42P17`。同じ式でも `TIMESTAMP`／`DATE` 引数なら作れる。
#[test]
fn type_dependent_determinism_is_rechecked_at_binding() {
    let (path, _guard) = open_fixture_ts("expr-index-tz");
    let core = core_at(&path);
    let mut s = allowed_session();
    for sql in [
        "CREATE INDEX t1 ON ev ((date_part('hour', at_tz)))",
        "CREATE INDEX t2 ON ev ((CAST(at_tz AS date)))",
        "CREATE INDEX t3 ON ev ((CAST(at AS timestamptz)))",
        "CREATE INDEX t4 ON ev ((CAST(day AS text)))",
    ] {
        assert_eq!(code_of(&core, &mut s, sql), "42P17", "{sql}");
    }
    for sql in [
        "CREATE INDEX ok1 ON ev ((date_part('hour', at)))",
        "CREATE INDEX ok2 ON ev ((day + 1))",
        "CREATE INDEX ok3 ON ev ((at_tz))",
        "CREATE INDEX ok4 ON ev ((lower(label)))",
    ] {
        run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// 参照列の `DROP COLUMN` は式索引を連鎖削除する。`RENAME COLUMN`／`ALTER COLUMN TYPE` は
/// `0A000` で何も変えない。参照しない列なら成功する。
#[test]
fn referenced_column_changes_follow_the_expression_index() {
    let (path, _guard) = open_fixture("expr-index-alter");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX e1 ON docs ((lower(lang)))").expect("create");
    drop(core);
    let before = list(&path);
    let core = core_at(&path);
    assert_eq!(
        code_of(&core, &mut s, "ALTER TABLE docs RENAME COLUMN lang TO l2"),
        "0A000"
    );
    assert_eq!(
        code_of(
            &core,
            &mut s,
            "ALTER TABLE docs ALTER COLUMN lang TYPE varchar(10)"
        ),
        "0A000"
    );
    // 参照しない列は改名できる。
    run(
        &core,
        &mut s,
        "ALTER TABLE docs RENAME COLUMN body TO body2",
    )
    .expect("rename body");
    drop(core);
    assert_eq!(list(&path), before);
    // 参照列の削除で索引ごと消える。
    let core = core_at(&path);
    run(&core, &mut s, "ALTER TABLE docs DROP COLUMN lang").expect("drop column");
    drop(core);
    assert!(list(&path).is_empty());
}

/// `DROP TABLE` は式索引ごと消す。
#[test]
fn drop_table_removes_expression_indexes() {
    let (path, _guard) = open_fixture("expr-index-drop-table");
    let core = core_at(&path);
    let mut s = allowed_session();
    run(&core, &mut s, "CREATE INDEX e1 ON docs ((lower(lang)))").expect("create");
    run(&core, &mut s, "DROP TABLE docs").expect("drop table");
    drop(core);
    assert!(list(&path).is_empty());
}
