//! curl 実クライアントでの NoSQL 表層（HTTPS）接続試験（Issue #968・親 #941・
//! TASK-228。手動専用・`#[ignore]`）。
//!
//! `tests/wire_scram_plus_psql_interop.rs`（libpq 実クライアント接続試験）と
//! 同じ役割分担: 層 A（`tests/http10_tls_surface.rs`・CI 常時実行）は本サーバー
//! 実装が持つ最小 TLS 1.3 クライアント（`tests/common/tls_client.rs`）で接続
//! 処理の中身を網羅する。本ファイルは実クライアント（curl・OpenSSL バック
//! エンド）と実際に相互運用できることの外形確認に限る。
//!
//! テスト証明書は SAN なし・署名は全 0（`tests/common/tls_client.rs`
//! `test_config` と同じ合成証明書。通信路の成立を確認する目的であり
//! 証明書チェーンの検証は対象外なため）で、curl 側は `-k`
//! （`--insecure`）を必須とする。
//!
//! CI には配線しない（curl 依存・`make ci` に含めない）。
//! `cargo test -p fandhe-vector-db-wire-server --test http10_curl_interop -- --ignored --nocapture`
//! で手動実行する（curl 8.18.0 / OpenSSL 3.5.5 で実測・完走を確認済み。
//! OpenSSL 3.5 系は ClientHello の `key_share` 先頭に hybrid ML-KEM を
//! 送るが、本サーバーの ClientHello 解析は複数 group から x25519 を探す
//! ため成立する）。

