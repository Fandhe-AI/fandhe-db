//! `SELECT` の行ロック句（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、
//! `OF`、`NOWAIT`／`SKIP LOCKED`。SQL-38・Issue #1957）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/` SQL-38。
//!
//! 構文解釈・併用検査（`ParsedSql::LockingSelect` が得られること）と、実行入口が #2230 まで
//! `0A000` の暫定ゲートで拒否することを別々に確かめる。ロックの取得（#2230）・`SKIP LOCKED` の
//! 実行（#2231）が入ったらゲートの期待値を更新すること。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::row_lock::{LockStrength, LockWaitPolicy};
use engine::sql::transaction::TransactionStatus;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql38-row-lock-clause");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("title", ColumnType::Text, false),
                ColumnDef::new("author_id", ColumnType::BigInt, true),
            ],
        ))
        .expect("create documents");
    storage
        .create_table(&TableSchema::new(
            "authors",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("name", ColumnType::Text, false),
            ],
        ))
        .expect("create authors");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn parse_code(core: &EngineCore, sql: &str) -> String {
    match core.parse_sql(sql) {
        Ok(p) => panic!("expected error, parsed: {p:?} for {sql}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn locking(core: &EngineCore, sql: &str) -> engine::sql::row_lock::LockingSelect {
    match core.parse_sql(sql) {
        Ok(ParsedSql::LockingSelect(l)) => *l,
        other => panic!("expected LockingSelect for {sql}, got {other:?}"),
    }
}

const KNN: &str = "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 5";
const JOIN: &str = "FROM documents JOIN authors ON documents.author_id = authors.id";

#[test]
fn accepted_forms_parse_into_locking_select() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // 広域取得・kNN。
    let l = locking(
        &core,
        "SELECT id, title FROM documents WHERE title = 'x' LIMIT 10 FOR UPDATE",
    );
    assert_eq!(l.locking().clauses().len(), 1);
    assert_eq!(l.locking().clauses()[0].strength(), LockStrength::Update);
    assert_eq!(l.locking().clauses()[0].wait(), LockWaitPolicy::Block);
    let l = locking(&core, &format!("{KNN} FOR NO KEY UPDATE NOWAIT"));
    assert_eq!(
        l.locking().clauses()[0].strength(),
        LockStrength::NoKeyUpdate
    );
    assert_eq!(l.locking().clauses()[0].wait(), LockWaitPolicy::NoWait);
    let l = locking(&core, &format!("{KNN} FOR KEY SHARE SKIP LOCKED"));
    assert_eq!(l.locking().clauses()[0].strength(), LockStrength::KeyShare);
    assert_eq!(l.locking().clauses()[0].wait(), LockWaitPolicy::SkipLocked);
    // 行ロック句が LIMIT の前にある形。
    locking(
        &core,
        "SELECT id FROM documents WHERE title = 'x' FOR SHARE LIMIT 10",
    );
    // 別名と、`public.` 修飾の FROM。
    let l = locking(
        &core,
        "SELECT d.id FROM documents AS d WHERE d.title = 'x' LIMIT 5 FOR UPDATE OF d",
    );
    assert_eq!(l.locking().clauses()[0].of(), ["d".to_string()]);
    locking(
        &core,
        "SELECT id FROM public.documents WHERE title = 'x' LIMIT 5 FOR UPDATE OF documents",
    );
    // JOIN（OF なし・OF で一方）。
    locking(
        &core,
        &format!("SELECT documents.title {JOIN} LIMIT 10 FOR UPDATE"),
    );
    locking(
        &core,
        &format!("SELECT documents.title {JOIN} LIMIT 10 FOR UPDATE OF documents, authors"),
    );
    // 外部結合で NULL を補わない側だけを指す。
    locking(
        &core,
        "SELECT documents.title FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE OF documents",
    );
    locking(
        &core,
        "SELECT documents.title FROM documents RIGHT JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE OF authors",
    );
}

#[test]
fn combination_violations_are_0a000() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    for sql in [
        "SELECT title, count(*) FROM documents GROUP BY title FOR UPDATE",
        "SELECT title FROM documents GROUP BY title HAVING count(*) > 1 FOR UPDATE",
        "SELECT count(*) FROM documents FOR UPDATE",
        "SELECT DISTINCT title FROM documents FOR UPDATE",
        "SELECT id, RANK() OVER (ORDER BY id) FROM documents LIMIT 5 FOR UPDATE",
        "SELECT id, RANK() OVER w FROM documents WINDOW w AS (ORDER BY id) LIMIT 5 FOR UPDATE",
        "SELECT id FROM documents UNION SELECT id FROM authors FOR UPDATE",
        "SELECT id FROM documents INTERSECT SELECT id FROM authors FOR UPDATE",
        "SELECT id FROM documents EXCEPT SELECT id FROM authors FOR UPDATE",
        "SELECT documents.title FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE",
        "SELECT documents.title FROM documents LEFT JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE OF authors",
        "SELECT documents.title FROM documents RIGHT JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE OF documents",
        "SELECT documents.title FROM documents FULL JOIN authors ON documents.author_id = authors.id LIMIT 10 FOR UPDATE OF authors",
        "SELECT 1 FOR UPDATE",
    ] {
        assert_eq!(parse_code(&core, sql), "0A000", "{sql}");
    }
}

