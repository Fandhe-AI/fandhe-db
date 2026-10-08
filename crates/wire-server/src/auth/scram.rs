//! SCRAM-SHA-256（RFC 5802／RFC 7677）の検証子・メッセージ解析・組み立て・
//! 照合ロジック。TLS 接続では SCRAM-SHA-256-PLUS（チャネルバインディング
//! `tls-server-end-point`。RFC 5929 §4。Issue #970）も提示・受理する
//! （[`ChannelBinding`]。TLS が無い接続では引き続き `None` のみを提示・
//! 受理する）。
//!
//! `handshake.rs` の SASL フローから呼ばれる（ポインタ: Issue #940・WIRE-18・
//! TASK-222・Issue #970）。本モジュールはメッセージのバイト列表現・パース・
//! `AuthMessage` の構成・proof の照合・チャネルバインディングの交渉判定の
//! みを担い、wire フレーミング（`AuthenticationSASL`/`Continue`/`Final` の
//! 型バイト・長さプレフィクス）・`tls-server-end-point` の値そのものの算出
//! （[`super::super::tls::channel_binding`]）は `handshake.rs` 側が担う。
//!
//! untrusted 入力（client-first/client-final）の解析は `unwrap`/`expect`/
//! 添字アクセスを使わず fail-closed に拒否する（`.claude/rules/coding-rust.md`）。

use super::base64_std;
use super::hmac_sha256::{hmac_sha256, pbkdf2_hmac_sha256_one_block};
use engine::crypto::sha256::digest;

/// PostgreSQL wire プロトコルへ提示する SASL 機構名（TLS 未接続、または
/// TLS 接続でもチャネルバインディング提示を無効化した設定の場合）。
pub const MECHANISM_NAME: &str = "SCRAM-SHA-256";

/// チャネルバインディング付きの SASL 機構名（TLS 接続かつ
/// `tls-server-end-point` を算出できた場合にのみ、`MECHANISM_NAME` と
/// あわせて機構リストへ提示する。Issue #970・RFC 5802 §6）。
pub const MECHANISM_NAME_PLUS: &str = "SCRAM-SHA-256-PLUS";

/// RFC 5929 §4 のチャネルバインディング種別名（gs2 `p=<cb-name>` の
/// `<cb-name>`）。本サーバーが対応する唯一の種別。
pub const CB_NAME_TLS_SERVER_END_POINT: &str = "tls-server-end-point";

/// `UserStore::load_from_file` が検証子を起動時検証する際に要求する反復回数
/// （完全一致のみ受理。反復回数がレコードごとに異なると server-first の
/// 形からユーザーの存在を区別できてしまうため。TASK-67・WIRE-2 の
/// Argon2id パラメータ完全一致検証と同じ設計判断）。
pub const SCRAM_ITERATIONS: u32 = 4096;

/// salt の必須長（バイト）。
pub const SALT_LEN: usize = 16;

/// StoredKey／ServerKey の長さ（SHA-256 の出力長固定）。
pub const KEY_LEN: usize = 32;

const VERIFIER_PREFIX: &str = "SCRAM-SHA-256$";

/// この接続で交渉されたチャネルバインディング（[`negotiate_channel_binding`]
/// の戻り値）。`None` は TLS 未接続、または TLS 接続でも `n`／`y` を選んだ
/// 場合。`TlsServerEndPoint` は SCRAM-SHA-256-PLUS で
/// `p=tls-server-end-point` を選んだ場合（Issue #970）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelBinding {
    None,
    TlsServerEndPoint,
}

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

// ---------------------------------------------------------------------------
// メッセージ解析
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ScramError {
    /// 構文違反（`08P01`。ProtocolViolation）。
    Malformed,
    /// `p=<cb-name>`（channel binding 要求）だが、この接続ではチャネル
    /// バインディングを提供できない（TLS 未接続、または TLS 接続でも
    /// `tls-server-end-point` を算出できなかった／提示無効化設定。`08P01`）。
    ChannelBindingRequested,
    /// サーバーが PLUS を提示したにもかかわらず、非 PLUS 機構を選びつつ
    /// gs2 cbind-flag `y`（クライアントは PLUS に対応するがサーバーは
    /// 対応しないと誤認）を送った（ダウングレード攻撃の検出。RFC 5802 §6。
    /// `08P01`）。
    ChannelBindingDowngrade,
    /// メッセージ長が上限を超過（`54000`。呼び出し元が別途フレーミング層で
    /// 検証する契約だが、本モジュール内の防御としても検証する）。
    TooLarge,
}

