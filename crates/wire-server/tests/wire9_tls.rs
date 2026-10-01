//! TLS 1.3 越しの 3 クライアント接続テストと負のテスト（Issue #969・
//! WIRE-9・WIRE-1、親 #941・TASK-228。ポインタ: `docs/spec/05-tasks.md`
//! TASK-73・TASK-228、`docs/spec/04-behavior/wire-protocol.md` WIRE-1・
//! WIRE-9）。
//!
//! 責務境界:
//! - 層 A（常時 `make ci`）: 内製の TLS 1.3 最小クライアント
//!   （`tests/common/tls_client.rs`）で、TLS 越しの C1〜C4（TASK-73／WIRE-1）・
//!   RLS 3 テナント分離・`--tls-mode require` の平文拒否を回帰保護する。
//!   加えて、TLS 1.2 しか提示しない ClientHello・改ざんレコード・
//!   長さ超過レコード・切り詰めハンドシェイクの 7 種が fail-closed で
//!   切断され、かつ同一サーバープロセスが後続の正規接続を完走できる
//!   （1 接続の失敗がプロセス全体を落とさない）ことを固定する
//! - 層 B（`#[ignore]`。`make e2e-three-client-tls` から明示実行）: 無改造の
//!   実クライアント 3 種（psql／psycopg／node pg）が `--tls-mode require`
//!   越しに C1〜C4・RLS 分離を完走すること、psql の TLS 1.2 上限指定が
//!   拒否されることを検証する
//!
//! `tests/three_client_e2e.rs`（層 B の psql／psycopg／pg 実行ヘルパー）・
//! `tests/wire_tls_cli.rs`（TLS CLI 起動・pg wire バイト列ヘルパー）とは
//! 意図的にヘルパーを重複させる（`tls_client.rs` 冒頭コメントと同じ方針。
//! 既存ファイルの大規模な相互依存を崩さずに新規結合テストを追加するため、
//! 本 Issue の範囲では共有モジュールへの一本化は行わない。重複解消は
//! 対象外＝別 Issue 候補）。
//!
//! TLS 証明書・鍵は RFC 8032 §7.1 TEST 1 の公開テストベクタ
//! （`tls_client::RFC8032_TEST1_SEED`）から実行時に一時ディレクトリへ生成し、
//! リポジトリには置かない（security.md P0）。

#[path = "common/mod.rs"]
mod common;
#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;
#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use wire_server::tls::record::{self, ContentType, RecordKind};

// --- 共通フィクスチャ -----------------------------------------------------

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（証明書・鍵・ユーザーストアの
/// 置き場）を確保し、`Drop` で削除するガード（`wire_tls_cli.rs::
/// TempFixtureDir` と同型。共有モジュール化は対象外＝別 Issue 候補）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-wire9-tls-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    fn path_str(&self, name: &str) -> String {
        self.dir
            .join(name)
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
        // パスとエラーだけを stderr へ出す（Issue #1303）。
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.dir.display()
                );
            }
        }
    }
}

/// [`tls_client::RFC8032_TEST1_SEED`]／`RFC8032_TEST1_PUBLIC_KEY`（IETF
/// 公開テストベクタ）から証明書・鍵 PEM を実行時に書き出す。秘密鍵は
/// リポジトリへ置かず、`TempFixtureDir` の `Drop` で削除される。
fn write_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    let cert_der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let cert_path = fixture.dir.join("cert.pem");
    tls_client::write_pem_file(&cert_path, &tls_client::pem_wrap("CERTIFICATE", &cert_der));
    let key_path = fixture.dir.join("key.pem");
    tls_client::write_pem_file(&key_path, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert_path, key_path)
}

