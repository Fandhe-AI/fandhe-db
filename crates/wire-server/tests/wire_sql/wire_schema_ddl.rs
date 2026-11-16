//! `CREATE SCHEMA` / `DROP SCHEMA`（TABLE-27・TABLE-36・SQL-23、Issue #1713）の簡易クエリプロトコル
//! 経由（生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/table27_schema_ddl.rs` が確定オラクル。本ファイルは
//! `wire_sequence_ddl.rs` と同じ流儀で、`CommandComplete` タグ・SQLSTATE
//! （`42P06`／`3F000`／`42939`／`42501`）・2 テナント接続での独立性と存在の秘匿に徹する。
//! `2BP01`（中身のある SCHEMA の DROP）は DDL でオブジェクトを tenant SCHEMA に作れない段階では
//! wire から到達できないため、engine 側の crate 内テストで検証する。

use crate::common;

use crate::temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-schema-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_server(store: UserStore, engine: Arc<EngineCore>) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

fn allowed_store(users: &[(&str, &str, &str)], allowed: &[&str]) -> UserStore {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    store
        .with_ddl_allowed_users(&allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("allowed usernames must be known")
}

/// DDL 実行権限を持たないユーザー（既定）は、SCHEMA の有無を問わず `42501`。
#[test]
fn wire_schema_ddl_without_permission_is_rejected() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for sql in [
        "CREATE SCHEMA s1",
        "CREATE SCHEMA pg_x",
        "DROP SCHEMA s1",
        "DROP SCHEMA IF EXISTS s1",
        "DROP SCHEMA public",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}

/// 許可ユーザーの CREATE／DROP は固定タグを返し、重複・不在・予約名・組み込み名は各 SQLSTATE。
#[test]
fn wire_schema_ddl_lifecycle_and_sqlstates() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE SCHEMA s1");
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE SCHEMA IF NOT EXISTS s1");
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    for (sql, sqlstate) in [
        ("CREATE SCHEMA s1", "42P06"),
        ("CREATE SCHEMA public", "42P06"),
        ("CREATE SCHEMA pg_x", "42939"),
        ("CREATE SCHEMA IF NOT EXISTS pg_x", "42939"),
        ("DROP SCHEMA public", "42501"),
        ("DROP SCHEMA ghost", "3F000"),
        // Issue #2236: 所有者の指定は解決される（存在しないロールは 42704）。
        ("CREATE SCHEMA AUTHORIZATION bob", "42704"),
        ("CREATE SCHEMA a.b", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }

    send_simple_query(&mut stream, "DROP SCHEMA s1");
    assert_eq!(read_command_complete(&mut stream), "DROP SCHEMA");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SCHEMA IF EXISTS s1");
    assert_eq!(read_command_complete(&mut stream), "DROP SCHEMA");
    read_ready_for_query(&mut stream);
}

/// `ALTER SCHEMA ... RENAME TO` と `ALTER TABLE ... SET SCHEMA`（Issue #1716）の CommandComplete タグと
/// SQLSTATE（権限なしは対象の有無によらず `42501`）。
#[test]
fn wire_alter_schema_and_set_schema_tags_and_sqlstates() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (sql, tag) in [
        ("CREATE SCHEMA a", "CREATE SCHEMA"),
        (
            "CREATE TABLE items (id BIGINT PRIMARY KEY, qty INTEGER)",
            "CREATE TABLE",
        ),
        ("ALTER SCHEMA a RENAME TO b", "ALTER SCHEMA"),
        ("ALTER TABLE items SET SCHEMA public", "ALTER TABLE"),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }

    for (sql, sqlstate) in [
        ("ALTER SCHEMA public RENAME TO z", "42501"),
        ("ALTER SCHEMA ghost RENAME TO z", "3F000"),
        ("ALTER SCHEMA b RENAME TO pg_x", "42939"),
        // Issue #2236: `OWNER TO` は現在ロールが束縛されていないセッションでは 42501（対象・ロールの有無を示さない）。
        ("ALTER SCHEMA b OWNER TO bob", "42501"),
        ("ALTER TABLE items SET SCHEMA b", "42501"),
        ("ALTER TABLE items SET SCHEMA ghost", "3F000"),
        ("ALTER TABLE items SET SCHEMA pg_catalog", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}

/// テナント SCHEMA 間の `ALTER TABLE a.t SET SCHEMA b;`（末尾 `;` つきの単独送信。Issue #3198）が
/// `ALTER TABLE` で完了し、移動後は新しい修飾名で読め、旧名は `42P01`、`public` との間は `42501`。
#[test]
fn wire_set_schema_between_tenant_schemas() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (sql, tag) in [
        ("CREATE SCHEMA a", "CREATE SCHEMA"),
        ("CREATE SCHEMA b", "CREATE SCHEMA"),
        (
            "CREATE TABLE a.t (id BIGINT PRIMARY KEY, qty INTEGER)",
            "CREATE TABLE",
        ),
        ("INSERT INTO a.t (id, qty) VALUES (1, 10)", "INSERT 0 1"),
        ("ALTER TABLE a.t SET SCHEMA b;", "ALTER TABLE"),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }

    send_simple_query(&mut stream, "SELECT id, qty FROM b.t");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "qty"]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string()), Some("10".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    for (sql, sqlstate) in [
        ("SELECT id FROM a.t", "42P01"),
        ("ALTER TABLE b.t SET SCHEMA public", "42501"),
        ("ALTER TABLE b.t SET SCHEMA ghost", "3F000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}

/// 2 テナントの接続で同名が独立し、他テナントにしか無い名前の DROP は存在しない名前と
/// 同じ SQLSTATE・同じ文言になる（存在の秘匿）。
#[test]
fn wire_schema_ddl_is_tenant_scoped_and_hides_existence() {
    let (core, _guard) = new_core();
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
        ],
        &["alice", "bob"],
    );
    let addr = spawn_server(store, core);
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    for stream in [&mut alice, &mut bob] {
        send_simple_query(stream, "CREATE SCHEMA shared");
        assert_eq!(read_command_complete(stream), "CREATE SCHEMA");
        read_ready_for_query(stream);
    }
    send_simple_query(&mut bob, "CREATE SCHEMA only_bob");
    assert_eq!(read_command_complete(&mut bob), "CREATE SCHEMA");
    read_ready_for_query(&mut bob);

    // alice から見て only_bob は存在しない名前と同じ応答（名前部分以外は同一文言）。
    send_simple_query(&mut alice, "DROP SCHEMA only_bob");
    expect_error_response_with_sqlstate_and_message(
        &mut alice,
        "3F000",
        "schema does not exist: only_bob",
    );
    read_ready_for_query(&mut alice);
    send_simple_query(&mut alice, "DROP SCHEMA ghost");
    expect_error_response_with_sqlstate_and_message(
        &mut alice,
        "3F000",
        "schema does not exist: ghost",
    );
    read_ready_for_query(&mut alice);

    // bob の shared を消しても alice の shared は残る。
    send_simple_query(&mut bob, "DROP SCHEMA shared");
    assert_eq!(read_command_complete(&mut bob), "DROP SCHEMA");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut alice, "CREATE SCHEMA shared");
    expect_error_response_with_sqlstate(&mut alice, "42P06");
    read_ready_for_query(&mut alice);
}