/// SASL メッセージ 1 個あたりの上限（nonce・proof 等を含む妥当な SCRAM
/// メッセージは全て数百バイト以内に収まる）。
pub const MAX_SCRAM_FIELD_LEN: usize = 1024;

fn validate_nonce(nonce: &str) -> Result<(), ScramError> {
    if nonce.is_empty() || nonce.len() > MAX_SCRAM_FIELD_LEN {
        return Err(ScramError::Malformed);
    }
    // RFC 5802 の printable 文字集合（`,` を除く US-ASCII 印字可能文字）。
    if !nonce
        .bytes()
        .all(|b| (0x21..=0x7e).contains(&b) && b != b',')
    {
        return Err(ScramError::Malformed);
    }
    Ok(())
}

/// gs2 cbind-flag（client-first-message の先頭要素）の解析結果。
/// [`negotiate_channel_binding`] の判定入力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gs2CbindFlag {
    /// `n`: クライアントはチャネルバインディングに対応しない。
    NotSupported,
    /// `y`: クライアントは対応するが、サーバーが対応しないと判断した
    /// （ダウングレード検出の対象）。
    ClientSupportsServerNot,
    /// `p=<cb-name>`: 指定した種別のチャネルバインディングを要求する。
    Requested(String),
}

/// client-first-message（gs2-header 部分を含む）を解析する。
#[derive(Debug)]
pub struct ClientFirstMessage {
    /// gs2-header（受信バイトそのまま。`c=` の照合対象）。
    pub gs2_header: Vec<u8>,
    /// client-first-message-bare（`AuthMessage` の先頭要素）。
    pub client_first_bare: Vec<u8>,
    pub client_nonce: String,
    /// gs2 cbind-flag の解析結果（[`negotiate_channel_binding`] の入力）。
    pub cbind_flag: Gs2CbindFlag,
}

/// RFC 5802 の gs2-cb-name 文字集合（`1*(ALPHA / DIGIT / "." / "-")`）を
/// 検証する。
fn validate_cbind_name(name: &str) -> Result<(), ScramError> {
    if name.is_empty() || name.len() > MAX_SCRAM_FIELD_LEN {
        return Err(ScramError::Malformed);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err(ScramError::Malformed);
    }
    Ok(())
}

