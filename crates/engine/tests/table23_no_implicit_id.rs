//! SQL 表層で新しく作る表が暗黙の `id` を持たないことの結合テスト（Issue #2199。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-23・TABLE-16・TABLE-12、`rls.md` RLS-9・RLS-10、
//! `sql-surface.md` SQL-17・SQL-18・SQL-20・SQL-32）。
//!
//! 検証の軸は 4 つ。(1) `id` 列を持たない表・PRIMARY KEY を持たない表で INSERT／SELECT／UPDATE／
//! DELETE が動くこと。(2) `id` を通常の TEXT 列として宣言でき、`SELECT *` は宣言した列だけを返すこと。
//! (3) 宣言の無い `id`・内部の行識別子を表す名前は未定義列（`42703`）になり、値がどこにも出ないこと。
//! (4) テナント境界（同じキー値を別テナントが持てる・他テナントの行が見えない）と、従来方式の表
//! （Rust API で作った表）の挙動が変わらないこと。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> SqlOutcome {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(ctx, &mut s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()))
}

fn code_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    let mut s = SessionState::default();
    s.allow_ddl();
    match core.execute_sql_in_session(ctx, &mut s, sql) {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run(core, ctx, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn column_names(q: &QueryResult) -> Vec<String> {
    q.columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "<pseudo id>".to_string(),
            ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::AliasedId { name } => name.clone(),
        })
        .collect()
}

fn texts(q: &QueryResult, column: usize) -> Vec<String> {
    let mut out: Vec<String> = q
        .rows
        .iter()
        .map(|r| match r.cells.get(column) {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected text cell, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

#[test]
fn table_without_id_or_primary_key_supports_dml() {
    let (core, _g) = new_core("t23-noid-dml");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT, n INTEGER)");
    run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('a', 1)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('b', 2), ('c', 3)",
    );
    // 同じ内容の行を重ねても拒否されない（PK なし）。
    run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('a', 1)",
    );

    let all = query(&core, &alice, "SELECT * FROM notes LIMIT 10");
    assert_eq!(column_names(&all), vec!["title", "n"]);
    assert_eq!(all.rows.len(), 4);

    run(
        &core,
        &alice,
        "UPDATE notes SET n = 9 WHERE title = 'b' USING OPERATION_ID 'u1'",
    );
    let b = query(
        &core,
        &alice,
        "SELECT n FROM notes WHERE title = 'b' LIMIT 10",
    );
    assert_eq!(b.rows.len(), 1);

    run(
        &core,
        &alice,
        "DELETE FROM notes WHERE title = 'a' USING OPERATION_ID 'd1'",
    );
    let rest = query(&core, &alice, "SELECT title FROM notes LIMIT 10");
    assert_eq!(texts(&rest, 0), vec!["b", "c"]);

    // 宣言の無い `id` は未定義列（PG と同じ）。値は応答に出ない。
    for sql in [
        "SELECT id FROM notes LIMIT 10",
        "SELECT title FROM notes WHERE id = 1 LIMIT 10",
        "SELECT title FROM notes ORDER BY id LIMIT 10",
        "SELECT COUNT(id) FROM notes",
    ] {
        assert_eq!(code_of(&core, &alice, sql), "42703", "{sql}");
    }
    assert_eq!(
        code_of(
            &core,
            &alice,
            "UPDATE notes SET n = 1 WHERE id = 1 USING OPERATION_ID 'u2'"
        ),
        "42703"
    );
    assert_eq!(
        code_of(
            &core,
            &alice,
            "DELETE FROM notes WHERE id = 1 USING OPERATION_ID 'd2'"
        ),
        "42703"
    );
    assert_eq!(
        code_of(&core, &alice, "SELECT ctid FROM notes LIMIT 10"),
        "42703"
    );
}