#[path = "common/mod.rs"]
mod common;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-curl-interop-{label}-{}-{}-{}",
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

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    fn path_str(&self, name: &str) -> String {
        self.path(name).to_str().expect("utf-8 path").to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// RFC 8032 §7.1 TEST 1 の Ed25519 seed／公開鍵から、有効な証明書・鍵 PEM の
/// 組を書き出す（`tests/wire_tls_cli.rs::write_valid_tls_pair` と同じ鍵材料。
/// テスト専用の DER 手組みロジックはそちらに依存せず本ファイル内で完結
/// させるため、最小限のヘルパーのみをここへ複製する）。
fn write_valid_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    use wire_server::tls::ed25519::SigningKey;
    use wire_server::tls::x509::ServerCertificateChain;

    const SEED_HEX: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    let seed: [u8; 32] = {
        let digits: Vec<u8> = SEED_HEX
            .bytes()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let bytes: Vec<u8> = digits
            .chunks(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ascii hex pair");
                u8::from_str_radix(text, 16).expect("valid hex byte")
            })
            .collect();
        bytes.try_into().expect("32 bytes")
    };
    let key = SigningKey::from_seed_bytes(seed);
    let public_key = key.public_key();

    // 最小限の自己署名 Ed25519 葉証明書 DER を手組みする（署名は検証対象外
    // のため全 0）。`tests/common/tls_client.rs` の同型ヘルパーと同じ構成。
    fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = body.len();
        if len < 0x80 {
            out.push(len as u8);
        } else {
            out.push(0x81);
            out.push(len as u8);
        }
        out.extend_from_slice(body);
        out
    }
    fn sequence(parts: &[&[u8]]) -> Vec<u8> {
        let mut body = Vec::new();
        for part in parts {
            body.extend_from_slice(part);
        }
        tlv(0x30, &body)
    }
    let oid_ed25519 = tlv(0x06, &[0x2b, 0x65, 0x70]);
    let algorithm_identifier = sequence(&[&oid_ed25519]);
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&public_key);
    let spki = sequence(&[&algorithm_identifier, &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&tlv(0x17, b"160801121924Z"), &tlv(0x17, b"401231235959Z")]);
    let common_name = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]);
    let issuer = sequence(&[&tlv(0x31, &common_name)]);
    let empty_subject = sequence(&[]);
    let tbs = sequence(&[
        &tlv(0xa0, &tlv(0x02, &[0x02])),
        &tlv(0x02, &[0x01]),
        &algorithm_identifier,
        &issuer,
        &validity,
        &empty_subject,
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let cert_der = sequence(&[&tbs, &algorithm_identifier, &tlv(0x03, &signature_bits)]);

    // 証明書チェーンとして受理可能であることを起動前に確認しておく
    // （読み込み経路は本番と同じ `wire_server::tls_opt::load_server_config`
    // へ委ねるため、ここでの検証は fixture 自体が壊れていないことの確認）。
    ServerCertificateChain::from_der_chain(vec![cert_der.clone()], &public_key, 1_600_000_000)
        .expect("valid synthetic chain");

    let cert_pem = pem_wrap("CERTIFICATE", &cert_der);
    let cert_path = fixture.path("cert.pem");
    std::fs::write(&cert_path, cert_pem).expect("write cert pem");

    // PKCS#8 v1 の Ed25519 秘密鍵 PEM（固定プレフィクス + 32 バイト seed）。
    let mut pkcs8_der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    pkcs8_der.extend_from_slice(&seed);
    let key_pem = pem_wrap("PRIVATE KEY", &pkcs8_der);
    let key_path = fixture.path("key.pem");
    std::fs::write(&key_path, key_pem).expect("write key pem");

    (cert_path, key_path)
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("ascii base64"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn write_user_store_with_alice(path: &std::path::Path) {
    let phc = wire_server::auth::argon2id::encode_phc(
        b"pw-alice",
        b"0123456789abcdef",
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// curl を実行し、`stdout` を返す。非 0 終了は panic で顕在化させる
/// （手動 gate のため、失敗を握りつぶさず即座に分かるようにする）。
fn run_curl(args: &[&str]) -> String {
    let output = Command::new("curl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn curl");
    assert!(
        output.status.success(),
        "curl exited non-zero: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 curl stdout")
}

#[test]
#[ignore = "手動専用: curl 実クライアントとの相互運用を要求実行者のマシンで確認する"]
fn curl_completes_session_query_close_over_https() {
    let fixture = TempFixtureDir::new("gate");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.path_str("db.redb"),
        "--bind",
        "127.0.0.1:0",
        "--surface",
        "nosql",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    // `/v1/session`: curl -k（自己署名・SAN なしのため証明書検証は行わない。
    // 通信路の成立自体が検証対象）。
    let session_out = run_curl(&[
        "-sS",
        "-k",
        "--http1.1",
        "-X",
        "POST",
        &format!("https://{addr}/v1/session"),
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"user":"alice","password":"pw-alice"}"#,
    ]);
    let parsed = engine::json::parse_json(&session_out).expect("valid json session response");
    let engine::json::JsonValue::Object(obj) = parsed else {
        panic!("expected json object, got {session_out}");
    };
    let engine::json::JsonValue::String(token) =
        obj.get("token").cloned().expect("token field present")
    else {
        panic!("expected string token field");
    };

    // `/v1/query`: スローアウェイ DB のため `42P01`（404）が非 vacuous な
    // 到達の証跡になる。curl の `%{http_code}` で確認する。
    let query_status = run_curl(&[
        "-sS",
        "-k",
        "--http1.1",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-X",
        "POST",
        &format!("https://{addr}/v1/query"),
        "-H",
        "Content-Type: application/json",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-d",
        r#"{"op":"scan","table":"docs","limit":1}"#,
    ]);
    assert_eq!(
        query_status, "404",
        "expected 42P01/404 against throwaway db"
    );

    // `/v1/session/close`。
    let close_status = run_curl(&[
        "-sS",
        "-k",
        "--http1.1",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-X",
        "POST",
        &format!("https://{addr}/v1/session/close"),
        "-H",
        "Content-Type: application/json",
        "-H",
        &format!("Authorization: Bearer {token}"),
        "-d",
        "",
    ]);
    assert_eq!(close_status, "200", "expected session close to succeed");

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("TLS enabled (mode=require)")),
        "expected TLS enabled line, got: {lines:?}"
    );
}
