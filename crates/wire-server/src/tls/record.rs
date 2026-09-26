//! TLS 1.3 レコード層（RFC 8446 §5.1）のフレーミング。
//!
//! 責務は「レコードヘッダ（content type・legacy_record_version・length）と
//! 本文の parse／serialize」「宣言長の上限検査（record_overflow 相当）」
//! 「受信バッファの組み立て（複数レコードの結合・分割受信への対応）」の
//! 3 点のみに限定する。**鍵・暗号・コネクション状態は一切持たない**（`Vec<u8>`
//! の入出力のみで完結する純粋な構造体とバッファ）。
//!
//! 呼び出し元・呼び出し先の予定（TASK-228・WIRE-9・HTTP-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-228）:
//! - ハンドシェイクメッセージの結合・分割（1 メッセージが複数レコードへ
//!   またがる場合の再構成）は #953 が本モジュールの [`Record`] を消費して担う
//! - `ClientHello` の `supported_versions` 検査によるバージョン交渉・
//!   TLS 1.3 以外の fail-closed 拒否は #954 が担う（本モジュールは
//!   `legacy_record_version` を RFC の指示どおり無視して保持するだけであり、
//!   ここでは拒否しない。理由は [`RecordHeader`] のドキュメンテーション
//!   コメントを参照）
//! - レコード保護（nonce・シーケンス番号・`TLSInnerPlaintext` の内側
//!   content type・パディング・AEAD 最小長）は [`super::record_protection`]
//!   （#959）が担う。[`RecordKind::Ciphertext`] はあくまで「このレコードに
//!   暗号文用の長さ上限を適用するか」を表すのみで、AEAD の復号・検証は
//!   この層の責務外
//! - alert の送出・状態機械・middlebox 互換 `ChangeCipherSpec` の破棄判定は
//!   #965 が担う。本モジュールは [`AlertDescription`] という定型コードの
//!   対応表を提供するのみで、実際に alert を送出しない
//! - 接続経路（`handshake.rs`・`server.rs`・`http::tls_transport`）への
//!   組み込みは本 Issue（#952）の時点では未接続だったが、#966（pg wire）・
//!   #968（NoSQL 表層）で実装済み
//!
//! 定数時間についての整理: レコードヘッダ（type・version・length）は通信路上で
//! 公開されている値であり、それによる分岐は秘密値に依存しない。本モジュールは
//! fragment（暗号文・鍵に関わるバイト列になりうる本文）の中身を一切解釈・
//! 比較せず、テーブル参照にも使わない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`・
//! `checked_*`・配列パターン束縛で処理する（`.claude/rules/coding-rust.md` P0）。
//! `unsafe` は使わない。

use std::io::{self, Read};

/// レコードヘッダの固定長（content type 1 バイト + legacy_record_version
/// 2 バイト + length 2 バイト）。
pub const RECORD_HEADER_LEN: usize = 5;

/// 平文レコード（`TLSPlaintext`）の fragment 長上限（RFC 8446 §5.1）。
pub const MAX_PLAINTEXT_LEN: usize = 1 << 14;

/// 暗号文レコード（`TLSCiphertext`）の fragment 長上限（RFC 8446 §5.2。
/// 平文上限 + 内側 content type・パディング・AEAD タグ分の余裕 256 バイト）。
pub const MAX_CIPHERTEXT_LEN: usize = (1 << 14) + 256;

/// [`RecordBuffer`] の内部バッファが取り得る最大バイト数（ヘッダ + 暗号文上限）。
/// この値を超えて伸びることはない（fail-closed な有界バッファ）。
pub const MAX_RECORD_WIRE_LEN: usize = RECORD_HEADER_LEN + MAX_CIPHERTEXT_LEN;

/// 送信するレコードの `legacy_record_version` に常に固定する値（TLS 1.2 互換）。
/// RFC 8446 §5.1 はこのフィールドを deprecated としており、受信側は無視
/// しなければならない（MUST ignore）。送信側は 0x0303 を書く。
pub const LEGACY_RECORD_VERSION: u16 = 0x0303;

const _: () = assert!(
    MAX_PLAINTEXT_LEN < MAX_CIPHERTEXT_LEN,
    "plaintext limit must be strictly smaller than ciphertext limit"
);
const _: () = assert!(
    MAX_CIPHERTEXT_LEN <= u16::MAX as usize,
    "ciphertext limit must fit in the 16-bit length field"
);

/// レコードヘッダの `content type`（RFC 8446 §5.1）。TLS 1.3 のレコード層が
/// 扱う 4 種類のみを閉じた語彙として受理する。
///
/// heartbeat(24) やその他の未知値は [`HeaderError::UnexpectedContentType`]
/// として fail-closed に拒否する。`ChangeCipherSpec`(20) は TLS 1.3 でも
/// middlebox 互換のために届き得るため、このレコード層では受理する
/// （実際に読み捨てるべきかどうかの判定は #965 の状態機械が担う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    ChangeCipherSpec,
    Alert,
    Handshake,
    ApplicationData,
}

impl ContentType {
    pub fn as_u8(self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }
}

impl TryFrom<u8> for ContentType {
    /// 受理できなかった生バイト値をそのまま保持する（呼び出し元がエラー
    /// 分類を組み立てる材料として使う）。
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, u8> {
        match value {
            20 => Ok(ContentType::ChangeCipherSpec),
            21 => Ok(ContentType::Alert),
            22 => Ok(ContentType::Handshake),
            23 => Ok(ContentType::ApplicationData),
            other => Err(other),
        }
    }
}

/// このレコードにどちらの長さ上限を適用するかを表す（レコード層自体は
/// TLS の接続状態を持たないため、どちらを渡すかは呼び出し元＝#965 の
/// 状態機械が判断する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// `TLSPlaintext`。fragment 長上限は [`MAX_PLAINTEXT_LEN`]。
    Plaintext,
    /// `TLSCiphertext`。fragment 長上限は content type に依存する
    /// （[`RecordKind::max_len_for`] を参照）。
    Ciphertext,
}