#[test]
fn unknown_of_name_is_42p01() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // FROM に無い名前（同名の実表があっても）。
    assert_eq!(
        parse_code(&core, &format!("{KNN} FOR UPDATE OF authors")),
        "42P01"
    );
    assert_eq!(
        parse_code(
            &core,
            "SELECT d.id FROM documents AS d LIMIT 5 FOR UPDATE OF documents"
        ),
        "42P01"
    );
    assert_eq!(
        parse_code(
            &core,
            &format!("SELECT documents.title {JOIN} LIMIT 10 FOR UPDATE OF nothing")
        ),
        "42P01"
    );
    // FROM の表が無ければ行ロック句の有無によらず 42P01。
    assert_eq!(
        parse_code(&core, "SELECT id FROM nosuch LIMIT 5 FOR UPDATE"),
        "42P01"
    );
    assert_eq!(parse_code(&core, "SELECT id FROM nosuch LIMIT 5"), "42P01");
}

#[test]
fn unsupported_positions_stay_42601() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    for sql in [
        "SELECT id FROM (SELECT id FROM documents LIMIT 5 FOR UPDATE) AS s LIMIT 5",
        "WITH c AS (SELECT id FROM documents LIMIT 5 FOR UPDATE) SELECT id FROM c LIMIT 5",
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 5 FOR UPDATE",
        "EXPLAIN SELECT id FROM documents LIMIT 5 FOR UPDATE",
        "INSERT INTO documents (embedding, title) SELECT embedding, title FROM documents LIMIT 5 FOR UPDATE",
        &format!("{KNN} FOR UPDATE OF public.documents"),
        &format!("{KNN} FOR UPDATE NOWAIT SKIP LOCKED"),
        &format!("{KNN} FOR UPDATE OF"),
        "SELECT id FROM documents LIMIT 1 FOR UPDATE OFFSET 2",
    ] {
        assert_eq!(parse_code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn cte_target_is_rejected() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // WITH 問い合わせへの適用は PostgreSQL も受理しない（本実装は 0A000 で拒否する）。
    let r = core.parse_sql(
        "WITH c AS (SELECT id, title FROM documents LIMIT 5) SELECT id FROM c LIMIT 5 FOR UPDATE OF c",
    );
    assert_eq!(r.expect_err("rejected").wire_code(), "0A000");
}

#[test]
fn execution_is_gated_with_0a000_and_changes_nothing() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let sql = "SELECT id FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE";
    // セッションを持たない後方互換の入口（`execute_sql`）は許可リストの検証器を直接呼ぶ入口で、
    // 行ロック句の構文を認識しない（従来どおり拒否側 `42601`。実行はしない）。
    assert!(core.execute_sql(&ctx(), sql).is_err());
    // autocommit・セッション。
    let mut session = SessionState::default();
    assert_eq!(
        core.execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err("gated")
            .wire_code(),
        "0A000"
    );
    // 明示トランザクション内（失敗状態へ移る）。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    assert_eq!(
        core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, sql)
            .expect_err("gated")
            .wire_code(),
        "0A000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    // 既存のエラーと同じ扱い（失敗状態では 25P02）。
    assert_eq!(
        core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "SELECT 1")
            .expect_err("failed txn")
            .wire_code(),
        "25P02"
    );
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