#[test]
fn id_can_be_declared_as_text_column() {
    let (core, _g) = new_core("t23-noid-text");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO users (id, name) VALUES ('u-1', 'Ann'), ('u-2', 'Bob')",
    );
    let all = query(&core, &alice, "SELECT * FROM users LIMIT 10");
    assert_eq!(column_names(&all), vec!["id", "name"]);
    assert_eq!(texts(&all, 0), vec!["u-1", "u-2"]);

    // PK の一意性（テナント内）。
    assert_eq!(
        code_of(
            &core,
            &alice,
            "INSERT INTO users (id, name) VALUES ('u-1', 'dup')"
        ),
        "23505"
    );
    // 省略した PK 列は NOT NULL 違反。
    assert_eq!(
        code_of(&core, &alice, "INSERT INTO users (name) VALUES ('x')"),
        "23502"
    );

    run(
        &core,
        &alice,
        "UPDATE users SET name = 'Anne' WHERE id = 'u-1' USING OPERATION_ID 'u1'",
    );
    let one = query(
        &core,
        &alice,
        "SELECT name FROM users WHERE id = 'u-1' LIMIT 10",
    );
    assert_eq!(texts(&one, 0), vec!["Anne"]);

    // `ON CONFLICT (id)` は通常の列（PK）として解決する。
    run(
        &core,
        &alice,
        "INSERT INTO users (id, name) VALUES ('u-2', 'Robert') \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
    );
    let two = query(
        &core,
        &alice,
        "SELECT name FROM users WHERE id = 'u-2' LIMIT 10",
    );
    assert_eq!(texts(&two, 0), vec!["Robert"]);

    run(
        &core,
        &alice,
        "DELETE FROM users WHERE id = 'u-1' USING OPERATION_ID 'd1'",
    );
    let left = query(&core, &alice, "SELECT id FROM users LIMIT 10");
    assert_eq!(texts(&left, 0), vec!["u-2"]);
}

#[test]
fn numeric_id_column_is_an_ordinary_column_and_single_row_dml_uses_it() {
    let (core, _g) = new_core("t23-noid-numeric");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, label TEXT)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO items (id, label) VALUES (5, 'five'), (-3, 'neg')",
    );
    // 数値リテラルの `WHERE id = <n>` は物理キー指定ではなく通常の述語。
    run(
        &core,
        &alice,
        "UPDATE items SET label = 'FIVE' WHERE id = 5 USING OPERATION_ID 'u1'",
    );
    let q = query(
        &core,
        &alice,
        "SELECT label FROM items WHERE id = 5 LIMIT 10",
    );
    assert_eq!(texts(&q, 0), vec!["FIVE"]);
    let neg = query(
        &core,
        &alice,
        "SELECT label FROM items WHERE id = -3 LIMIT 10",
    );
    assert_eq!(texts(&neg, 0), vec!["neg"]);
    run(
        &core,
        &alice,
        "DELETE FROM items WHERE id = 5 USING OPERATION_ID 'd1'",
    );
    let left = query(&core, &alice, "SELECT label FROM items LIMIT 10");
    assert_eq!(texts(&left, 0), vec!["neg"]);
}

#[test]
fn returning_star_lists_only_declared_columns() {
    let (core, _g) = new_core("t23-noid-returning");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT, n INTEGER)");
    match run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('a', 1) RETURNING *",
    ) {
        SqlOutcome::Returning(r) => {
            let q = r.result;
            assert_eq!(column_names(&q), vec!["title", "n"]);
            assert_eq!(q.rows.len(), 1);
        }
        other => panic!("expected RETURNING rows, got {other:?}"),
    }
    assert_eq!(
        code_of(
            &core,
            &alice,
            "INSERT INTO notes (title, n) VALUES ('b', 2) RETURNING id"
        ),
        "42703"
    );
}

#[test]
fn tenants_keep_separate_rows_and_keys() {
    let (core, _g) = new_core("t23-noid-tenants");
    let alice = ctx("alice");
    let bob = ctx("bob");
    run(
        &core,
        &alice,
        "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO users (id, name) VALUES ('same', 'alice-row')",
    );
    // 別テナントは同じキー値を持てる（テナントごとの一意性）。
    run(
        &core,
        &bob,
        "INSERT INTO users (id, name) VALUES ('same', 'bob-row')",
    );
    let a = query(&core, &alice, "SELECT name FROM users LIMIT 10");
    assert_eq!(texts(&a, 0), vec!["alice-row"]);
    let b = query(&core, &bob, "SELECT name FROM users LIMIT 10");
    assert_eq!(texts(&b, 0), vec!["bob-row"]);
    // 他テナントの行は更新も削除もできない（影響 0 行）。
    run(
        &core,
        &bob,
        "DELETE FROM users WHERE id = 'same' USING OPERATION_ID 'd1'",
    );
    let after = query(&core, &alice, "SELECT name FROM users LIMIT 10");
    assert_eq!(texts(&after, 0), vec!["alice-row"]);
    let bob_after = query(&core, &bob, "SELECT name FROM users LIMIT 10");
    assert!(bob_after.rows.is_empty());
}

