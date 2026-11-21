//! USAGE の無い束縛ロールによる SCHEMA 修飾名の参照を、簡易・拡張クエリの段ごとに wire 越しで固定する
//! （Issue #3532、ポインタ: SQL-40・RLS-12）。
//!
//! 意図的な挙動の根拠は `docs/design/schema-tenant-namespace.md` の「拒否の根拠（Issue #3532）」節。
//! 同一テナント内の実在する SCHEMA は `42501`、不在は `3F000`。存否で応答が変わるのは PostgreSQL 17 と同じだが、
//! PostgreSQL 17 の不在は `42P01` で、`3F000` は同 ADR D4 の既存の差。他テナントの SCHEMA は不在と同じ `3F000` で、
//! 応答本体まで区別できない。拡張クエリは Parse が USAGE を見ないため、表の SELECT 権限が
//! あれば Parse は通り、Bind と文の Describe（'D' 'S'。カーソル文の DECLARE を含む）で `42501` になる。どの段でも
//! 行（DataRow）と列メタ（RowDescription）は返らない。
//!
//! 束縛の作り方は `wire_privilege_oracle.rs` と同じ（users ファイルのユーザー `alice` と同名の LOGIN ロールが
//! テナント内にあるため、接続が現在ロールへ束縛される）。engine の単体は `core/schema_acl_tests.rs`。
//! サーバーはポート 0・一意な一時 DB で独立させる。

use crate::common;
use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

const OBSERVER_TENANT: &str = "tenant-a";
const FOREIGN_TENANT: &str = "tenant-b";
/// 観測側テナントで SCHEMA `s` を作る所有者ロールの ID（作成順。`alice` が 1）。
const OWNER: u64 = 2;
/// 他テナントで SCHEMA `zz_secret_s` を作るロールの ID。
const FOREIGN_OWNER: u64 = 3;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn bound_session(id: u64, ddl: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if ddl {
        s.allow_ddl();
    }
    s
}

fn setup(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    if let Err(e) = core.execute_sql_in_session(&ctx(tenant), s, sql) {
        panic!("setup {tenant}: {sql}: {e}");
    }
}

/// サーバーを起動する。観測側テナントに `s.t`（alice に SELECT 付与済み）と `s.u`（付与なし）を置き、USAGE は
/// 付与しない。`foreign` なら他テナントに `zz_secret_s.t` を置く（無ければどこにも無い名前の双子になる）。
fn serve(
    label: &str,
    foreign: bool,
) -> (std::net::SocketAddr, temp_db::CleanupGuard, Arc<EngineCore>) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut op_a = SessionState::default();
    op_a.allow_ddl();
    setup(&core, OBSERVER_TENANT, &mut op_a, "CREATE ROLE alice LOGIN");
    setup(&core, OBSERVER_TENANT, &mut op_a, "CREATE ROLE owner_r");
    let mut op_b = SessionState::default();
    op_b.allow_ddl();
    setup(&core, FOREIGN_TENANT, &mut op_b, "CREATE ROLE zz_owner");
    let mut owner = bound_session(OWNER, true);
    for sql in [
        "CREATE SCHEMA s",
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
        "INSERT INTO s.t (id, name) VALUES (1, 'x')",
        "CREATE TABLE s.u (id BIGINT PRIMARY KEY)",
        "INSERT INTO s.u (id) VALUES (1)",
        "GRANT SELECT ON s.t TO alice",
    ] {
        setup(&core, OBSERVER_TENANT, &mut owner, sql);
    }
    if foreign {
        let mut fo = bound_session(FOREIGN_OWNER, true);
        for sql in [
            "CREATE SCHEMA zz_secret_s",
            "CREATE TABLE zz_secret_s.t (id BIGINT PRIMARY KEY, name TEXT)",
            "INSERT INTO zz_secret_s.t (id, name) VALUES (1, 'x')",
        ] {
            setup(&core, FOREIGN_TENANT, &mut fo, sql);
        }
    }
    let users_path = write_user_store_file(&[("alice", OBSERVER_TENANT, "pw-alice")]);
    let store =
        Arc::new(wire_server::auth::UserStore::load_from_file(&users_path).expect("user store"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = wire_server::limits::ConnectionLimiter::new(16);
    let served = Arc::clone(&core);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            served,
            limiter,
            std::time::Duration::from_secs(5),
        );
    });
    (addr, guard, core)
}