impl RecordKind {
    /// この `RecordKind` かつ `content_type` の組み合わせで許容する
    /// fragment 長の上限を返す。
    ///
    /// `Ciphertext` は本来すべて外側 content type が `ApplicationData`(23)
    /// になる（RFC 8446 §5.2）。それ以外の型（主に middlebox 互換の
    /// `ChangeCipherSpec`）が `Ciphertext` の探索中に届いた場合は、暗号文用の
    /// 緩い上限（2^14+256）を適用せず平文上限（2^14）のまま fail-closed に
    /// 絞る（この層はレコード保護〔#959〕より前段のため、受理範囲を必要以上に
    /// 広げない）。
    pub fn max_len_for(self, content_type: ContentType) -> usize {
        match self {
            RecordKind::Plaintext => MAX_PLAINTEXT_LEN,
            RecordKind::Ciphertext => match content_type {
                ContentType::ApplicationData => MAX_CIPHERTEXT_LEN,
                _ => MAX_PLAINTEXT_LEN,
            },
        }
    }
}

/// 0 長 fragment を拒否すべきかどうかを判定する（RFC 8446 §5.1）。
///
/// - `Plaintext`: `Handshake`／`Alert` の 0 長は禁止（Handshake は 0 長構造を
///   持てず、Alert は必ず 2 バイト固定長のため）。`ApplicationData`／
///   `ChangeCipherSpec` の 0 長はこの層では受理し、判定は上位層に委ねる
/// - `Ciphertext`: 内側 content type（1 バイト）と AEAD タグが必須のため、
///   content type を問わず 0 長は構造的に不正
fn rejects_empty_fragment(kind: RecordKind, content_type: ContentType) -> bool {
    match kind {
        RecordKind::Ciphertext => true,
        RecordKind::Plaintext => {
            matches!(content_type, ContentType::Handshake | ContentType::Alert)
        }
    }
}

/// TLS alert の `AlertDescription`（RFC 8446 §6）の定型コード対応表。
/// レコード層に限らず TLS 層全体（`handshake.rs`・`client_hello.rs` を
/// 含む）で共有し、各モジュールは自身が検出した違反をこの型へ写像する
/// のみで、実際の alert 送出・状態機械は #965 が担う。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertDescription {
    UnexpectedMessage,
    RecordOverflow,
    DecodeError,
    /// AEAD タグ検証の失敗（レコード保護層。#959）。
    BadRecordMac,
    /// 呼び出し契約違反（レコード保護層の送信側長さ超過・不正な状態遷移。#959）。
    InternalError,
    /// TLS 1.3 の必須暗号スイート（`TLS_AES_128_GCM_SHA256`）を含まない
    /// （RFC 8446 §4.1.1。判定は `client_hello::negotiate` が担う）。
    HandshakeFailure,
    /// 拡張の重複・compression 値の不正など、値そのものの意味検査違反
    /// （RFC 8446 §4.1.2・§4.2。判定は `client_hello::negotiate` が担う）。
    IllegalParameter,
    /// `supported_versions` に TLS 1.3（0x0304）が無い（RFC 8446 §4.2.1・
    /// 付録 D。判定は `client_hello::negotiate` が担う）。
    ProtocolVersion,
    /// 必須拡張（`signature_algorithms` 等）が欠落している（RFC 8446
    /// §9.2。判定は `client_hello::negotiate` が担う）。
    MissingExtension,
    /// Finished.verify_data の不一致（RFC 8446 §4.4.4 の実装注記どおり、
    /// TLS 1.3 では bad_record_mac ではなく decrypt_error を用いる。
    /// 判定は `super::finished::verify_client_finished` が担う。Issue #964）。
    DecryptError,
    /// 正常終了（RFC 8446 §6.1）。受信したら状態機械（#965）は
    /// エラーではなく `ClosedByPeer` として扱う。
    CloseNotify,
    /// クライアントが自発的にハンドシェイクを中断した（RFC 8446 §6.1）。
    /// `CloseNotify` と同じく正常終了として扱う。
    UserCanceled,
}

impl AlertDescription {
    pub fn as_u8(self) -> u8 {
        match self {
            AlertDescription::CloseNotify => 0,
            AlertDescription::UnexpectedMessage => 10,
            AlertDescription::BadRecordMac => 20,
            AlertDescription::RecordOverflow => 22,
            AlertDescription::DecodeError => 50,
            AlertDescription::DecryptError => 51,
            AlertDescription::HandshakeFailure => 40,
            AlertDescription::IllegalParameter => 47,
            AlertDescription::UserCanceled => 90,
            AlertDescription::ProtocolVersion => 70,
            AlertDescription::InternalError => 80,
            AlertDescription::MissingExtension => 109,
        }
    }
}

/// レコードヘッダ単体の検証で検出できるエラー（[`RecordHeader::parse`] の
/// 戻り値）。[`RecordBuffer`] の poison 状態としてもこの型のまま保持する
/// （`Copy` であり、`RecordError::Io` のような複製できない値を持たないため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// 宣言長が `kind`／`content_type` に応じた上限を超過（record_overflow）。
    Overflow { declared: usize, max: usize },
    /// 閉じた語彙 4 値のいずれでもない content type（unexpected_message）。
    UnexpectedContentType(u8),
    /// 0 長にできない種別の 0 長レコード（decode_error）。
    EmptyFragment(ContentType),
}

impl HeaderError {
    pub fn alert_description(self) -> AlertDescription {
        match self {
            HeaderError::Overflow { .. } => AlertDescription::RecordOverflow,
            HeaderError::UnexpectedContentType(_) => AlertDescription::UnexpectedMessage,
            HeaderError::EmptyFragment(_) => AlertDescription::DecodeError,
        }
    }
}

