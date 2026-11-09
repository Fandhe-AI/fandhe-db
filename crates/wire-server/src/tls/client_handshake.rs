//! TLS 1.3 クライアント側ハンドシェイク（Issue #2177・親 #2171・RECOVER-15 (3)・WIRE-9
//! ポインタ。方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2177）」）。
//!
//! 責務境界: レプリケーションの受信側（[`crate::replica`]・[`crate::replica_resync`]）が
//! プライマリへ TLS で接続するときだけ使う。サーバー側（[`super::server_handshake`]）と同じ
//! 最小集合（TLS 1.3・`TLS_AES_128_GCM_SHA256`・X25519・Ed25519）だけを提示・受理し、他は
//! fail-closed で拒否する。SNI・PSK・0-RTT・セッション再開・HelloRetryRequest・クライアント証明書は
//! 非対応。
//!
//! 証明書の検証は**ピン留め**（[`PinnedServerCertificate`]）: プライマリが提示した葉証明書の DER が
//! ピンの DER と完全一致し、`CertificateVerify` の署名がピンの Ed25519 公開鍵で検証でき、server
//! `Finished` が一致して初めて成功する。CA チェーン・ホスト名の検証は行わない（`x509` は証明書署名の
//! 検証を持たず、依存も追加しない。運用はプライマリの証明書ファイルを受信側へ配る方式）。
//!
//! 不変条件（資格情報の保護）: 呼び出し元（受信側の接続処理）は本関数が `Ok` を返した**後にだけ**
//! Hello（ロール名・パスワード）を書く。成功前にアプリケーションデータを送る経路は無い。
//!
//! 入力は untrusted: レコード長・ハンドシェイクメッセージ長は既存のレコード層・メッセージ層が
//! 確保前に上限検証し、本モジュールは受信データの経路で `unwrap`／`expect`／添字アクセスを使わない。
//! エラーは固定文言で、相手のバイト列・証明書の中身・長さを保持しない。

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use super::certificate_verify;
use super::client_hello;
use super::finished;
use super::handshake::{self, HandshakeBuffer, HandshakeType, RawHandshake};
use super::key_schedule::EarlySecret;
use super::pem;
use super::record::{self, ContentType, Record, RecordKind};
use super::record_protection::{Opener, Sealer};
use super::server_handshake::{DeadlineReader, DeadlineWriter, HandshakeTransport, TlsSession};
use super::transcript::Transcript;
use super::x25519::EphemeralSecret;
use super::x509;

/// ハンドシェイク全体の既定の絶対期限。
pub const CLIENT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// ピン留めした葉証明書（DER 全体と Ed25519 公開鍵）。公開情報だが、`Debug` は中身を出さない
/// （ログへ証明書の内容を流さない方針。Issue #2177）。
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedServerCertificate {
    leaf_der: Vec<u8>,
    public_key: [u8; 32],
}

impl std::fmt::Debug for PinnedServerCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedServerCertificate")
            .finish_non_exhaustive()
    }
}

/// ピンの読み込み失敗。分類のみで、パス・ファイルの中身は保持しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinLoadError {
    /// ファイルを読めない・PEM として不正・証明書が無い。
    Unreadable,
    /// 証明書が不正・期限外・Ed25519 でない。
    InvalidCertificate,
    /// 現在時刻を取得できない。
    Clock,
}

impl std::fmt::Display for PinLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable => write!(f, "the pinned certificate file could not be read"),
            Self::InvalidCertificate => write!(
                f,
                "the pinned certificate is not a currently valid Ed25519 certificate"
            ),
            Self::Clock => write!(f, "the system clock could not be read"),
        }
    }
}

impl std::error::Error for PinLoadError {}

impl PinnedServerCertificate {
    /// DER（葉証明書 1 枚）から組み立てる。有効期間内の Ed25519 証明書だけを受理する。
    pub fn from_leaf_der(leaf_der: Vec<u8>, now_unix_secs: i64) -> Result<Self, PinLoadError> {
        let public_key = x509::ed25519_leaf_public_key(&leaf_der, now_unix_secs)
            .map_err(|_| PinLoadError::InvalidCertificate)?;
        Ok(Self {
            leaf_der,
            public_key,
        })
    }

    /// PEM ファイル（先頭の証明書を葉として使う。プライマリの `--replication-tls-cert` と同じ
    /// ファイルを配る運用）から読み込む。受信側の起動時に 1 回だけ呼ぶ。
    pub fn load_file(path: &Path) -> Result<Self, PinLoadError> {
        let chain = pem::load_certificate_chain_file(path).map_err(|_| PinLoadError::Unreadable)?;
        let leaf = chain.into_iter().next().ok_or(PinLoadError::Unreadable)?;
        let now = x509::current_unix_secs().map_err(|_| PinLoadError::Clock)?;
        Self::from_leaf_der(leaf, now)
    }

