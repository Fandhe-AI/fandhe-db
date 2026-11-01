//! `SELECT` の行ロック句（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、
//! `OF`、`NOWAIT`／`SKIP LOCKED`。SQL-38・Issue #1957）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/` SQL-38。
//!
//! 構文解釈・併用検査（`ParsedSql::LockingSelect` が得られること）と、実行（Issue #2230。writer の
//! 取得・保持期間・句なし SELECT との結果同一性）を確かめる。`SKIP LOCKED` の実行（#2231）と一時表への
//! 行ロック句は未対応の `0A000` のまま。2 セッションの待機・時間計測を伴うテストは
//! `tests/write_lock_wait.rs`（単独バイナリ）に置く。

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

/// 行を投入する（autocommit の INSERT）。`author_id` は 1 または 2。
fn seed(core: &EngineCore, session: &mut SessionState) {
    for (id, title, author) in [(1, "x", 1), (2, "x", 2), (3, "y", 1), (4, "x", 1)] {
        core.execute_sql_in_session(
            &ctx(),
            session,
            &format!(
                "INSERT INTO documents (id, embedding, title, author_id) VALUES ({id}, '[1.0, 0.0]', '{title}', {author}) USING OPERATION_ID 'seed-d{id}'"
            ),
        )
        .expect("insert document");
    }
    for (id, name) in [(1, "alice"), (2, "bob")] {
        core.execute_sql_in_session(
            &ctx(),
            session,
            &format!(
                "INSERT INTO authors (id, embedding, name) VALUES ({id}, '[0.0, 1.0]', '{name}') USING OPERATION_ID 'seed-a{id}'"
            ),
        )
        .expect("insert author");
    }
}

/// ロック句つきと句なしで同じ結果になる文の組（句は末尾に付ける）。
const SAME_RESULT_QUERIES: &[&str] = &[
    "SELECT id, title FROM documents WHERE title = 'x' LIMIT 10",
    "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 3",
    "SELECT documents.id, authors.name FROM documents JOIN authors ON documents.author_id = authors.id LIMIT 10",
    "SELECT documents.id FROM documents JOIN authors ON documents.author_id = authors.id WHERE documents.title = 'x' LIMIT 10",
];

/// SQL-38・Issue #2230: ロック句つき SELECT の結果は、句のない同じ SELECT と同一
/// （autocommit・明示 Tx〔取得直後〕のいずれでも）。
#[test]
fn locking_select_returns_same_rows_as_plain_select() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let clauses = [
        " FOR UPDATE",
        " FOR NO KEY UPDATE",
        " FOR SHARE",
        " FOR KEY SHARE",
        " FOR UPDATE NOWAIT",
        " FOR SHARE NOWAIT",
    ];
    for q in SAME_RESULT_QUERIES {
        let plain = core
            .execute_sql_in_session(&ctx(), &mut session, q)
            .expect("plain");
        for c in clauses {
            let sql = format!("{q}{c}");
            // autocommit。
            let locked = core
                .execute_sql_in_session(&ctx(), &mut session, &sql)
                .unwrap_or_else(|e| panic!("autocommit {sql}: {e}"));
            assert_eq!(format!("{plain:?}"), format!("{locked:?}"), "{sql}");
            // 明示 Tx（取得直後）。
            let mut txn = core.new_session_transaction();
            core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
                .expect("begin");
            let locked = core
                .execute_sql_in_txn(&ctx(), &mut session, &mut txn, &sql)
                .unwrap_or_else(|e| panic!("in txn {sql}: {e}"));
            assert_eq!(format!("{plain:?}"), format!("{locked:?}"), "{sql}");
            assert_eq!(txn.status(), TransactionStatus::InTransaction);
            // 取得後の句なし SELECT とも一致し、COMMIT できる。
            let again = core
                .execute_sql_in_txn(&ctx(), &mut session, &mut txn, q)
                .expect("plain in txn");
            assert_eq!(format!("{plain:?}"), format!("{again:?}"), "{sql}");
            core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "COMMIT")
                .expect("commit");
        }
    }
}

