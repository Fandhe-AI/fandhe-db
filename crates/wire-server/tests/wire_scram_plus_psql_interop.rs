//! libpq（psql）実クライアントでの SCRAM-SHA-256-PLUS 相互運用ゲート
//! （Issue #970・§3 ブロック C。手動専用・`#[ignore]`）。
//!
//! 本サーバーが受理する葉鍵は Ed25519 のみであり、libpq（OpenSSL バック
//! エンド）は `PLUS`（`p=tls-server-end-point`）選択時に本サーバーの署名
//! アルゴリズムに対応するダイジェストを解決できない（`docs/design/
//! tls-channel-binding.md` 参照。TLS ハンドシェイク自体は成立し、失敗は
//! TLS 確立後の SCRAM 交換〔tls-server-end-point の算出〕で起きる）。
//! `TlsServerConfig::with_scram_channel_binding` の有効・無効 × `sslmode=
//! require` かつ `channel_binding={disable,prefer,require}` の 2×3 通りで
//! psql の認証段階を実測し、`docs/design/tls-channel-binding.md` の既定値
//! 決定に使う。
//!
//! CI には配線しない（psql 依存・`make ci` に含めない）。
//! `cargo test -p fandhe-vector-db-wire-server --test wire_scram_plus_psql_interop -- --ignored --nocapture`
//! で手動実行する。

#[path = "common/tls_client.rs"]
mod tls_client;

#[path = "common/fixture_guard.rs"]
mod fixture_guard;

use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

const TEST_SCRAM_MOCK_KEY_SECRET: &[u8] = b"wire-scram-psql-interop-test-mock-key-secret!!";

/// `PLUS` 提示有効・無効 × `channel_binding` 設定 3 通りに対して実測済みの
/// 期待結果（`docs/design/tls-channel-binding.md` の 2×3 表と対応）。
/// アサーションを弱めて「失敗しなければ何でもよい」にはせず、失敗する
/// はずのセルではその原因文字列まで固定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    /// psql が認証段階を通過する（クエリ実行自体は engine 未接続のため
    /// 別エラーになってよい）。
    AuthSuccess,
    /// `PLUS` 選択時に libpq がダイジェスト未解決で失敗する
    /// （Ed25519 葉証明書に起因。TLS ハンドシェイク自体は成立済み）。
    DigestUnresolved,
    /// サーバーが `PLUS` を提示しないため、libpq がクライアント側で
    /// 接続を拒否する（`channel_binding=require` の契約どおりの拒否）。
    ClientRefusesNoPlus,
}

fn write_scram_user_store_file(username: &str, password: &[u8]) -> fixture_guard::UserStoreFile {
    let dir = std::env::temp_dir().join(format!(
        "wire-server-psql-interop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
    ));
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");
    let scram_salt = [7u8; wire_server::auth::scram::SALT_LEN];
    let verifier = wire_server::auth::scram::generate_verifier(
        password,
        &scram_salt,
        wire_server::auth::scram::SCRAM_ITERATIONS,
    )
    .expect("valid");
    let phc = wire_server::auth::argon2id::encode_phc(
        b"unused-in-scram-mode",
        b"0123456789abcdef",
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc");
    let content = format!(
        "{username}:tenant-a:{phc}:{}\n",
        verifier.to_verifier_string()
    );
    std::fs::write(&path, &content).expect("write fixture");
    fixture_guard::UserStoreFile::new(dir, path)
}

/// `plus_enabled` で `TlsServerConfig::with_scram_channel_binding` を切り替えた
/// サーバーを起動する（[`Expected`] のセルに対応する実測条件）。
fn spawn_tls_scram_server(
    users_path: &std::path::Path,
    plus_enabled: bool,
) -> std::net::SocketAddr {
    let store = Arc::new(
        UserStore::load_from_file(users_path)
            .expect("valid user store")
            .require_scram(TEST_SCRAM_MOCK_KEY_SECRET)
            .expect("all records carry scram verifiers"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    let tls = tls_client::test_config_with_scram_channel_binding(plus_enabled);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_tls(
            listener,
            store,
            None,
            Some(tls),
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// `channel_binding` 設定値ごとに psql を実行し、`expected` に対応する
/// 判定を行う（SQL 実行そのものは engine 未接続のため別エラーになって
/// よい。判定は認証・チャネルバインディング関連のエラー文字列で行う）。
fn run_psql(
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
    channel_binding: &str,
    plus_enabled: bool,
    expected: Expected,
) {
    let conninfo = format!(
        "host=127.0.0.1 port={} user={username} dbname=irrelevant sslmode=require channel_binding={channel_binding}",
        addr.port()
    );
    let output = Command::new("psql")
        .arg(&conninfo)
        .arg("-c")
        .arg("SELECT 1")
        .env("PGPASSWORD", password)
        .output()
        .expect("failed to spawn psql (interop gate requires psql on PATH)");
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("[plus_enabled={plus_enabled} channel_binding={channel_binding}] stderr:\n{stderr}");

    match expected {
        Expected::AuthSuccess => {
            // psql の接続失敗プレフィックス（TLS／認証段階で切断された場合
            // に出る）が無いことを確認する。engine 未接続によるクエリ
            // エラーは許容する。
            assert!(
                !stderr.contains("connection to server at"),
                "expected auth success (plus_enabled={plus_enabled}, channel_binding={channel_binding}) but psql failed to connect: {stderr}"
            );
        }
        Expected::DigestUnresolved => {
            assert!(
                stderr.contains("could not find digest"),
                "expected libpq digest-resolution failure (plus_enabled={plus_enabled}, channel_binding={channel_binding}) but got: {stderr}"
            );
        }
        Expected::ClientRefusesNoPlus => {
            assert!(
                stderr.contains("channel binding is required"),
                "expected libpq client-side refusal for missing PLUS (plus_enabled={plus_enabled}, channel_binding={channel_binding}) but got: {stderr}"
            );
        }
    }
}

#[test]
#[ignore]
fn wire_scram_plus_psql_interop_gate() {
    let username = "alice";
    let password = "correct horse battery staple";
    let users_path = write_scram_user_store_file(username, password.as_bytes());

    // `docs/design/tls-channel-binding.md` の 2×3 表と同一の実測条件・
    // 期待結果（psql 18.6・OpenSSL 3.5.5）。
    let matrix: &[(bool, &str, Expected)] = &[
        (false, "disable", Expected::AuthSuccess),
        (false, "prefer", Expected::AuthSuccess),
        (false, "require", Expected::ClientRefusesNoPlus),
        (true, "disable", Expected::AuthSuccess),
        (true, "prefer", Expected::DigestUnresolved),
        (true, "require", Expected::DigestUnresolved),
    ];

    for &(plus_enabled, channel_binding, expected) in matrix {
        let addr = spawn_tls_scram_server(&users_path, plus_enabled);
        // サーバー起動（`thread::spawn` の accept ループ）が listen 開始する
        // までの猶予。
        std::thread::sleep(Duration::from_millis(50));
        run_psql(
            addr,
            username,
            password,
            channel_binding,
            plus_enabled,
            expected,
        );
    }
}