#[test]
fn knn_over_table_without_id_returns_declared_columns_only() {
    let (core, _g) = new_core("t23-noid-knn");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE docs (title TEXT, embedding VECTOR(2))",
    );
    run(
        &core,
        &alice,
        "INSERT INTO docs (title, embedding) VALUES ('near', '[1.0,0.0]'), ('far', '[0.0,1.0]')",
    );
    let hits = query(
        &core,
        &alice,
        "SELECT title FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
    );
    assert_eq!(column_names(&hits), vec!["title"]);
    assert_eq!(hits.rows.len(), 2);
    let star = query(
        &core,
        &alice,
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2",
    );
    assert_eq!(column_names(&star), vec!["title", "embedding"]);
}

#[test]
fn legacy_tables_created_by_the_rust_api_keep_the_pseudo_id() {
    let (core, g) = new_core("t23-noid-legacy");
    drop(core);
    let storage = Storage::open(&g.0).expect("reopen");
    storage
        .create_table(&TableSchema::new(
            "legacy",
            vec![ColumnDef::new("title", ColumnType::Text, true)],
        ))
        .expect("create legacy table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "INSERT INTO legacy (id, title) VALUES (7, 'seven')",
    );
    let q = query(&core, &alice, "SELECT * FROM legacy LIMIT 10");
    assert_eq!(column_names(&q), vec!["<pseudo id>", "title"]);
    assert_eq!(q.rows.len(), 1);
    // 従来方式の表では `id` が必須のまま。
    assert_eq!(
        code_of(&core, &alice, "INSERT INTO legacy (title) VALUES ('x')"),
        "22000"
    );
    run(
        &core,
        &alice,
        "UPDATE legacy SET title = 'SEVEN' WHERE id = 7 USING OPERATION_ID 'u1'",
    );
    run(
        &core,
        &alice,
        "DELETE FROM legacy WHERE id = 7 USING OPERATION_ID 'd1'",
    );
    assert!(query(&core, &alice, "SELECT id FROM legacy LIMIT 10")
        .rows
        .is_empty());
}

#[test]
fn relational_paths_work_without_implicit_id() {
    let (core, _g) = new_core("t23-noid-relational");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE authors (name TEXT, country TEXT)",
    );
    run(
        &core,
        &alice,
        "CREATE TABLE books (title TEXT, author TEXT, pages INTEGER)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO authors (name, country) VALUES ('ann', 'jp'), ('bob', 'us')",
    );
    run(
        &core,
        &alice,
        "INSERT INTO books (title, author, pages) VALUES \
         ('t1', 'ann', 10), ('t2', 'ann', 20), ('t3', 'bob', 30)",
    );
    let ok = |sql: &str| -> QueryResult { query(&core, &alice, sql) };

    let join = ok("SELECT * FROM books JOIN authors ON books.author = authors.name LIMIT 10");
    assert_eq!(
        column_names(&join),
        vec!["title", "author", "pages", "name", "country"]
    );
    assert_eq!(join.rows.len(), 3);
    let join_cols = ok(
        "SELECT books.title, authors.country FROM books JOIN authors \
         ON books.author = authors.name WHERE authors.country = 'jp' LIMIT 10",
    );
    assert_eq!(join_cols.rows.len(), 2);

    let agg = ok("SELECT COUNT(*) FROM books");
    assert_eq!(agg.rows.len(), 1);
    let grouped = ok("SELECT author, SUM(pages) FROM books GROUP BY author");
    assert_eq!(grouped.rows.len(), 2);

    let sub = ok("SELECT title FROM books WHERE author IN (SELECT name FROM authors WHERE country = 'us' LIMIT 10) LIMIT 10");
    assert_eq!(texts(&sub, 0), vec!["t3"]);
    let ordered = ok("SELECT title FROM books ORDER BY pages DESC LIMIT 2");
    assert_eq!(ordered.rows.len(), 2);
    let distinct = ok("SELECT DISTINCT author FROM books");
    assert_eq!(distinct.rows.len(), 2);
    let cte =
        ok("WITH big AS (SELECT * FROM books WHERE author = 'ann') SELECT title FROM big LIMIT 10");
    assert_eq!(cte.rows.len(), 2);
    let union = ok("SELECT name FROM authors UNION SELECT author FROM books");
    assert_eq!(union.rows.len(), 2);
    let window = ok("SELECT title, ROW_NUMBER() OVER (ORDER BY pages) FROM books LIMIT 10");
    assert_eq!(window.rows.len(), 3);

    run(
        &core,
        &alice,
        "CREATE VIEW long_books AS SELECT * FROM books WHERE author = 'ann'",
    );
    let view = ok("SELECT * FROM long_books LIMIT 10");
    assert_eq!(column_names(&view), vec!["title", "author", "pages"]);
    assert_eq!(view.rows.len(), 2);

    // EXPLAIN は rowid を出さず通る。
    match run(
        &core,
        &alice,
        "EXPLAIN SELECT title FROM books WHERE author = 'ann' LIMIT 10",
    ) {
        SqlOutcome::Explain(_) => {}
        other => panic!("expected Explain, got {other:?}"),
    }
}