/// [`parse_client_first`]／[`parse_client_first_with_cbind`] が共有する本体。
/// `allow_cbind_request` が `false` の場合、`p=<cb-name>` は（cb-name の
/// 形状を検査せず）直ちに [`ScramError::ChannelBindingRequested`] にする
/// （既存 `parse_client_first` の挙動をビット単位で保つため）。
fn parse_client_first_inner(
    body: &[u8],
    allow_cbind_request: bool,
) -> Result<ClientFirstMessage, ScramError> {
    if body.len() > MAX_SCRAM_FIELD_LEN {
        return Err(ScramError::TooLarge);
    }
    let s = std::str::from_utf8(body).map_err(|_| ScramError::Malformed)?;

    let mut parts = s.splitn(3, ',');
    let cbind_flag_str = parts.next().ok_or(ScramError::Malformed)?;
    let authzid = parts.next().ok_or(ScramError::Malformed)?;
    let bare = parts.next().ok_or(ScramError::Malformed)?;

    let cbind_flag = if let Some(cb_name) = cbind_flag_str.strip_prefix("p=") {
        if !allow_cbind_request {
            return Err(ScramError::ChannelBindingRequested);
        }
        validate_cbind_name(cb_name)?;
        Gs2CbindFlag::Requested(cb_name.to_string())
    } else if cbind_flag_str == "n" {
        Gs2CbindFlag::NotSupported
    } else if cbind_flag_str == "y" {
        Gs2CbindFlag::ClientSupportsServerNot
    } else {
        return Err(ScramError::Malformed);
    };
    // authzid（`a=<authzid>`）は空でなければ拒否する（テナントは StartupMessage
    // の `user` からのみ導出し、SASL 側の authzid は一切使わない設計判断）。
    if !authzid.is_empty() {
        return Err(ScramError::Malformed);
    }

    let gs2_header_len = cbind_flag_str.len() + 1 + authzid.len() + 1;
    let gs2_header = body
        .get(..gs2_header_len)
        .ok_or(ScramError::Malformed)?
        .to_vec();
    let client_first_bare = body
        .get(gs2_header_len..)
        .ok_or(ScramError::Malformed)?
        .to_vec();

    let mut bare_parts = bare.split(',');
    let username_field = bare_parts.next().ok_or(ScramError::Malformed)?;
    if username_field.starts_with("m=") {
        // reserved-mext（将来拡張の予約領域）。本実装は対応せず拒否する。
        return Err(ScramError::Malformed);
    }
    if !username_field.starts_with("n=") {
        return Err(ScramError::Malformed);
    }
    let nonce_field = bare_parts.next().ok_or(ScramError::Malformed)?;
    if bare_parts.next().is_some() {
        // extensions（クライアント拡張属性）は対応せず拒否する。
        return Err(ScramError::Malformed);
    }
    let nonce = nonce_field
        .strip_prefix("r=")
        .ok_or(ScramError::Malformed)?;
    validate_nonce(nonce)?;

    Ok(ClientFirstMessage {
        gs2_header,
        client_first_bare,
        client_nonce: nonce.to_string(),
        cbind_flag,
    })
}

/// TLS 未接続、またはチャネルバインディング非提供の接続向け（既存の
/// `p=<cb-name>` 拒否をビット単位で保つ）。
pub fn parse_client_first(body: &[u8]) -> Result<ClientFirstMessage, ScramError> {
    parse_client_first_inner(body, false)
}

/// TLS 接続かつチャネルバインディングを提供できる接続向け（Issue #970）。
/// `p=<cb-name>` を [`Gs2CbindFlag::Requested`] として受理し、交渉判定は
/// [`negotiate_channel_binding`] が別途行う。
pub fn parse_client_first_with_cbind(body: &[u8]) -> Result<ClientFirstMessage, ScramError> {
    parse_client_first_inner(body, true)
}

/// gs2 cbind-flag・選択した機構・サーバーが PLUS を提示したかから
/// チャネルバインディングの交渉結果を決める純粋関数（RFC 5802 §6・§7。
/// Issue #970）。ユーザーの存在に依存せず client-first 受信後・server-first
/// 送出前に呼べる（列挙攻撃対策として重要）。
///
/// | サーバーが PLUS を提示 | 選択機構 | cbind-flag | 結果 |
/// |---|---|---|---|
/// | no | 非PLUS | `n`／`y` | `None` |
/// | no | 非PLUS | `p=…` | `ChannelBindingRequested` |
/// | yes | PLUS | `p=tls-server-end-point` | `TlsServerEndPoint` |
/// | yes | PLUS | それ以外 | `Malformed`（構造上到達しない想定の防御） |
/// | yes | 非PLUS | `n` | `None` |
/// | yes | 非PLUS | `y` | `ChannelBindingDowngrade` |
/// | yes | 非PLUS | `p=…` | `Malformed`（構造上到達しない想定の防御） |
pub fn negotiate_channel_binding(
    selected_plus: bool,
    flag: &Gs2CbindFlag,
    server_offers_plus: bool,
) -> Result<ChannelBinding, ScramError> {
    match (server_offers_plus, selected_plus, flag) {
        (false, false, Gs2CbindFlag::NotSupported | Gs2CbindFlag::ClientSupportsServerNot) => {
            Ok(ChannelBinding::None)
        }
        (false, false, Gs2CbindFlag::Requested(_)) => Err(ScramError::ChannelBindingRequested),
        (true, true, Gs2CbindFlag::Requested(name)) if name == CB_NAME_TLS_SERVER_END_POINT => {
            Ok(ChannelBinding::TlsServerEndPoint)
        }
        (true, false, Gs2CbindFlag::NotSupported) => Ok(ChannelBinding::None),
        (true, false, Gs2CbindFlag::ClientSupportsServerNot) => {
            Err(ScramError::ChannelBindingDowngrade)
        }
        // 呼び出し元（`handshake.rs`）の判定順序・機構選択のバリデーション上、
        // 到達しないはずの組み合わせ（PLUS 未提示なのに PLUS を選んだ、
        // PLUS 選択時に未知の cb-name、非PLUS 選択時に `p=` 等）を
        // fail-closed に拒否する多層防御。
        (false, true, _)
        | (
            true,
            true,
            Gs2CbindFlag::NotSupported
            | Gs2CbindFlag::ClientSupportsServerNot
            | Gs2CbindFlag::Requested(_),
        )
        | (true, false, Gs2CbindFlag::Requested(_)) => Err(ScramError::Malformed),
    }
}