    /// ピンの葉証明書の DER。
    pub fn leaf_der(&self) -> &[u8] {
        &self.leaf_der
    }
}

/// クライアント側ハンドシェイクの失敗。固定文言で、相手の内容を保持しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientHandshakeError {
    /// 接続・読み書き・期限切れ（一時障害として再試行してよい）。
    Io,
    /// 乱数・鍵交換の準備に失敗した（ローカルの問題）。
    Local,
    /// 提示された証明書がピンと一致しない。
    PinMismatch,
    /// `CertificateVerify`・`Finished` の検証に失敗した。
    VerificationFailed,
    /// 相手が TLS 1.3 の最小集合から外れた応答・不正なメッセージを返した。
    Protocol,
    /// 相手が alert を送って終了した。
    PeerAlert,
}

impl ClientHandshakeError {
    /// 再試行しても結果が変わらない拒否（中間者・設定違いの疑い）か。`false` は一時障害。
    pub fn is_rejection(self) -> bool {
        !matches!(self, Self::Io | Self::Local)
    }
}

impl std::fmt::Display for ClientHandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Io => "TLS handshake I/O failure",
            Self::Local => "TLS handshake could not be started",
            Self::PinMismatch => "the server certificate did not match the pinned certificate",
            Self::VerificationFailed => "TLS handshake verification failed",
            Self::Protocol => "TLS handshake protocol violation by the server",
            Self::PeerAlert => "the server aborted the TLS handshake",
        };
        write!(f, "{text}")
    }
}

impl std::error::Error for ClientHandshakeError {}

impl From<record::RecordError> for ClientHandshakeError {
    fn from(e: record::RecordError) -> Self {
        match e {
            record::RecordError::Io(_) | record::RecordError::Truncated => Self::Io,
            _ => Self::Protocol,
        }
    }
}

/// OS の CSPRNG と期限（既定 [`CLIENT_HANDSHAKE_TIMEOUT`]）でハンドシェイクを進める。成功すると
/// application epoch へ切り替え済みの [`TlsSession`] を返す。失敗時は接続を閉じる（alert は送らない）。
/// 成功時は読み書きタイムアウトを解除する（呼び出し元が用途に合わせて設定し直す）。
pub fn perform_client_handshake<S: HandshakeTransport>(
    stream: &mut S,
    pin: &PinnedServerCertificate,
) -> Result<Box<TlsSession>, ClientHandshakeError> {
    perform_client_handshake_with_timeout(stream, pin, CLIENT_HANDSHAKE_TIMEOUT)
}

/// [`perform_client_handshake`] の期限指定版（`timeout` はハンドシェイク開始からの絶対期限）。
pub fn perform_client_handshake_with_timeout<S: HandshakeTransport>(
    stream: &mut S,
    pin: &PinnedServerCertificate,
    timeout: Duration,
) -> Result<Box<TlsSession>, ClientHandshakeError> {
    let deadline = Instant::now() + timeout;
    let result = drive(stream, pin, deadline);
    match &result {
        Ok(_) => {
            // 期限用に設定した短いタイムアウトを残さない。解除できない接続は渡さない（fail-closed）。
            if stream.set_read_timeout(None).is_err() || stream.set_write_timeout(None).is_err() {
                let _ = stream.shutdown();
                return Err(ClientHandshakeError::Io);
            }
        }
        Err(_) => {
            let _ = stream.shutdown();
        }
    }
    result
}

fn write_records<S: HandshakeTransport>(
    stream: &mut S,
    records: &[Record],
    kind: RecordKind,
    deadline: Instant,
) -> Result<(), ClientHandshakeError> {
    let mut buf = Vec::new();
    for r in records {
        r.serialize_into(&mut buf, kind)?;
    }
    let mut w = DeadlineWriter { stream, deadline };
    w.write_all(&buf).map_err(|_| ClientHandshakeError::Io)?;
    w.flush().map_err(|_| ClientHandshakeError::Io)
}

fn read_record_by<S: HandshakeTransport>(
    stream: &mut S,
    kind: RecordKind,
    deadline: Instant,
) -> Result<Record, ClientHandshakeError> {
    let mut r = DeadlineReader { stream, deadline };
    match record::read_record(&mut r, kind)? {
        Some(rec) => Ok(rec),
        None => Err(ClientHandshakeError::Io),
    }
}