/// alice/bob/carol（tenant-a/b/c）の argon2 ユーザーストアを書く。
fn write_users_file(path: &std::path::Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let mut content = String::new();
    for (user, tenant, pw) in [
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ] {
        let phc = argon2id::encode_phc(pw.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{user}:{tenant}:{phc}\n"));
    }
    std::fs::write(path, content).expect("write users file");
}

/// `docs` テーブルへ 3 テナント分の Public 行のみを投入する
/// （`three_client_e2e.rs::seed_three_tenant_db` と同一コーパス。C1〜C4 の
/// オラクルを共有できるようにする）。
fn seed_three_tenant_db() -> (std::path::PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire9-tls-c1-c4");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let tenants: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in tenants {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (path, guard)
}

/// 各テナントに Public 行（id 1/2/3）＋ Private 行（id 11/12/13）を投入する
/// （`wire1_simple_query.rs::
/// wire1_three_tenant_visibility_public_shared_own_private_visible` と同じ
/// 方針。TLS 越しでも他テナントの Private 行が 0 件のまま漏れないことを
/// 検証するための seed）。
fn seed_rls_private_db() -> (std::path::PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire9-tls-rls");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let tenants: [(&str, u64, u64, [f32; 2]); 3] = [
        ("tenant-a", 1, 11, [1.0, 0.0]),
        ("tenant-b", 2, 12, [0.0, 1.0]),
        ("tenant-c", 3, 13, [-1.0, 0.0]),
    ];
    for (tenant, public_id, private_id, dir) in tenants {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let public_op_id = format!("test-op-public-{tenant}");
        let private_op_id = format!("test-op-private-{tenant}");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            public_id,
            Visibility::Public,
            &[Value::Vector(dir.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&public_op_id)
                .expect("valid operation_id"),
        )
        .expect("insert public row");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            private_id,
            Visibility::Private,
            &[Value::Vector(dir.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&private_op_id)
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    (path, guard)
}

/// `--tls-cert`／`--tls-key`（`--tls-mode` 省略＝既定 `require`）付きで
/// `wire-server` バイナリを起動する（`wire_tls_cli.rs` と同型）。listen
/// port は `SpawnedServer` 自体が公開しないため、ここで確定させて併せて返す。
fn spawn_tls_server_with_port(
    users_path: &str,
    db_path: &str,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> (common::SpawnedServer, u16) {
    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        users_path,
        "--db",
        db_path,
        "--bind",
        "127.0.0.1:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");
    // addr は "127.0.0.1:<port>" 形。呼び出し元は port だけ知っていればよい。
    let port: u16 = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("valid port suffix");
    (server, port)
}

// --- pg wire バイトレベルヘルパー（`wire_tls_cli.rs` と同型。意図的な重複）---

const SSL_REQUEST_CODE: i32 = 80_877_103;

fn write_ssl_request(stream: &mut impl Write) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send SSLRequest");
}

fn write_startup_message(stream: &mut impl Write, username: &str, database: &str) {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(username.as_bytes());
    params.push(0);
    params.extend_from_slice(b"database\0");
    params.extend_from_slice(database.as_bytes());
    params.push(0);
    params.push(0);
    let total_len = (4 + 4 + params.len()) as i32;
    let mut startup = Vec::new();
    startup.extend_from_slice(&total_len.to_be_bytes());
    startup.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    startup.extend_from_slice(&params);
    stream.write_all(&startup).expect("send StartupMessage");
}

fn write_password_message(stream: &mut impl Write, password: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(password.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send PasswordMessage");
}

fn write_simple_query(stream: &mut impl Write, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'Q');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send simple query");
}

fn read_exact_n(stream: &mut impl Read, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).expect("read exact");
    buf
}

fn read_typed_message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let type_byte = read_exact_n(stream, 1)[0];
    let len_bytes = read_exact_n(stream, 4);
    let len = i32::from_be_bytes(len_bytes.try_into().expect("4 bytes")) as usize;
    let body_len = len.checked_sub(4).expect("length includes itself");
    let body = read_exact_n(stream, body_len);
    (type_byte, body)
}

/// AuthenticationOk 以降を読み飛ばし、単一の結果セット（RowDescription・
/// DataRow*・CommandComplete）を `id` 列 1 個の文字列集合として集めつつ
/// ReadyForQuery に到達するまで進める。C1〜C4 は単一列 `id` の SELECT の
/// ため、DataRow の先頭列だけを取り出せば `three_client_e2e.rs` のオラクル
/// と同じ形の結果になる。
fn drain_to_ready_for_query(stream: &mut impl Read) {
    loop {
        let (type_byte, _body) = read_typed_message(stream);
        if type_byte == b'Z' {
            return;
        }
    }
}

/// 簡易クエリを実行し、DataRow の各列（テキスト形式）を `|` 区切りで結合
/// した文字列の集合として返す（`three_client_e2e.rs::run_psql` 等と同じ
/// 区切り規約。単一列なら値そのもの）。
fn run_simple_query_collect_rows(channel: &mut impl ReadWrite, sql: &str) -> Vec<String> {
    write_simple_query(channel, sql);
    let mut rows = Vec::new();
    loop {
        let (type_byte, body) = read_typed_message(channel);
        match type_byte {
            b'T' => {} // RowDescription
            b'D' => rows.push(parse_data_row(&body)),
            b'C' | b'I' => {}
            // DataRow を返した後でもクエリが ErrorResponse で終わる経路
            // （例: LIMIT 節評価中のエラー等）を「成功」として握りつぶさない。
            // ReadyForQuery まで読み進めて接続を安定状態に戻したうえで
            // sqlstate 付きで panic させ、握りつぶしを検出可能にする
            // （codex #969 PR #1090 レビュー指摘）。
            b'E' => {
                let sqlstate = extract_sqlstate(&body);
                drain_to_ready_for_query(channel);
                panic!(
                    "unexpected ErrorResponse (sqlstate={sqlstate}) while running simple query: {sql}"
                );
            }
            b'Z' => return rows,
            other => panic!("unexpected message type {other:?} while draining result set"),
        }
    }
}