/// client-final-message（`c=<gs2-header-b64>,r=<nonce>,p=<proof-b64>`）。
#[derive(Debug)]
pub struct ClientFinalMessage {
    pub channel_binding_b64: String,
    pub nonce: String,
    pub proof: Vec<u8>,
}

pub fn parse_client_final(body: &[u8]) -> Result<ClientFinalMessage, ScramError> {
    if body.len() > MAX_SCRAM_FIELD_LEN {
        return Err(ScramError::TooLarge);
    }
    let s = std::str::from_utf8(body).map_err(|_| ScramError::Malformed)?;

    let mut parts = s.split(',');
    let cbind_field = parts.next().ok_or(ScramError::Malformed)?;
    let nonce_field = parts.next().ok_or(ScramError::Malformed)?;
    let proof_field = parts.next().ok_or(ScramError::Malformed)?;
    if parts.next().is_some() {
        return Err(ScramError::Malformed);
    }

    let channel_binding_b64 = cbind_field
        .strip_prefix("c=")
        .ok_or(ScramError::Malformed)?
        .to_string();
    let nonce = nonce_field
        .strip_prefix("r=")
        .ok_or(ScramError::Malformed)?;
    validate_nonce(nonce)?;
    let proof_b64 = proof_field
        .strip_prefix("p=")
        .ok_or(ScramError::Malformed)?;
    let proof = base64_std::decode(proof_b64).map_err(|_| ScramError::Malformed)?;

    Ok(ClientFinalMessage {
        channel_binding_b64,
        nonce: nonce.to_string(),
        proof,
    })
}

// ---------------------------------------------------------------------------
// AuthMessage 構成・server-first／server-final 組み立て・照合
// ---------------------------------------------------------------------------

/// `AuthMessage = client-first-message-bare + "," + server-first-message +
/// "," + client-final-message-without-proof`（RFC 5802 §3）。
pub fn compute_auth_message(
    client_first_bare: &[u8],
    server_first: &[u8],
    client_final_without_proof: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(
        client_first_bare.len() + server_first.len() + client_final_without_proof.len() + 2,
    );
    msg.extend_from_slice(client_first_bare);
    msg.push(b',');
    msg.extend_from_slice(server_first);
    msg.push(b',');
    msg.extend_from_slice(client_final_without_proof);
    msg
}

/// `c=<base64(gs2_header)>,r=<nonce>` を組み立てる（client-final-without-proof。
/// クライアントが送った client-final のうち `p=` を除いた部分と一致するべき値を
/// サーバー側でも独立に再構成し、`AuthMessage` の構成に使う）。
/// [`client_final_without_proof_with_cbind`] の `cbind_data` 空版に等しい。
pub fn client_final_without_proof(gs2_header: &[u8], nonce: &str) -> Vec<u8> {
    client_final_without_proof_with_cbind(gs2_header, &[], nonce)
}

/// `c=<base64(gs2_header ++ cbind_data)>,r=<nonce>` を組み立てる
/// （cbind-input = gs2-header ‖ cbind-data。RFC 5802 §5・RFC 5929 §4。
/// Issue #970）。`cbind_data` が空なら [`client_final_without_proof`] と
/// バイト同一。
pub fn client_final_without_proof_with_cbind(
    gs2_header: &[u8],
    cbind_data: &[u8],
    nonce: &str,
) -> Vec<u8> {
    let mut cbind_input = Vec::with_capacity(gs2_header.len() + cbind_data.len());
    cbind_input.extend_from_slice(gs2_header);
    cbind_input.extend_from_slice(cbind_data);
    format!("c={},r={nonce}", base64_std::encode(&cbind_input)).into_bytes()
}