/// レコード層で検出しうるエラー全体。ヘッダ検証由来の 3 種
/// （[`HeaderError`] から変換）に加え、I/O 由来の 2 種を持つ。
///
/// `wire_code`（ERR-1／ERR-2）への写像は追加しない。TLS 層の失敗は
/// `ErrorResponse` ではなく TLS alert と切断で表すため、既存の RLS・
/// エラー契約（ERR-1/2/4）はこの型の追加によって変わらない。alert・切断への
/// 実際の写像は #965 の担当。
#[derive(Debug)]
pub enum RecordError {
    Overflow {
        declared: usize,
        max: usize,
    },
    UnexpectedContentType(u8),
    EmptyFragment(ContentType),
    /// 宣言長に達する前に EOF が来た（途中切断）。応答せず切断してよい
    /// （`crate::framing::FrameError::Truncated` と同じ位置付け）。
    Truncated,
    /// 上記以外の I/O 異常（タイムアウト等）。
    Io(io::Error),
}

impl From<HeaderError> for RecordError {
    fn from(e: HeaderError) -> Self {
        match e {
            HeaderError::Overflow { declared, max } => RecordError::Overflow { declared, max },
            HeaderError::UnexpectedContentType(b) => RecordError::UnexpectedContentType(b),
            HeaderError::EmptyFragment(ct) => RecordError::EmptyFragment(ct),
        }
    }
}

impl RecordError {
    /// クライアントへ返すべき TLS alert の種別。`None` は応答せず切断する
    /// 種別（`Truncated`・`Io`）を表す（`crate::framing::FrameError::sqlstate`
    /// と同じ位置付け）。
    pub fn alert_description(&self) -> Option<AlertDescription> {
        let header_err = match *self {
            RecordError::Overflow { declared, max } => HeaderError::Overflow { declared, max },
            RecordError::UnexpectedContentType(b) => HeaderError::UnexpectedContentType(b),
            RecordError::EmptyFragment(ct) => HeaderError::EmptyFragment(ct),
            RecordError::Truncated | RecordError::Io(_) => return None,
        };
        Some(header_err.alert_description())
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Overflow { declared, max } => {
                write!(
                    f,
                    "TLS record fragment length {declared} exceeds limit {max}"
                )
            }
            RecordError::UnexpectedContentType(b) => {
                write!(f, "unexpected TLS record content type {b}")
            }
            RecordError::EmptyFragment(ct) => {
                write!(f, "empty TLS record fragment not allowed for {ct:?}")
            }
            RecordError::Truncated => write!(f, "truncated TLS record"),
            RecordError::Io(e) => write!(f, "I/O error while reading TLS record: {e}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<io::Error> for RecordError {
    /// `read_exact` が返す `UnexpectedEof`（宣言長より実送信が短い途中切断）は
    /// `Truncated` へ写像する。それ以外は `Io` のまま呼び出し元へ伝える
    /// （`crate::framing::FrameError` と同じ方針）。
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            RecordError::Truncated
        } else {
            RecordError::Io(e)
        }
    }
}

/// レコードヘッダ（content type・legacy_record_version・length）。
///
/// `legacy_version` はパース時の生の値を保持するだけで、この層では検査に
/// 使わない。RFC 8446 §5.1 はこのフィールドを deprecated としており、受信側は
/// すべての目的で無視しなければならない（MUST ignore）と定めている。初回の
/// `ClientHello` は 0x0301 で届くことが多く、ここで値を理由に拒否すると
/// 実クライアントの接続を落としてしまう。「TLS 1.3 以外を fail-closed で
/// 拒否する」判定は `ClientHello` の `supported_versions` 拡張を見る #954 が
/// 担う分担であり、このレコード層は担わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub content_type: ContentType,
    pub legacy_version: u16,
    pub length: u16,
}

impl RecordHeader {
    /// 固定 5 バイトのヘッダを検証しつつ parse する。宣言長の上限超過・
    /// 未知 content type・禁止された 0 長は、本文を 1 バイトも読む前に
    /// このヘッダ単体の情報だけで判定できる（呼び出し元はこれを利用して
    /// 本文を読む前に打ち切ることができる）。
    pub fn parse(bytes: &[u8; RECORD_HEADER_LEN], kind: RecordKind) -> Result<Self, HeaderError> {
        let [type_byte, v0, v1, l0, l1] = *bytes;
        let content_type =
            ContentType::try_from(type_byte).map_err(HeaderError::UnexpectedContentType)?;
        let legacy_version = u16::from_be_bytes([v0, v1]);
        let length = u16::from_be_bytes([l0, l1]);
        let declared = usize::from(length);
        let max = kind.max_len_for(content_type);
        if declared > max {
            return Err(HeaderError::Overflow { declared, max });
        }
        if declared == 0 && rejects_empty_fragment(kind, content_type) {
            return Err(HeaderError::EmptyFragment(content_type));
        }
        Ok(RecordHeader {
            content_type,
            legacy_version,
            length,
        })
    }

    /// ヘッダをそのまま 5 バイトへ直列化する（`legacy_version` は保持している
    /// 値をそのまま書く）。送信レコード全体の組み立ては [`Record::
    /// serialize_into`] を使い、そちらが `legacy_version` を常に
    /// [`LEGACY_RECORD_VERSION`] へ正規化する。
    pub fn to_bytes(&self) -> [u8; RECORD_HEADER_LEN] {
        let v = self.legacy_version.to_be_bytes();
        let l = self.length.to_be_bytes();
        [self.content_type.as_u8(), v[0], v[1], l[0], l[1]]
    }
}