/// トレイトエイリアス代わり（`Read + Write` の組をヘルパー間で使い回す）。
trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// DataRow（'D'）本体からテキスト形式の各列値を取り出し `|` 区切りで結合する。
fn parse_data_row(body: &[u8]) -> String {
    let field_count = i16::from_be_bytes(body[..2].try_into().expect("2 bytes")) as usize;
    let mut idx = 2usize;
    let mut values = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let len = i32::from_be_bytes(body[idx..idx + 4].try_into().expect("4 bytes"));
        idx += 4;
        if len < 0 {
            values.push("NULL".to_string());
            continue;
        }
        let len = len as usize;
        let value = String::from_utf8(body[idx..idx + len].to_vec()).expect("utf-8 value");
        idx += len;
        values.push(value);
    }
    values.join("|")
}

fn cleartext_auth_over(channel: &mut impl ReadWrite, username: &str, password: &str) {
    write_startup_message(channel, username, "irrelevant-db-name");
    let (type_byte, body) = read_typed_message(channel);
    assert_eq!(type_byte, b'R', "expected Authentication* message");
    let auth_code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    write_password_message(channel, password);
    let (type_byte, body) = read_typed_message(channel);
    assert_eq!(type_byte, b'R', "expected AuthenticationOk");
    let code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(code, 0, "AuthenticationOk code");

    drain_to_ready_for_query(channel);
}

/// TLS ハンドシェイクを完走し、cleartext 認証まで済ませた
/// [`tls_client::TlsTestChannel`] を返す。
fn connect_and_authenticate(
    port: u16,
    username: &str,
    password: &str,
) -> tls_client::TlsTestChannel {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);
    cleartext_auth_over(&mut channel, username, password);
    channel
}

// --- Step 3: 層 A の正常系 ------------------------------------------------

/// R1（受け入れ条件）: TLS 越しに C1〜C4（TASK-73／WIRE-1）が 3 テナント
/// いずれのユーザーでも独立オラクルと一致すること。
#[test]
fn tls_c1_through_c4_match_oracle_for_three_tenants() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let fixture = TempFixtureDir::new("c1c4");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));

    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    const C1_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C2_SQL: &str =
        "SELECT id, lang FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C3_SQL: &str =
        "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C4_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3";

    let expected_c1 = vec!["1".to_string(), "2".to_string(), "3".to_string()];
    let expected_c2 = vec!["1|ja".to_string(), "3|ja".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        let mut channel = connect_and_authenticate(port, user, pw);
        for (label, sql, expected) in [
            ("C1", C1_SQL, &expected_c1),
            ("C2", C2_SQL, &expected_c2),
            ("C3", C3_SQL, &expected_c1),
            ("C4", C4_SQL, &expected_c1),
        ] {
            let rows = run_simple_query_collect_rows(&mut channel, sql);
            assert_eq!(&rows, expected, "unexpected {label} result for user {user}");
        }
    }

    drop(server);
}

/// R3（受け入れ条件）: TLS 越しでも RLS 3 テナント分離が成立し、他テナント
/// の Private 行の混入が 0 件であること。
#[test]
fn tls_rls_three_tenant_isolation_has_zero_cross_tenant_rows() {
    let (db_path, _db_guard) = seed_rls_private_db();
    let fixture = TempFixtureDir::new("rls");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));

    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    // Public 行（id 1/2/3）は全テナント共有で見える。Private 行（id
    // 11/12/13）は自テナントのみ見える契約
    // （`wire1_three_tenant_visibility_public_shared_own_private_visible` と
    // 同じオラクル）。「他テナントの Private 行が 0 件」というのが本テストの
    // 非自明な検証対象であり、Public 3 行の共有可視性はそれ自体が仕様。
    let cases: [(&str, &str, &[&str]); 3] = [
        ("alice", "pw-alice", &["1", "2", "3", "11"]),
        ("bob", "pw-bob", &["1", "2", "3", "12"]),
        ("carol", "pw-carol", &["1", "2", "3", "13"]),
    ];
    let all_private_ids = ["11", "12", "13"];
    for (user, pw, expected_ids) in cases {
        let mut channel = connect_and_authenticate(port, user, pw);
        let rows = run_simple_query_collect_rows(
            &mut channel,
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20",
        );
        let mut got: Vec<&str> = rows.iter().map(String::as_str).collect();
        got.sort_unstable();
        let mut want: Vec<&str> = expected_ids.to_vec();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "user {user} must see Public rows + its own Private id over TLS"
        );
        let own_private = expected_ids
            .iter()
            .find(|id| all_private_ids.contains(id))
            .expect("case defines exactly one own private id");
        let cross_tenant_private_leaked: Vec<&&str> = all_private_ids
            .iter()
            .filter(|id| *id != own_private && got.contains(id))
            .collect();
        assert!(
            cross_tenant_private_leaked.is_empty(),
            "user {user} must see zero cross-tenant Private rows, leaked={cross_tenant_private_leaked:?}"
        );
    }

    drop(server);
}

