//! TLS 1.3 ハンドシェイクメッセージ（RFC 8446 §4）の最小集合の
//! parse／serialize と、レコード境界をまたぐメッセージの再組み立て。
//!
//! 対象は 6 種類（`ClientHello`・`ServerHello`・`EncryptedExtensions`・
//! `Certificate`・`CertificateVerify`・`Finished`）のみ。本モジュールが
//! 担うのは**構造（シンタックス）の検証だけ**であり、フィールドの値の
//! 意味は解釈しない（不透明なバイト列として保持する）。鍵・状態は
//! 一切持たない（`super::record` と同じ純粋なコーデック方針）。
//!
//! `crate::handshake`（PostgreSQL wire プロトコルの `StartupMessage`／
//! `SSLRequest` 処理。既存挙動は不変）とは全くの別物であり、混同しない
//! よう本モジュールは `crate::tls::handshake` の完全修飾で参照する運用とする。
//!
//! 呼び出し元・呼び出し先の予定（親 Issue #941・TASK-228・WIRE-9・HTTP-10
//! ポインタ。TASK-228・spec: `docs/spec/05-tasks.md`）:
//! - 拡張の意味解釈（`supported_versions`・`key_share`・
//!   `signature_algorithms` 等）・重複拡張の拒否・TLS 1.3 以外の拒否・
//!   `legacy_version`／compression 値の検査・HelloRetryRequest の意味論は
//!   #954 が担う。本モジュールは [`Extension`] を不透明な
//!   `(extension_type, extension_data)` の列としてのみ扱う
//! - `Certificate::certificate_list` の `cert_data`（X.509 DER）の解釈は
//!   `super::x509` が担い、`Certificate` メッセージそのものの組み立ても
//!   同モジュールの `ServerCertificateChain::certificate_message` が担う
//!   （Issue #963）
//! - `CertificateVerify` の署名生成・署名対象の構成は #961 が担う
//! - transcript hash・`Finished::verify_data` の生成と検証は
//!   [`super::transcript`]・[`super::finished`]（Issue #964）が担う。
//!   本モジュールは `Finished` の長さ（32 バイト固定。暗号スイートが
//!   `TLS_AES_128_GCM_SHA256` に固定のため）のみ検証する
//! - 状態機械・alert の実送出・「状態外メッセージ」判定・レコード種別
//!   （平文／暗号文）の選択は #965 が担う
//! - 接続経路への組み込みは本モジュール新設当時は未接続だったが、
//!   #966（pg wire）・#968（NoSQL 表層）で実装済み
//!
//! 定数時間についての整理: ハンドシェイクメッセージの各フィールド
//! （type・長さ・random・拡張・証明書・署名・verify_data）は通信路上に
//! 平文で流れる、または相手と共有される値であり、本モジュールでは秘密値
//! として扱わない。長さによる分岐は公開値にのみ依存する。`verify_data`
//! の比較は本モジュールでは行わない（[`super::finished`]・Issue #964 が
//! 定数時間比較を担う）。本
//! モジュールは中身をバイト比較・テーブル参照に用いない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`・
//! `checked_*`・配列パターン束縛・`try_into()` で処理する
//! （`.claude/rules/coding-rust.md` P0）。`unsafe` は使わない。

use super::record;

/// ハンドシェイクメッセージヘッダの固定長（`HandshakeType` 1 バイト +
/// 24bit 長）。
pub const HANDSHAKE_HEADER_LEN: usize = 4;

/// 受信時に [`HandshakeBuffer`] が再組み立てできるメッセージ本文長の上限。
///
/// サーバーが受け取るのはクライアントの `ClientHello` と `Finished` のみ
/// （クライアント証明書は本リポの対象外）であり、いずれも小さい。
/// 一次資料（RFC 8446）はこの受信側上限を規定していないため、本リポ独自の
/// 実装既定値として 65535（u16 の最大値）を採用する。
pub const MAX_HANDSHAKE_MESSAGE_LEN: usize = 0xFFFF;

/// 送信するハンドシェイクメッセージ本文長の上限（24bit 長フィールドの
/// 最大値）。サーバーが送る `Certificate` チェーンは [`MAX_HANDSHAKE_MESSAGE_LEN`]
/// を超え得るため、受信上限とは別に定義する。
pub const MAX_HANDSHAKE_WIRE_BODY_LEN: u32 = 0xFF_FFFF;

/// `Finished::verify_data` の固定長。暗号スイートが `TLS_AES_128_GCM_SHA256`
/// （ハッシュ長 32 バイト）に固定されているための本リポの実装既定値。
pub const FINISHED_VERIFY_DATA_LEN: usize = 32;

/// [`HandshakeBuffer`] の内部バッファが取り得る最大バイト数（fail-closed な
/// 有界バッファ）。ヘッダ + 受信上限本文 + レコード平文 1 個分の余裕を
/// 確保し、無制限には伸びない。
pub const MAX_HANDSHAKE_BUFFER_LEN: usize =
    HANDSHAKE_HEADER_LEN + MAX_HANDSHAKE_MESSAGE_LEN + record::MAX_PLAINTEXT_LEN;

const _: () = assert!(
    MAX_HANDSHAKE_MESSAGE_LEN as u32 <= MAX_HANDSHAKE_WIRE_BODY_LEN,
    "receive limit must fit within the 24-bit wire length field"
);

/// ハンドシェイクメッセージの `HandshakeType`（RFC 8446 §4）のうち、
/// 本モジュールが扱う最小集合 6 値のみを閉じた語彙として受理する。
///
/// それ以外の値（`new_session_ticket`(4)・`end_of_early_data`(5)・
/// `certificate_request`(13)・`key_update`(24)・`message_hash`(254) 等）は
/// [`HandshakeError::UnexpectedType`] として fail-closed に拒否する
/// （どれも本リポの最小サーバー実装では送受信しない種別のため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeType {
    ClientHello,
    ServerHello,
    EncryptedExtensions,
    Certificate,
    CertificateVerify,
    Finished,
}

impl HandshakeType {
    pub fn as_u8(self) -> u8 {
        match self {
            HandshakeType::ClientHello => 1,
            HandshakeType::ServerHello => 2,
            HandshakeType::EncryptedExtensions => 8,
            HandshakeType::Certificate => 11,
            HandshakeType::CertificateVerify => 15,
            HandshakeType::Finished => 20,
        }
    }
}

impl TryFrom<u8> for HandshakeType {
    /// 受理できなかった生バイト値をそのまま保持する（`record::ContentType`
    /// と同じ流儀）。
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            1 => Ok(HandshakeType::ClientHello),
            2 => Ok(HandshakeType::ServerHello),
            8 => Ok(HandshakeType::EncryptedExtensions),
            11 => Ok(HandshakeType::Certificate),
            15 => Ok(HandshakeType::CertificateVerify),
            20 => Ok(HandshakeType::Finished),
            other => Err(other),
        }
    }
}