/// parse 済みの 1 レコード（`TLSPlaintext`／`TLSCiphertext` は同一のバイト
/// 構造を持つため、区別は呼び出し元が渡す [`RecordKind`] のみで行い、
/// この型自体は区別しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub content_type: ContentType,
    /// パース時に受信した生の値。送信時には使わない（[`Record::
    /// serialize_into`] のドキュメンテーションコメントを参照）。
    pub legacy_version: u16,
    pub fragment: Vec<u8>,
}

impl Record {
    /// このレコードを `kind` の上限規則に従って `out` の末尾へ直列化する。
    /// `fragment` が上限を超えている、または禁止された 0 長の場合は書き込まず
    /// `Err` を返す（panic させない）。
    ///
    /// 送信する `legacy_record_version` は常に [`LEGACY_RECORD_VERSION`]
    /// （0x0303）に固定し、`self.legacy_version`（受信時に保持した値）は
    /// 書かない。TLS 1.3 の送信側はこのフィールドを 0x0303 に固定するのが
    /// 規範であり、受信した値をそのまま送り返す理由はない。
    pub fn serialize_into(&self, out: &mut Vec<u8>, kind: RecordKind) -> Result<(), RecordError> {
        let declared = self.fragment.len();
        let max = kind.max_len_for(self.content_type);
        if declared > max {
            return Err(RecordError::Overflow { declared, max });
        }
        if declared == 0 && rejects_empty_fragment(kind, self.content_type) {
            return Err(RecordError::EmptyFragment(self.content_type));
        }
        let length =
            u16::try_from(declared).map_err(|_| RecordError::Overflow { declared, max })?;
        out.push(self.content_type.as_u8());
        out.extend_from_slice(&LEGACY_RECORD_VERSION.to_be_bytes());
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&self.fragment);
        Ok(())
    }
}

/// 受信データを蓄積し、そろったレコードを 1 つずつ取り出す push 型バッファ。
///
/// 1 回の受信に複数レコードがまとめて届く場合（[`RecordBuffer::feed`] で
/// 一括投入 → [`RecordBuffer::next_record`] を `Ok(None)` になるまで繰り返す）と、
/// 1 レコードが複数回の受信に分かれて届く場合（そろうまで [`RecordBuffer::
/// next_record`] は `Ok(None)` を返し続ける）の双方を同じ手続きで扱う。
///
/// 内部バッファは [`MAX_RECORD_WIRE_LEN`] を超えて伸びない（fail-closed な
/// 有界バッファ。無制限確保はしない）。
pub struct RecordBuffer {
    buf: Vec<u8>,
    /// 一度 [`RecordBuffer::next_record`] がエラーを返したら、以後の呼び出しは
    /// 同じ理由のエラーを返し続ける（poison。エラー後にバッファの残骸を
    /// 引き続き解釈しない fail-closed な設計）。
    poison: Option<HeaderError>,
}

impl Default for RecordBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            poison: None,
        }
    }

    /// `input` のうち、内部バッファの空き容量（[`MAX_RECORD_WIRE_LEN`] 上限）に
    /// 収まる分だけを取り込み、実際に取り込んだバイト数を返す。**`input` の
    /// 全部を取り込むとは限らない**。取り込み切れなかった残りは呼び出し元が
    /// 保持し、[`RecordBuffer::next_record`] でバッファを空けてから再度渡す。
    pub fn feed(&mut self, input: &[u8]) -> usize {
        let capacity_left = self.remaining_capacity();
        let take = input.len().min(capacity_left);
        match input.get(..take) {
            Some(chunk) => {
                self.buf.extend_from_slice(chunk);
                chunk.len()
            }
            None => 0,
        }
    }

    /// [`Self::feed`] が次に取り込める残り容量（[`MAX_RECORD_WIRE_LEN`]
    /// 上限からバッファ使用量を差し引いた値）。呼び出し元（[`super::
    /// stream::TlsStream`]。Issue #966）がこの値を超える生バイト列を
    /// 一度に読んでしまうと、`feed` が取り込み切れなかった超過分を
    /// 呼び出し元が保持せずに捨てた場合、次のレコードの先頭バイト列を
    /// 欠落させてしまう（"feed" のドキュメント参照）。読み取りバッファの
    /// 上限をこの値で絞ることで、そもそも取り込み切れない量を読まない
    /// ようにする。
    pub fn remaining_capacity(&self) -> usize {
        MAX_RECORD_WIRE_LEN.saturating_sub(self.buf.len())
    }

    /// バッファから 1 レコードを取り出す。
    ///
    /// - ヘッダ 5 バイトに満たない、またはヘッダはそろったが本文が
    ///   そろっていない場合は `Ok(None)`（呼び出し元は `feed` で追加の
    ///   バイト列を投入してから再度呼ぶ）
    /// - ヘッダが検証済みの時点で上限超過・未知 content type・禁止された
    ///   0 長のいずれかであれば、本文がそろうのを待たずに即 `Err`
    /// - 一度 `Err` を返したら、以後は poison 状態として同じ理由の `Err` を
    ///   返し続ける
    pub fn next_record(&mut self, kind: RecordKind) -> Result<Option<Record>, RecordError> {
        if let Some(poison) = self.poison {
            return Err(poison.into());
        }
        if self.buf.len() < RECORD_HEADER_LEN {
            return Ok(None);
        }
        let header_bytes: [u8; RECORD_HEADER_LEN] = match self
            .buf
            .get(..RECORD_HEADER_LEN)
            .and_then(|s| s.try_into().ok())
        {
            Some(arr) => arr,
            None => return Ok(None),
        };
        let header = match RecordHeader::parse(&header_bytes, kind) {
            Ok(h) => h,
            Err(e) => {
                self.poison = Some(e);
                return Err(e.into());
            }
        };
        let total = RECORD_HEADER_LEN + usize::from(header.length);
        if self.buf.len() < total {
            return Ok(None);
        }
        let fragment = match self.buf.get(RECORD_HEADER_LEN..total) {
            Some(s) => s.to_vec(),
            None => return Ok(None),
        };
        self.buf.drain(..total);
        Ok(Some(Record {
            content_type: header.content_type,
            legacy_version: header.legacy_version,
            fragment,
        }))
    }

    /// 接続の EOF で呼ぶ。バッファに部分的なバイト列（次のレコードの断片）が
    /// 残っていれば `Truncated`、何も残っていなければ `Ok(())`。
    pub fn finish(&self) -> Result<(), RecordError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(RecordError::Truncated)
        }
    }
}