/// server-first-message（`r=<nonce>,s=<salt_b64>,i=<iterations>`）。
pub fn build_server_first(combined_nonce: &str, salt: &[u8], iterations: u32) -> Vec<u8> {
    format!(
        "r={combined_nonce},s={},i={iterations}",
        base64_std::encode(salt)
    )
    .into_bytes()
}

/// server-final-message（成功時。`v=<ServerSignature_b64>`）。
pub fn build_server_final_success(server_signature: &[u8; KEY_LEN]) -> Vec<u8> {
    format!("v={}", base64_std::encode(server_signature)).into_bytes()
}

pub struct ClientFinalVerification {
    pub ok: bool,
    pub server_signature: [u8; KEY_LEN],
}

/// client-final の proof を検証し、成功時に返す ServerSignature も併せて
/// 計算する（RFC 5802 §3。proof の形状不正でも計算そのものは必ず最後まで
/// 実行し、早期 return によるタイミング差を作らない。呼び出し元
/// `handshake.rs` が固定遅延〔[`crate::auth::AUTH_FAILURE_DELAY`]〕を課す）。
pub fn verify_client_final(
    verifier: &ScramVerifier,
    auth_message: &[u8],
    proof: &[u8],
) -> ClientFinalVerification {
    let client_signature = hmac_sha256(&verifier.stored_key, auth_message);

    let proof_len_ok = proof.len() == KEY_LEN;
    let mut client_key = [0u8; KEY_LEN];
    for (i, slot) in client_key.iter_mut().enumerate() {
        let p = proof.get(i).copied().unwrap_or(0);
        let s = client_signature.get(i).copied().unwrap_or(0);
        *slot = p ^ s;
    }
    let computed_stored_key = digest(&client_key);
    let stored_key_matches =
        super::argon2id::constant_time_eq(&computed_stored_key, &verifier.stored_key);

    let server_signature = hmac_sha256(&verifier.server_key, auth_message);

    ClientFinalVerification {
        ok: proof_len_ok && stored_key_matches,
        server_signature,
    }
}