/// 非 vacuous 性の担保: 同じ起動構成（`--tls-cert`／`--tls-key`、`--tls-mode`
/// 省略＝既定 `require`）で、平文 StartupMessage が `08P01` で拒否される
/// ことを確認する。これにより上記 2 テストの往復が実際に TLS 上で成立して
/// いたことを示す（`wire_tls_cli.rs` R3 と同じ最小契約。共有モジュール化は
/// 対象外）。
#[test]
fn tls_require_rejects_plaintext_startup() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let fixture = TempFixtureDir::new("plaintext-reject");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));

    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_startup_message(&mut stream, "alice", "irrelevant-db-name");
    let (type_byte, body) = read_typed_message(&mut stream);
    assert_eq!(type_byte, b'E', "expected ErrorResponse");
    let sqlstate = extract_sqlstate(&body);
    assert_eq!(
        sqlstate, "08P01",
        "plaintext startup under --tls-mode require (default) must be rejected with 08P01"
    );

    drop(server);
}

fn extract_sqlstate(body: &[u8]) -> String {
    let mut idx = 0usize;
    while idx < body.len() {
        let tag = body[idx];
        if tag == 0 {
            break;
        }
        let value_start = idx + 1;
        let nul = body[value_start..]
            .iter()
            .position(|&b| b == 0)
            .expect("null-terminated field value");
        let value_end = value_start + nul;
        let value = String::from_utf8(body[value_start..value_end].to_vec()).expect("utf-8 value");
        if tag == b'C' {
            return value;
        }
        idx = value_end + 1;
    }
    panic!("ErrorResponse did not contain a C (sqlstate) field");
}

// --- Step 4: 層 A の負のテスト ---------------------------------------------

/// 負のテスト共通の後続確認 (iii): 同じサーバープロセスが次の正規 TLS
/// 接続（認証＋簡易クエリ）を完走できること（1 接続の失敗がプロセスを
/// 落とさないことの確認。DoS 耐性。A04）。
fn assert_server_still_accepts_normal_connections(port: u16) {
    let mut channel = connect_and_authenticate(port, "alice", "pw-alice");
    // 負のテストの seed（[`seed_three_tenant_db_labeled`]）は tenant-a の
    // id=1 のみを持つ `docs` を使う。C1 相当の SELECT で完走を確認する
    // （`SELECT 1` は本エンジンの SQL 表層が受理する形と限らないため使わない）。
    let rows = run_simple_query_collect_rows(
        &mut channel,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
    );
    assert_eq!(rows, vec!["1".to_string()]);
}

/// fatal alert 送出後、接続が実際に閉じており追加のバイトが一切来ないこと
/// を確認する。読み取りタイムアウト（`TimedOut`／`WouldBlock`）を空読み
/// （`n=0`）と区別せず握りつぶすと、サーバーが接続を開いたまま応答しない
/// fail-open の退行が「無応答で正常」と誤認され pass してしまうため、
/// タイムアウトは明示的に `panic!` させる。
fn assert_connection_closed_with_no_trailing_bytes(stream: &mut impl Read) {
    let mut trailing = [0u8; 1];
    match stream.read(&mut trailing) {
        Ok(0) => {}
        Ok(n) => panic!("connection must be closed after the alert; got {n} trailing byte(s)"),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!(
            "connection must be closed (EOF) after the alert, not hang or error otherwise: {e:?}"
        ),
    }
}

/// 平文の fatal alert レコード（ClientHello 段階。まだ暗号化されていない）
/// を読み、`(level, description)` を返す。
fn read_plaintext_alert(stream: &mut impl Read) -> (u8, u8) {
    let rec = record::read_record(stream, RecordKind::Plaintext)
        .expect("read alert record")
        .expect("alert record present");
    assert_eq!(
        rec.content_type,
        ContentType::Alert,
        "expected Alert record"
    );
    assert_eq!(rec.fragment.len(), 2, "alert body must be exactly 2 bytes");
    (rec.fragment[0], rec.fragment[1])
}