/// `reader` から 1 レコードを読む（`crate::framing::read_typed_frame_header`と
/// 同じ流儀のアダプタ）。
///
/// 1. 先頭 1 バイトは `read()` で読む。0 バイトはレコード境界での正常な EOF
///    （`Ok(None)`）。`Interrupted` は再試行する
/// 2. 残り 4 バイトのヘッダを `read_exact` で読み検証する（**検証を通って
///    から**でなければ本文用の `Vec` を確保しない）
/// 3. 本文を `read_exact` で読む（検証済みなので最大 [`MAX_CIPHERTEXT_LEN`]
///    で有界）
///
/// 宣言長が上限を超えている場合は、ヘッダの 5 バイトだけを消費した時点で
/// `Err` を返し、本文は 1 バイトも読まない。
pub fn read_record<R: Read>(
    reader: &mut R,
    kind: RecordKind,
) -> Result<Option<Record>, RecordError> {
    let mut first = [0u8; 1];
    loop {
        match reader.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(RecordError::Io(e)),
        }
    }
    let mut rest = [0u8; RECORD_HEADER_LEN - 1];
    reader.read_exact(&mut rest)?;
    let header_bytes = [first[0], rest[0], rest[1], rest[2], rest[3]];
    let header = RecordHeader::parse(&header_bytes, kind)?;
    let len = usize::from(header.length);
    // header.length は RecordHeader::parse の上限検証を通過済みのため、
    // ここでの確保は最大 MAX_CIPHERTEXT_LEN で有界（未検証の値で
    // Vec::with_capacity/vec! しない、coding-rust.md 準拠）。
    let mut fragment = vec![0u8; len];
    reader.read_exact(&mut fragment)?;
    Ok(Some(Record {
        content_type: header.content_type,
        legacy_version: header.legacy_version,
        fragment,
    }))
}