#[test]
fn upsert_and_multi_row_insert_without_implicit_id() {
    let (core, _g) = new_core("t23-noid-upsert");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER, tag TEXT UNIQUE)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO kv (k, v, tag) VALUES ('a', 1, 't-a'), ('b', 2, 't-b'), ('c', 3, 't-c')",
    );
    // DO NOTHING・DO UPDATE（PRIMARY KEY 対象・UNIQUE 対象）。
    run(
        &core,
        &alice,
        "INSERT INTO kv (k, v, tag) VALUES ('a', 10, 'x') ON CONFLICT (k) DO NOTHING",
    );
    run(
        &core,
        &alice,
        "INSERT INTO kv (k, v, tag) VALUES ('d', 4, 't-d'), ('a', 11, 'y') \
         ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
    );
    run(
        &core,
        &alice,
        "INSERT INTO kv (k, v, tag) VALUES ('z', 99, 't-b') \
         ON CONFLICT (tag) DO UPDATE SET v = EXCLUDED.v",
    );
    let rows = query(&core, &alice, "SELECT k, v FROM kv ORDER BY k LIMIT 10");
    assert_eq!(rows.rows.len(), 4);
    assert_eq!(texts(&rows, 0), vec!["a", "b", "c", "d"]);
    // 一意制約違反。
    assert_eq!(
        code_of(
            &core,
            &alice,
            "INSERT INTO kv (k, v, tag) VALUES ('q', 0, 't-c')"
        ),
        "23505"
    );
    // RETURNING 付き UPSERT。
    match run(
        &core,
        &alice,
        "INSERT INTO kv (k, v, tag) VALUES ('e', 5, 't-e') \
         ON CONFLICT (k) DO NOTHING RETURNING *",
    ) {
        SqlOutcome::Returning(r) => {
            assert_eq!(column_names(&r.result), vec!["k", "v", "tag"]);
            assert_eq!(r.result.rows.len(), 1);
        }
        other => panic!("expected RETURNING rows, got {other:?}"),
    }
}

#[test]
fn explicit_transaction_and_resend_work_without_implicit_id() {
    let (core, _g) = new_core("t23-noid-txn");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT)");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "INSERT INTO notes (title) VALUES ('in-txn')",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&alice, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
    }
    assert_eq!(
        query(&core, &alice, "SELECT title FROM notes LIMIT 10")
            .rows
            .len(),
        1
    );
    // operation_id の再送は同じ内容なら重複として拒否され、行は増えない。
    run(
        &core,
        &alice,
        "INSERT INTO notes (title) VALUES ('once') USING OPERATION_ID 'op-1'",
    );
    assert_ne!(
        code_of(
            &core,
            &alice,
            "INSERT INTO notes (title) VALUES ('once') USING OPERATION_ID 'op-1'"
        )
        .as_str(),
        "ok",
    );
    assert_eq!(
        query(&core, &alice, "SELECT title FROM notes LIMIT 10")
            .rows
            .len(),
        2
    );
}