/// `ClientHello` を組む（サーバー側 `negotiate` が要求する最小集合。SNI・PSK・cookie なし）。
fn build_client_hello(
    random: [u8; 32],
    client_public: &[u8; 32],
) -> Result<RawHandshake, ClientHandshakeError> {
    let mut key_share = vec![0x00, 0x24];
    key_share.extend_from_slice(&client_hello::GROUP_X25519.to_be_bytes());
    key_share.extend_from_slice(&32u16.to_be_bytes());
    key_share.extend_from_slice(client_public);
    let extensions = vec![
        handshake::Extension {
            extension_type: 10,
            extension_data: vec![0x00, 0x02, 0x00, 0x1d],
        },
        handshake::Extension {
            extension_type: 13,
            extension_data: vec![0x00, 0x02, 0x08, 0x07],
        },
        handshake::Extension {
            extension_type: 43,
            extension_data: vec![0x02, 0x03, 0x04],
        },
        handshake::Extension {
            extension_type: 51,
            extension_data: key_share,
        },
    ];
    let ch = handshake::ClientHello {
        legacy_version: 0x0303,
        random,
        legacy_session_id: Vec::new(),
        cipher_suites: vec![client_hello::TLS_AES_128_GCM_SHA256],
        legacy_compression_methods: vec![0x00],
        extensions,
    };
    let mut body = Vec::new();
    ch.serialize_body_into(&mut body)
        .map_err(|_| ClientHandshakeError::Local)?;
    Ok(RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body,
    })
}

/// `ServerHello` を検証してサーバーの X25519 公開鍵を返す。想定外の暗号スイート・拡張・
/// session id・圧縮・版数はすべて拒否する。
fn server_key_share(sh: &handshake::ServerHello) -> Result<[u8; 32], ClientHandshakeError> {
    let proto = ClientHandshakeError::Protocol;
    if sh.legacy_version != 0x0303
        || !sh.legacy_session_id_echo.is_empty()
        || sh.cipher_suite != client_hello::TLS_AES_128_GCM_SHA256
        || sh.legacy_compression_method != 0
    {
        return Err(proto);
    }
    let mut key_share: Option<[u8; 32]> = None;
    let mut versions = false;
    for ext in &sh.extensions {
        match ext.extension_type {
            0x0033 if key_share.is_none() => {
                let group = ext.extension_data.get(..2).ok_or(proto)?;
                let len = ext.extension_data.get(2..4).ok_or(proto)?;
                let key = ext.extension_data.get(4..).ok_or(proto)?;
                if group != client_hello::GROUP_X25519.to_be_bytes() || len != 32u16.to_be_bytes() {
                    return Err(proto);
                }
                key_share = Some(<[u8; 32]>::try_from(key).map_err(|_| proto)?);
            }
            0x002b if !versions => {
                if ext.extension_data != client_hello::TLS13_VERSION.to_be_bytes() {
                    return Err(proto);
                }
                versions = true;
            }
            _ => return Err(proto),
        }
    }
    match (key_share, versions) {
        (Some(k), true) => Ok(k),
        _ => Err(proto),
    }
}

/// サーバーから受け取る暗号化ハンドシェイクメッセージ本文長の上限。
/// `Certificate` チェーン（証明書 1 通が `MAX_CERTIFICATE_DER_LEN` まで）を 16 通分とする。
const MAX_SERVER_HANDSHAKE_MESSAGE_LEN: usize = 16 * super::x509::MAX_CERTIFICATE_DER_LEN;

/// 暗号化されたレコードから次のハンドシェイクメッセージを 1 つ取り出す（足りなければレコードを読む）。
fn next_encrypted_message<S: HandshakeTransport>(
    stream: &mut S,
    opener: &mut Opener,
    buffer: &mut HandshakeBuffer,
    deadline: Instant,
) -> Result<RawHandshake, ClientHandshakeError> {
    loop {
        if let Some(m) = buffer
            .next_message()
            .map_err(|_| ClientHandshakeError::Protocol)?
        {
            return Ok(m);
        }
        let rec = read_record_by(stream, RecordKind::Ciphertext, deadline)?;
        let inner = opener
            .open(&rec)
            .map_err(|_| ClientHandshakeError::Protocol)?;
        match inner.content_type {
            ContentType::Handshake => buffer
                .feed(&inner.content)
                .map_err(|_| ClientHandshakeError::Protocol)?,
            ContentType::Alert => return Err(ClientHandshakeError::PeerAlert),
            _ => return Err(ClientHandshakeError::Protocol),
        }
    }
}