/// 本モジュールで検出しうるエラー全体。`Copy` であり、[`HandshakeBuffer`]
/// の poison 状態としてもそのまま保持する。
///
/// `wire_code`（ERR-1／ERR-2）への写像は追加しない。TLS 層の失敗は
/// `ErrorResponse` ではなく TLS alert と切断で表すため、既存のエラー契約
/// （ERR-1/2/4）はこの型の追加によって変わらない。alert の実送出は #965 の担当。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// 閉じた語彙 6 値のいずれでもない `HandshakeType`（unexpected_message）。
    UnexpectedType(u8),
    /// ハンドシェイク以外のレコードが [`HandshakeBuffer::feed_record`] へ
    /// 渡された（unexpected_message。RFC 8446 §5.1 はハンドシェイク
    /// メッセージを他のレコード種別と混ぜて送ることを許さない）。
    UnexpectedContentType(record::ContentType),
    /// 構造上の違反（decode_error）。理由は固定の英語文字列にし、
    /// 受信バイト列そのものは含めない。
    Decode(&'static str),
    /// 宣言長が受信上限を超過（decode_error）。本文を確保する前に
    /// ヘッダ単体の情報だけで判定する。
    MessageTooLarge { declared: usize, max: usize },
    /// EOF 時に途中のバイト列が残っていた（応答せず切断してよい）。
    Truncated,
    /// serialize 側の範囲外入力（送信側のバグ扱い）。
    Encode(&'static str),
}

impl HandshakeError {
    /// クライアントへ返すべき TLS alert の種別。`None` は応答せず切断する
    /// 種別（`Truncated`・`Encode`）を表す。
    pub fn alert_description(&self) -> Option<record::AlertDescription> {
        match self {
            HandshakeError::UnexpectedType(_) | HandshakeError::UnexpectedContentType(_) => {
                Some(record::AlertDescription::UnexpectedMessage)
            }
            HandshakeError::Decode(_) | HandshakeError::MessageTooLarge { .. } => {
                Some(record::AlertDescription::DecodeError)
            }
            HandshakeError::Truncated | HandshakeError::Encode(_) => None,
        }
    }
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandshakeError::UnexpectedType(b) => {
                write!(f, "unexpected TLS handshake message type {b}")
            }
            HandshakeError::UnexpectedContentType(ct) => {
                write!(f, "non-handshake record fed to handshake buffer: {ct:?}")
            }
            HandshakeError::Decode(reason) => write!(f, "TLS handshake decode error: {reason}"),
            HandshakeError::MessageTooLarge { declared, max } => {
                write!(
                    f,
                    "TLS handshake message length {declared} exceeds limit {max}"
                )
            }
            HandshakeError::Truncated => write!(f, "truncated TLS handshake message"),
            HandshakeError::Encode(reason) => write!(f, "TLS handshake encode error: {reason}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// バイト列を先頭から消費していく読み取りカーソル。すべてのメソッドは
/// 未検証の残りバイト数を確認してから切り出すため、`unwrap`／`expect`／
/// 添字アクセスなしに fail-closed へ倒れる。
///
/// `pub(super)` として `super::client_hello`（#954）からも共有する。
/// ClientHello 本体の parse（本モジュール）と拡張の意味解析（#954）が
/// 同じ fail-closed カーソル実装を再利用するための契約であり、
/// parse／serialize の意味論・既存テストはこの可視性変更で変わらない。
pub(super) struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    pub(super) fn remaining(&self) -> usize {
        self.rest.len()
    }

    pub(super) fn bytes(&mut self, n: usize) -> Result<&'a [u8], HandshakeError> {
        let (head, tail) = self.rest.split_at_checked(n).ok_or(HandshakeError::Decode(
            "unexpected end of handshake message",
        ))?;
        self.rest = tail;
        Ok(head)
    }

    /// 固定長 `N` バイトを読み、配列へ変換する。`bytes(N)` が返す
    /// スライスは常にちょうど `N` バイトのため `try_into` は必ず成功するが、
    /// 添字アクセスを避けるため `?` で明示的に処理する。
    pub(super) fn array<const N: usize>(&mut self) -> Result<[u8; N], HandshakeError> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| HandshakeError::Decode("internal fixed-length read mismatch"))
    }

    pub(super) fn u8(&mut self) -> Result<u8, HandshakeError> {
        let [b] = self.array::<1>()?;
        Ok(b)
    }

    pub(super) fn u16(&mut self) -> Result<u16, HandshakeError> {
        Ok(u16::from_be_bytes(self.array::<2>()?))
    }

    fn u24(&mut self) -> Result<u32, HandshakeError> {
        let [a, b, c] = self.array::<3>()?;
        Ok(u32::from_be_bytes([0, a, b, c]))
    }

    /// 8bit 長接頭辞のベクタを読み、`[min, max]`（バイト数）に収まって
    /// いることを確認してから本体を切り出す。
    pub(super) fn vec_u8_len(
        &mut self,
        min: usize,
        max: usize,
    ) -> Result<&'a [u8], HandshakeError> {
        let len = usize::from(self.u8()?);
        if len < min || len > max {
            return Err(HandshakeError::Decode("8-bit vector length out of range"));
        }
        self.bytes(len)
    }

    pub(super) fn vec_u16_len(
        &mut self,
        min: usize,
        max: usize,
    ) -> Result<&'a [u8], HandshakeError> {
        let len = usize::from(self.u16()?);
        if len < min || len > max {
            return Err(HandshakeError::Decode("16-bit vector length out of range"));
        }
        self.bytes(len)
    }

    fn vec_u24_len(&mut self, min: usize, max: usize) -> Result<&'a [u8], HandshakeError> {
        let len = self.u24()? as usize;
        if len < min || len > max {
            return Err(HandshakeError::Decode("24-bit vector length out of range"));
        }
        self.bytes(len)
    }

    /// 末尾まで読み切ったことを確認する（余りバイトを許さない）。
    pub(super) fn expect_end(&self) -> Result<(), HandshakeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(HandshakeError::Decode(
                "trailing bytes after handshake message body",
            ))
        }
    }
}

fn put_u24(out: &mut Vec<u8>, value: u32) -> Result<(), HandshakeError> {
    if value > MAX_HANDSHAKE_WIRE_BODY_LEN {
        return Err(HandshakeError::Encode("24-bit length field overflow"));
    }
    let b = value.to_be_bytes();
    out.extend_from_slice(&b[1..]);
    Ok(())
}

fn put_vec_u8(
    out: &mut Vec<u8>,
    data: &[u8],
    min: usize,
    max: usize,
) -> Result<(), HandshakeError> {
    if data.len() < min || data.len() > max {
        return Err(HandshakeError::Encode("8-bit vector length out of range"));
    }
    let len = u8::try_from(data.len())
        .map_err(|_| HandshakeError::Encode("8-bit vector length exceeds field width"))?;
    out.push(len);
    out.extend_from_slice(data);
    Ok(())
}

fn put_vec_u16(
    out: &mut Vec<u8>,
    data: &[u8],
    min: usize,
    max: usize,
) -> Result<(), HandshakeError> {
    if data.len() < min || data.len() > max {
        return Err(HandshakeError::Encode("16-bit vector length out of range"));
    }
    let len = u16::try_from(data.len())
        .map_err(|_| HandshakeError::Encode("16-bit vector length exceeds field width"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(data);
    Ok(())
}

fn put_vec_u24(
    out: &mut Vec<u8>,
    data: &[u8],
    min: usize,
    max: usize,
) -> Result<(), HandshakeError> {
    if data.len() < min || data.len() > max {
        return Err(HandshakeError::Encode("24-bit vector length out of range"));
    }
    let len = u32::try_from(data.len())
        .map_err(|_| HandshakeError::Encode("24-bit vector length exceeds field width"))?;
    put_u24(out, len)?;
    out.extend_from_slice(data);
    Ok(())
}

/// 拡張 1 件（`struct { ExtensionType extension_type; opaque
/// extension_data<0..2^16-1>; }`）。中身は不透明なまま保持し、意味解釈は
/// #954 が担う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub extension_type: u16,
    pub extension_data: Vec<u8>,
}

/// 拡張列の中身（`extensions<...>` フィールドが指す本文）を、宣言長
/// ちょうど使い切るまで走査する。1 件でも途中で切れていれば `Decode`。
fn parse_extension_list(body: &[u8]) -> Result<Vec<Extension>, HandshakeError> {
    let mut r = Reader::new(body);
    let mut extensions = Vec::new();
    while r.remaining() > 0 {
        let extension_type = r.u16()?;
        let extension_data = r.vec_u16_len(0, 0xFFFF)?.to_vec();
        extensions.push(Extension {
            extension_type,
            extension_data,
        });
    }
    Ok(extensions)
}

fn encode_extension_list(extensions: &[Extension]) -> Result<Vec<u8>, HandshakeError> {
    let mut body = Vec::new();
    for ext in extensions {
        body.extend_from_slice(&ext.extension_type.to_be_bytes());
        put_vec_u16(&mut body, &ext.extension_data, 0, 0xFFFF)?;
    }
    Ok(body)
}

/// `ClientHello`（RFC 8446 §4.1.2）。`legacy_version`・
/// `legacy_compression_methods` の値の意味検査は行わない（#954 の担当。
/// `super::record::RecordHeader` が `legacy_record_version` を無視する
/// 方針と同じ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub legacy_version: u16,
    pub random: [u8; 32],
    pub legacy_session_id: Vec<u8>,
    pub cipher_suites: Vec<u16>,
    pub legacy_compression_methods: Vec<u8>,
    /// RFC 8446 §4.1.2: TLS 1.2 以前互換の `ClientHello` は拡張フィールドを
    /// 一切持たないことがある。その場合は空リストとして受理する（本体の
    /// `parse` を参照）。`supported_versions` 拡張が無いことを理由に
    /// `protocol_version` alert で拒否する判定は #954 が担う。
    pub extensions: Vec<Extension>,
}