#[test]
fn partitioned_dml_and_truncate_work_without_implicit_id() {
    let (core, _g) = new_core("t23-noid-partitioned");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT, n INTEGER)");
    for i in 0..5 {
        run(
            &core,
            &alice,
            &format!("INSERT INTO notes (title, n) VALUES ('t{i}', {i})"),
        );
    }
    run(
        &core,
        &alice,
        "UPDATE notes SET n = 100 WHERE n > 1 USING OPERATION_ID 'pu1' PARTITIONED CHUNK 2",
    );
    run(
        &core,
        &alice,
        "DELETE FROM notes WHERE n = 100 USING OPERATION_ID 'pd1' PARTITIONED CHUNK 2",
    );
    assert_eq!(
        query(&core, &alice, "SELECT n FROM notes LIMIT 10")
            .rows
            .len(),
        2
    );
    run(&core, &alice, "TRUNCATE notes USING OPERATION_ID 'tr1'");
    assert!(query(&core, &alice, "SELECT n FROM notes LIMIT 10")
        .rows
        .is_empty());
    // 採番は単調に続き、再投入できる。
    run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('again', 1)",
    );
    assert_eq!(
        query(&core, &alice, "SELECT n FROM notes LIMIT 10")
            .rows
            .len(),
        1
    );
}

#[test]
fn temp_table_has_no_implicit_id() {
    let (core, _g) = new_core("t23-noid-temp");
    let alice = ctx("alice");
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE TEMP TABLE scratch (id TEXT, n INTEGER)",
        "INSERT INTO scratch (id, n) VALUES ('x', 1), ('y', 2)",
    ] {
        core.execute_sql_in_session(&alice, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
    }
    match core
        .execute_sql_in_session(&alice, &mut s, "SELECT * FROM scratch LIMIT 10")
        .expect("select")
    {
        SqlOutcome::Query(q) => {
            assert_eq!(column_names(&q), vec!["id", "n"]);
            assert_eq!(q.rows.len(), 2);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn copy_from_does_not_require_an_id_column() {
    let (core, _g) = new_core("t23-noid-copy");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT, n INTEGER)");
    let session = SessionState::default();
    let plan = core
        .begin_copy(
            &alice,
            &session,
            "COPY notes (title, n) FROM STDIN USING OPERATION_ID 'copy-1'",
        )
        .expect("begin copy");
    let CopyPlan::From(mut copy) = plan else {
        panic!("expected COPY FROM STDIN plan");
    };
    copy.feed(b"a\t1\nb\t2\nc\t3\n").expect("feed");
    let batch = copy.finish().expect("finish");
    let outcome = core.commit_copy_in(&alice, batch).expect("commit");
    assert_eq!(outcome.rows_affected, 3);
    let rows = query(&core, &alice, "SELECT title FROM notes LIMIT 10");
    assert_eq!(texts(&rows, 0), vec!["a", "b", "c"]);
    // 従来方式の表と違い、列リストに `id` を含めても未定義列（宣言していないため）。
    assert!(core
        .begin_copy(
            &alice,
            &session,
            "COPY notes (id, title) FROM STDIN USING OPERATION_ID 'copy-2'"
        )
        .is_err());
}

#[test]
fn alter_table_keeps_the_row_identity_and_ddl_works() {
    let (core, _g) = new_core("t23-noid-alter");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT)");
    run(
        &core,
        &alice,
        "INSERT INTO notes (title) VALUES ('a'), ('b')",
    );
    run(
        &core,
        &alice,
        "ALTER TABLE notes ADD COLUMN n INTEGER DEFAULT 7",
    );
    run(&core, &alice, "ALTER TABLE notes ADD UNIQUE (title)");
    run(&core, &alice, "CREATE INDEX notes_n ON notes (n)");
    run(&core, &alice, "INSERT INTO notes (title) VALUES ('c')");
    let q = query(&core, &alice, "SELECT * FROM notes LIMIT 10");
    assert_eq!(column_names(&q), vec!["title", "n"]);
    assert_eq!(q.rows.len(), 3);
    assert_eq!(
        code_of(&core, &alice, "INSERT INTO notes (title) VALUES ('a')"),
        "23505"
    );
    run(&core, &alice, "ALTER TABLE notes RENAME TO notes2");
    assert_eq!(
        query(&core, &alice, "SELECT title FROM notes2 LIMIT 10")
            .rows
            .len(),
        3
    );
    run(&core, &alice, "ALTER TABLE notes2 DROP COLUMN n");
    run(&core, &alice, "DROP TABLE notes2");
}

#[test]
fn views_over_a_table_without_id_work() {
    let (core, _g) = new_core("t23-noid-views");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT, n INTEGER)");
    run(
        &core,
        &alice,
        "INSERT INTO notes (title, n) VALUES ('a', 1), ('b', 2)",
    );
    run(&core, &alice, "CREATE VIEW v AS SELECT * FROM notes");
    run(&core, &alice, "INSERT INTO v (title, n) VALUES ('c', 3)");
    assert_eq!(
        query(&core, &alice, "SELECT title FROM v LIMIT 10")
            .rows
            .len(),
        3
    );
    run(
        &core,
        &alice,
        "UPDATE v SET n = 9 WHERE title = 'a' USING OPERATION_ID 'vu1'",
    );
    run(
        &core,
        &alice,
        "DELETE FROM v WHERE title = 'b' USING OPERATION_ID 'vd1'",
    );
    let q = query(&core, &alice, "SELECT title FROM notes LIMIT 10");
    assert_eq!(texts(&q, 0), vec!["a", "c"]);
    // 宣言の無い `id` は未定義列（ビュー経由の単一行形も同じ）。
    assert_eq!(
        code_of(
            &core,
            &alice,
            "UPDATE v SET n = 1 WHERE id = 1 USING OPERATION_ID 'vu2'"
        ),
        "42703"
    );
    run(
        &core,
        &alice,
        "CREATE MATERIALIZED VIEW m AS SELECT title FROM notes",
    );
    run(&core, &alice, "REFRESH MATERIALIZED VIEW m");
    assert_eq!(
        query(&core, &alice, "SELECT title FROM m LIMIT 10")
            .rows
            .len(),
        2
    );
}