/// `payload` を [`MAX_PLAINTEXT_LEN`] ごとに分割し、`content_type` の平文
/// レコード列へ組み立てる（暗号化前段の送信ヘルパー。暗号化後の
/// `TLSInnerPlaintext` 側の上限〔2^14+1〕・パディング調整は #959 が担う）。
///
/// - `payload` が空の場合、`Handshake`／`Alert` は禁止された 0 長として
///   `Err` を返す。`ApplicationData`／`ChangeCipherSpec` は 0 長レコードを
///   一切送らない契約とし、空の `Vec` を返す
/// - `Alert` は RFC 8446 §5.1 により複数レコードへ分割してはならないため、
///   `MAX_PLAINTEXT_LEN` を超える場合は分割せず `Err` を返す
pub fn fragment_plaintext(
    content_type: ContentType,
    payload: &[u8],
) -> Result<Vec<Record>, RecordError> {
    if payload.is_empty() {
        return if matches!(content_type, ContentType::Handshake | ContentType::Alert) {
            Err(HeaderError::EmptyFragment(content_type).into())
        } else {
            Ok(Vec::new())
        };
    }
    if content_type == ContentType::Alert && payload.len() > MAX_PLAINTEXT_LEN {
        return Err(HeaderError::Overflow {
            declared: payload.len(),
            max: MAX_PLAINTEXT_LEN,
        }
        .into());
    }
    let records = payload
        .chunks(MAX_PLAINTEXT_LEN)
        .map(|chunk| Record {
            content_type,
            legacy_version: LEGACY_RECORD_VERSION,
            fragment: chunk.to_vec(),
        })
        .collect();
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// RFC 8448 §3「Simple 1-RTT Handshake」の ClientHello の完全なレコード
    /// （201 octets）。IETF の公開文書（RFC 8448）由来の値であり、
    /// `docs/spec`（private）の内容ではない。
    fn rfc8448_client_hello_record() -> Vec<u8> {
        hex(
            "16030100c4010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dec\
             e7000006130113031302010000910000000b0009000006736572766572ff01000100000a00140012001d\
             0017001800190100010101020103010400230000003300260024001d002099381de560e4bd43d23d8e43\
             5a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b0003020304000d0020001e040305030603020308\
             040805080604010501060102010402050206020202002d00020101001c00024001",
        )
    }

    /// RFC 8448 §3 の ServerHello の完全なレコード（95 octets）。
    fn rfc8448_server_hello_record() -> Vec<u8> {
        hex(
            "160303005a020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e26928\
             00130100002e00330024001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e\
             751f0f002b00020304",
        )
    }

    /// RFC 8448 §3 のクライアント側 Finished を運ぶ暗号化レコード（58 octets）。
    /// fragment の中身は暗号文であり、このレコード層はその内容を一切解釈
    /// しない（往復一致のみを確認する）。
    fn rfc8448_encrypted_record() -> Vec<u8> {
        hex(
            "170303003575ec4dc238cce60b298044a71e219c56cc77b0517fe9b93c7a4bfc44d87f38f80338ac98fc46\
             deb384bd1caeacab6867d726c40546",
        )
    }

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

    // ---- RFC 8448 ベクタ ----

    #[test]
    fn client_hello_record_parses_with_legacy_version_0301() {
        let raw = rfc8448_client_hello_record();
        assert_eq!(raw.len(), 201);
        let mut cursor = Cursor::new(raw.clone());
        let record = read_record(&mut cursor, RecordKind::Plaintext)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(record.content_type, ContentType::Handshake);
        // legacy_record_version は 0x0301 で届く（RFC の指示どおり拒否しない）。
        assert_eq!(record.legacy_version, 0x0301);
        assert_eq!(record.fragment.len(), 0xc4);
        assert_eq!(cursor.position() as usize, raw.len());
    }

    #[test]
    fn client_hello_record_roundtrips_with_version_normalized() {
        let raw = rfc8448_client_hello_record();
        let mut cursor = Cursor::new(raw.clone());
        let record = read_record(&mut cursor, RecordKind::Plaintext)
            .expect("must parse")
            .expect("must be Some");
        let mut out = Vec::new();
        record
            .serialize_into(&mut out, RecordKind::Plaintext)
            .expect("must serialize");
        // version だけ 0x0303 へ正規化される以外はビット同一。
        let mut expected = raw.clone();
        expected[1] = 0x03;
        expected[2] = 0x03;
        assert_eq!(out, expected);
    }

    #[test]
    fn server_hello_record_roundtrips() {
        let raw = rfc8448_server_hello_record();
        assert_eq!(raw.len(), 95);
        let mut cursor = Cursor::new(raw.clone());
        let record = read_record(&mut cursor, RecordKind::Plaintext)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(record.content_type, ContentType::Handshake);
        assert_eq!(record.legacy_version, 0x0303);
        let mut out = Vec::new();
        record
            .serialize_into(&mut out, RecordKind::Plaintext)
            .expect("must serialize");
        assert_eq!(out, raw);
    }

    #[test]
    fn encrypted_record_roundtrips_as_ciphertext() {
        let raw = rfc8448_encrypted_record();
        assert_eq!(raw.len(), 58);
        let mut cursor = Cursor::new(raw.clone());
        let record = read_record(&mut cursor, RecordKind::Ciphertext)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(record.content_type, ContentType::ApplicationData);
        let mut out = Vec::new();
        record
            .serialize_into(&mut out, RecordKind::Ciphertext)
            .expect("must serialize");
        assert_eq!(out, raw);
    }

    // ---- 上限の境界値 ----

    fn header(content_type: u8, version: u16, length: u16) -> [u8; RECORD_HEADER_LEN] {
        let v = version.to_be_bytes();
        let l = length.to_be_bytes();
        [content_type, v[0], v[1], l[0], l[1]]
    }

    #[test]
    fn plaintext_accepts_exactly_max_len_and_rejects_one_more() {
        let ok = header(23, LEGACY_RECORD_VERSION, MAX_PLAINTEXT_LEN as u16);
        assert!(RecordHeader::parse(&ok, RecordKind::Plaintext).is_ok());

        let over = header(23, LEGACY_RECORD_VERSION, (MAX_PLAINTEXT_LEN + 1) as u16);
        let err = RecordHeader::parse(&over, RecordKind::Plaintext).expect_err("must overflow");
        assert!(matches!(err, HeaderError::Overflow { .. }));
        assert_eq!(err.alert_description(), AlertDescription::RecordOverflow);
    }

    #[test]
    fn ciphertext_accepts_exactly_max_len_and_rejects_one_more() {
        let ok = header(23, LEGACY_RECORD_VERSION, MAX_CIPHERTEXT_LEN as u16);
        assert!(RecordHeader::parse(&ok, RecordKind::Ciphertext).is_ok());

        let over = header(23, LEGACY_RECORD_VERSION, (MAX_CIPHERTEXT_LEN + 1) as u16);
        let err = RecordHeader::parse(&over, RecordKind::Ciphertext).expect_err("must overflow");
        assert!(matches!(err, HeaderError::Overflow { .. }));
    }

    #[test]
    fn length_0xffff_is_overflow_for_both_kinds() {
        let bytes = header(23, LEGACY_RECORD_VERSION, 0xFFFF);
        assert!(matches!(
            RecordHeader::parse(&bytes, RecordKind::Plaintext),
            Err(HeaderError::Overflow { .. })
        ));
        assert!(matches!(
            RecordHeader::parse(&bytes, RecordKind::Ciphertext),
            Err(HeaderError::Overflow { .. })
        ));
    }

    #[test]
    fn overflow_consumes_only_the_header_via_read_record() {
        let mut raw = header(23, LEGACY_RECORD_VERSION, (MAX_PLAINTEXT_LEN + 1) as u16).to_vec();
        raw.extend(std::iter::repeat_n(0u8, 16));
        let mut cursor = Cursor::new(raw);
        let err = read_record(&mut cursor, RecordKind::Plaintext).expect_err("must overflow");
        assert!(matches!(err, RecordError::Overflow { .. }));
        assert_eq!(cursor.position(), RECORD_HEADER_LEN as u64);
    }

    #[test]
    fn ciphertext_applies_plaintext_limit_to_non_application_data() {
        // 互換 CCS(20) が Ciphertext 探索中に届いた場合、暗号文の緩い上限では
        // なく平文上限(2^14)が適用される（fail-closed 側）。
        let over = header(20, LEGACY_RECORD_VERSION, (MAX_PLAINTEXT_LEN + 1) as u16);
        let err = RecordHeader::parse(&over, RecordKind::Ciphertext).expect_err("must be overflow");
        assert!(matches!(
            err,
            HeaderError::Overflow {
                max: MAX_PLAINTEXT_LEN,
                ..
            }
        ));
    }

    // ---- content type ----

    #[test]
    fn unknown_content_types_are_rejected() {
        for bad in [0u8, 19, 24, 255] {
            let bytes = header(bad, LEGACY_RECORD_VERSION, 1);
            let err = RecordHeader::parse(&bytes, RecordKind::Plaintext)
                .expect_err("must reject unknown content type");
            assert!(matches!(err, HeaderError::UnexpectedContentType(b) if b == bad));
            assert_eq!(err.alert_description(), AlertDescription::UnexpectedMessage);
        }
    }

    #[test]
    fn known_content_types_are_accepted() {
        for good in [20u8, 21, 22, 23] {
            let bytes = header(good, LEGACY_RECORD_VERSION, 1);
            assert!(RecordHeader::parse(&bytes, RecordKind::Plaintext).is_ok());
        }
    }

    #[test]
    fn sslv2_style_high_bit_first_byte_is_rejected() {
        let bytes = header(0x80, LEGACY_RECORD_VERSION, 1);
        let err = RecordHeader::parse(&bytes, RecordKind::Plaintext)
            .expect_err("SSLv2-style framing must be rejected");
        assert!(matches!(err, HeaderError::UnexpectedContentType(0x80)));
    }

    // ---- 0 長 ----

    #[test]
    fn plaintext_handshake_and_alert_zero_length_is_rejected() {
        for ct in [22u8, 21] {
            let bytes = header(ct, LEGACY_RECORD_VERSION, 0);
            let err = RecordHeader::parse(&bytes, RecordKind::Plaintext)
                .expect_err("zero length must be rejected");
            assert!(matches!(err, HeaderError::EmptyFragment(_)));
            assert_eq!(err.alert_description(), AlertDescription::DecodeError);
        }
    }

    #[test]
    fn plaintext_application_data_and_ccs_zero_length_is_accepted() {
        for ct in [23u8, 20] {
            let bytes = header(ct, LEGACY_RECORD_VERSION, 0);
            assert!(RecordHeader::parse(&bytes, RecordKind::Plaintext).is_ok());
        }
    }

    #[test]
    fn ciphertext_zero_length_is_always_rejected() {
        for ct in [20u8, 21, 22, 23] {
            let bytes = header(ct, LEGACY_RECORD_VERSION, 0);
            let err = RecordHeader::parse(&bytes, RecordKind::Ciphertext)
                .expect_err("zero length ciphertext must be rejected");
            assert!(matches!(err, HeaderError::EmptyFragment(_)));
        }
    }

    // ---- 途中切断（WIRE-10 と同じ形） ----

    #[test]
    fn header_truncated_at_3_bytes_does_not_panic() {
        let mut cursor = Cursor::new(vec![23u8, 0x03, 0x03]);
        let err = read_record(&mut cursor, RecordKind::Plaintext).expect_err("must be truncated");
        assert!(matches!(err, RecordError::Truncated));
    }

    #[test]
    fn body_truncated_after_full_header_does_not_panic() {
        let mut raw = header(23, LEGACY_RECORD_VERSION, 10).to_vec();
        raw.extend_from_slice(&[0u8; 4]); // 宣言 10 バイトだが実際は 4 バイトのみ
        let mut cursor = Cursor::new(raw);
        let err = read_record(&mut cursor, RecordKind::Plaintext).expect_err("must be truncated");
        assert!(matches!(err, RecordError::Truncated));
    }

    #[test]
    fn empty_input_is_clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_record(&mut cursor, RecordKind::Plaintext).expect("clean EOF is ok");
        assert_eq!(result, None);
    }

    #[test]
    fn record_buffer_finish_detects_partial_leftover() {
        let mut buffer = RecordBuffer::new();
        assert_eq!(buffer.feed(&[23u8, 0x03, 0x03, 0x00]), 4);
        assert!(buffer.finish().is_err());
    }

    #[test]
    fn record_buffer_finish_is_ok_when_empty() {
        let buffer = RecordBuffer::new();
        assert!(buffer.finish().is_ok());
    }

    #[test]
    fn read_record_retries_on_interrupted_first_byte() {
        struct InterruptOnceThenByte {
            interrupted: bool,
            rest: Cursor<Vec<u8>>,
        }

        impl Read for InterruptOnceThenByte {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.rest.read(buf)
            }
        }

        let mut raw = header(23, LEGACY_RECORD_VERSION, 3).to_vec();
        raw.extend_from_slice(&[1, 2, 3]);
        let mut reader = InterruptOnceThenByte {
            interrupted: false,
            rest: Cursor::new(raw),
        };
        let record = read_record(&mut reader, RecordKind::Plaintext)
            .expect("interrupted read must be retried")
            .expect("must be Some");
        assert_eq!(record.fragment, vec![1, 2, 3]);
    }

    // ---- バッファ層 ----

    #[test]
    fn buffer_yields_records_fed_as_one_concatenated_chunk() {
        let a = rfc8448_client_hello_record();
        let b = rfc8448_server_hello_record();
        let mut combined = a.clone();
        combined.extend_from_slice(&b);

        let mut buffer = RecordBuffer::new();
        assert_eq!(buffer.feed(&combined), combined.len());

        let first = buffer
            .next_record(RecordKind::Plaintext)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(first.fragment.len(), a.len() - RECORD_HEADER_LEN);

        let second = buffer
            .next_record(RecordKind::Plaintext)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(second.fragment.len(), b.len() - RECORD_HEADER_LEN);

        assert_eq!(buffer.next_record(RecordKind::Plaintext).unwrap(), None);
    }

    #[test]
    fn buffer_yields_record_only_after_last_byte_fed_one_at_a_time() {
        let raw = rfc8448_server_hello_record();
        let mut buffer = RecordBuffer::new();
        for (i, byte) in raw.iter().enumerate() {
            let taken = buffer.feed(std::slice::from_ref(byte));
            assert_eq!(taken, 1);
            let result = buffer
                .next_record(RecordKind::Plaintext)
                .expect("must not error while incomplete");
            if i + 1 < raw.len() {
                assert_eq!(result, None, "must stay None until the last byte");
            } else {
                let record = result.expect("must be Some on the final byte");
                assert_eq!(record.fragment.len(), raw.len() - RECORD_HEADER_LEN);
            }
        }
    }

    #[test]
    fn buffer_splits_two_records_at_every_possible_boundary() {
        let a = rfc8448_client_hello_record();
        let b = rfc8448_server_hello_record();
        let mut combined = a.clone();
        combined.extend_from_slice(&b);

        for split_at in 0..=combined.len() {
            let mut buffer = RecordBuffer::new();
            let (left, right) = combined.split_at(split_at);
            buffer.feed(left);
            buffer.feed(right);

            let first = buffer.next_record(RecordKind::Plaintext);
            let (first, second) = match first {
                Ok(Some(rec)) => (rec, buffer.next_record(RecordKind::Plaintext)),
                Ok(None) => {
                    // ヘッダすら届いていない極端な分割位置（split_at が 0 の
                    // ケースは起きない: combined は非空のため split_at=0 でも
                    // right に全体が入る）。分割位置 split_at の網羅の一部として
                    // None を許容しつつ、後続の feed 完了後は必ず取得できる
                    // ことを別途 continue で担保する。
                    continue;
                }
                Err(e) => panic!("must not error at split_at={split_at}: {e}"),
            };
            let second = second
                .expect("second next_record must not error")
                .expect("second record must be available once both are fed");
            assert_eq!(first.fragment.len(), a.len() - RECORD_HEADER_LEN);
            assert_eq!(second.fragment.len(), b.len() - RECORD_HEADER_LEN);
        }
    }

    #[test]
    fn buffer_overflow_header_consumes_no_body_bytes() {
        let mut buffer = RecordBuffer::new();
        let over = header(23, LEGACY_RECORD_VERSION, (MAX_PLAINTEXT_LEN + 1) as u16);
        buffer.feed(&over);
        let err = buffer
            .next_record(RecordKind::Plaintext)
            .expect_err("must overflow before body arrives");
        assert!(matches!(err, RecordError::Overflow { .. }));
    }

    #[test]
    fn buffer_poisons_after_error_and_repeats_same_error() {
        let mut buffer = RecordBuffer::new();
        let bad = header(24, LEGACY_RECORD_VERSION, 1); // heartbeat: 未知 content type
        buffer.feed(&bad);
        let first_err = buffer
            .next_record(RecordKind::Plaintext)
            .expect_err("must reject unknown content type");
        assert!(matches!(first_err, RecordError::UnexpectedContentType(24)));

        // 追加のバイト列を投入しても poison 状態は解除されない。
        buffer.feed(&[0u8; 8]);
        let second_err = buffer
            .next_record(RecordKind::Plaintext)
            .expect_err("must stay poisoned");
        assert!(matches!(second_err, RecordError::UnexpectedContentType(24)));
    }

    #[test]
    fn feed_does_not_grow_past_max_record_wire_len() {
        let mut buffer = RecordBuffer::new();
        let oversized = vec![0u8; MAX_RECORD_WIRE_LEN + 100];
        let taken = buffer.feed(&oversized);
        assert!(taken < oversized.len());
        assert_eq!(taken, MAX_RECORD_WIRE_LEN);
    }

    // ---- 送信側の分割 ----

    #[test]
    fn fragment_plaintext_splits_application_data_into_three_records() {
        let payload = vec![7u8; MAX_PLAINTEXT_LEN * 2 + 1];
        let records =
            fragment_plaintext(ContentType::ApplicationData, &payload).expect("must fragment");
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].fragment.len(), MAX_PLAINTEXT_LEN);
        assert_eq!(records[1].fragment.len(), MAX_PLAINTEXT_LEN);
        assert_eq!(records[2].fragment.len(), 1);
    }

    #[test]
    fn fragment_plaintext_empty_application_data_yields_no_records() {
        let records = fragment_plaintext(ContentType::ApplicationData, &[]).expect("must be ok");
        assert!(records.is_empty());
    }

    #[test]
    fn fragment_plaintext_empty_handshake_is_rejected() {
        let err = fragment_plaintext(ContentType::Handshake, &[]).expect_err("must reject");
        assert!(matches!(
            err,
            RecordError::EmptyFragment(ContentType::Handshake)
        ));
    }

    #[test]
    fn fragment_plaintext_oversized_alert_is_rejected_not_split() {
        let payload = vec![1u8; MAX_PLAINTEXT_LEN + 1];
        let err =
            fragment_plaintext(ContentType::Alert, &payload).expect_err("alert must not split");
        assert!(matches!(err, RecordError::Overflow { .. }));
    }

    #[test]
    fn fragment_plaintext_roundtrips_through_record_buffer() {
        let payload = vec![9u8; MAX_PLAINTEXT_LEN + 5];
        let records =
            fragment_plaintext(ContentType::ApplicationData, &payload).expect("must fragment");
        let mut wire = Vec::new();
        for record in &records {
            record
                .serialize_into(&mut wire, RecordKind::Plaintext)
                .expect("must serialize");
        }
        let mut buffer = RecordBuffer::new();
        buffer.feed(&wire);
        let mut recovered = Vec::new();
        while let Some(record) = buffer
            .next_record(RecordKind::Plaintext)
            .expect("must not error")
        {
            recovered.extend_from_slice(&record.fragment);
        }
        assert_eq!(recovered, payload);
    }

    #[test]
    fn serialize_into_rejects_oversized_fragment_without_panicking() {
        let record = Record {
            content_type: ContentType::ApplicationData,
            legacy_version: LEGACY_RECORD_VERSION,
            fragment: vec![0u8; MAX_PLAINTEXT_LEN + 1],
        };
        let mut out = Vec::new();
        let err = record
            .serialize_into(&mut out, RecordKind::Plaintext)
            .expect_err("must reject oversized fragment");
        assert!(matches!(err, RecordError::Overflow { .. }));
        assert!(out.is_empty());
    }
}