fn drive<S: HandshakeTransport>(
    stream: &mut S,
    pin: &PinnedServerCertificate,
    deadline: Instant,
) -> Result<Box<TlsSession>, ClientHandshakeError> {
    let proto = ClientHandshakeError::Protocol;
    let random: [u8; 32] = crate::auth::read_urandom(32)
        .map_err(|_| ClientHandshakeError::Local)?
        .try_into()
        .map_err(|_| ClientHandshakeError::Local)?;
    let ephemeral = EphemeralSecret::generate().map_err(|_| ClientHandshakeError::Local)?;
    let client_public = *ephemeral.public_key().as_bytes();

    let mut transcript = Transcript::new();
    let ch_raw = build_client_hello(random, &client_public)?;
    transcript
        .append_client_hello(&ch_raw)
        .map_err(|_| ClientHandshakeError::Local)?;
    let ch_wire = ch_raw.to_bytes().map_err(|_| ClientHandshakeError::Local)?;
    let ch_records = record::fragment_plaintext(ContentType::Handshake, &ch_wire)?;
    write_records(stream, &ch_records, RecordKind::Plaintext, deadline)?;

    // ServerHello（平文）。alert・別種のレコード・HelloRetryRequest は拒否する。
    let sh_record = read_record_by(stream, RecordKind::Plaintext, deadline)?;
    match sh_record.content_type {
        ContentType::Handshake => {}
        ContentType::Alert => return Err(ClientHandshakeError::PeerAlert),
        _ => return Err(proto),
    }
    let mut sh_buf = HandshakeBuffer::new();
    sh_buf.feed_record(&sh_record).map_err(|_| proto)?;
    let sh_raw = sh_buf.next_message().map_err(|_| proto)?.ok_or(proto)?;
    if sh_raw.msg_type != HandshakeType::ServerHello || sh_buf.has_partial() {
        return Err(proto);
    }
    let sh = handshake::ServerHello::parse(&sh_raw.body).map_err(|_| proto)?;
    // HelloRetryRequest は append_server_hello が拒否する（再試行しない）。
    transcript.append_server_hello(&sh_raw).map_err(|_| proto)?;
    let server_public = server_key_share(&sh)?;
    let th_ch_sh = transcript.hash_through_server_hello().map_err(|_| proto)?;

    let shared = ephemeral
        .diffie_hellman(&server_public)
        .map_err(|_| proto)?;
    let handshake_secret = EarlySecret::new_without_psk()
        .into_handshake(&shared)
        .map_err(|_| ClientHandshakeError::Local)?;
    let traffic = handshake_secret
        .traffic_secrets(&th_ch_sh)
        .map_err(|_| ClientHandshakeError::Local)?;
    let mut sealer = Sealer::new();
    let mut opener = Opener::new();
    sealer
        .install_handshake_keys(
            &traffic
                .client
                .traffic_keys()
                .map_err(|_| ClientHandshakeError::Local)?,
        )
        .map_err(|_| ClientHandshakeError::Local)?;
    opener
        .install_handshake_keys(
            &traffic
                .server
                .traffic_keys()
                .map_err(|_| ClientHandshakeError::Local)?,
        )
        .map_err(|_| ClientHandshakeError::Local)?;

    // サーバーの Certificate チェーンは既定の受信上限（65535）を超え得るため、
    // 証明書 1 通 64 KiB の上限を 16 通分見込んだ専用の上限を使う（有界・fail-closed）。
    let mut buffer = HandshakeBuffer::with_max_message_len(MAX_SERVER_HANDSHAKE_MESSAGE_LEN);

    let ee = next_encrypted_message(stream, &mut opener, &mut buffer, deadline)?;
    if ee.msg_type != HandshakeType::EncryptedExtensions {
        return Err(proto);
    }
    transcript
        .append_encrypted_extensions(&ee)
        .map_err(|_| proto)?;

    let cert_raw = next_encrypted_message(stream, &mut opener, &mut buffer, deadline)?;
    if cert_raw.msg_type != HandshakeType::Certificate {
        return Err(proto);
    }
    let certificate = handshake::Certificate::parse(&cert_raw.body).map_err(|_| proto)?;
    let leaf = certificate.certificate_list.first().ok_or(proto)?;
    // ピン照合: 資格情報を渡す相手が、配布された証明書そのものを提示していること。
    if leaf.cert_data != pin.leaf_der {
        return Err(ClientHandshakeError::PinMismatch);
    }
    transcript
        .append_certificate(&cert_raw)
        .map_err(|_| proto)?;
    let th_ch_cert = transcript.hash_through_certificate().map_err(|_| proto)?;

    let cv_raw = next_encrypted_message(stream, &mut opener, &mut buffer, deadline)?;
    if cv_raw.msg_type != HandshakeType::CertificateVerify {
        return Err(proto);
    }
    let cv = handshake::CertificateVerify::parse(&cv_raw.body).map_err(|_| proto)?;
    certificate_verify::verify_server_certificate_verify(&pin.public_key, &th_ch_cert, &cv)
        .map_err(|_| ClientHandshakeError::VerificationFailed)?;
    transcript
        .append_certificate_verify(&cv_raw)
        .map_err(|_| proto)?;
    let th_ch_cv = transcript
        .hash_through_certificate_verify()
        .map_err(|_| proto)?;

    let sf_raw = next_encrypted_message(stream, &mut opener, &mut buffer, deadline)?;
    if sf_raw.msg_type != HandshakeType::Finished {
        return Err(proto);
    }
    let server_finished = handshake::Finished::parse(&sf_raw.body).map_err(|_| proto)?;
    finished::verify_server_finished(&traffic.server, &th_ch_cv, &server_finished)
        .map_err(|_| ClientHandshakeError::VerificationFailed)?;
    transcript
        .append_server_finished(&sf_raw)
        .map_err(|_| proto)?;
    let th_ch_sf = transcript
        .hash_through_server_finished()
        .map_err(|_| proto)?;
    // server flight の後ろに余分なメッセージ・断片があれば拒否する。
    if buffer.has_partial() {
        return Err(proto);
    }

    // client Finished を handshake epoch で送り、application epoch へ切り替える。
    let client_finished = finished::build_client_finished(&traffic.client, &th_ch_sf)
        .map_err(|_| ClientHandshakeError::Local)?;
    let mut cf_body = Vec::new();
    client_finished
        .serialize_body_into(&mut cf_body)
        .map_err(|_| ClientHandshakeError::Local)?;
    let cf_wire = RawHandshake {
        msg_type: HandshakeType::Finished,
        body: cf_body,
    }
    .to_bytes()
    .map_err(|_| ClientHandshakeError::Local)?;
    let cf_records = sealer
        .seal_fragmented(ContentType::Handshake, &cf_wire)
        .map_err(|_| ClientHandshakeError::Local)?;
    write_records(stream, &cf_records, RecordKind::Ciphertext, deadline)?;

    let app = handshake_secret
        .into_master()
        .and_then(|m| m.application_traffic_secrets(&th_ch_sf))
        .map_err(|_| ClientHandshakeError::Local)?;
    sealer
        .install_application_keys(
            &app.client
                .traffic_keys()
                .map_err(|_| ClientHandshakeError::Local)?,
        )
        .map_err(|_| ClientHandshakeError::Local)?;
    opener
        .install_application_keys(
            &app.server
                .traffic_keys()
                .map_err(|_| ClientHandshakeError::Local)?,
        )
        .map_err(|_| ClientHandshakeError::Local)?;
    Ok(Box::new(TlsSession::from_client_handshake(sealer, opener)))
}