fn new_test_db_for_negative_case(label: &str) -> (std::path::PathBuf, temp_db::CleanupGuard) {
    seed_three_tenant_db_labeled(label)
}

fn seed_three_tenant_db_labeled(label: &str) -> (std::path::PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0])],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    (path, guard)
}

/// 変種 1: `supported_versions` 拡張自体を持たない ClientHello は
/// `protocol_version`(70) の平文 fatal alert を受けたのち切断される。
#[test]
fn tls12_only_client_hello_without_supported_versions_gets_protocol_version_alert() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-1");
    let fixture = TempFixtureDir::new("neg1");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(&resp, b"S");
    let record_bytes = tls_client::tls12_only_client_hello_record_bytes(false);
    stream.write_all(&record_bytes).expect("write ClientHello");

    let (level, description) = read_plaintext_alert(&mut stream);
    assert_eq!(level, 2, "expected fatal alert level");
    assert_eq!(description, 70, "expected protocol_version alert");

    assert_connection_closed_with_no_trailing_bytes(&mut stream);

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// 変種 2: `supported_versions` に TLS 1.2（0x0303）のみを提示する
/// ClientHello も同じく `protocol_version`(70) で拒否される。
#[test]
fn supported_versions_with_only_tls12_gets_protocol_version_alert() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-2");
    let fixture = TempFixtureDir::new("neg2");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(&resp, b"S");
    let record_bytes = tls_client::tls12_only_client_hello_record_bytes(true);
    stream.write_all(&record_bytes).expect("write ClientHello");

    let (level, description) = read_plaintext_alert(&mut stream);
    assert_eq!(level, 2, "expected fatal alert level");
    assert_eq!(description, 70, "expected protocol_version alert");

    assert_connection_closed_with_no_trailing_bytes(&mut stream);

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// application data レコードのタグ／暗号文を 1 バイト改ざんすると
/// `bad_record_mac`(20) の暗号化 fatal alert を受けたのち切断され、
/// `'R'`（Authentication*）が一切届かないこと。
#[test]
fn tampered_application_data_record_gets_bad_record_mac_and_closes() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-3");
    let fixture = TempFixtureDir::new("neg3");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");
    let mut client = tls_client::drive_client_handshake_over_socket(&mut socket);

    // StartupMessage を平文として組み、seal してから 1 バイト改ざんする
    // （末尾バイトは AEAD タグの一部で、反転すると必ず検証に失敗する）。
    let mut startup_payload = Vec::new();
    {
        let mut params = Vec::new();
        params.extend_from_slice(b"user\0alice\0database\0irrelevant-db-name\0\0");
        let total_len = (4 + 4 + params.len()) as i32;
        startup_payload.extend_from_slice(&total_len.to_be_bytes());
        startup_payload.extend_from_slice(&0x0003_0000i32.to_be_bytes());
        startup_payload.extend_from_slice(&params);
    }
    let records = client
        .sealer
        .seal_fragmented(ContentType::ApplicationData, &startup_payload)
        .expect("valid seal");
    let mut buf = Vec::new();
    for record in &records {
        record
            .serialize_into(&mut buf, RecordKind::Ciphertext)
            .expect("serialize application data record");
    }
    let last = buf.len() - 1;
    buf[last] ^= 0xFF;
    socket.write_all(&buf).expect("write tampered record");

    let rec = record::read_record(&mut socket, RecordKind::Ciphertext)
        .expect("read response record")
        .expect("response record present");
    let opened = client
        .opener
        .open(&rec)
        .expect("server must respond with a decryptable fatal alert");
    assert_eq!(opened.content_type, ContentType::Alert, "expected Alert");
    assert_eq!(opened.content.len(), 2, "alert body must be 2 bytes");
    assert_eq!(opened.content[0], 2, "expected fatal alert level");
    assert_eq!(opened.content[1], 20, "expected bad_record_mac alert");

    assert_connection_closed_with_no_trailing_bytes(&mut socket);

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// application data フェーズで [`record::MAX_CIPHERTEXT_LEN`] を超える
/// 長さを宣言するレコードヘッダだけを送ると、本体を待たずに
/// `record_overflow`(22) で拒否されること。
#[test]
fn oversized_record_length_is_rejected_without_processing() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-4");
    let fixture = TempFixtureDir::new("neg4");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);

    let over_len = (record::MAX_CIPHERTEXT_LEN + 1) as u16;
    let mut header = Vec::with_capacity(5);
    header.push(23u8); // outer content type: application_data
    header.extend_from_slice(&0x0303u16.to_be_bytes());
    header.extend_from_slice(&over_len.to_be_bytes());
    socket
        .write_all(&header)
        .expect("write oversized record header (body intentionally withheld)");

    let rec = record::read_record(&mut socket, RecordKind::Ciphertext)
        .expect("read response record")
        .expect("response record present");
    let mut client = client;
    let opened = client
        .opener
        .open(&rec)
        .expect("server must respond with a decryptable fatal alert");
    assert_eq!(opened.content_type, ContentType::Alert);
    assert_eq!(opened.content[0], 2, "expected fatal alert level");
    assert_eq!(opened.content[1], 22, "expected record_overflow alert");

    assert_connection_closed_with_no_trailing_bytes(&mut socket);

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// ClientHello レコードの宣言長より短い本体だけを送って半クローズすると、
/// ServerHello を受け取ることなく接続が閉じること。
#[test]
fn truncated_client_hello_record_then_eof_closes_without_server_hello() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-5");
    let fixture = TempFixtureDir::new("neg5");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let full = tls_client::build_client_hello_record_bytes([0x11u8; 32]);
    assert!(
        full.len() > 10,
        "ClientHello record must be non-trivially long"
    );
    let truncated = &full[..full.len() - 5];
    socket
        .write_all(truncated)
        .expect("write truncated ClientHello");
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close write side");

    // ServerHello（暗号化されていない最初のレコードは Handshake 型）が
    // 届かないことを、読み取り結果が「有効な ServerHello レコード」に
    // ならないことで確認する。alert が来る場合と無応答 EOF になる場合の
    // 両方を許容しつつ、ServerHello（Handshake かつ非 Alert）だけは
    // 明確に拒否する。
    // 実測: `record::read_record` は宣言長に届く前の EOF を
    // `RecordError::Truncated` として検出し、`alert_description()` が
    // `None` を返すため（`Truncated`／`Io` は無応答）、サーバーは alert を
    // 送らずそのまま切断する。`read_to_end` の `Err`（特に `TimedOut`）を
    // 黙って握りつぶすと fail-open（応答なしで接続を開いたまま保持する
    // 退行）が「無応答」と誤認され pass してしまうため、明示的に区別する。
    let mut buf = Vec::new();
    match socket.read_to_end(&mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!(
            "server must close the connection cleanly (EOF) for a truncated ClientHello, \
             not hang or error otherwise: {e:?} (partial={buf:?})"
        ),
    }
    assert!(
        buf.is_empty(),
        "server must send no bytes (no alert, no ServerHello) for a truncated ClientHello; got {buf:?}"
    );

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// レコードヘッダ（5 バイト）自体を切り詰めて（3 バイトのみ）半クローズ
/// すると、同様に応答なしで接続が閉じること。
#[test]
fn truncated_record_header_then_eof_closes() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-6");
    let fixture = TempFixtureDir::new("neg6");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    // レコードヘッダ 5 バイトのうち先頭 3 バイトのみ（content_type +
    // legacy_version）を送る。
    let header_prefix = [22u8, 0x03, 0x03];
    socket
        .write_all(&header_prefix)
        .expect("write partial header");
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close write side");

    // 実測: レコードヘッダ自体の read_exact が UnexpectedEof で失敗し
    // `RecordError::Io` となる（`alert_description()` は `None`）ため、
    // 上記 neg5 と同じく無応答のまま切断される。
    let mut buf = Vec::new();
    match socket.read_to_end(&mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!(
            "server must close the connection cleanly (EOF) for a header-truncated ClientHello, \
             not hang or error otherwise: {e:?} (partial={buf:?})"
        ),
    }
    assert!(
        buf.is_empty(),
        "server must send no bytes for a header-truncated ClientHello; got {buf:?}"
    );

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