/// 要素つき `CREATE SCHEMA`（Issue #3286）。psql のように末尾 `;` を付けた単独の簡易クエリで SCHEMA と
/// 全要素が作られ（タグは `CREATE SCHEMA`）、作った表・ビューへ書き込み・読み取りできる。
/// 別 SCHEMA での作成は `42P15`、途中で失敗した文は SCHEMA を残さない（`3F000`）。
// intentional: docs/design/schema-tenant-namespace.md
#[test]
fn wire_schema_with_elements_simple_query_creates_schema_table_and_view() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "CREATE SCHEMA s CREATE TABLE t (id BIGINT PRIMARY KEY, a INTEGER) \
         CREATE VIEW v AS SELECT id, a FROM t;",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "INSERT INTO s.t (id, a) VALUES (1, 10)");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id, a FROM s.v ORDER BY id LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("10".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    for (sql, sqlstate) in [
        ("CREATE SCHEMA s2 CREATE TABLE public.x (a INT)", "42P15"),
        (
            "CREATE SCHEMA s3 CREATE TABLE t (id BIGINT) CREATE VIEW v AS SELECT id FROM s3.nosuch",
            "42P01",
        ),
        ("DROP SCHEMA s3", "3F000"),
        ("CREATE SCHEMA s4 CREATE INDEX i ON t (a)", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}

/// 要素つき `CREATE SCHEMA` は拡張クエリ（Parse／Bind／Execute）でも同じ結果になり、権限の無い
/// ユーザーは常に `42501`。
#[test]
fn wire_schema_with_elements_extended_query_and_permission_gate() {
    let (core, _guard) = new_core();
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("carol", "tenant-a", "tr0ub4dor"),
        ],
        &["alice"],
    );
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql =
        "CREATE SCHEMA ex CREATE TABLE t (id BIGINT PRIMARY KEY) CREATE VIEW v AS SELECT id FROM t";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", sql, 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(
        kind,
        b'1',
        "ParseComplete: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'C', "CommandComplete");
    assert_eq!(body, b"CREATE SCHEMA\0");
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    send_simple_query(&mut stream, "SELECT id FROM ex.v LIMIT 1");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);

    // DDL を許可されていないユーザーは、SCHEMA の有無・要素の中身に関わらず `42501`。
    let mut carol = authenticate_to_ready_for_query(addr, "carol", "tr0ub4dor");
    for sql in [
        "CREATE SCHEMA ex2 CREATE TABLE t (id BIGINT)",
        "CREATE SCHEMA ex CREATE TABLE t (id BIGINT)",
    ] {
        send_simple_query(&mut carol, sql);
        expect_error_response_with_sqlstate(&mut carol, "42501");
        read_ready_for_query(&mut carol);
    }
}