#[test]
fn foreign_keys_between_tables_without_id_use_declared_keys() {
    let (core, _g) = new_core("t23-noid-fk");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE parents (code TEXT PRIMARY KEY, name TEXT)",
    );
    run(
        &core,
        &alice,
        "CREATE TABLE kids (parent_code TEXT REFERENCES parents (code) ON DELETE CASCADE, note TEXT)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO parents (code, name) VALUES ('p1', 'one'), ('p2', 'two')",
    );
    run(
        &core,
        &alice,
        "INSERT INTO kids (parent_code, note) VALUES ('p1', 'x'), ('p1', 'y'), ('p2', 'z')",
    );
    assert_eq!(
        code_of(
            &core,
            &alice,
            "INSERT INTO kids (parent_code, note) VALUES ('missing', 'w')"
        ),
        "23503"
    );
    run(
        &core,
        &alice,
        "DELETE FROM parents WHERE code = 'p1' USING OPERATION_ID 'fk-d1'",
    );
    let left = query(&core, &alice, "SELECT note FROM kids LIMIT 10");
    assert_eq!(texts(&left, 0), vec!["z"]);
    // 暗黙の `id` 参照（親の `id` 疑似列）は採番方式の親には無い（PG と同じく主キーの無い親は `42830`）。
    run(&core, &alice, "CREATE TABLE loose (a TEXT)");
    assert_eq!(
        code_of(
            &core,
            &alice,
            "CREATE TABLE refs (r BIGINT REFERENCES loose, note TEXT)"
        ),
        "42830"
    );
    // 親が宣言した `id` 列（主キー）への FK は、物理キーの `id` 参照と区別できないため未対応（`0A000`）。
    run(
        &core,
        &alice,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)",
    );
    assert_eq!(
        code_of(
            &core,
            &alice,
            "CREATE TABLE posts (author BIGINT REFERENCES users (id), body TEXT)"
        ),
        "0A000"
    );
}