/// 公開 API が `io::Error` を要求する呼び出し元向けの変換（固定文言）。
impl From<ClientHandshakeError> for io::Error {
    fn from(e: ClientHandshakeError) -> Self {
        io::Error::other(e.to_string())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::certificate_verify::build_server_certificate_verify;
    use crate::tls::ed25519::SigningKey;
    use crate::tls::handshake::{Certificate, CertificateEntry, EncryptedExtensions};
    use std::net::{TcpListener, TcpStream};

    /// RFC 8032 §7.1 TEST 1 の seed（IETF の公開文書由来。テスト専用の鍵材料）。
    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    const OID_ED25519: [u8; 3] = [0x2b, 0x65, 0x70];
    const OID_X25519: [u8; 3] = [0x2b, 0x65, 0x6e];
    /// 検証時刻（2020-09-13。テスト証明書の有効期間内）。
    const NOW: i64 = 1_600_000_000;

    fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = body.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len < 0x100 {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        out.extend_from_slice(body);
        out
    }

    fn seq(parts: &[&[u8]]) -> Vec<u8> {
        tlv(0x30, &parts.concat())
    }

    /// 最小の自己署名風の葉証明書（署名値は検証対象外のため 0 埋め）。
    fn cert_der(spki_oid: &[u8], key: &[u8; 32], not_before: &str, not_after: &str) -> Vec<u8> {
        let alg = seq(&[&tlv(0x06, &OID_ED25519)]);
        let mut bits = vec![0u8];
        bits.extend_from_slice(key);
        let spki = seq(&[&seq(&[&tlv(0x06, spki_oid)]), &tlv(0x03, &bits)]);
        let validity = seq(&[
            &tlv(0x17, not_before.as_bytes()),
            &tlv(0x17, not_after.as_bytes()),
        ]);
        let issuer = seq(&[&tlv(
            0x31,
            &seq(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]),
        )]);
        let tbs = seq(&[
            &tlv(0xa0, &tlv(0x02, &[0x02])),
            &tlv(0x02, &[0x01]),
            &alg,
            &issuer,
            &validity,
            &seq(&[]),
            &spki,
        ]);
        let mut sig = vec![0u8];
        sig.extend_from_slice(&[0u8; 64]);
        seq(&[&tbs, &alg, &tlv(0x03, &sig)])
    }