/// server flight まで受け取ったあと client Finished を送らずに半クローズ
/// すると、pg wire バイト（`'R'` を含む）が一切届かずに接続が閉じること。
#[test]
fn handshake_abandoned_before_client_finished_closes_without_wire_data() {
    let (db_path, _db_guard) = new_test_db_for_negative_case("wire9-neg-7");
    let fixture = TempFixtureDir::new("neg7");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_stop_before_finished(&mut socket);
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close write side");

    // 実測: server flight 送出後に client Finished 待ちで EOF に達すると
    // `Ok(None)`（driver）→ `RecordError::Truncated` として shutdown する
    // （alert は送らない。neg5／neg6 と同じ経路）。pg wire バイトはおろか
    // 暗号化 alert すら一切届かない。`_client`（handshake 鍵導入済み）は
    // 万一暗号化データが来た場合に備えて保持するが、実測どおり無応答なので
    // 復号の出番はない。
    let _client = client;
    let mut buf = Vec::new();
    match socket.read_to_end(&mut buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!(
            "server must close the connection cleanly (EOF) when client Finished is never sent, \
             not hang or error otherwise: {e:?} (partial={buf:?})"
        ),
    }
    assert!(
        buf.is_empty(),
        "server must send no bytes (no alert, no pg wire data) when client Finished is \
         withheld; got {buf:?}"
    );

    assert_server_still_accepts_normal_connections(port);
    drop(server);
}