/// 応答 1 件（種別バイト・本体）。
type Reply = (u8, Vec<u8>);

/// ReadyForQuery（'Z'）までの応答をすべて読む。
fn read_until_ready(stream: &mut TcpStream) -> Vec<Reply> {
    let mut out = Vec::new();
    loop {
        let reply = read_message(stream);
        let done = reply.0 == b'Z';
        out.push(reply);
        if done {
            return out;
        }
    }
}

/// 1 つの text パラメータを持つ Bind 本体（`common::bind_body` はパラメータ無しの固定形のため）。
fn bind_body_one_text_param(value: &str) -> Vec<u8> {
    let mut body = vec![0u8, 0u8]; // 無名 portal・無名文
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count（text）
    body.extend_from_slice(&1i16.to_be_bytes()); // param count
    let len = i32::try_from(value.len()).expect("short value");
    body.extend_from_slice(&len.to_be_bytes());
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

/// 無名の文・portal で Parse → Bind → Execute → Sync を 1 度に送る。
fn parse_bind_execute(stream: &mut TcpStream, sql: &str, param: Option<&str>) -> Vec<Reply> {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    let bind = match param {
        Some(v) => bind_body_one_text_param(v),
        None => bind_body("", ""),
    };
    send_length_prefixed_message(stream, b'B', &bind);
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

/// 無名の文で Parse → Describe(S) → Sync を送る。
fn parse_describe_statement(stream: &mut TcpStream, sql: &str) -> Vec<Reply> {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(stream, b'D', b"S\0");
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

/// 無名の文・portal で Parse → Bind → Describe(P) → Execute → Sync を送る。
fn parse_bind_describe_portal(stream: &mut TcpStream, sql: &str) -> Vec<Reply> {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(stream, b'B', &bind_body("", ""));
    send_length_prefixed_message(stream, b'D', b"P\0");
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

fn simple(stream: &mut TcpStream, sql: &str) -> Vec<Reply> {
    send_simple_query(stream, sql);
    read_until_ready(stream)
}

fn kinds(replies: &[Reply]) -> Vec<u8> {
    replies.iter().map(|(k, _)| *k).collect()
}

/// ErrorResponse 本体から指定のフィールド（`C`・`M` など）の値を取り出す。
fn field(body: &[u8], kind: u8) -> String {
    let mut rest = body;
    while let Some((&ty, tail)) = rest.split_first() {
        if ty == 0 {
            break;
        }
        let end = tail.iter().position(|&b| b == 0).expect("nul-terminated");
        if ty == kind {
            return String::from_utf8_lossy(&tail[..end]).into_owned();
        }
        rest = &tail[end + 1..];
    }
    panic!("field {} missing", kind as char);
}

/// 応答列が `expected` の種別列と一致し、その中の ErrorResponse が `sqlstate` であることを確認して、
/// ErrorResponse の文言を返す。行（'D'）・列メタ（'T'）が無いことは種別列の一致で固定される。
fn expect_error_at(label: &str, replies: &[Reply], expected: &[u8], sqlstate: &str) -> String {
    assert_eq!(
        kinds(replies),
        expected.to_vec(),
        "{label}: message sequence"
    );
    let (_, body) = replies
        .iter()
        .find(|(k, _)| *k == b'E')
        .unwrap_or_else(|| panic!("{label}: ErrorResponse expected"));
    assert_eq!(field(body, b'C'), sqlstate, "{label}: SQLSTATE");
    let message = field(body, b'M');
    assert!(
        !message.contains('#'),
        "{label}: internal key in {message:?}"
    );
    assert!(
        !message.contains("tenant-"),
        "{label}: tenant id in {message:?}"
    );
    message
}

/// 同一テナント内: 実在する SCHEMA は `42501`、不在は `3F000`。拡張クエリは SELECT 権限があれば Parse を通り、
/// Bind・文の Describe で `42501`（Bind と同じ文言）。SELECT 権限が無ければ Parse で `42501`。portal の Describe は
/// Bind で止まる。どの段でも行・列メタは返らず、GRANT USAGE の後は同じ手順で行と列メタが返る（陽性対照）。
// intentional: docs/design/schema-tenant-namespace.md
// 根拠の節: 拒否の根拠（Issue #3532）
#[test]
fn wire_schema_reference_without_usage_is_rejected_on_every_step() {
    let (addr, _guard, core) = serve("wire-schema-usage-steps", false);
    let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let st = "SELECT id, name FROM s.t";
    let st_param = "SELECT id, name FROM s.t WHERE id = $1";
    let su = "SELECT id FROM s.u";
    let missing = "SELECT id FROM nope.t";

    // 簡易クエリ。
    let simple_denied = expect_error_at(st, &simple(&mut a, st), b"EZ", "42501");
    expect_error_at(su, &simple(&mut a, su), b"EZ", "42501");
    expect_error_at(missing, &simple(&mut a, missing), b"EZ", "3F000");

    // 拡張クエリ: SELECT 権限あり・USAGE 無しは Parse を通り（'1'）、Bind で `42501`。
    let bind_denied = expect_error_at(st, &parse_bind_execute(&mut a, st, None), b"1EZ", "42501");
    assert_eq!(bind_denied, simple_denied, "bind and simple query wording");
    expect_error_at(
        st_param,
        &parse_bind_execute(&mut a, st_param, Some("1")),
        b"1EZ",
        "42501",
    );
    // 文の Describe も Bind と同じ `42501`・同じ文言（ParameterDescription・RowDescription を返さない）。
    for sql in [st, st_param] {
        let described =
            expect_error_at(sql, &parse_describe_statement(&mut a, sql), b"1EZ", "42501");
        assert_eq!(described, bind_denied, "{sql}: describe and bind wording");
    }
    // カーソル文（DECLARE）は解決結果を使い回す Describe の分岐を通る。そこでも Bind と同じ `42501`
    // （以前は NoData を返し、Bind だけが拒否していた）。
    let declare = "DECLARE c CURSOR FOR SELECT id FROM s.t";
    let declare_denied = expect_error_at(
        declare,
        &parse_describe_statement(&mut a, declare),
        b"1EZ",
        "42501",
    );
    assert_eq!(declare_denied, bind_denied, "declare describe wording");
    expect_error_at(
        declare,
        &parse_bind_execute(&mut a, declare, None),
        b"1EZ",
        "42501",
    );
    // portal の Describe は Bind で止まる（portal が作られない）。
    expect_error_at(st, &parse_bind_describe_portal(&mut a, st), b"1EZ", "42501");
    // SELECT 権限も無い表は Parse で `42501`。不在の SCHEMA は Parse で `3F000`。
    for (sql, state) in [(su, "42501"), (missing, "3F000")] {
        expect_error_at(sql, &parse_bind_execute(&mut a, sql, None), b"EZ", state);
        expect_error_at(sql, &parse_describe_statement(&mut a, sql), b"EZ", state);
    }

    // 陽性対照: USAGE を付与すると同じ接続・同じ手順で列メタと行が返る。
    let mut owner = bound_session(OWNER, true);
    setup(
        &core,
        OBSERVER_TENANT,
        &mut owner,
        "GRANT USAGE ON SCHEMA s TO alice",
    );
    assert_eq!(kinds(&simple(&mut a, st)), b"TDCZ".to_vec(), "simple");
    assert_eq!(
        kinds(&parse_bind_execute(&mut a, st, None)),
        b"12DCZ".to_vec(),
        "extended"
    );
    assert_eq!(
        kinds(&parse_bind_execute(&mut a, st_param, Some("1"))),
        b"12DCZ".to_vec(),
        "extended with a parameter"
    );
    for sql in [st, st_param] {
        assert_eq!(
            kinds(&parse_describe_statement(&mut a, sql)),
            b"1tTZ".to_vec(),
            "{sql}: describe statement"
        );
    }
    assert_eq!(
        kinds(&parse_describe_statement(&mut a, declare)),
        b"1tnZ".to_vec(),
        "declare: describe statement"
    );
    assert_eq!(
        kinds(&parse_bind_describe_portal(&mut a, st)),
        b"12TDCZ".to_vec(),
        "describe portal"
    );
    // USAGE があっても表の SELECT 権限が無ければ従来どおり Parse で `42501`。
    expect_error_at(su, &parse_bind_execute(&mut a, su, None), b"EZ", "42501");
}

/// 他テナントの SCHEMA は、簡易クエリ・拡張クエリ（Parse・Describe）のどの段でも不在と同じ `3F000` で、
/// 応答（種別列と ErrorResponse 本体）がどこにも無い名前の双子サーバーとバイト一致する。
// intentional: docs/design/schema-tenant-namespace.md
// 根拠の節: 拒否の根拠（Issue #3532）
#[test]
fn wire_foreign_schema_is_indistinguishable_from_missing_on_every_step() {
    let sql = "SELECT id, name FROM zz_secret_s.t";
    let observe = |foreign: bool| -> Vec<Vec<Reply>> {
        let (addr, _guard, _core) = serve("wire-schema-usage-foreign", foreign);
        let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        vec![
            simple(&mut a, sql),
            parse_bind_execute(&mut a, sql, None),
            parse_describe_statement(&mut a, sql),
            parse_bind_describe_portal(&mut a, sql),
        ]
    };
    let theirs = observe(true);
    let nowhere = observe(false);
    assert_eq!(theirs, nowhere, "foreign schema must look missing");
    for replies in &theirs {
        expect_error_at(sql, replies, b"EZ", "3F000");
    }
}

/// 名前付きの文を Parse だけして Sync する（USAGE が有効な時点の Parse を、後の Describe・Bind と分けるため）。
fn parse_named(stream: &mut TcpStream, name: &str, sql: &str) -> Vec<Reply> {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

/// 名前付きの文の Describe(S) だけを送って Sync する。
fn describe_named(stream: &mut TcpStream, name: &str) -> Vec<Reply> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    send_length_prefixed_message(stream, b'D', &body);
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

/// 名前付きの文を無名 portal へ Bind → Execute → Sync する（`param` があれば text で 1 つ渡す）。
fn bind_execute_named(stream: &mut TcpStream, name: &str, param: Option<&str>) -> Vec<Reply> {
    let mut body = vec![0u8]; // 無名 portal
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count（text）
    match param {
        Some(v) => {
            body.extend_from_slice(&1i16.to_be_bytes());
            let len = i32::try_from(v.len()).expect("short value");
            body.extend_from_slice(&len.to_be_bytes());
            body.extend_from_slice(v.as_bytes());
        }
        None => body.extend_from_slice(&0i16.to_be_bytes()),
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    send_length_prefixed_message(stream, b'B', &body);
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(stream, b'S', &[]);
    read_until_ready(stream)
}

/// USAGE がある時点で Parse した名前付きの文は、その後の `REVOKE USAGE` を Describe(S)・Bind の時点で反映する
/// （Parse 時の USAGE を使い回さない）。REVOKE 前の Describe(S) は成功し（陰性対照）、REVOKE 後は Bind と同じ
/// `42501`・同じ文言で、ParameterDescription・RowDescription を返さない。
// intentional: docs/design/schema-tenant-namespace.md
// 根拠の節: 拒否の根拠（Issue #3532）
#[test]
fn wire_revoke_usage_after_parse_is_reflected_by_describe_and_bind() {
    let (addr, _guard, core) = serve("wire-schema-usage-revoke", false);
    let mut owner = bound_session(OWNER, true);
    setup(
        &core,
        OBSERVER_TENANT,
        &mut owner,
        "GRANT USAGE ON SCHEMA s TO alice",
    );
    let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let cases = [
        ("stmt_plain", "SELECT id, name FROM s.t", None),
        (
            "stmt_param",
            "SELECT id, name FROM s.t WHERE id = $1",
            Some("1"),
        ),
    ];
    for (name, sql, _) in cases {
        assert_eq!(
            kinds(&parse_named(&mut a, name, sql)),
            b"1Z".to_vec(),
            "{name}: parse"
        );
        // 陰性対照: REVOKE 前の Describe(S) は成功する。
        assert_eq!(
            kinds(&describe_named(&mut a, name)),
            b"tTZ".to_vec(),
            "{name}: describe before revoke"
        );
    }

    setup(
        &core,
        OBSERVER_TENANT,
        &mut owner,
        "REVOKE USAGE ON SCHEMA s FROM alice",
    );

    for (name, _, param) in cases {
        let described = expect_error_at(name, &describe_named(&mut a, name), b"EZ", "42501");
        let bound = expect_error_at(
            name,
            &bind_execute_named(&mut a, name, param),
            b"EZ",
            "42501",
        );
        assert_eq!(described, bound, "{name}: describe and bind wording");
    }
}
