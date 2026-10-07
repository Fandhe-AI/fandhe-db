//! PEM デコード（`tls::pem`）・PKCS#8 Ed25519 パース（`tls::pkcs8`。
//! TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #962・親 #941）の公開 API
//! だけを使う結合テスト。単体テストの内部関数照合（sextet 全数照合・DER
//! リーダーの境界確認等）とは別に、外部から見える契約
//! （PEM ファイル読み込み → 証明書チェーン／秘密鍵のデコード）が
//! RFC 8032・RFC 8410 の公開ベクタと一致すること、および fail-closed な
//! 拒否・ファイル入出力エラーの分類を固定する。

use std::io::Write;
use std::path::PathBuf;

/// プロセス内単調増加の連番を払い出す。一時パス名へ時刻・pid と併せて埋め込み、
/// 名前を一意にする。macOS の `SystemTime` はマイクロ秒分解能のため、同一
/// プロセス内で並列実行されるテスト同士が時刻だけでは衝突しうる（Issue #1463）。
fn next_fixture_seq() -> u64 {
    static FIXTURE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
use wire_server::tls::pem::{
    self, CertificateLoadError, FileLoadError, LegacyKeyFormat, PemError, TlsFileError,
};
use wire_server::tls::pkcs8::{self, KeyAlgorithm, Pkcs8Error, PrivateKeyLoadError};

// RFC 8410 §10.3 v1 の Ed25519 秘密鍵 PEM。
const ED25519_V1_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
const ED25519_V1_SEED_HEX: &str =
    "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";

fn hex_decode(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2));
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex"))
        .collect()
}

/// 一意な一時ファイルパスを作る（テスト間の競合・ゴミ残留を避ける。
/// 既存の `crates/wire-server/tests/common/mod.rs` と同じ方式）。
fn unique_temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tls-pem-pkcs8-e2e-{label}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        next_fixture_seq()
    ))
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn write(label: &str, contents: &[u8]) -> Self {
        let path = unique_temp_path(label);
        let mut file = std::fs::File::create(&path).expect("create temp file");
        file.write_all(contents).expect("write temp file");
        TempFile { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
        // パスとエラーだけを stderr へ出す（Issue #1303）。
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

#[test]
fn load_ed25519_private_key_file_matches_rfc8410_vector() {
    let file = TempFile::write("ed25519-v1", ED25519_V1_PEM.as_bytes());
    let seed = pkcs8::load_ed25519_private_key_file(&file.path).expect("valid key file");
    assert_eq!(
        seed.as_bytes().as_slice(),
        hex_decode(ED25519_V1_SEED_HEX).as_slice()
    );
}

#[test]
fn load_certificate_chain_file_reads_multiple_blocks_in_order() {
    // 決定的な合成 DER（実際の X.509 意味論は #963 の担当・対象外）を
    // 2 つ連結したチェーンが順序どおりに読めることを、ファイル読み込みの
    // 入口から確認する。
    let der_a = pem::decode_certificate_chain_pem(
        b"-----BEGIN CERTIFICATE-----\nAAAAAAAAAAAAAAAAAAAAAA==\n-----END CERTIFICATE-----\n",
    )
    .expect("valid single block");
    assert_eq!(der_a.len(), 1);

    let text = b"-----BEGIN CERTIFICATE-----\nAAAAAAAAAAAAAAAAAAAAAA==\n-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nAQIDBAUGBwgJCgsMDQ4PEA==\n-----END CERTIFICATE-----\n";
    let file = TempFile::write("cert-chain", text);
    let chain = pem::load_certificate_chain_file(&file.path).expect("valid chain file");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0], der_a[0]);
    assert_ne!(chain[0], chain[1]);
}

#[test]
fn load_certificate_chain_file_rejects_missing_file() {
    let path = unique_temp_path("cert-missing");
    let err = pem::load_certificate_chain_file(&path).unwrap_err();
    match err {
        CertificateLoadError::File(FileLoadError { error, .. }) => {
            assert_eq!(error, TlsFileError::NotFound);
        }
        other => panic!("expected File(NotFound), got {other:?}"),
    }
}

#[test]
fn load_ed25519_private_key_file_rejects_missing_file() {
    let path = unique_temp_path("key-missing");
    let err = pkcs8::load_ed25519_private_key_file(&path).unwrap_err();
    match err {
        PrivateKeyLoadError::File(FileLoadError { error, .. }) => {
            assert_eq!(error, TlsFileError::NotFound);
        }
        other => panic!("expected File(NotFound), got {other:?}"),
    }
}

#[test]
fn load_ed25519_private_key_file_rejects_directory() {
    let dir = std::env::temp_dir();
    let err = pkcs8::load_ed25519_private_key_file(&dir).unwrap_err();
    match err {
        PrivateKeyLoadError::File(FileLoadError { error, .. }) => {
            assert_eq!(error, TlsFileError::NotRegularFile);
        }
        other => panic!("expected File(NotRegularFile), got {other:?}"),
    }
}