/// ENUM 型の SCHEMA 配置（Issue #3288）。psql 形（末尾 `;` 付き・1 メッセージ 1 文）の簡易クエリで、修飾名の
/// `CREATE TYPE`／`DROP TYPE` と `DROP SCHEMA ... CASCADE` が通り、固定タグを返す。同名の `public` の型と
/// 共存し、拒否後も接続が使える。列型・キャストの型名位置の `s.mood`（Issue #3289）も末尾 `;` 付きで通る。
#[test]
fn wire_enum_type_in_schema_simple_query() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (sql, tag) in [
        ("CREATE SCHEMA s;", "CREATE SCHEMA"),
        ("CREATE TYPE s.mood AS ENUM ('a');", "CREATE TYPE"),
        ("CREATE TYPE mood AS ENUM ('b');", "CREATE TYPE"),
        ("DROP TYPE s.mood;", "DROP TYPE"),
        ("DROP TYPE IF EXISTS s.mood;", "DROP TYPE"),
        ("CREATE TYPE s.mood2 AS ENUM ('a');", "CREATE TYPE"),
        ("DROP SCHEMA s CASCADE;", "DROP SCHEMA"),
        ("DROP TYPE mood;", "DROP TYPE"),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }
    for (sql, sqlstate) in [
        ("DROP TYPE s.mood;", "3F000"),
        ("CREATE TYPE ghost.mood AS ENUM ('a');", "3F000"),
        ("CREATE TYPE pg_catalog.mood AS ENUM ('a');", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
    // 拒否の後も接続は使える。列型・キャストの型名位置の SCHEMA 修飾（Issue #3289）も末尾 `;` 付きで通る。
    for (sql, tag) in [
        ("CREATE SCHEMA s;", "CREATE SCHEMA"),
        ("CREATE TYPE s.mood AS ENUM ('c', 'a', 'b');", "CREATE TYPE"),
        (
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood, ms s.mood[]);",
            "CREATE TABLE",
        ),
        ("INSERT INTO s.t (id, m) VALUES (1, 'b');", "INSERT 0 1"),
        ("INSERT INTO s.t (id, m) VALUES (2, 'c');", "INSERT 0 1"),
        ("ALTER TABLE s.t ADD COLUMN m2 s.mood;", "ALTER TABLE"),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }
    // 語彙外ラベルは `22P02`。型が使われている間の `DROP TYPE` は `2BP01`。
    // ラベル違反の文言は公開名（`s.mood`）で、内部キーの `#` を含まない。
    send_simple_query(&mut stream, "INSERT INTO s.t (id, m) VALUES (3, 'zzz');");
    expect_error_response_with_sqlstate_and_message(&mut stream, "22P02", "(enum \"s.mood\")");
    read_ready_for_query(&mut stream);
    for (sql, sqlstate) in [
        ("DROP TYPE s.mood;", "2BP01"),
        ("CREATE TABLE t (id BIGINT, m z.mood);", "3F000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
    // ORDER BY は宣言順（c < a < b）。ENUM の値はラベルのテキストで返る。
    send_simple_query(&mut stream, "SELECT id, m FROM s.t ORDER BY m, id;");
    let _columns = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("c".to_string())]
    );
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("b".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ（Parse／Bind／Execute）でも、末尾 `;` 付きの修飾名の `CREATE TYPE`／`DROP TYPE` が通る。
#[test]
fn wire_enum_type_in_schema_extended_query() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut stream, "CREATE SCHEMA s");
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    for (name, sql, tag) in [
        (
            "c1",
            "CREATE TYPE s.m2 AS ENUM ('a');",
            &b"CREATE TYPE\0"[..],
        ),
        // 列型の型名位置の修飾（Issue #3289）も拡張クエリの Parse・Bind・Execute で通る。
        (
            "t1",
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.m2);",
            &b"CREATE TABLE\0"[..],
        ),
        (
            "a1",
            "ALTER TABLE s.t ADD COLUMN m3 s.m2[];",
            &b"ALTER TABLE\0"[..],
        ),
        ("x1", "DROP TABLE s.t;", &b"DROP TABLE\0"[..]),
        ("d1", "DROP TYPE s.m2;", &b"DROP TYPE\0"[..]),
    ] {
        send_length_prefixed_message(&mut stream, b'P', &parse_body(name, sql, 0));
        let (kind, body) = read_message(&mut stream);
        assert_eq!(
            kind,
            b'1',
            "ParseComplete: {}",
            String::from_utf8_lossy(&body)
        );
        send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", name));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'2', "BindComplete");
        send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'C', "CommandComplete");
        assert_eq!(body, tag);
        send_length_prefixed_message(&mut stream, b'S', b"");
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'Z');
    }
}