// --- Step 6: 層 B（psql／psycopg／pg の TLS 越し検証）----------------------

fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

fn run_psql_tls(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let conninfo =
        format!("host=127.0.0.1 port={port} user={user} dbname=irrelevant-db-name sslmode=require");
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args([
            "-d",
            &conninfo,
            "-X",
            "-w",
            "-q",
            "-At",
            "-F",
            "|",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        output.status.success(),
        "psql (TLS) exited non-zero for user {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn run_psycopg_tls(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/three_client/psycopg_client.py");
    let output = Command::new(&python)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql)
        .env("WIRE_SSLMODE", "require")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"));
    assert!(
        output.status.success(),
        "psycopg_client.py (TLS) failed for user {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn run_pg_tls(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    let node = resolve_tool("NODE_BIN", "node");
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
    let output = Command::new(&node)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql)
        .env("WIRE_SSL", "no-verify")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"));
    assert!(
        output.status.success(),
        "pg_client.js (TLS) failed for user {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// 3 クライアント（psql／psycopg／pg）が `--tls-mode require` 越しに
/// C1〜C4（TASK-73／WIRE-1）を独立オラクルどおりに実行できること。
/// `sslmode=require` での成功自体が TLS 経由であることの証明になる
/// うえ、追加で psql `sslmode=disable` が非 0 終了することも確認する
/// （非 vacuous 性）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client-tls`"]
fn three_clients_run_c1_through_c4_over_tls() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let fixture = TempFixtureDir::new("layerb-c1c4");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    const C1_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C2_SQL: &str =
        "SELECT id, lang FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C3_SQL: &str =
        "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    const C4_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3";
    let expected_c1 = vec!["1".to_string(), "2".to_string(), "3".to_string()];
    let expected_c2 = vec!["1|ja".to_string(), "3|ja".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        for (label, sql, expected) in [
            ("C1", C1_SQL, &expected_c1),
            ("C2", C2_SQL, &expected_c2),
            ("C3", C3_SQL, &expected_c1),
            ("C4", C4_SQL, &expected_c1),
        ] {
            assert_eq!(
                &run_psql_tls(port, user, pw, sql),
                expected,
                "psql (TLS): unexpected {label} for {user}"
            );
            assert_eq!(
                &run_psycopg_tls(port, user, pw, sql),
                expected,
                "psycopg (TLS): unexpected {label} for {user}"
            );
            assert_eq!(
                &run_pg_tls(port, user, pw, sql),
                expected,
                "pg (TLS): unexpected {label} for {user}"
            );
        }
    }

    // 非 vacuous 性: sslmode=disable は --tls-mode require により拒否される。
    // 実行するクエリは他ケースと同じ実在オラクル（C1 相当。結果 "1"）を
    // 使う。裸の `SELECT 1` は本エンジンの SQL 表層が受理しないため、万一
    // TLS 必須ポリシーが機能せず平文接続がそのまま認証・クエリ実行まで
    // 進んでも「クエリ自体の失敗」で非 0 終了して見えてしまい
    // （Vacuous rejection。codex/cursor #969 PR #1090 レビュー指摘）、TLS
    // 拒否の証拠にならない。実在クエリへ差し替えたうえで、(i) 終了コード
    // 非 0 に加え (ii) 認証前に平文 startup を拒否する `08P01` の実装
    // エラー文言が stderr に現れること (iii) 行データが一切標準出力に
    // 出ないことの 3 点を確認し、平文接続が起動応答すら受け取れずに拒否
    // されたことを積極的に検証する。
    let psql = resolve_tool("PSQL_BIN", "psql");
    let conninfo =
        format!("host=127.0.0.1 port={port} user=alice dbname=irrelevant-db-name sslmode=disable");
    let output = Command::new(&psql)
        .env("PGPASSWORD", "pw-alice")
        .args([
            "-d",
            &conninfo,
            "-X",
            "-w",
            "-At",
            "-c",
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "sslmode=disable must be rejected under --tls-mode require: stderr={stderr}"
    );
    assert!(
        stderr.contains("TLS is required by this server"),
        "rejection must be the plaintext-startup TLS policy error, not an unrelated query \
         failure: stderr={stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "no row must be returned when sslmode=disable is rejected before authentication"
    );

    drop(server);
}

/// 3 クライアントが TLS 越しでも RLS 3 テナント分離を保ち、他テナントの
/// Private 行が 0 件のまま混入しないこと。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client-tls`"]
fn three_clients_rls_isolation_over_tls() {
    let (db_path, _db_guard) = seed_rls_private_db();
    let fixture = TempFixtureDir::new("layerb-rls");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    // Public 行（id 1/2/3）は全テナント共有で見える。Private 行（id
    // 11/12/13）は自テナントのみ見える契約（層 A の
    // `tls_rls_three_tenant_isolation_has_zero_cross_tenant_rows` と同じ
    // オラクル）。
    const SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";
    for (user, pw, expected) in [
        (
            "alice",
            "pw-alice",
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "11".to_string(),
            ],
        ),
        (
            "bob",
            "pw-bob",
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "12".to_string(),
            ],
        ),
        (
            "carol",
            "pw-carol",
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "13".to_string(),
            ],
        ),
    ] {
        for (label, mut got) in [
            ("psql", run_psql_tls(port, user, pw, SQL)),
            ("psycopg", run_psycopg_tls(port, user, pw, SQL)),
            ("pg", run_pg_tls(port, user, pw, SQL)),
        ] {
            got.sort();
            let mut want = expected.clone();
            want.sort();
            assert_eq!(
                got, want,
                "{label} (TLS): user {user} must see exactly its own rows, zero cross-tenant"
            );
        }
    }

    drop(server);
}