/// 自 Tx の未 commit の INSERT の後でも、ロック句つきと句なしの結果は同じで、未 commit 行を含む。
#[test]
fn locking_select_sees_own_uncommitted_rows_like_plain_select() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &ctx(),
        &mut session,
        &mut txn,
        "INSERT INTO documents (id, embedding, title, author_id) VALUES (9, '[1.0, 0.0]', 'x', 2) USING OPERATION_ID 'own-9'",
    )
    .expect("insert");
    let q = "SELECT id FROM documents WHERE title = 'x' LIMIT 10";
    let plain = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, q)
        .expect("plain");
    let locked = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, &format!("{q} FOR UPDATE"))
        .expect("locked");
    assert_eq!(format!("{plain:?}"), format!("{locked:?}"));
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// 他テナントの Private 行は、ロック句つき SELECT の結果にも現れない（句なしと同一）。
#[test]
fn locking_select_does_not_leak_other_tenants_rows() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let other =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("tenant b");
    let q = "SELECT id FROM documents WHERE title = 'x' LIMIT 10";
    let plain = core
        .execute_sql_in_session(&other, &mut session, q)
        .expect("plain");
    let locked = core
        .execute_sql_in_session(&other, &mut session, &format!("{q} FOR UPDATE"))
        .expect("locked");
    assert_eq!(format!("{plain:?}"), format!("{locked:?}"));
}

/// `SKIP LOCKED` は #2231 まで `0A000`。writer に触れない（別スレッドの NOWAIT 取得が成功する）。
#[test]
fn skip_locked_is_still_0a000_without_touching_writer() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let sql = "SELECT id FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE SKIP LOCKED";
    // セッションを持たない後方互換の入口（`execute_sql`）は行ロック句の構文を認識しない。
    assert!(core.execute_sql(&ctx(), sql).is_err());
    let mut session = SessionState::default();
    assert_eq!(
        core.execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err("gated")
            .wire_code(),
        "0A000"
    );
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
    assert_eq!(
        core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "SELECT 1")
            .expect_err("failed txn")
            .wire_code(),
        "25P02"
    );
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    // writer を取っていないので、別セッションの NOWAIT 取得が成功する。
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut s2 = SessionState::default();
                core.execute_sql_in_session(
                    &ctx(),
                    &mut s2,
                    "SELECT id FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE NOWAIT",
                )
                .expect("nowait succeeds");
            })
            .join()
            .expect("join");
    });
}

type Attempt = (Result<(), (String, String)>, TransactionStatus);

/// 別セッション（別スレッド）で `BEGIN` → `sql` を実行し、結果と Tx の状態を返す。
fn attempt_in_other_session(core: &EngineCore, sql: &str) -> Attempt {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut s2 = SessionState::default();
                let mut t2 = core.new_session_transaction();
                core.execute_sql_in_txn(&ctx(), &mut s2, &mut t2, "BEGIN")
                    .expect("begin");
                let r = core.execute_sql_in_txn(&ctx(), &mut s2, &mut t2, sql);
                let status = t2.status();
                let _ = core.execute_sql_in_txn(&ctx(), &mut s2, &mut t2, "ROLLBACK");
                (
                    r.map(|_| ())
                        .map_err(|e| (e.wire_code().to_string(), e.to_string())),
                    status,
                )
            })
            .join()
            .expect("join")
    })
}

/// 明示 Tx のロック句は COMMIT まで writer を保持する（別セッションの NOWAIT は `55P03`）。
/// 解放後は取得できる。エラー文に tenant 名・表名を含まない。
#[test]
fn explicit_txn_holds_writer_until_commit() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let lock_sql = "SELECT id FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE";
    let nowait_sql = format!("{lock_sql} NOWAIT");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, lock_sql)
        .expect("lock");
    let (r, status) = attempt_in_other_session(&core, &nowait_sql);
    let (code, msg) = r.expect_err("held");
    assert_eq!(code, "55P03");
    assert!(
        !msg.contains("tenant-a") && !msg.contains("documents"),
        "{msg}"
    );
    assert_eq!(status, TransactionStatus::Failed);
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "COMMIT")
        .expect("commit");
    let (r, status) = attempt_in_other_session(&core, &nowait_sql);
    r.expect("acquired after commit");
    assert_eq!(status, TransactionStatus::InTransaction);
}

/// autocommit のロック句は文の終わりで writer を解放する（直後の別セッションの NOWAIT が成功する）。
#[test]
fn autocommit_releases_writer_at_statement_end() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let sql = "SELECT id FROM documents WHERE title = 'x' LIMIT 5 FOR UPDATE";
    core.execute_sql_in_session(&ctx(), &mut session, sql)
        .expect("autocommit lock");
    let (r, _) = attempt_in_other_session(&core, &format!("{sql} NOWAIT"));
    r.expect("released at statement end");
}