/// 同名の SCHEMA・型を持つ 2 テナントの接続が互いに影響せず、SCHEMA を持たないテナントの応答は、どのテナント
/// にも無い SCHEMA 名の応答と区別できない（`3F000`・名前以外は同一文言）。拒否後も接続が使える。
#[test]
fn wire_enum_type_in_schema_is_tenant_scoped() {
    let (core, _guard) = new_core();
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
            ("carol", "tenant-c", "tr0ub4dor"),
        ],
        &["alice", "bob", "carol"],
    );
    let addr = spawn_server(store, core);
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    let mut carol = authenticate_to_ready_for_query(addr, "carol", "tr0ub4dor");

    for stream in [&mut alice, &mut bob] {
        for (sql, tag) in [
            ("CREATE SCHEMA shared;", "CREATE SCHEMA"),
            ("CREATE TYPE shared.mood AS ENUM ('a');", "CREATE TYPE"),
        ] {
            send_simple_query(stream, sql);
            assert_eq!(read_command_complete(stream), tag);
            read_ready_for_query(stream);
        }
    }
    // carol は SCHEMA `shared` を持たない。どのテナントにも無い `ghost` と同じ SQLSTATE・文言（名前以外）。
    for (sql, ghost_sql) in [
        ("DROP TYPE shared.mood;", "DROP TYPE ghost.mood;"),
        (
            "CREATE TYPE shared.mood AS ENUM ('a');",
            "CREATE TYPE ghost.mood AS ENUM ('a');",
        ),
    ] {
        send_simple_query(&mut carol, sql);
        expect_error_response_with_sqlstate_and_message(
            &mut carol,
            "3F000",
            "schema does not exist: shared",
        );
        read_ready_for_query(&mut carol);
        send_simple_query(&mut carol, ghost_sql);
        expect_error_response_with_sqlstate_and_message(
            &mut carol,
            "3F000",
            "schema does not exist: ghost",
        );
        read_ready_for_query(&mut carol);
    }
    // bob の型の削除は alice の型に影響しない。
    send_simple_query(&mut bob, "DROP TYPE shared.mood;");
    assert_eq!(read_command_complete(&mut bob), "DROP TYPE");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut alice, "CREATE TYPE shared.mood AS ENUM ('z');");
    expect_error_response_with_sqlstate(&mut alice, "42710");
    read_ready_for_query(&mut alice);
    // 拒否の後も接続が使える。
    send_simple_query(&mut carol, "CREATE SCHEMA own;");
    assert_eq!(read_command_complete(&mut carol), "CREATE SCHEMA");
    read_ready_for_query(&mut carol);
}
/// 値 1 個（テキスト形式）を束縛する Bind メッセージ本体。
fn bind_body_one_text_param(portal: &str, statement: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&1i16.to_be_bytes()); // param count
    body.extend_from_slice(&(value.len() as i32).to_be_bytes());
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