    fn good_cert(not_before: &str) -> Vec<u8> {
        let key = SigningKey::from_seed_bytes(SEED).public_key();
        cert_der(&OID_ED25519, &key, not_before, "401231235959Z")
    }

    fn pin_of(der: Vec<u8>) -> PinnedServerCertificate {
        PinnedServerCertificate::from_leaf_der(der, NOW).expect("valid pin")
    }

    #[test]
    fn pin_accepts_a_valid_ed25519_certificate_and_exposes_nothing_in_debug() {
        let der = good_cert("160801121924Z");
        let pin = pin_of(der.clone());
        assert_eq!(pin.leaf_der(), der.as_slice());
        assert_eq!(
            pin.public_key,
            SigningKey::from_seed_bytes(SEED).public_key()
        );
        // RFC 8032 TEST 1 の公開鍵（先頭 8 バイト）。
        assert_eq!(
            pin.public_key.get(..8),
            Some(&[0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7][..])
        );
        assert_eq!(
            format!("{pin:?}"),
            "PinnedServerCertificate { .. }",
            "no certificate bytes in Debug"
        );
    }

    #[test]
    fn pin_rejects_expired_not_yet_valid_non_ed25519_and_garbage() {
        let key = SigningKey::from_seed_bytes(SEED).public_key();
        let expired = cert_der(&OID_ED25519, &key, "160801121924Z", "170801121924Z");
        let future = cert_der(&OID_ED25519, &key, "300101000000Z", "401231235959Z");
        let wrong_algorithm = cert_der(&OID_X25519, &key, "160801121924Z", "401231235959Z");
        for der in [
            expired,
            future,
            wrong_algorithm,
            vec![],
            vec![0x30, 0x03, 1, 2, 3],
        ] {
            assert_eq!(
                PinnedServerCertificate::from_leaf_der(der, NOW).err(),
                Some(PinLoadError::InvalidCertificate)
            );
        }
        assert_eq!(
            PinnedServerCertificate::load_file(Path::new("/nonexistent/pin.pem")).err(),
            Some(PinLoadError::Unreadable)
        );
    }

    fn plaintext_handshake(raw: &RawHandshake) -> Vec<u8> {
        let rec = Record {
            content_type: ContentType::Handshake,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: raw.to_bytes().expect("wire bytes"),
        };
        let mut out = Vec::new();
        rec.serialize_into(&mut out, RecordKind::Plaintext)
            .expect("serialize");
        out
    }

    fn server_hello_raw(
        random: [u8; 32],
        cipher: u16,
        extra_extension: bool,
        server_public: &[u8; 32],
    ) -> RawHandshake {
        let mut key_share = client_hello::GROUP_X25519.to_be_bytes().to_vec();
        key_share.extend_from_slice(&32u16.to_be_bytes());
        key_share.extend_from_slice(server_public);
        let mut extensions = vec![
            handshake::Extension {
                extension_type: 0x0033,
                extension_data: key_share,
            },
            handshake::Extension {
                extension_type: 0x002b,
                extension_data: client_hello::TLS13_VERSION.to_be_bytes().to_vec(),
            },
        ];
        if extra_extension {
            extensions.push(handshake::Extension {
                extension_type: 0x0000,
                extension_data: vec![0, 0],
            });
        }
        let sh = handshake::ServerHello {
            legacy_version: 0x0303,
            random,
            legacy_session_id_echo: Vec::new(),
            cipher_suite: cipher,
            legacy_compression_method: 0,
            extensions,
        };
        let mut body = Vec::new();
        sh.serialize_body_into(&mut body).expect("body");
        RawHandshake {
            msg_type: HandshakeType::ServerHello,
            body,
        }
    }