#[test]
fn load_ed25519_private_key_file_rejects_oversized_file() {
    // `MAX_PRIVATE_KEY_FILE_LEN` を 1 バイト超える PEM もどきのファイルは、
    // PEM 構文の妥当性を検査するより前にサイズで拒否される。
    let oversized = vec![b'a'; (pem::MAX_PRIVATE_KEY_FILE_LEN + 1) as usize];
    let file = TempFile::write("key-oversized", &oversized);
    let err = pkcs8::load_ed25519_private_key_file(&file.path).unwrap_err();
    match err {
        PrivateKeyLoadError::File(FileLoadError { error, .. }) => {
            assert_eq!(
                error,
                TlsFileError::TooLarge {
                    max: pem::MAX_PRIVATE_KEY_FILE_LEN
                }
            );
        }
        other => panic!("expected File(TooLarge), got {other:?}"),
    }
}

#[test]
fn load_ed25519_private_key_file_rejects_rsa_pem() {
    let text = b"-----BEGIN RSA PRIVATE KEY-----\nAA==\n-----END RSA PRIVATE KEY-----\n";
    let file = TempFile::write("key-rsa", text);
    let err = pkcs8::load_ed25519_private_key_file(&file.path).unwrap_err();
    assert_eq!(
        err,
        PrivateKeyLoadError::Pem(PemError::UnsupportedKeyFormat(LegacyKeyFormat::Pkcs1Rsa))
    );
}

#[test]
fn load_ed25519_private_key_file_rejects_two_private_key_blocks() {
    let doubled = format!("{ED25519_V1_PEM}{ED25519_V1_PEM}");
    let file = TempFile::write("key-doubled", doubled.as_bytes());
    let err = pkcs8::load_ed25519_private_key_file(&file.path).unwrap_err();
    assert_eq!(
        err,
        PrivateKeyLoadError::Pem(PemError::ExpectedSingleBlock { found: 2 })
    );
}

#[test]
fn decode_ed25519_private_key_pem_rejects_rsa_der() {
    // rsaEncryption OID・NULL parameters を持つ最小 PKCS#8 DER。
    let mut body = vec![0x02, 0x01, 0x00];
    let mut alg = vec![0x06, 0x09];
    alg.extend_from_slice(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01]);
    alg.extend_from_slice(&[0x05, 0x00]);
    body.push(0x30);
    body.push(alg.len() as u8);
    body.extend_from_slice(&alg);
    body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
    let mut der = vec![0x30, body.len() as u8];
    der.extend_from_slice(&body);
    let der_b64 = base64_std_encode(&der);
    let pem_text = format!("-----BEGIN PRIVATE KEY-----\n{der_b64}\n-----END PRIVATE KEY-----\n");

    let err = pkcs8::decode_ed25519_private_key_pem(pem_text.as_bytes()).unwrap_err();
    assert_eq!(
        err,
        PrivateKeyLoadError::Pkcs8(Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Rsa))
    );
}

#[test]
fn decode_ed25519_private_key_pem_rejects_ecdsa_der() {
    let mut body = vec![0x02, 0x01, 0x00];
    let mut alg = vec![0x06, 0x07];
    alg.extend_from_slice(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]);
    body.push(0x30);
    body.push(alg.len() as u8);
    body.extend_from_slice(&alg);
    body.extend_from_slice(&[0x04, 0x02, 0x00, 0x00]);
    let mut der = vec![0x30, body.len() as u8];
    der.extend_from_slice(&body);
    let der_b64 = base64_std_encode(&der);
    let pem_text = format!("-----BEGIN PRIVATE KEY-----\n{der_b64}\n-----END PRIVATE KEY-----\n");

    let err = pkcs8::decode_ed25519_private_key_pem(pem_text.as_bytes()).unwrap_err();
    assert_eq!(
        err,
        PrivateKeyLoadError::Pkcs8(Pkcs8Error::UnsupportedAlgorithm(KeyAlgorithm::Ecdsa))
    );
}

#[test]
fn error_display_strings_do_not_leak_secret_bytes() {
    let file = TempFile::write("key-display-check", ED25519_V1_PEM.as_bytes());
    let seed = pkcs8::load_ed25519_private_key_file(&file.path).expect("valid key");
    let debug = format!("{seed:?}");
    let expected_hex = hex::encode_lower(&hex_decode(ED25519_V1_SEED_HEX));
    assert!(!debug.contains(&expected_hex));
    assert!(debug.contains("redacted"));
}

/// テスト専用の標準 base64 エンコーダ（依存を増やさず、`wire_server` の
/// 公開 API 越しに再利用できるものが無いため最小限だけ実装する）。
fn base64_std_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let (b0, b1, b2, n_out) = match chunk {
            [a, b, c] => (*a, *b, *c, 4),
            [a, b] => (*a, *b, 0u8, 3),
            [a] => (*a, 0u8, 0u8, 2),
            _ => unreachable!(),
        };
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        let sextets = [
            ((n >> 18) & 0x3f) as usize,
            ((n >> 12) & 0x3f) as usize,
            ((n >> 6) & 0x3f) as usize,
            (n & 0x3f) as usize,
        ];
        for (i, s) in sextets.iter().enumerate() {
            if i < n_out {
                out.push(ALPHABET[*s] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// テスト専用の最小 hex エンコーダ（秘密値が `Debug`/`Display` に漏れて
/// いないことを確認するための比較対象を作るだけに使う）。
mod hex {
    pub fn encode_lower(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