/// 未知ユーザー向けのモック検証子（列挙攻撃対策。ポインタ: WIRE-2, WIRE-3）。
///
/// salt は `HMAC(mock_key, "vector-db/scram/mock-salt/v1" || username)` の
/// 先頭 16 バイトとし、同一ユーザー名なら常に同じ値（実在ユーザーの salt が
/// 再接続しても変わらない性質と対称）になる。StoredKey／ServerKey は
/// `mock_key` から導出した固定値で、`verify_client_final` の全計算経路
/// （XOR・`H(ClientKey)`・`constant_time_eq`）を必ず最後まで実行させる目的
/// のみに使う（proof が既知の SaltedPassword に対応しない限り一致しない）。
/// プロジェクト名変更（vector-db → fandhe-db）後も値を固定する。存在秘匿のダミー応答の決定性に使う識別子でありブランド名ではないため、変更すると決定性契約が変わる。
pub fn mock_verifier(mock_key: &[u8; KEY_LEN], username: &str, iterations: u32) -> ScramVerifier {
    let mut salt_input = Vec::with_capacity(32 + username.len());
    salt_input.extend_from_slice(b"vector-db/scram/mock-salt/v1");
    salt_input.extend_from_slice(username.as_bytes());
    let salt_mac = hmac_sha256(mock_key, &salt_input);
    let salt = salt_mac
        .get(..SALT_LEN)
        .map(|s| s.to_vec())
        .unwrap_or_else(|| vec![0u8; SALT_LEN]);

    let stored_key = hmac_sha256(mock_key, b"vector-db/scram/mock-stored-key/v1");
    let server_key = hmac_sha256(mock_key, b"vector-db/scram/mock-server-key/v1");

    ScramVerifier {
        iterations,
        salt,
        stored_key,
        server_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7677 §3 の完全な往復ベクタ（user/pencil）。
    const RFC7677_SALT_B64: &str = "W22ZaJ0SNY7soEsUEjb6gQ==";
    const RFC7677_ITERATIONS: u32 = 4096;
    const RFC7677_CLIENT_NONCE: &str = "rOprNGfwEbeRWgbNEkqO";
    const RFC7677_SERVER_NONCE: &str = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";

    #[test]
    fn rfc7677_full_round_trip() {
        let salt = base64_std::decode(RFC7677_SALT_B64).expect("decode salt");
        let verifier =
            generate_verifier(b"pencil", &salt, RFC7677_ITERATIONS).expect("generate verifier");

        let client_first_bare = b"n=user,r=rOprNGfwEbeRWgbNEkqO";
        let combined_nonce = format!("{RFC7677_CLIENT_NONCE}{RFC7677_SERVER_NONCE}");
        let server_first = build_server_first(&combined_nonce, &salt, RFC7677_ITERATIONS);
        assert_eq!(
            std::str::from_utf8(&server_first).expect("utf8"),
            format!("r={combined_nonce},s={RFC7677_SALT_B64},i={RFC7677_ITERATIONS}")
        );

        let gs2_header = b"n,,";
        let client_final_no_proof = client_final_without_proof(gs2_header, &combined_nonce);
        assert_eq!(
            std::str::from_utf8(&client_final_no_proof).expect("utf8"),
            format!("c=biws,r={combined_nonce}")
        );

        let auth_message =
            compute_auth_message(client_first_bare, &server_first, &client_final_no_proof);

        let proof_b64 = "dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        let proof = base64_std::decode(proof_b64).expect("decode proof");

        let verification = verify_client_final(&verifier, &auth_message, &proof);
        assert!(verification.ok, "proof must verify against RFC 7677 vector");

        let expected_v = "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=";
        assert_eq!(
            base64_std::encode(&verification.server_signature),
            expected_v
        );
    }

    #[test]
    fn generate_verifier_matches_rfc7677_keys() {
        let salt = base64_std::decode(RFC7677_SALT_B64).expect("decode salt");
        let verifier =
            generate_verifier(b"pencil", &salt, RFC7677_ITERATIONS).expect("generate verifier");
        // StoredKey/ServerKey は RFC 7677 の proof/v の値から逆算した既知値と
        // 一致する（往復テストで proof/v 双方が一致することで間接的に固定
        // 済みだが、ここでは verifier 自体のシリアライズ往復も確認する）。
        let roundtrip = ScramVerifier::parse(&verifier.to_verifier_string()).expect("parse");
        assert_eq!(roundtrip, verifier);
    }

    #[test]
    fn parse_client_first_rejects_channel_binding_request() {
        let body = b"p=tls-server-end-point,,n=user,r=abc";
        assert_eq!(
            parse_client_first(body).unwrap_err(),
            ScramError::ChannelBindingRequested
        );
    }

    #[test]
    fn parse_client_first_rejects_nonempty_authzid() {
        let body = b"n,a=someone,n=user,r=abc";
        assert_eq!(parse_client_first(body).unwrap_err(), ScramError::Malformed);
    }

    #[test]
    fn parse_client_first_rejects_mext_and_extensions() {
        assert_eq!(
            parse_client_first(b"n,,m=x,n=user,r=abc").unwrap_err(),
            ScramError::Malformed
        );
        assert_eq!(
            parse_client_first(b"n,,n=user,r=abc,ext=1").unwrap_err(),
            ScramError::Malformed
        );
    }

    #[test]
    fn parse_client_first_accepts_y_flag() {
        let parsed = parse_client_first(b"y,,n=,r=abc").expect("parse");
        assert_eq!(parsed.client_nonce, "abc");
        assert_eq!(parsed.gs2_header, b"y,,");
    }

    #[test]
    fn parse_client_first_rejects_comma_in_nonce() {
        assert_eq!(
            parse_client_first(b"n,,n=,r=a,b").unwrap_err(),
            ScramError::Malformed
        );
    }

    #[test]
    fn parse_client_final_rejects_extra_fields() {
        assert_eq!(
            parse_client_final(b"c=biws,r=abc,p=AAAA,x=1").unwrap_err(),
            ScramError::Malformed
        );
    }

    #[test]
    fn parse_client_final_rejects_malformed_proof_base64() {
        assert_eq!(
            parse_client_final(b"c=biws,r=abc,p=not-base64!!").unwrap_err(),
            ScramError::Malformed
        );
    }

    #[test]
    fn verifier_parse_rejects_wrong_key_length() {
        assert_eq!(
            ScramVerifier::parse("SCRAM-SHA-256$4096:AAAAAAAAAAAAAAAAAAAAAA==$AAAA:AAAA"),
            Err(VerifierParseError::InvalidKeyLength)
        );
    }

    #[test]
    fn verifier_parse_rejects_zero_iterations() {
        let salt_b64 = base64_std::encode(&[0u8; SALT_LEN]);
        let key_b64 = base64_std::encode(&[0u8; KEY_LEN]);
        let s = format!("SCRAM-SHA-256$0:{salt_b64}${key_b64}:{key_b64}");
        assert_eq!(
            ScramVerifier::parse(&s),
            Err(VerifierParseError::InvalidIterations)
        );
    }

    #[test]
    fn generate_verifier_rejects_non_printable_ascii_password() {
        let salt = [0u8; SALT_LEN];
        assert_eq!(
            generate_verifier(b"pass\x01word", &salt, SCRAM_ITERATIONS),
            Err(GenerateVerifierError::NonPrintableAsciiPassword)
        );
        assert_eq!(
            generate_verifier("pässwörd".as_bytes(), &salt, SCRAM_ITERATIONS),
            Err(GenerateVerifierError::NonPrintableAsciiPassword)
        );
    }

    #[test]
    fn mock_verifier_salt_is_deterministic_per_username() {
        let mock_key = [7u8; KEY_LEN];
        let v1 = mock_verifier(&mock_key, "alice", SCRAM_ITERATIONS);
        let v2 = mock_verifier(&mock_key, "alice", SCRAM_ITERATIONS);
        let v3 = mock_verifier(&mock_key, "bob", SCRAM_ITERATIONS);
        assert_eq!(v1.salt, v2.salt);
        assert_ne!(v1.salt, v3.salt);
        assert_eq!(v1.salt.len(), SALT_LEN);
    }

    #[test]
    fn verify_client_final_rejects_wrong_proof_but_still_computes_signature() {
        let salt = [1u8; SALT_LEN];
        let verifier = generate_verifier(b"pencil", &salt, SCRAM_ITERATIONS).expect("verifier");
        let auth_message = b"dummy-auth-message";
        let wrong_proof = [0u8; KEY_LEN];
        let result = verify_client_final(&verifier, auth_message, &wrong_proof);
        assert!(!result.ok);
    }

    // ---- Issue #970: SCRAM-SHA-256-PLUS チャネルバインディング ----

    #[test]
    fn parse_client_first_with_cbind_accepts_p_and_returns_requested_flag() {
        let body = b"p=tls-server-end-point,,n=user,r=abc";
        let parsed = parse_client_first_with_cbind(body).expect("parse");
        assert_eq!(
            parsed.cbind_flag,
            Gs2CbindFlag::Requested("tls-server-end-point".to_string())
        );
        assert_eq!(parsed.gs2_header, b"p=tls-server-end-point,,");
        assert_eq!(parsed.client_nonce, "abc");
    }

    #[test]
    fn parse_client_first_with_cbind_rejects_invalid_cbind_name_charset() {
        // gs2-cb-name は ALPHA/DIGIT/"."/"-" のみ（`,` はフィールド区切りに
        // 使われるため元々別フィールドとして切り出されるが、`_` 等の非対応
        // 文字は cb-name 自体として拒否する）。
        assert_eq!(
            parse_client_first_with_cbind(b"p=tls_server,,n=user,r=abc").unwrap_err(),
            ScramError::Malformed
        );
    }

    #[test]
    fn parse_client_first_with_cbind_still_accepts_n_and_y() {
        let n = parse_client_first_with_cbind(b"n,,n=user,r=abc").expect("parse n");
        assert_eq!(n.cbind_flag, Gs2CbindFlag::NotSupported);
        let y = parse_client_first_with_cbind(b"y,,n=user,r=abc").expect("parse y");
        assert_eq!(y.cbind_flag, Gs2CbindFlag::ClientSupportsServerNot);
    }

    #[test]
    fn parse_client_first_unchanged_still_rejects_p_immediately() {
        // 既存 `parse_client_first`（非 PLUS 対応接続向け）はビット単位で
        // 不変（`p=` は cb-name の形状を検査せず即座に拒否）。
        assert_eq!(
            parse_client_first(b"p=tls-server-end-point,,n=user,r=abc").unwrap_err(),
            ScramError::ChannelBindingRequested
        );
    }

    #[test]
    fn negotiate_channel_binding_no_plus_offered_accepts_n_and_y() {
        assert_eq!(
            negotiate_channel_binding(false, &Gs2CbindFlag::NotSupported, false),
            Ok(ChannelBinding::None)
        );
        assert_eq!(
            negotiate_channel_binding(false, &Gs2CbindFlag::ClientSupportsServerNot, false),
            Ok(ChannelBinding::None)
        );
    }

    #[test]
    fn negotiate_channel_binding_no_plus_offered_rejects_p() {
        assert_eq!(
            negotiate_channel_binding(
                false,
                &Gs2CbindFlag::Requested("tls-server-end-point".to_string()),
                false
            ),
            Err(ScramError::ChannelBindingRequested)
        );
    }

    #[test]
    fn negotiate_channel_binding_plus_offered_and_selected_with_known_cb_name_succeeds() {
        assert_eq!(
            negotiate_channel_binding(
                true,
                &Gs2CbindFlag::Requested(CB_NAME_TLS_SERVER_END_POINT.to_string()),
                true
            ),
            Ok(ChannelBinding::TlsServerEndPoint)
        );
    }

    #[test]
    fn negotiate_channel_binding_plus_offered_and_selected_with_unknown_cb_name_is_rejected() {
        assert_eq!(
            negotiate_channel_binding(
                true,
                &Gs2CbindFlag::Requested("tls-unique".to_string()),
                true
            ),
            Err(ScramError::Malformed)
        );
    }

    #[test]
    fn negotiate_channel_binding_plus_offered_non_plus_selected_with_n_succeeds() {
        assert_eq!(
            negotiate_channel_binding(false, &Gs2CbindFlag::NotSupported, true),
            Ok(ChannelBinding::None)
        );
    }

    #[test]
    fn negotiate_channel_binding_plus_offered_non_plus_selected_with_y_is_downgrade() {
        assert_eq!(
            negotiate_channel_binding(false, &Gs2CbindFlag::ClientSupportsServerNot, true),
            Err(ScramError::ChannelBindingDowngrade)
        );
    }

    #[test]
    fn negotiate_channel_binding_plus_offered_non_plus_selected_with_p_is_rejected() {
        assert_eq!(
            negotiate_channel_binding(
                false,
                &Gs2CbindFlag::Requested(CB_NAME_TLS_SERVER_END_POINT.to_string()),
                true
            ),
            Err(ScramError::Malformed)
        );
    }

    #[test]
    fn client_final_without_proof_with_cbind_matches_empty_data_form() {
        let gs2_header = b"n,,";
        let nonce = "abc123";
        assert_eq!(
            client_final_without_proof(gs2_header, nonce),
            client_final_without_proof_with_cbind(gs2_header, &[], nonce)
        );
    }

    #[test]
    fn client_final_without_proof_with_cbind_concatenates_gs2_header_and_cbind_data() {
        let gs2_header = b"p=tls-server-end-point,,";
        let cbind_data = [0xaa, 0xbb, 0xcc];
        let nonce = "abc123";
        let result = client_final_without_proof_with_cbind(gs2_header, &cbind_data, nonce);
        let mut expected_input = gs2_header.to_vec();
        expected_input.extend_from_slice(&cbind_data);
        let expected = format!("c={},r={nonce}", base64_std::encode(&expected_input));
        assert_eq!(std::str::from_utf8(&result).expect("utf8"), expected);
    }
}