impl ClientHello {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(body);
        let legacy_version = r.u16()?;
        let random = r.array::<32>()?;
        let legacy_session_id = r.vec_u8_len(0, 32)?.to_vec();
        let cipher_suites_raw = r.vec_u16_len(2, 0xFFFE)?;
        if cipher_suites_raw.len() % 2 != 0 {
            return Err(HandshakeError::Decode(
                "cipher_suites length must be a multiple of 2",
            ));
        }
        // 直前で長さが 2 の倍数であることを確認済みのため `as_chunks::<2>()`
        // の remainder は常に空になる。
        let cipher_suites = cipher_suites_raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes(*c))
            .collect();
        let legacy_compression_methods = r.vec_u8_len(1, 255)?.to_vec();
        // 拡張フィールドが丸ごと省略された TLS 1.2 以前互換の ClientHello を
        // 空リストとして受理する（この後 #954 が拒否するかどうかを判定する）。
        let extensions = if r.remaining() == 0 {
            Vec::new()
        } else {
            let ext_body = r.vec_u16_len(0, 0xFFFF)?;
            parse_extension_list(ext_body)?
        };
        r.expect_end()?;
        Ok(ClientHello {
            legacy_version,
            random,
            legacy_session_id,
            cipher_suites,
            legacy_compression_methods,
            extensions,
        })
    }

    /// 拡張が空でも常に `extensions<0..2^16-1>` の長さ接頭辞を書く
    /// （TLS 1.2 以前互換の「拡張省略」形は書き出さない）。サーバーは
    /// `ClientHello` を送信しないため、この非対称は実運用上問題にならない。
    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        out.extend_from_slice(&self.legacy_version.to_be_bytes());
        out.extend_from_slice(&self.random);
        put_vec_u8(out, &self.legacy_session_id, 0, 32)?;
        let mut cipher_suites_bytes = Vec::with_capacity(self.cipher_suites.len() * 2);
        for cs in &self.cipher_suites {
            cipher_suites_bytes.extend_from_slice(&cs.to_be_bytes());
        }
        put_vec_u16(out, &cipher_suites_bytes, 2, 0xFFFE)?;
        put_vec_u8(out, &self.legacy_compression_methods, 1, 255)?;
        let ext_body = encode_extension_list(&self.extensions)?;
        put_vec_u16(out, &ext_body, 0, 0xFFFF)?;
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::ClientHello, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// `ServerHello`（RFC 8446 §4.1.3）。拡張リストは最低 6 バイト
/// （`supported_versions` 相当）を要する構造上の下限のみ検証する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub legacy_version: u16,
    pub random: [u8; 32],
    pub legacy_session_id_echo: Vec<u8>,
    pub cipher_suite: u16,
    pub legacy_compression_method: u8,
    pub extensions: Vec<Extension>,
}

impl ServerHello {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(body);
        let legacy_version = r.u16()?;
        let random = r.array::<32>()?;
        let legacy_session_id_echo = r.vec_u8_len(0, 32)?.to_vec();
        let cipher_suite = r.u16()?;
        let legacy_compression_method = r.u8()?;
        let ext_body = r.vec_u16_len(6, 0xFFFF)?;
        let extensions = parse_extension_list(ext_body)?;
        r.expect_end()?;
        Ok(ServerHello {
            legacy_version,
            random,
            legacy_session_id_echo,
            cipher_suite,
            legacy_compression_method,
            extensions,
        })
    }

    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        out.extend_from_slice(&self.legacy_version.to_be_bytes());
        out.extend_from_slice(&self.random);
        put_vec_u8(out, &self.legacy_session_id_echo, 0, 32)?;
        out.extend_from_slice(&self.cipher_suite.to_be_bytes());
        out.push(self.legacy_compression_method);
        let ext_body = encode_extension_list(&self.extensions)?;
        put_vec_u16(out, &ext_body, 6, 0xFFFF)?;
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::ServerHello, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// `EncryptedExtensions`（RFC 8446 §4.3.1）。拡張リストのみを本文に持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedExtensions {
    pub extensions: Vec<Extension>,
}

impl EncryptedExtensions {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(body);
        let ext_body = r.vec_u16_len(0, 0xFFFF)?;
        let extensions = parse_extension_list(ext_body)?;
        r.expect_end()?;
        Ok(EncryptedExtensions { extensions })
    }

    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        let ext_body = encode_extension_list(&self.extensions)?;
        put_vec_u16(out, &ext_body, 0, 0xFFFF)?;
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::EncryptedExtensions, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// `Certificate` の 1 エントリ（RFC 8446 §4.4.2）。`cert_data`（X.509 DER）は
/// 不透明なバイト列のまま保持する（解釈・組み立ては `super::x509` が
/// 担う。Issue #963）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateEntry {
    pub cert_data: Vec<u8>,
    pub extensions: Vec<Extension>,
}

/// `Certificate`（RFC 8446 §4.4.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub certificate_request_context: Vec<u8>,
    pub certificate_list: Vec<CertificateEntry>,
}

fn parse_certificate_list(body: &[u8]) -> Result<Vec<CertificateEntry>, HandshakeError> {
    let mut r = Reader::new(body);
    let mut list = Vec::new();
    while r.remaining() > 0 {
        let cert_data = r.vec_u24_len(1, 0xFF_FFFF)?.to_vec();
        let ext_body = r.vec_u16_len(0, 0xFFFF)?;
        let extensions = parse_extension_list(ext_body)?;
        list.push(CertificateEntry {
            cert_data,
            extensions,
        });
    }
    Ok(list)
}

fn encode_certificate_list(list: &[CertificateEntry]) -> Result<Vec<u8>, HandshakeError> {
    let mut body = Vec::new();
    for entry in list {
        put_vec_u24(&mut body, &entry.cert_data, 1, 0xFF_FFFF)?;
        let ext_body = encode_extension_list(&entry.extensions)?;
        put_vec_u16(&mut body, &ext_body, 0, 0xFFFF)?;
    }
    Ok(body)
}

impl Certificate {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(body);
        let certificate_request_context = r.vec_u8_len(0, 255)?.to_vec();
        let list_body = r.vec_u24_len(0, 0xFF_FFFF)?;
        let certificate_list = parse_certificate_list(list_body)?;
        r.expect_end()?;
        Ok(Certificate {
            certificate_request_context,
            certificate_list,
        })
    }

    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        put_vec_u8(out, &self.certificate_request_context, 0, 255)?;
        let list_body = encode_certificate_list(&self.certificate_list)?;
        put_vec_u24(out, &list_body, 0, 0xFF_FFFF)?;
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::Certificate, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// `CertificateVerify`（RFC 8446 §4.4.3）。署名の生成・署名対象の構成は
/// #961 が担う。本モジュールは `algorithm`／`signature` を不透明に保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateVerify {
    pub algorithm: u16,
    pub signature: Vec<u8>,
}