#[test]
fn prepared_statement_parameters_are_typed_by_the_declared_id_column() {
    let (core, _g) = new_core("t23-noid-prepared");
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE users (id TEXT PRIMARY KEY, n INTEGER)",
    );
    run(
        &core,
        &alice,
        "INSERT INTO users (id, n) VALUES ('u1', 1), ('u2', 2)",
    );
    let rows = query(
        &core,
        &alice,
        "SELECT n FROM users WHERE id = 'u2' LIMIT 10",
    );
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn rust_api_row_apis_reject_tables_without_implicit_id() {
    let path = unique_db_path("t23-noid-rustapi");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx("alice");
    run(
        &core,
        &alice,
        "CREATE TABLE docs (title TEXT, embedding VECTOR(2))",
    );
    run(
        &core,
        &alice,
        "INSERT INTO docs (title, embedding) VALUES ('a', '[1.0,0.0]')",
    );
    use engine::core::VectorCore;
    // 行の識別子を `SearchHit.id` として返す Rust API は拒否される（行の rowid を露出しない）。
    let err = core
        .search(&alice, "docs", &[1.0, 0.0], 3)
        .expect_err("rust api search is rejected");
    let text = format!("{err} {err:?}");
    assert!(
        text.contains("row identity mode is not supported yet"),
        "{text}"
    );
    assert!(!text.contains("alice") && !text.contains("docs"), "{text}");
    assert!(core.get_row(&alice, "docs", "alice", 1).is_err());
}

#[test]
fn path_and_body_columns_are_ordinary_columns_on_a_table_without_id() {
    let (core, _g) = new_core("t23-noid-pathbody");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE pages (path TEXT, body TEXT)");
    // 従来方式の表ではファイル形 INSERT（`id`・ベクトル列なしの `path`／`body`）として束縛される形が、
    // 採番方式の表では通常の行形 INSERT になる。
    run(
        &core,
        &alice,
        "INSERT INTO pages (path, body) VALUES ('a.txt', 'alpha'), ('b.txt', 'beta')",
    );
    let q = query(&core, &alice, "SELECT path FROM pages LIMIT 10");
    assert_eq!(texts(&q, 0), vec!["a.txt", "b.txt"]);
}

#[test]
fn cursors_over_a_table_without_id_return_declared_columns() {
    let (core, _g) = new_core("t23-noid-cursor");
    let alice = ctx("alice");
    run(&core, &alice, "CREATE TABLE notes (title TEXT)");
    run(
        &core,
        &alice,
        "INSERT INTO notes (title) VALUES ('a'), ('b'), ('c')",
    );
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "DECLARE c CURSOR FOR SELECT * FROM notes LIMIT 100",
    ] {
        core.execute_sql_in_txn(&alice, &mut s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e:?}", e.wire_code()));
    }
    let mut seen = 0;
    loop {
        match core
            .execute_sql_in_txn(&alice, &mut s, &mut txn, "FETCH 2 FROM c")
            .expect("fetch")
        {
            SqlOutcome::Fetch(q) => {
                assert_eq!(column_names(&q), vec!["title"]);
                if q.rows.is_empty() {
                    break;
                }
                seen += q.rows.len();
            }
            other => panic!("expected rows, got {other:?}"),
        }
    }
    assert_eq!(seen, 3);
    core.execute_sql_in_txn(&alice, &mut s, &mut txn, "COMMIT")
        .expect("commit");
}

#[test]
fn constraints_cannot_reference_an_undeclared_id() {
    let (core, _g) = new_core("t23-noid-constraints");
    let alice = ctx("alice");
    // 暗黙の `id` が無い表では、CHECK・UNIQUE・索引・PRIMARY KEY が `id` を参照しても未定義列になる。
    for sql in [
        "CREATE TABLE c1 (a INTEGER, CHECK (id > 0))",
        "CREATE TABLE c2 (a INTEGER, UNIQUE (id))",
        "CREATE TABLE c3 (a INTEGER, PRIMARY KEY (id))",
    ] {
        let code = code_of(&core, &alice, sql);
        assert!(code == "42703" || code == "42601", "{sql}: {code}");
    }
    run(&core, &alice, "CREATE TABLE c4 (a INTEGER)");
    for sql in [
        "CREATE INDEX c4_id ON c4 (id)",
        "CREATE UNIQUE INDEX c4_uid ON c4 (id)",
        "ALTER TABLE c4 ADD UNIQUE (id)",
        "ALTER TABLE c4 ADD CHECK (id > 0)",
    ] {
        let code = code_of(&core, &alice, sql);
        assert!(!code.starts_with("ok"), "{sql} must be rejected: {code}");
    }
    // 表に何も書かれていない。
    assert!(query(&core, &alice, "SELECT a FROM c4 LIMIT 10")
        .rows
        .is_empty());
}