    /// クライアントの `ClientHello` を読み、(生メッセージ, クライアントの X25519 公開鍵) を返す。
    fn read_client_hello(conn: &mut TcpStream) -> (RawHandshake, [u8; 32]) {
        let rec = record::read_record(conn, RecordKind::Plaintext)
            .expect("record")
            .expect("client hello record");
        let mut buf = HandshakeBuffer::new();
        buf.feed_record(&rec).expect("feed");
        let raw = buf.next_message().expect("message").expect("complete");
        let ch = handshake::ClientHello::parse(&raw.body).expect("client hello");
        let key = ch
            .extensions
            .iter()
            .find(|e| e.extension_type == 51)
            .and_then(|e| e.extension_data.get(6..38))
            .and_then(|k| <[u8; 32]>::try_from(k).ok())
            .expect("client key share");
        (raw, key)
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Fault {
        None,
        TamperCertificateVerify,
        TamperFinished,
    }

    /// 本物のサーバーと同じ順序で server flight を組む偽サーバー（試験専用）。`fault` で署名・Finished を壊せる。
    /// 証明書メッセージには `presented_der` を載せ、署名は `SEED` の鍵で行う。
    fn serve_flight(conn: &mut TcpStream, presented_der: &[u8], fault: Fault) {
        let (ch_raw, client_public) = read_client_hello(conn);
        let mut transcript = Transcript::new();
        transcript.append_client_hello(&ch_raw).expect("ch");
        let eph = EphemeralSecret::from_bytes([7u8; 32]);
        let server_public = *eph.public_key().as_bytes();
        let sh_raw = server_hello_raw(
            [9u8; 32],
            client_hello::TLS_AES_128_GCM_SHA256,
            false,
            &server_public,
        );
        transcript.append_server_hello(&sh_raw).expect("sh");
        let th_ch_sh = transcript.hash_through_server_hello().expect("th");
        conn.write_all(&plaintext_handshake(&sh_raw))
            .expect("write sh");

        let shared = eph.diffie_hellman(&client_public).expect("dh");
        let hs = EarlySecret::new_without_psk()
            .into_handshake(&shared)
            .expect("hs");
        let traffic = hs.traffic_secrets(&th_ch_sh).expect("traffic");
        let mut sealer = Sealer::new();
        sealer
            .install_handshake_keys(&traffic.server.traffic_keys().expect("keys"))
            .expect("install");

        let raw_of = |msg_type, body: Vec<u8>| RawHandshake { msg_type, body };
        let mut body = Vec::new();
        EncryptedExtensions { extensions: vec![] }
            .serialize_body_into(&mut body)
            .expect("ee");
        let ee = raw_of(HandshakeType::EncryptedExtensions, body);
        transcript.append_encrypted_extensions(&ee).expect("ee");

        let mut body = Vec::new();
        Certificate {
            certificate_request_context: Vec::new(),
            certificate_list: vec![CertificateEntry {
                cert_data: presented_der.to_vec(),
                extensions: Vec::new(),
            }],
        }
        .serialize_body_into(&mut body)
        .expect("cert");
        let cert = raw_of(HandshakeType::Certificate, body);
        transcript.append_certificate(&cert).expect("cert");
        let th_ch_cert = transcript.hash_through_certificate().expect("th");

        let key = SigningKey::from_seed_bytes(SEED);
        let mut cv = build_server_certificate_verify(&key, &th_ch_cert);
        if fault == Fault::TamperCertificateVerify {
            if let Some(b) = cv.signature.first_mut() {
                *b ^= 0x01;
            }
        }
        let mut body = Vec::new();
        cv.serialize_body_into(&mut body).expect("cv");
        let cv_raw = raw_of(HandshakeType::CertificateVerify, body);
        transcript.append_certificate_verify(&cv_raw).expect("cv");
        let th_ch_cv = transcript.hash_through_certificate_verify().expect("th");

        let mut fin = finished::build_server_finished(&traffic.server, &th_ch_cv).expect("fin");
        if fault == Fault::TamperFinished {
            if let Some(b) = fin.verify_data.first_mut() {
                *b ^= 0x01;
            }
        }
        let mut body = Vec::new();
        fin.serialize_body_into(&mut body).expect("fin");
        let fin_raw = raw_of(HandshakeType::Finished, body);

        let mut flight = Vec::new();
        for m in [&ee, &cert, &cv_raw, &fin_raw] {
            flight.extend_from_slice(&m.to_bytes().expect("wire"));
        }
        let records = sealer
            .seal_fragmented(ContentType::Handshake, &flight)
            .expect("seal");
        let mut out = Vec::new();
        for r in &records {
            r.serialize_into(&mut out, RecordKind::Ciphertext)
                .expect("serialize");
        }
        conn.write_all(&out).expect("write flight");
    }

    /// 偽サーバーを別スレッドで動かし、クライアントのハンドシェイク結果を返す。
    fn run(
        pin: &PinnedServerCertificate,
        server: impl FnOnce(TcpStream) + Send + 'static,
    ) -> Result<(), ClientHandshakeError> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            server(conn);
        });
        let mut stream = TcpStream::connect(addr).expect("connect");
        let result =
            perform_client_handshake_with_timeout(&mut stream, pin, Duration::from_secs(5))
                .map(|_| ());
        drop(stream);
        handle.join().expect("server thread");
        result
    }

    fn hello_then(send: Vec<u8>) -> impl FnOnce(TcpStream) + Send + 'static {
        move |mut conn| {
            let _ = read_client_hello(&mut conn);
            let _ = conn.write_all(&send);
        }
    }

    #[test]
    fn malformed_server_hello_variants_are_protocol_violations_not_panics() {
        let pin = pin_of(good_cert("160801121924Z"));
        let public = [5u8; 32];
        let ok_cipher = client_hello::TLS_AES_128_GCM_SHA256;
        // 切り詰めた ServerHello（宣言長に本文が足りない）。
        let truncated = {
            let rec = Record {
                content_type: ContentType::Handshake,
                legacy_version: record::LEGACY_RECORD_VERSION,
                fragment: vec![0x02, 0x00, 0x00, 0x20, 0x03, 0x03],
            };
            let mut out = Vec::new();
            rec.serialize_into(&mut out, RecordKind::Plaintext)
                .expect("serialize");
            out
        };
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("truncated", truncated),
            ("not TLS", b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec()),
            (
                "retry request",
                plaintext_handshake(&server_hello_raw(
                    client_hello::HELLO_RETRY_REQUEST_RANDOM,
                    ok_cipher,
                    false,
                    &public,
                )),
            ),
            (
                "unknown cipher suite",
                plaintext_handshake(&server_hello_raw([1u8; 32], 0x1302, false, &public)),
            ),
            (
                "unexpected extension",
                plaintext_handshake(&server_hello_raw([1u8; 32], ok_cipher, true, &public)),
            ),
        ];
        for (name, bytes) in cases {
            let err = run(&pin, hello_then(bytes)).expect_err(name);
            assert_eq!(err, ClientHandshakeError::Protocol, "{name}");
            assert!(err.is_rejection(), "{name}");
        }
    }

    #[test]
    fn a_peer_that_closes_before_answering_is_a_transient_io_failure() {
        let pin = pin_of(good_cert("160801121924Z"));
        let err = run(&pin, |mut conn| {
            let _ = read_client_hello(&mut conn);
        })
        .expect_err("closed");
        assert_eq!(err, ClientHandshakeError::Io);
        assert!(!err.is_rejection());
    }

    #[test]
    fn a_certificate_other_than_the_pinned_one_is_rejected_before_anything_else() {
        let pin = pin_of(good_cert("160801121924Z"));
        let other = good_cert("170801121924Z");
        let err = run(&pin, move |mut conn| {
            serve_flight(&mut conn, &other, Fault::None)
        })
        .expect_err("mismatch");
        assert_eq!(err, ClientHandshakeError::PinMismatch);
    }

    #[test]
    fn a_tampered_signature_or_finished_fails_verification() {
        let der = good_cert("160801121924Z");
        let pin = pin_of(der.clone());
        for (name, fault) in [
            ("signature", Fault::TamperCertificateVerify),
            ("finished", Fault::TamperFinished),
        ] {
            let presented = der.clone();
            let err = run(&pin, move |mut conn| {
                serve_flight(&mut conn, &presented, fault)
            })
            .expect_err(name);
            assert_eq!(err, ClientHandshakeError::VerificationFailed, "{name}");
        }
    }

    #[test]
    fn errors_use_fixed_texts() {
        for e in [
            ClientHandshakeError::Io,
            ClientHandshakeError::Local,
            ClientHandshakeError::PinMismatch,
            ClientHandshakeError::VerificationFailed,
            ClientHandshakeError::Protocol,
            ClientHandshakeError::PeerAlert,
        ] {
            let text = e.to_string();
            assert!(!text.is_empty() && !text.contains("0x"), "{text}");
        }
    }
}