impl CertificateVerify {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        let mut r = Reader::new(body);
        let algorithm = r.u16()?;
        let signature = r.vec_u16_len(0, 0xFFFF)?.to_vec();
        r.expect_end()?;
        Ok(CertificateVerify {
            algorithm,
            signature,
        })
    }

    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        out.extend_from_slice(&self.algorithm.to_be_bytes());
        put_vec_u16(out, &self.signature, 0, 0xFFFF)?;
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::CertificateVerify, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// `Finished`（RFC 8446 §4.4.4）。本文は内側に長さ接頭辞を持たず、
/// 残り全体が `verify_data`。暗号スイートが `TLS_AES_128_GCM_SHA256` に
/// 固定されているため長さはちょうど [`FINISHED_VERIFY_DATA_LEN`]
/// （32 バイト）でなければならない。値そのものの検証（transcript hash
/// との比較）は [`super::finished`]（Issue #964）が担う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub verify_data: Vec<u8>,
}

impl Finished {
    pub fn parse(body: &[u8]) -> Result<Self, HandshakeError> {
        if body.len() != FINISHED_VERIFY_DATA_LEN {
            return Err(HandshakeError::Decode(
                "Finished.verify_data must be exactly 32 bytes",
            ));
        }
        Ok(Finished {
            verify_data: body.to_vec(),
        })
    }

    pub fn serialize_body_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        if self.verify_data.len() != FINISHED_VERIFY_DATA_LEN {
            return Err(HandshakeError::Encode(
                "Finished.verify_data must be exactly 32 bytes",
            ));
        }
        out.extend_from_slice(&self.verify_data);
        Ok(())
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        encode_message(out, HandshakeType::Finished, |body| {
            self.serialize_body_into(body)
        })
    }
}

/// ヘッダ（type + u24 長）と本文を組み立てて `out` へ追記する共通ヘルパー。
/// `body_fn` が本文へ書き込んだ後の長さを検査してから 4 バイトヘッダを書く
/// （宣言長と実際の本文長が食い違うことがない設計）。
fn encode_message(
    out: &mut Vec<u8>,
    msg_type: HandshakeType,
    body_fn: impl FnOnce(&mut Vec<u8>) -> Result<(), HandshakeError>,
) -> Result<(), HandshakeError> {
    let mut body = Vec::new();
    body_fn(&mut body)?;
    let len =
        u32::try_from(body.len()).map_err(|_| HandshakeError::Encode("body length overflow"))?;
    // 長さ検証を `out` への書き込み前に行う（all-or-nothing 契約。#953 指摘）。
    // `put_u24` の失敗時に type バイトだけが `out` に残ることを防ぐ。
    if len > MAX_HANDSHAKE_WIRE_BODY_LEN {
        return Err(HandshakeError::Encode("24-bit length field overflow"));
    }
    out.push(msg_type.as_u8());
    put_u24(out, len)?;
    out.extend_from_slice(&body);
    Ok(())
}

/// parse 済みの 1 メッセージ（型付き解釈前）。受信した生バイト列を
/// 完全に復元できることが要件（ヘッダは `(msg_type, body.len())` から
/// 一意に決まる）。[`super::transcript`]（Issue #964）の transcript hash は
/// 受信した生バイト列を
/// そのまま入力にする必要があるため、この復元可能性を崩さない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHandshake {
    pub msg_type: HandshakeType,
    pub body: Vec<u8>,
}

impl RawHandshake {
    /// 4 バイトのハンドシェイクヘッダ（`msg_type` + u24 本文長）だけを
    /// 組み立てる。本文をコピーせずにヘッダと本文を別々に扱いたい呼び出し元
    /// （[`super::transcript::Transcript`] がヘッダ・本文を順に SHA-256 へ
    /// 投入する経路。PR #1033 レビュー指摘）と [`RawHandshake::encode_into`]
    /// が共有する、ヘッダ構成の単一情報源。本文長が 24 ビットに収まらない
    /// 場合は `Err`。
    pub fn header(&self) -> Result<[u8; HANDSHAKE_HEADER_LEN], HandshakeError> {
        let len = u32::try_from(self.body.len())
            .map_err(|_| HandshakeError::Encode("body length overflow"))?;
        if len > MAX_HANDSHAKE_WIRE_BODY_LEN {
            return Err(HandshakeError::Encode("24-bit length field overflow"));
        }
        // 上の検査で上位バイトは 0 が保証される。添字アクセスを避けるため
        // パターンで分解する。
        let [_, b1, b2, b3] = len.to_be_bytes();
        Ok([self.msg_type.as_u8(), b1, b2, b3])
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        // encode_message と同じ all-or-nothing 契約: 検証（`header` 内）を
        // 書き込み前に行う（#953 指摘）。
        let header = self.header()?;
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.body);
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, HandshakeError> {
        let mut out = Vec::new();
        self.encode_into(&mut out)?;
        Ok(out)
    }
}

/// 型付きハンドシェイクメッセージのまとめ（最小集合 6 種）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeMessage {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    EncryptedExtensions(EncryptedExtensions),
    Certificate(Certificate),
    CertificateVerify(CertificateVerify),
    Finished(Finished),
}

impl HandshakeMessage {
    pub fn parse(raw: &RawHandshake) -> Result<Self, HandshakeError> {
        match raw.msg_type {
            HandshakeType::ClientHello => Ok(HandshakeMessage::ClientHello(ClientHello::parse(
                &raw.body,
            )?)),
            HandshakeType::ServerHello => Ok(HandshakeMessage::ServerHello(ServerHello::parse(
                &raw.body,
            )?)),
            HandshakeType::EncryptedExtensions => Ok(HandshakeMessage::EncryptedExtensions(
                EncryptedExtensions::parse(&raw.body)?,
            )),
            HandshakeType::Certificate => Ok(HandshakeMessage::Certificate(Certificate::parse(
                &raw.body,
            )?)),
            HandshakeType::CertificateVerify => Ok(HandshakeMessage::CertificateVerify(
                CertificateVerify::parse(&raw.body)?,
            )),
            HandshakeType::Finished => Ok(HandshakeMessage::Finished(Finished::parse(&raw.body)?)),
        }
    }

    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), HandshakeError> {
        match self {
            HandshakeMessage::ClientHello(m) => m.encode_into(out),
            HandshakeMessage::ServerHello(m) => m.encode_into(out),
            HandshakeMessage::EncryptedExtensions(m) => m.encode_into(out),
            HandshakeMessage::Certificate(m) => m.encode_into(out),
            HandshakeMessage::CertificateVerify(m) => m.encode_into(out),
            HandshakeMessage::Finished(m) => m.encode_into(out),
        }
    }
}

/// 受信データを蓄積し、そろったハンドシェイクメッセージを 1 つずつ
/// 取り出す push 型バッファ（`super::record::RecordBuffer` と同じ流儀）。
///
/// 1 つの fragment から複数メッセージが取れる場合と、1 メッセージが複数
/// レコードにまたがる場合の双方を、同じ手続き（[`HandshakeBuffer::feed`]
/// で投入 → [`HandshakeBuffer::next_message`] を `Ok(None)` になるまで
/// 繰り返す）で扱う。
///
/// 内部バッファは [`MAX_HANDSHAKE_BUFFER_LEN`] を超えて伸びない
/// （fail-closed な有界バッファ）。
pub struct HandshakeBuffer {
    buf: Vec<u8>,
    /// 一度 [`HandshakeBuffer::next_message`] 等がエラーを返したら、
    /// 以後の呼び出しは同じ理由のエラーを返し続ける（poison。エラー後に
    /// バッファの残骸を引き続き解釈しない fail-closed な設計）。
    poison: Option<HandshakeError>,
}