#[test]
fn describe_matches_the_statement_without_the_clause() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let session = SessionState::default();
    let plain = core
        .describe_parsed_in_session(
            &session,
            &core
                .parse_sql("SELECT id, title FROM documents WHERE title = 'x' LIMIT 5")
                .expect("parse plain"),
        )
        .expect("describe plain");
    let locked = core
        .describe_parsed_in_session(
            &session,
            &core
                .parse_sql("SELECT id, title FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE")
                .expect("parse locked"),
        )
        .expect("describe locked");
    assert!(plain.is_some());
    assert_eq!(plain, locked);
}

#[test]
fn extended_query_parse_and_describe_succeed_and_execute_is_gated() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let session = SessionState::default();
    let sql = "SELECT id, title FROM documents WHERE title = $1 LIMIT 5 FOR UPDATE";
    let plain_sql = "SELECT id, title FROM documents WHERE title = $1 LIMIT 5";
    let prepared = core.parse_sql_prepared(sql).expect("parse extended");
    let plain = core.parse_sql_prepared(plain_sql).expect("parse plain");
    assert_eq!(
        core.describe_prepared_in_session(&session, &prepared)
            .expect("describe"),
        core.describe_prepared_in_session(&session, &plain)
            .expect("describe plain")
    );
    let bound = core
        .bind_prepared(&prepared, &[Some(b"x".to_vec())])
        .expect("bind");
    assert!(matches!(bound, ParsedSql::LockingSelect(_)));
    let mut session = SessionState::default();
    assert_eq!(
        core.execute_parsed_in_session(&ctx(), &mut session, &bound)
            .expect_err("gated")
            .wire_code(),
        "0A000"
    );
}

#[test]
fn temp_table_select_with_clause_is_gated() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TEMP TABLE tmp_lock (id BIGINT, v TEXT)",
    )
    .expect("create temp");
    let code = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT v FROM tmp_lock WHERE v = 'x' LIMIT 5 FOR UPDATE",
        )
        .expect_err("rejected")
        .wire_code();
    assert_eq!(code, "0A000");
}

#[test]
fn extended_query_param_before_the_clause_is_accepted() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // `$n` の位置規則は行ロック句の有無で変わらない（句の前の `WHERE title = $1` を受理する）。
    let prepared = core
        .parse_sql_prepared("SELECT id FROM documents WHERE title = $1 FOR UPDATE LIMIT 5")
        .expect("parse extended");
    let bound = core
        .bind_prepared(&prepared, &[Some(b"x".to_vec())])
        .expect("bind");
    assert!(matches!(bound, ParsedSql::LockingSelect(_)));
    // `LIMIT $n` は句が無くても非対応で、行ロック句があっても同じ応答になる。
    let with = core.parse_sql_prepared("SELECT id FROM documents FOR UPDATE LIMIT $1");
    let without = core.parse_sql_prepared("SELECT id FROM documents LIMIT $1");
    assert_eq!(
        with.err().map(|e| e.wire_code()),
        without.err().map(|e| e.wire_code())
    );
}

#[test]
fn missing_tables_answer_the_same_with_or_without_the_clause() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // 表が存在しない（他テナントの表と区別できない）ときの応答は、句の有無で変わらない。
    let with = core.parse_sql("SELECT id FROM other_tenant_t LIMIT 5 FOR UPDATE");
    let without = core.parse_sql("SELECT id FROM other_tenant_t LIMIT 5");
    assert_eq!(
        with.expect_err("with").wire_code(),
        without.expect_err("without").wire_code()
    );
}
