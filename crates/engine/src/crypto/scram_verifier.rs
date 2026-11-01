//! SCRAM-SHA-256 検証子（RFC 5802 の StoredKey／ServerKey）の表現・解析・生成。
//!
//! 元は wire-server の `auth::scram` にあったが、SQL の `CREATE ROLE ... PASSWORD`
//! （Issue #1962・SQL-39。`sql::role_ddl`）が engine 内で検証子を作る必要があり、
//! engine は wire-server に依存できないため、`sha256`・`hmac_sha256`・`base64_std`
//! と同様に engine へ置く。wire-server 側は `pub use` で再公開し、SCRAM の
//! メッセージ解析・照合（`auth::scram`）だけを持つ。保存形式は両者で単一実装を共有する。

use super::base64_std;
use super::hmac_sha256::{hmac_sha256, pbkdf2_hmac_sha256_one_block};
use super::sha256::digest;

/// `UserStore::load_from_file` が検証子を起動時検証する際に要求する反復回数
/// （完全一致のみ受理。反復回数がレコードごとに異なると server-first の
/// 形からユーザーの存在を区別できてしまうため。TASK-67・WIRE-2 の
/// Argon2id パラメータ完全一致検証と同じ設計判断）。
pub const SCRAM_ITERATIONS: u32 = 4096;

/// salt の必須長（バイト）。
pub const SALT_LEN: usize = 16;

/// StoredKey／ServerKey の長さ（SHA-256 の出力長固定）。
pub const KEY_LEN: usize = 32;

pub const VERIFIER_PREFIX: &str = "SCRAM-SHA-256$";

/// 検証子（RFC 5802 の StoredKey／ServerKey）。PostgreSQL 互換形式
/// `SCRAM-SHA-256$<iter>:<salt_b64>$<StoredKey_b64>:<ServerKey_b64>` で
/// `UserStore` の 4 番目のフィールドに保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScramVerifier {
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub stored_key: [u8; KEY_LEN],
    pub server_key: [u8; KEY_LEN],
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifierParseError {
    Malformed,
    InvalidIterations,
    InvalidSaltLength,
    InvalidKeyLength,
}

impl ScramVerifier {
    /// `SCRAM-SHA-256$<iter>:<salt_b64>$<StoredKey_b64>:<ServerKey_b64>` を
    /// 解析する。反復回数・salt 長・鍵長は呼び出し元（`UserStore`）が
    /// [`SCRAM_ITERATIONS`]／[`SALT_LEN`]／[`KEY_LEN`] への完全一致を別途
    /// 要求する契約のため、ここでは構文的な妥当性のみ検証する。
    pub fn parse(input: &str) -> Result<Self, VerifierParseError> {
        let rest = input
            .strip_prefix(VERIFIER_PREFIX)
            .ok_or(VerifierParseError::Malformed)?;
        let (params, keys) = rest.split_once('$').ok_or(VerifierParseError::Malformed)?;
        let (iter_str, salt_b64) = params
            .split_once(':')
            .ok_or(VerifierParseError::Malformed)?;
        let (stored_b64, server_b64) = keys.split_once(':').ok_or(VerifierParseError::Malformed)?;

        let iterations: u32 = iter_str
            .parse()
            .map_err(|_| VerifierParseError::InvalidIterations)?;
        if iterations == 0 {
            return Err(VerifierParseError::InvalidIterations);
        }

        let salt =
            base64_std::decode(salt_b64).map_err(|_| VerifierParseError::InvalidSaltLength)?;

        let stored_vec =
            base64_std::decode(stored_b64).map_err(|_| VerifierParseError::InvalidKeyLength)?;
        let server_vec =
            base64_std::decode(server_b64).map_err(|_| VerifierParseError::InvalidKeyLength)?;
        let stored_key: [u8; KEY_LEN] = stored_vec
            .as_slice()
            .try_into()
            .map_err(|_| VerifierParseError::InvalidKeyLength)?;
        let server_key: [u8; KEY_LEN] = server_vec
            .as_slice()
            .try_into()
            .map_err(|_| VerifierParseError::InvalidKeyLength)?;

        Ok(ScramVerifier {
            iterations,
            salt,
            stored_key,
            server_key,
        })
    }

    pub fn to_verifier_string(&self) -> String {
        format!(
            "{VERIFIER_PREFIX}{}:{}${}:{}",
            self.iterations,
            base64_std::encode(&self.salt),
            base64_std::encode(&self.stored_key),
            base64_std::encode(&self.server_key),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GenerateVerifierError {
    /// パスワードに印字可能 ASCII（0x20〜0x7e）以外の文字が含まれる。
    /// SASLprep（RFC 4013）を自作しないための制約（README 参照）。
    NonPrintableAsciiPassword,
    InvalidSaltLength,
}

/// パスワードから [`ScramVerifier`] を生成する（`hash-password
/// --with-scram-sha-256` サブコマンドから呼ばれる）。
///
/// SASLprep（RFC 4013、NFKC 正規化を含む）は自作しない。印字可能 ASCII への
/// SASLprep は恒等変換のため、パスワードを印字可能 ASCII に限定することで
/// libpq／node-postgres の実装と結果が一致することを保証する（この制約は
/// README に明記する）。
pub fn generate_verifier(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<ScramVerifier, GenerateVerifierError> {
    if salt.len() != SALT_LEN {
        return Err(GenerateVerifierError::InvalidSaltLength);
    }
    if !password.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
        return Err(GenerateVerifierError::NonPrintableAsciiPassword);
    }

    let salted_password = pbkdf2_hmac_sha256_one_block(password, salt, iterations);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = digest(&client_key);
    let server_key = hmac_sha256(&salted_password, b"Server Key");

    Ok(ScramVerifier {
        iterations,
        salt: salt.to_vec(),
        stored_key,
        server_key,
    })
}