impl Default for HandshakeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl HandshakeBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            poison: None,
        }
    }

    /// レコード層から 1 レコードを受け取る。`ContentType::Handshake`
    /// 以外は RFC 8446 §5.1 違反として `unexpected_message` で拒否する
    /// （ハンドシェイクメッセージを他のレコード種別と混ぜて送ることは
    /// 許されない）。
    pub fn feed_record(&mut self, record: &record::Record) -> Result<(), HandshakeError> {
        if let Some(poison) = self.poison {
            return Err(poison);
        }
        if record.content_type != record::ContentType::Handshake {
            let err = HandshakeError::UnexpectedContentType(record.content_type);
            self.poison = Some(err);
            return Err(err);
        }
        self.feed(&record.fragment)
    }

    /// `input` を内部バッファへ追記する。[`MAX_HANDSHAKE_BUFFER_LEN`] を
    /// 超える投入は、呼び出し元が [`HandshakeBuffer::next_message`] で
    /// バッファを空けずに投入を続けたことを意味するため、poison として
    /// 拒否する（`record::RecordBuffer::feed` の「取り込み切れなかった分は
    /// 呼び出し元が保持する」方式とは異なり、本バッファは 1 レコード
    /// fragment 全体を原子的に受理するか拒否するかの二択とする。1
    /// メッセージが複数レコードにまたがる正常系では、`feed` のたびに
    /// バッファは単調に伸びるが、`next_message` の呼び出しで完成した
    /// メッセージ分だけ都度縮むため、通常の利用では上限に達しない）。
    pub fn feed(&mut self, input: &[u8]) -> Result<(), HandshakeError> {
        if let Some(poison) = self.poison {
            return Err(poison);
        }
        if self.buf.len() + input.len() > MAX_HANDSHAKE_BUFFER_LEN {
            let err = HandshakeError::Decode("handshake reassembly buffer capacity exceeded");
            self.poison = Some(err);
            return Err(err);
        }
        self.buf.extend_from_slice(input);
        Ok(())
    }

    /// 途中まで届いているメッセージ（未完成の断片）が残っているかを返す。
    /// #965 が「鍵切替の直前はレコード境界にそろっていなければならない」
    /// 等の判定に使うフック。
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// バッファから 1 メッセージを取り出す。
    ///
    /// - ヘッダ 4 バイトに満たない、またはヘッダはそろったが本文が
    ///   そろっていない場合は `Ok(None)`
    /// - ヘッダが検証済みの時点で未対応の `HandshakeType`・受信上限超過の
    ///   いずれかであれば、本文がそろうのを待たずに即 `Err`
    /// - 一度 `Err` を返したら、以後は poison 状態として同じ理由の `Err`
    ///   を返し続ける
    pub fn next_message(&mut self) -> Result<Option<RawHandshake>, HandshakeError> {
        if let Some(poison) = self.poison {
            return Err(poison);
        }
        if self.buf.len() < HANDSHAKE_HEADER_LEN {
            return Ok(None);
        }
        let header: [u8; HANDSHAKE_HEADER_LEN] = match self
            .buf
            .get(..HANDSHAKE_HEADER_LEN)
            .and_then(|s| s.try_into().ok())
        {
            Some(arr) => arr,
            None => return Ok(None),
        };
        let [type_byte, l0, l1, l2] = header;
        let msg_type = match HandshakeType::try_from(type_byte) {
            Ok(t) => t,
            Err(b) => {
                let err = HandshakeError::UnexpectedType(b);
                self.poison = Some(err);
                return Err(err);
            }
        };
        let declared = (usize::from(l0) << 16) | (usize::from(l1) << 8) | usize::from(l2);
        if declared > MAX_HANDSHAKE_MESSAGE_LEN {
            let err = HandshakeError::MessageTooLarge {
                declared,
                max: MAX_HANDSHAKE_MESSAGE_LEN,
            };
            self.poison = Some(err);
            return Err(err);
        }
        let total = HANDSHAKE_HEADER_LEN + declared;
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = match self.buf.get(HANDSHAKE_HEADER_LEN..total) {
            Some(s) => s.to_vec(),
            None => return Ok(None),
        };
        self.buf.drain(..total);
        Ok(Some(RawHandshake { msg_type, body }))
    }

    /// 接続の EOF で呼ぶ。バッファに部分的なバイト列が残っていれば
    /// `Truncated`、何も残っていなければ `Ok(())`。
    pub fn finish(&self) -> Result<(), HandshakeError> {
        if let Some(poison) = self.poison {
            return Err(poison);
        }
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(HandshakeError::Truncated)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- RFC 8448 §3「Simple 1-RTT Handshake」ベクタ ----
    //
    // 以下はいずれも IETF の公開文書（RFC 8448）由来の値であり、
    // `docs/spec`（private）の内容ではない（`super::record` のテストと
    // 同じ方針）。

    /// 空白・改行を無視して 16 進文字列をバイト列へ変換する（テスト専用の
    /// 補助関数。受信データ経路ではないため `expect` を使ってよい）。
    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        digits
            .chunks(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("ascii hex pair");
                u8::from_str_radix(text, 16).expect("valid hex byte")
            })
            .collect()
    }

    fn client_hello_message() -> Vec<u8> {
        hex(
            "01 00 00 c0 03 03 cb 34 ec b1 e7 81 63 ba 1c 38 c6 da cb 19 6a 6d ff a2 1a 8d 99 12 \
             ec 18 a2 ef 62 83 02 4d ec e7 00 00 06 13 01 13 03 13 02 01 00 00 91 00 00 00 0b 00 \
             09 00 00 06 73 65 72 76 65 72 ff 01 00 01 00 00 0a 00 14 00 12 00 1d 00 17 00 18 00 \
             19 01 00 01 01 01 02 01 03 01 04 00 23 00 00 00 33 00 26 00 24 00 1d 00 20 99 38 1d \
             e5 60 e4 bd 43 d2 3d 8e 43 5a 7d ba fe b3 c0 6e 51 c1 3c ae 4d 54 13 69 1e 52 9a af \
             2c 00 2b 00 03 02 03 04 00 0d 00 20 00 1e 04 03 05 03 06 03 02 03 08 04 08 05 08 06 \
             04 01 05 01 06 01 02 01 04 02 05 02 06 02 02 02 00 2d 00 02 01 01 00 1c 00 02 40 01",
        )
    }

    fn server_hello_message() -> Vec<u8> {
        hex(
            "02 00 00 56 03 03 a6 af 06 a4 12 18 60 dc 5e 6e 60 24 9c d3 4c 95 93 0c 8a c5 cb 14 \
             34 da c1 55 77 2e d3 e2 69 28 00 13 01 00 00 2e 00 33 00 24 00 1d 00 20 c9 82 88 76 \
             11 20 95 fe 66 76 2b db f7 c6 72 e1 56 d6 cc 25 3b 83 3d f1 dd 69 b1 b0 4e 75 1f 0f \
             00 2b 00 02 03 04",
        )
    }

    fn encrypted_extensions_message() -> Vec<u8> {
        hex(
            "08 00 00 24 00 22 00 0a 00 14 00 12 00 1d 00 17 00 18 00 19 01 00 01 01 01 02 01 03 \
             01 04 00 1c 00 02 40 01 00 00 00 00",
        )
    }

    fn certificate_message() -> Vec<u8> {
        hex(
            "0b 00 01 b9 00 00 01 b5 00 01 b0 30 82 01 ac 30 82 01 15 a0 03 02 01 02 02 01 02 30 \
             0d 06 09 2a 86 48 86 f7 0d 01 01 0b 05 00 30 0e 31 0c 30 0a 06 03 55 04 03 13 03 72 \
             73 61 30 1e 17 0d 31 36 30 37 33 30 30 31 32 33 35 39 5a 17 0d 32 36 30 37 33 30 30 \
             31 32 33 35 39 5a 30 0e 31 0c 30 0a 06 03 55 04 03 13 03 72 73 61 30 81 9f 30 0d 06 \
             09 2a 86 48 86 f7 0d 01 01 01 05 00 03 81 8d 00 30 81 89 02 81 81 00 b4 bb 49 8f 82 \
             79 30 3d 98 08 36 39 9b 36 c6 98 8c 0c 68 de 55 e1 bd b8 26 d3 90 1a 24 61 ea fd 2d \
             e4 9a 91 d0 15 ab bc 9a 95 13 7a ce 6c 1a f1 9e aa 6a f9 8c 7c ed 43 12 09 98 e1 87 \
             a8 0e e0 cc b0 52 4b 1b 01 8c 3e 0b 63 26 4d 44 9a 6d 38 e2 2a 5f da 43 08 46 74 80 \
             30 53 0e f0 46 1c 8c a9 d9 ef bf ae 8e a6 d1 d0 3e 2b d1 93 ef f0 ab 9a 80 02 c4 74 \
             28 a6 d3 5a 8d 88 d7 9f 7f 1e 3f 02 03 01 00 01 a3 1a 30 18 30 09 06 03 55 1d 13 04 \
             02 30 00 30 0b 06 03 55 1d 0f 04 04 03 02 05 a0 30 0d 06 09 2a 86 48 86 f7 0d 01 01 \
             0b 05 00 03 81 81 00 85 aa d2 a0 e5 b9 27 6b 90 8c 65 f7 3a 72 67 17 06 18 a5 4c 5f \
             8a 7b 33 7d 2d f7 a5 94 36 54 17 f2 ea e8 f8 a5 8c 8f 81 72 f9 31 9c f3 6b 7f d6 c5 \
             5b 80 f2 1a 03 01 51 56 72 60 96 fd 33 5e 5e 67 f2 db f1 02 70 2e 60 8c ca e6 be c1 \
             fc 63 a4 2a 99 be 5c 3e b7 10 7c 3c 54 e9 b9 eb 2b d5 20 3b 1c 3b 84 e0 a8 b2 f7 59 \
             40 9b a3 ea c9 d9 1d 40 2d cc 0c c8 f8 96 12 29 ac 91 87 b4 2b 4d e1 00 00",
        )
    }

    fn certificate_verify_message() -> Vec<u8> {
        hex(
            "0f 00 00 84 08 04 00 80 5a 74 7c 5d 88 fa 9b d2 e5 5a b0 85 a6 10 15 b7 21 1f 82 4c \
             d4 84 14 5a b3 ff 52 f1 fd a8 47 7b 0b 7a bc 90 db 78 e2 d3 3a 5c 14 1a 07 86 53 fa \
             6b ef 78 0c 5e a2 48 ee aa a7 85 c4 f3 94 ca b6 d3 0b be 8d 48 59 ee 51 1f 60 29 57 \
             b1 54 11 ac 02 76 71 45 9e 46 44 5c 9e a5 8c 18 1e 81 8e 95 b8 c3 fb 0b f3 27 84 09 \
             d3 be 15 2a 3d a5 04 3e 06 3d da 65 cd f5 ae a2 0d 53 df ac d4 2f 74 f3",
        )
    }

    fn server_finished_message() -> Vec<u8> {
        hex(
            "14 00 00 20 9b 9b 14 1d 90 63 37 fb d2 cb dc e7 1d f4 de da 4a b4 2c 30 95 72 cb 7f \
             ff ee 54 54 b7 8f 07 18",
        )
    }

    fn client_finished_message() -> Vec<u8> {
        hex(
            "14 00 00 20 a8 ec 43 6d 67 76 34 ae 52 5a c1 fc eb e1 1a 03 9e c1 76 94 fa c6 e9 85 \
             27 b6 42 f2 ed d5 ce 61",
        )
    }

    /// RFC 8448 §3 のサーバー暗号化フライト payload（657 octets。
    /// `EncryptedExtensions`(40) + `Certificate`(445) + `CertificateVerify`(136)
    /// + `Finished`(36) = 657 の 4 メッセージを平文のまま連結したもの。
    ///
    /// レコード層の内側 content type バイトはこの payload には含まれない
    /// （`record.rs` の暗号化レコードとは別の、平文段階の値）。
    fn server_flight_payload() -> Vec<u8> {
        let mut combined = encrypted_extensions_message();
        combined.extend_from_slice(&certificate_message());
        combined.extend_from_slice(&certificate_verify_message());
        combined.extend_from_slice(&server_finished_message());
        combined
    }

    fn parse_one(buf: &[u8]) -> RawHandshake {
        let mut buffer = HandshakeBuffer::new();
        buffer.feed(buf).expect("feed must not error");
        buffer
            .next_message()
            .expect("must parse")
            .expect("must be Some")
    }

    // ---- 5.1 往復テスト ----

    #[test]
    fn client_hello_roundtrips() {
        let raw_bytes = client_hello_message();
        assert_eq!(raw_bytes.len(), 196);
        let raw = parse_one(&raw_bytes);
        assert_eq!(raw.msg_type, HandshakeType::ClientHello);
        assert_eq!(raw.body.len(), 0xc0);
        let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
        let HandshakeMessage::ClientHello(client_hello) = &message else {
            panic!("must be ClientHello");
        };
        assert_eq!(client_hello.legacy_version, 0x0303);
        assert_eq!(
            client_hello.cipher_suites,
            vec![0x1301, 0x1303, 0x1302],
            "cipher_suites must not pass through empty"
        );
        assert!(
            !client_hello.extensions.is_empty(),
            "extensions must not pass through empty"
        );
        let mut out = Vec::new();
        message.encode_into(&mut out).expect("must serialize");
        assert_eq!(out, raw_bytes);
    }

    #[test]
    fn server_hello_roundtrips() {
        let raw_bytes = server_hello_message();
        assert_eq!(raw_bytes.len(), 90);
        let raw = parse_one(&raw_bytes);
        assert_eq!(raw.msg_type, HandshakeType::ServerHello);
        let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
        let HandshakeMessage::ServerHello(server_hello) = &message else {
            panic!("must be ServerHello");
        };
        assert_eq!(server_hello.cipher_suite, 0x1301);
        assert_eq!(server_hello.extensions.len(), 2);
        let mut out = Vec::new();
        message.encode_into(&mut out).expect("must serialize");
        assert_eq!(out, raw_bytes);
    }

    #[test]
    fn encrypted_extensions_roundtrips() {
        let raw_bytes = encrypted_extensions_message();
        assert_eq!(raw_bytes.len(), 40);
        let raw = parse_one(&raw_bytes);
        assert_eq!(raw.msg_type, HandshakeType::EncryptedExtensions);
        let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
        let mut out = Vec::new();
        message.encode_into(&mut out).expect("must serialize");
        assert_eq!(out, raw_bytes);
    }

    #[test]
    fn certificate_roundtrips() {
        let raw_bytes = certificate_message();
        assert_eq!(raw_bytes.len(), 445);
        let raw = parse_one(&raw_bytes);
        assert_eq!(raw.msg_type, HandshakeType::Certificate);
        let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
        let HandshakeMessage::Certificate(certificate) = &message else {
            panic!("must be Certificate");
        };
        assert_eq!(certificate.certificate_list.len(), 1);
        assert!(!certificate.certificate_list[0].cert_data.is_empty());
        let mut out = Vec::new();
        message.encode_into(&mut out).expect("must serialize");
        assert_eq!(out, raw_bytes);
    }

    #[test]
    fn certificate_verify_roundtrips() {
        let raw_bytes = certificate_verify_message();
        assert_eq!(raw_bytes.len(), 136);
        let raw = parse_one(&raw_bytes);
        assert_eq!(raw.msg_type, HandshakeType::CertificateVerify);
        let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
        let HandshakeMessage::CertificateVerify(cv) = &message else {
            panic!("must be CertificateVerify");
        };
        assert_eq!(cv.algorithm, 0x0804);
        assert_eq!(cv.signature.len(), 128);
        let mut out = Vec::new();
        message.encode_into(&mut out).expect("must serialize");
        assert_eq!(out, raw_bytes);
    }

    #[test]
    fn finished_roundtrips() {
        for raw_bytes in [server_finished_message(), client_finished_message()] {
            assert_eq!(raw_bytes.len(), 36);
            let raw = parse_one(&raw_bytes);
            assert_eq!(raw.msg_type, HandshakeType::Finished);
            let message = HandshakeMessage::parse(&raw).expect("must parse typed message");
            let HandshakeMessage::Finished(finished) = &message else {
                panic!("must be Finished");
            };
            assert_eq!(finished.verify_data.len(), FINISHED_VERIFY_DATA_LEN);
            let mut out = Vec::new();
            message.encode_into(&mut out).expect("must serialize");
            assert_eq!(out, raw_bytes);
        }
    }

    // ---- 5.2 ベクタ境界・不足 ----

    fn header(msg_type: u8, len: u32) -> [u8; HANDSHAKE_HEADER_LEN] {
        let b = len.to_be_bytes();
        [msg_type, b[1], b[2], b[3]]
    }

    #[test]
    fn client_hello_session_id_33_bytes_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(33); // 宣言長 33（上限 32 を超える）
        body.extend_from_slice(&[0u8; 33]);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites
        body.push(1);
        body.push(0); // compression
        body.extend_from_slice(&[0x00, 0x00]); // extensions（空）
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_cipher_suites_zero_length_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0); // session_id 長 0
        body.extend_from_slice(&[0x00, 0x00]); // cipher_suites 長 0（最小 2 未満）
        body.push(1);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_cipher_suites_odd_length_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x03, 0x13, 0x01, 0x00]); // 奇数長 3
        body.push(1);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_compression_zero_length_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        body.push(0); // compression 長 0（最小 1 未満）
        body.extend_from_slice(&[0x00, 0x00]);
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_extension_body_overrun_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        body.push(1);
        body.push(0);
        // 拡張リスト長 4 だが、内部の extension_data 長が宣言長をはみ出す。
        body.extend_from_slice(&[0x00, 0x04, 0x00, 0x00, 0x00, 0x10]);
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_trailing_garbage_after_extensions_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x00]); // extensions 空
        body.push(0xff); // 末尾の余り
        assert!(matches!(
            ClientHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn client_hello_omitted_extensions_field_is_accepted_as_empty() {
        let mut body = vec![0x03, 0x01]; // TLS 1.0 legacy_version（値自体は検査しない）
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x02, 0x00, 0x0a]);
        body.push(1);
        body.push(0);
        // extensions フィールドを丸ごと省略（残りバイト数 0）。
        let client_hello = ClientHello::parse(&body).expect("omitted extensions must be accepted");
        assert!(client_hello.extensions.is_empty());
    }

    #[test]
    fn server_hello_extensions_below_minimum_is_rejected() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0);
        body.extend_from_slice(&[0x13, 0x01]);
        body.push(0);
        body.extend_from_slice(&[0x00, 0x05]); // 拡張リスト長 5（最小 6 未満）
        body.extend_from_slice(&[0u8; 5]);
        assert!(matches!(
            ServerHello::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn certificate_empty_cert_data_is_rejected() {
        let mut body = vec![0]; // certificate_request_context 長 0
        let mut list = Vec::new();
        list.extend_from_slice(&[0x00, 0x00, 0x00]); // cert_data 長 0（最小 1 未満）
        list.extend_from_slice(&[0x00, 0x00]); // extensions 長 0
        let mut list_len = Vec::new();
        put_u24(&mut list_len, u32::try_from(list.len()).unwrap()).unwrap();
        body.extend_from_slice(&list_len);
        body.extend_from_slice(&list);
        assert!(matches!(
            Certificate::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn certificate_list_length_mismatch_is_rejected() {
        let mut body = vec![0];
        // certificate_list 長を 10 と宣言するが実際には 4 バイトしかない。
        body.extend_from_slice(&[0x00, 0x00, 0x0a]);
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        assert!(matches!(
            Certificate::parse(&body),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn finished_31_and_33_bytes_are_rejected() {
        assert!(matches!(
            Finished::parse(&[0u8; 31]),
            Err(HandshakeError::Decode(_))
        ));
        assert!(matches!(
            Finished::parse(&[0u8; 33]),
            Err(HandshakeError::Decode(_))
        ));
    }

    #[test]
    fn all_prefix_truncations_do_not_panic() {
        // RFC 8448 の ClientHello 本文をちょうど 45 バイト（legacy_version
        // 2 + random 32 + session_id 長 1 + cipher_suites 長 2 + 本体 6 +
        // compression 長 1 + 本体 1）で切ると、拡張フィールド丸ごと省略の
        // 正規形（`client_hello_omitted_extensions_field_is_accepted_as_empty`
        // 参照）とビット同一になり、意図的に受理される。この 1 点だけを
        // 例外として扱い、それ以外の全切り詰め位置は拒否されることを
        // 固定する。
        const CLIENT_HELLO_OMITTED_EXTENSIONS_CUT: usize = 45;

        for raw_bytes in [
            client_hello_message(),
            server_hello_message(),
            encrypted_extensions_message(),
            certificate_message(),
            certificate_verify_message(),
            server_finished_message(),
        ] {
            // ヘッダ 4 バイトを除いた本文を、0 バイト目から全長まで
            // 順に切り詰めても panic せず `Err` になることを確認する。
            let body = &raw_bytes[HANDSHAKE_HEADER_LEN..];
            let msg_type = HandshakeType::try_from(raw_bytes[0]).expect("known type");
            for cut in 0..body.len() {
                let truncated = &body[..cut];
                let result = match msg_type {
                    HandshakeType::ClientHello => ClientHello::parse(truncated).map(|_| ()),
                    HandshakeType::ServerHello => ServerHello::parse(truncated).map(|_| ()),
                    HandshakeType::EncryptedExtensions => {
                        EncryptedExtensions::parse(truncated).map(|_| ())
                    }
                    HandshakeType::Certificate => Certificate::parse(truncated).map(|_| ()),
                    HandshakeType::CertificateVerify => {
                        CertificateVerify::parse(truncated).map(|_| ())
                    }
                    HandshakeType::Finished => Finished::parse(truncated).map(|_| ()),
                };
                if msg_type == HandshakeType::ClientHello
                    && cut == CLIENT_HELLO_OMITTED_EXTENSIONS_CUT
                {
                    assert!(
                        result.is_ok(),
                        "cut={cut} is the legitimate omitted-extensions boundary and must be accepted"
                    );
                    continue;
                }
                assert!(result.is_err(), "cut={cut} must be rejected, not accepted");
            }
        }
    }

    #[test]
    fn serialize_rejects_oversized_session_id_without_panicking() {
        let client_hello = ClientHello {
            legacy_version: 0x0303,
            random: [0u8; 32],
            legacy_session_id: vec![0u8; 33],
            cipher_suites: vec![0x1301],
            legacy_compression_methods: vec![0],
            extensions: Vec::new(),
        };
        let mut out = Vec::new();
        let err = client_hello
            .encode_into(&mut out)
            .expect_err("must reject oversized session id");
        assert!(matches!(err, HandshakeError::Encode(_)));
        assert!(out.is_empty());
    }

    #[test]
    fn certificate_encode_rejects_body_exceeding_24bit_length_without_partial_write() {
        // #953 レビュー指摘: `encode_message` が `MAX_HANDSHAKE_WIRE_BODY_LEN` 超過を
        // 検出する前に type バイトを `out` へ書き込んでいたため、失敗時に `out` が
        // 部分的に変更される（all-or-nothing 契約違反）不変条件破りがあった。
        // `certificate_request_context`（255 バイト）＋各種長さ接頭辞を足すと
        // 本体全体が `MAX_HANDSHAKE_WIRE_BODY_LEN` をわずかに超えるように
        // `cert_data` を構成し、`out` が失敗前後で完全に無変更のままであることを固定する。
        let cert_data = vec![0u8; MAX_HANDSHAKE_WIRE_BODY_LEN as usize];
        let certificate = Certificate {
            certificate_request_context: vec![0u8; 255],
            certificate_list: vec![CertificateEntry {
                cert_data,
                extensions: Vec::new(),
            }],
        };
        let mut out = Vec::new();
        let err = certificate
            .encode_into(&mut out)
            .expect_err("must reject body exceeding 24-bit length field");
        assert!(matches!(err, HandshakeError::Encode(_)));
        assert!(
            out.is_empty(),
            "encode failure must not leave partial output"
        );
    }

    // ---- 5.3 再組み立て ----

    #[test]
    fn buffer_yields_four_messages_from_one_concatenated_fragment() {
        let payload = server_flight_payload();
        assert_eq!(payload.len(), 657, "40+445+136+36 must equal 657");

        let mut buffer = HandshakeBuffer::new();
        buffer.feed(&payload).expect("must feed");

        let mut messages = Vec::new();
        while let Some(raw) = buffer.next_message().expect("must not error") {
            messages.push(raw);
        }
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].msg_type, HandshakeType::EncryptedExtensions);
        assert_eq!(messages[1].msg_type, HandshakeType::Certificate);
        assert_eq!(messages[2].msg_type, HandshakeType::CertificateVerify);
        assert_eq!(messages[3].msg_type, HandshakeType::Finished);
        assert!(!buffer.has_partial());
        assert!(buffer.finish().is_ok());
    }

    #[test]
    fn buffer_splits_four_messages_at_every_possible_boundary() {
        let payload = server_flight_payload();

        for split_at in 0..=payload.len() {
            let mut buffer = HandshakeBuffer::new();
            let (left, right) = payload.split_at(split_at);
            buffer.feed(left).expect("must feed left half");
            buffer.feed(right).expect("must feed right half");

            let mut messages = Vec::new();
            while let Some(raw) = buffer.next_message().expect("must not error at split") {
                messages.push(raw);
            }
            assert_eq!(
                messages.len(),
                4,
                "split_at={split_at} must yield exactly four messages once fully fed"
            );
            assert_eq!(messages[0].msg_type, HandshakeType::EncryptedExtensions);
            assert_eq!(messages[3].msg_type, HandshakeType::Finished);
        }
    }

    #[test]
    fn buffer_yields_message_only_after_last_byte_fed_one_at_a_time() {
        let raw_bytes = server_hello_message();
        let mut buffer = HandshakeBuffer::new();
        for (i, byte) in raw_bytes.iter().enumerate() {
            buffer
                .feed(std::slice::from_ref(byte))
                .expect("must feed one byte");
            let result = buffer.next_message().expect("must not error while partial");
            if i + 1 < raw_bytes.len() {
                assert_eq!(result, None, "must stay None until the last byte");
                assert!(buffer.has_partial());
            } else {
                let raw = result.expect("must be Some on the final byte");
                assert_eq!(raw.msg_type, HandshakeType::ServerHello);
                assert!(!buffer.has_partial());
            }
        }
    }

    #[test]
    fn buffer_reassembles_message_fragmented_across_multiple_records_via_record_layer() {
        // Certificate（445 octets）を record::MAX_PLAINTEXT_LEN 未満の
        // 小さなチャンクへ人為的に分割し、record::RecordBuffer と
        // HandshakeBuffer を通した往復で元のメッセージへ復元できることを
        // 固定する（レコード層との結合確認）。
        let payload = certificate_message();
        let mut wire = Vec::new();
        for chunk in payload.chunks(64) {
            let rec = record::Record {
                content_type: record::ContentType::Handshake,
                legacy_version: record::LEGACY_RECORD_VERSION,
                fragment: chunk.to_vec(),
            };
            rec.serialize_into(&mut wire, record::RecordKind::Plaintext)
                .expect("must serialize record");
        }

        let mut record_buffer = record::RecordBuffer::new();
        record_buffer.feed(&wire);
        let mut handshake_buffer = HandshakeBuffer::new();
        while let Some(rec) = record_buffer
            .next_record(record::RecordKind::Plaintext)
            .expect("must parse record")
        {
            handshake_buffer
                .feed_record(&rec)
                .expect("must feed record into handshake buffer");
        }

        let raw = handshake_buffer
            .next_message()
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(raw.msg_type, HandshakeType::Certificate);
        assert_eq!(raw.body, payload[HANDSHAKE_HEADER_LEN..]);
    }

    // ---- 5.4 未対応の型・上限 ----

    #[test]
    fn unsupported_handshake_types_are_rejected_before_body_arrives() {
        for bad in [0u8, 3, 4, 5, 13, 24, 254, 255] {
            let mut buffer = HandshakeBuffer::new();
            // ヘッダのみ投入（本文は届いていない）。
            buffer.feed(&header(bad, 1)).expect("must feed header");
            let err = buffer
                .next_message()
                .expect_err("must reject unsupported handshake type");
            assert!(matches!(err, HandshakeError::UnexpectedType(b) if b == bad));
            assert_eq!(
                err.alert_description(),
                Some(record::AlertDescription::UnexpectedMessage)
            );
        }
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_body_allocation() {
        let mut buffer = HandshakeBuffer::new();
        let over_len = (MAX_HANDSHAKE_MESSAGE_LEN + 1) as u32;
        buffer
            .feed(&header(HandshakeType::ClientHello.as_u8(), over_len))
            .expect("must feed header only");
        let err = buffer
            .next_message()
            .expect_err("must reject oversized declared length");
        assert!(matches!(err, HandshakeError::MessageTooLarge { .. }));
        assert_eq!(
            err.alert_description(),
            Some(record::AlertDescription::DecodeError)
        );
    }

    #[test]
    fn buffer_poisons_after_error_and_repeats_same_error() {
        let mut buffer = HandshakeBuffer::new();
        buffer.feed(&header(24, 1)).expect("must feed"); // key_update: 未対応
        let first_err = buffer
            .next_message()
            .expect_err("must reject unsupported type");
        assert!(matches!(first_err, HandshakeError::UnexpectedType(24)));

        buffer
            .feed(&[0u8; 8])
            .expect_err("poisoned feed must error");
        let second_err = buffer.next_message().expect_err("must stay poisoned");
        assert!(matches!(second_err, HandshakeError::UnexpectedType(24)));
    }

    #[test]
    fn feed_record_rejects_non_handshake_content_type() {
        for content_type in [
            record::ContentType::ApplicationData,
            record::ContentType::Alert,
            record::ContentType::ChangeCipherSpec,
        ] {
            let rec = record::Record {
                content_type,
                legacy_version: record::LEGACY_RECORD_VERSION,
                fragment: vec![0u8; 4],
            };
            let mut fresh = HandshakeBuffer::new();
            let err = fresh
                .feed_record(&rec)
                .expect_err("must reject non-handshake content type");
            assert!(matches!(err, HandshakeError::UnexpectedContentType(ct) if ct == content_type));
            assert_eq!(
                err.alert_description(),
                Some(record::AlertDescription::UnexpectedMessage)
            );
        }
    }

    #[test]
    fn feed_does_not_grow_past_max_handshake_buffer_len() {
        let mut buffer = HandshakeBuffer::new();
        let oversized = vec![0u8; MAX_HANDSHAKE_BUFFER_LEN + 1];
        let err = buffer
            .feed(&oversized)
            .expect_err("must reject capacity-exceeding feed");
        assert!(matches!(err, HandshakeError::Decode(_)));
    }

    // PR #1033 review（P2）: `RawHandshake::header` は `encode_into`／`to_bytes`
    // の先頭 4 バイトと一致し（ヘッダ構成の単一情報源）、24 ビット長を超える
    // 本文は拒否する。
    #[test]
    fn raw_handshake_header_matches_encoded_prefix() {
        for len in [0usize, 1, 0xFF, 0x100, 0x1_0000, 0x12_3456] {
            let raw = RawHandshake {
                msg_type: HandshakeType::Certificate,
                body: vec![0xA5; len],
            };
            let header = raw.header().expect("length fits in u24");
            let encoded = raw.to_bytes().expect("encodable");
            assert_eq!(encoded.get(..HANDSHAKE_HEADER_LEN), Some(&header[..]));
            assert_eq!(encoded.get(HANDSHAKE_HEADER_LEN..), Some(&raw.body[..]));
        }
        let max = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![0u8; MAX_HANDSHAKE_WIRE_BODY_LEN as usize],
        };
        assert_eq!(
            max.header().expect("max u24 length"),
            [20, 0xFF, 0xFF, 0xFF]
        );
        let over = RawHandshake {
            msg_type: HandshakeType::Finished,
            body: vec![0u8; MAX_HANDSHAKE_WIRE_BODY_LEN as usize + 1],
        };
        assert!(matches!(over.header(), Err(HandshakeError::Encode(_))));
        let mut out = vec![0x42];
        assert!(over.encode_into(&mut out).is_err());
        // all-or-nothing: 失敗時に `out` へ何も書き込まない。
        assert_eq!(out, vec![0x42]);
    }
}