/// 先行の句なし SELECT が読み取り集合に入った後、同じ表へ可視の commit が入ると、ロック句つき
/// SELECT は書き込み時直列化検査で `40001`（#2396 と同じ判定経路）。不可視の行の書き込みでは起きない。
#[test]
fn serialization_failure_on_lock_after_stale_read() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let q = "SELECT id FROM documents WHERE title = 'x' LIMIT 10";
    let other =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("tenant b");
    // 他テナントの不可視の行への書き込みでは 40001 にならない。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, q)
        .expect("read");
    let mut s2 = SessionState::default();
    core.execute_sql_in_session(
        &other,
        &mut s2,
        "INSERT INTO documents (id, embedding, title, author_id) VALUES (100, '[1.0, 0.0]', 'x', 1) USING OPERATION_ID 'other-100'",
    )
    .expect("other tenant insert");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, &format!("{q} FOR UPDATE"))
        .expect("invisible commit does not conflict");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    // 可視の行への書き込みは 40001。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&ctx(), &mut session, &mut txn, q)
        .expect("read");
    core.execute_sql_in_session(
        &ctx(),
        &mut s2,
        "INSERT INTO documents (id, embedding, title, author_id) VALUES (101, '[1.0, 0.0]', 'x', 1) USING OPERATION_ID 'own-101'",
    )
    .expect("visible insert");
    let err = core
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, &format!("{q} FOR UPDATE"))
        .expect_err("serialization failure");
    assert_eq!(err.wire_code(), "40001");
    assert_eq!(txn.status(), TransactionStatus::Failed);
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
fn extended_query_parse_and_describe_succeed_and_execute_returns_plain_rows() {
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
    let params = [Some(b"x".to_vec())];
    let bound = core.bind_prepared(&prepared, &params).expect("bind");
    assert!(matches!(bound, ParsedSql::LockingSelect(_)));
    let bound_plain = core.bind_prepared(&plain, &params).expect("bind plain");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let locked = core
        .execute_parsed_in_session(&ctx(), &mut session, &bound)
        .expect("execute locking select");
    let plain_out = core
        .execute_parsed_in_session(&ctx(), &mut session, &bound_plain)
        .expect("execute plain");
    assert_eq!(format!("{plain_out:?}"), format!("{locked:?}"));
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
        .expect_err("rejected");
    assert_eq!(code.wire_code(), "0A000");
    // 一時表向けの汎用拒否ではなく、行ロックの実行ゲートに到達している。
    assert!(
        code.to_string().contains("row-level locking"),
        "unexpected: {code}"
    );
}

#[test]
fn clause_words_as_quoted_names_are_not_combination_violations() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    // 引用付きの表名 `having`／`distinct` を HAVING／DISTINCT 句と誤認しない（FROM の表は未存在で 42P01）。
    for sql in [
        "SELECT id FROM \"having\" LIMIT 5 FOR UPDATE",
        "SELECT id FROM \"distinct\" LIMIT 5 FOR UPDATE",
    ] {
        let code = parse_code(&core, sql);
        assert_ne!(code, "0A000", "{sql}");
    }
}

#[test]
fn cte_with_column_list_is_rejected() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let r = core.parse_sql(
        "WITH c(a, b) AS (SELECT id, title FROM documents LIMIT 5) SELECT a FROM c LIMIT 5 FOR UPDATE",
    );
    assert_eq!(r.expect_err("rejected").wire_code(), "0A000");
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

/// 拡張クエリでは schema 修飾名が内部キーへ書き換わった後でも、`OF` は公開名（名前部）で
/// 照合され、単純クエリと同じ結果になる。別名があれば元の表名では指せない。
#[test]
fn extended_query_schema_qualified_of_matches_simple_query() {
    let (core, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut s = SessionState::default();
    s.allow_ddl();
    assert!(s.bind_resolution_tenant("tenant-a"));
    for sql in [
        "CREATE SCHEMA s",
        "CREATE TABLE s.docs (id BIGINT PRIMARY KEY, n INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx(), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let ext = |sql: &str| {
        core.parse_sql_prepared_with_declared_in_session(&s, sql, &[])
            .map(|_| ())
            .map_err(|e| e.wire_code().to_string())
    };
    assert_eq!(
        ext("SELECT id FROM s.docs LIMIT 5 FOR UPDATE OF docs"),
        Ok(())
    );
    assert_eq!(
        ext("SELECT id FROM s.docs LIMIT 5 FOR UPDATE OF nothing"),
        Err("42P01".to_string())
    );
    assert_eq!(
        ext("SELECT d.id FROM s.docs AS d LIMIT 5 FOR UPDATE OF docs"),
        Err("42P01".to_string())
    );
    assert_eq!(
        ext("SELECT d.id FROM s.docs AS d LIMIT 5 FOR UPDATE OF d"),
        Ok(())
    );
    // 暗黙の別名が句の語と同名（`window`）でも別名として扱う。
    assert_eq!(
        ext("SELECT id FROM documents window LIMIT 5 FOR UPDATE OF documents"),
        Err("42P01".to_string())
    );
}