/// psql に `ssl_max_protocol_version=TLSv1.2` を指定すると拒否され、その後
/// 同じサーバーへの正規 `sslmode=require` 接続は成功すること。
#[test]
#[ignore = "requires psql; run via `make e2e-three-client-tls`"]
fn psql_with_tls12_max_protocol_is_rejected() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let fixture = TempFixtureDir::new("layerb-tls12-cap");
    let (cert_path, key_path) = write_tls_pair(&fixture);
    write_users_file(&std::path::PathBuf::from(fixture.path_str("users.txt")));
    let (server, port) = spawn_tls_server_with_port(
        &fixture.path_str("users.txt"),
        db_path.to_str().expect("utf-8 path"),
        &cert_path,
        &key_path,
    );

    // 非 vacuous 性: `SELECT 1` は本エンジンの SQL 表層が受理しない裸の
    // SELECT のため、万一 `ssl_max_protocol_version=TLSv1.2` の制限が効かず
    // ハンドシェイクがそのまま成立して認証・クエリ実行まで進んでも
    // 「クエリ自体の失敗」で非 0 終了して見えてしまい、TLS 拒否の証拠に
    // ならない（codex/cursor #969 PR #1090 レビュー指摘）。他ケースと同じ
    // 実在オラクル（C1 相当。結果 "1"）に差し替え、終了コード非 0 に加え
    // 標準出力に行データが一切出ないことも確認する。ハンドシェイク自体が
    // 成立しなければクエリは送信すらされないため、この 2 点が揃って初めて
    // 「TLS 1.2 上限が実際にネゴシエーションを拒否した」ことの証拠になる。
    let psql = resolve_tool("PSQL_BIN", "psql");
    let conninfo = format!(
        "host=127.0.0.1 port={port} user=alice dbname=irrelevant-db-name \
         sslmode=require ssl_max_protocol_version=TLSv1.2"
    );
    let output = Command::new(&psql)
        .env("PGPASSWORD", "pw-alice")
        .args([
            "-d",
            &conninfo,
            "-X",
            "-w",
            "-At",
            "-c",
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql with ssl_max_protocol_version=TLSv1.2 must fail against a TLS 1.3-only server: \
         stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "no row must be returned when the TLS 1.2 cap is rejected before the query runs"
    );

    // 直後に同じサーバーへ通常の sslmode=require で接続でき、プロセスが
    // 生きたままであることを確認する（本エンジンの SQL 表層は `FROM`
    // 句を持たない裸の `SELECT 1` を受理しないため、C1 相当の SELECT で
    // 完走を確認する）。
    let rows = run_psql_tls(
        port,
        "alice",
        "pw-alice",
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1",
    );
    assert_eq!(rows, vec!["1".to_string()]);

    drop(server);
}