/// SCHEMA 配下の型を使う列へ、拡張クエリで `$1` に ENUM ラベルを渡して INSERT できる（Issue #3289）。語彙外の
/// ラベルは `22P02` で、文言は公開名の型名を含み内部キーの `#` を含まない。拒否後も接続が使える。
#[test]
fn wire_schema_enum_column_accepts_bound_label_in_extended_query() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    for (sql, tag) in [
        ("CREATE SCHEMA s;", "CREATE SCHEMA"),
        ("CREATE TYPE s.mood AS ENUM ('a', 'b');", "CREATE TYPE"),
        (
            "CREATE TABLE s.t (id BIGINT PRIMARY KEY, m s.mood);",
            "CREATE TABLE",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("ins", "INSERT INTO s.t (id, m) VALUES (1, $1);", 0),
    );
    let (kind, body) = read_message(&mut stream);
    assert_eq!(
        kind,
        b'1',
        "ParseComplete: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_one_text_param("p1", "ins", "a"),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'C', "CommandComplete");
    assert_eq!(body, &b"INSERT 0 1\0"[..]);
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    // 語彙外のラベル。Bind か Execute のどちらかで `22P02`（公開名の型名つき）。
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_one_text_param("p2", "ins", "zzz"),
    );
    let (mut kind, mut body) = read_message(&mut stream);
    if kind == b'2' {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 0));
        (kind, body) = read_message(&mut stream);
    }
    assert_eq!(kind, b'E', "ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("22P02"), "{text}");
    assert!(text.contains("s.mood"), "{text}");
    assert!(!text.contains('#'), "{text}");
    send_length_prefixed_message(&mut stream, b'S', b"");
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'Z' {
            break;
        }
    }
    send_simple_query(&mut stream, "SELECT id FROM s.t;");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}
