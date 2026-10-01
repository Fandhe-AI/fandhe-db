//! TLS 1.3 レコード保護層（RFC 8446 §5.2〜§5.5。TASK-228・WIRE-9・HTTP-10
//! ポインタ。Issue #959・親 #941）。
//!
//! [`super::record`]（鍵・暗号・状態を持たないフレーミング）・
//! [`super::key_schedule`]（traffic secret → `TrafficKeys` の導出）・
//! [`super::aes_gcm`]（鍵一つ分の AEAD 暗号化／復号）の 3 モジュールを
//! つなぎ、以下を担う:
//! - per-record nonce（iv XOR シーケンス番号）の導出とシーケンス番号の管理
//!   （wrap 防止・RFC 8446 §5.5 の AEAD 使用上限）
//! - `TLSInnerPlaintext`（内容型の付加・ゼロパディング）の組み立て・復元
//! - AAD（外側レコードヘッダ 5 バイト）の構成
//! - handshake 鍵 → application 鍵への、送信・受信それぞれ独立したタイミング
//!   での切替（[`Sealer`]・[`Opener`]）
//!
//! # 対象外の範囲
//!
//! - 「いつ切り替えるか」の判断（server Finished を送った直後・client
//!   Finished を受け取った直後、という具体的なタイミング）は [`Sealer`]・
//!   [`Opener`] の呼び出し元＝#965 の状態機械が担う。本モジュールは切替の
//!   仕組み（`install_handshake_keys`／`install_application_keys` による
//!   状態遷移の強制）だけを提供する
//! - transcript hash・Finished の verify_data 計算/検証（#964）
//! - alert の実送出・接続への結線（#965・#966 以降）
//! - `KeyUpdate` による rekey・0-RTT・送信パディング方針の決定（対象外。
//!   [`Opener::open`] は application epoch で内側 Handshake 型を受理せず
//!   `unexpected_message` で拒否するため、`KeyUpdate`（type 24）を含む
//!   post-handshake Handshake メッセージは構造的に受理できない。送信側も
//!   [`Sealer`]（`seal`／`seal_fragmented`／`seal_detached`）が application
//!   epoch の内側 Handshake を [`ProtectionError::SendContractViolation`]
//!   で拒否し、送受信で対称にしている）
//!
//! # 定数時間についての整理
//!
//! - `TLSInnerPlaintext` の末尾パディング除去（[`decode_inner_plaintext`]）は
//!   復号後の平文全体を固定長で 1 回走査し、各バイトが非ゼロかどうかの
//!   マスク演算だけで「末尾の非ゼロバイト（内容型）の位置」を選び出す。
//!   秘密値（平文の中身）に依存する分岐・添字は使わない
//! - 走査後に分岐するのは、公開してよい結果（全ゼロで拒否するか、
//!   切り詰め後の長さ）だけである。切り詰め後の長さはこの後の処理
//!   （呼び出し元へ渡す `Vec` の長さ）で結局公開されるため、走査自体を
//!   定数時間にする意味は「パディング長そのもの」を計算時間差から
//!   推測できないようにする点にある
//! - nonce の XOR（[`RecordCipher::nonce`]）も分岐なし
//! - AEAD のタグ検証・復号は [`super::aes_gcm::Aes128Gcm`] の契約
//!   （検証成功時のみ復号する）にそのまま従う
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いず `get()`・
//! `checked_*`・マスク演算で処理する（`.claude/rules/coding-rust.md` P0）。
//! `unsafe` は使わない。

use super::aes_gcm::{AeadError, Aes128Gcm, TAG_LEN};
use super::hkdf::zeroize;
use super::key_schedule::TrafficKeys;
use super::record::{self, AlertDescription, ContentType, Record, RecordHeader, RecordKind};
use std::fmt;

/// `TLSInnerPlaintext`（RFC 8446 §5.4）の長さ上限。平文レコード上限
/// （[`record::MAX_PLAINTEXT_LEN`]）に内容型 1 バイト分を加えた値。
pub const MAX_INNER_PLAINTEXT_LEN: usize = record::MAX_PLAINTEXT_LEN + 1;

/// RFC 8446 §5.5 が定める `TLS_AES_128_GCM_SHA256` の AEAD 使用上限
/// （約 2^24.5 レコード）に対し、余裕を持たせた本リポの実装既定値。
/// `KeyUpdate`（対象外）を実装しないため、この上限が実際に接続を終了
/// させる条件になる（`docs/design/tls-record-protection.md` 参照）。
pub const MAX_RECORDS_PER_KEY: u64 = 1 << 24;

const _: () = assert!(
    MAX_INNER_PLAINTEXT_LEN + TAG_LEN <= record::MAX_CIPHERTEXT_LEN,
    "inner plaintext + AEAD tag must fit within the ciphertext record limit"
);

/// レコード保護層で検出しうるエラー全体。
///
/// `wire_code`（ERR-1／ERR-2／ERR-4）への写像は追加しない。TLS 層の失敗は
/// `ErrorResponse` ではなく TLS alert と切断で表す（[`super::record::
/// RecordError`] と同じ方針）。`Display` には長さ・内容・鍵情報を含めない
/// （公開プロトコル値である content type のバイト値のみ例外的に含む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionError {
    /// 外側レコードの content type が、この epoch で許容される値ではない。
    UnexpectedOuterType,
    /// AEAD タグ検証に失敗した、または暗号文長が構造的に不正
    /// （[`AeadError::TagMismatch`]／[`AeadError::CiphertextLength`] の写像）。
    BadRecordMac,
    /// 復号した `TLSInnerPlaintext` が全ゼロで、内容型が存在しない。
    NoContentType,
    /// 内側 content type が禁止された値（`ChangeCipherSpec`・未知値・
    /// application epoch での `Handshake`・handshake epoch での
    /// `ApplicationData` を含む）。
    ForbiddenInnerType(u8),
    /// `Handshake`／`Alert` の内容が 0 長。
    EmptyContent,
    /// `TLSInnerPlaintext` の長さが [`MAX_INNER_PLAINTEXT_LEN`] を超過。
    InnerOverflow,
    /// シーケンス番号が上限に達した（wrap 直前、または
    /// [`MAX_RECORDS_PER_KEY`] 到達）。alert を送らずに切断する
    /// （[`ProtectionError::alert_description`] が `None` を返す）。
    SequenceExhausted,
    /// [`Sealer`]／[`Opener`] の鍵切替 API を許されない順序で呼んだ
    /// （逆戻り・同じ epoch への二重 install・`Plaintext` から
    /// `Application` への直接遷移）。
    InvalidTransition,
    /// 呼び出し契約違反（`seal` 側の長さ超過、および epoch で許可されない
    /// content type の送信要求: Plaintext の ApplicationData／CCS・
    /// Handshake epoch の ApplicationData・Application epoch の Handshake）。
    SendContractViolation,
}

impl fmt::Display for ProtectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtectionError::UnexpectedOuterType => {
                write!(f, "unexpected outer TLS record content type for this epoch")
            }
            ProtectionError::BadRecordMac => write!(f, "TLS record AEAD authentication failed"),
            ProtectionError::NoContentType => {
                write!(f, "TLSInnerPlaintext has no content type (all zero)")
            }
            ProtectionError::ForbiddenInnerType(b) => {
                write!(f, "forbidden TLSInnerPlaintext content type {b}")
            }
            ProtectionError::EmptyContent => {
                write!(f, "empty content not allowed for this content type")
            }
            ProtectionError::InnerOverflow => write!(f, "TLSInnerPlaintext length exceeds limit"),
            ProtectionError::SequenceExhausted => write!(f, "TLS record sequence number exhausted"),
            ProtectionError::InvalidTransition => write!(f, "invalid TLS key epoch transition"),
            ProtectionError::SendContractViolation => {
                write!(f, "TLS record protection send contract violation")
            }
        }
    }
}

impl std::error::Error for ProtectionError {}

impl From<AeadError> for ProtectionError {
    /// `open` 側の失敗（タグ不一致・暗号文長不正）はいずれも `bad_record_mac`
    /// へ収束させ、詳細を漏らさない。`seal` 側の長さ超過（呼び出し契約違反）は
    /// `internal_error` 系（[`ProtectionError::SendContractViolation`]）とする。
    fn from(e: AeadError) -> Self {
        match e {
            AeadError::TagMismatch | AeadError::CiphertextLength => ProtectionError::BadRecordMac,
            AeadError::PlaintextTooLong | AeadError::AadTooLong => {
                ProtectionError::SendContractViolation
            }
        }
    }
}

impl ProtectionError {
    /// クライアントへ返すべき TLS alert の種別。`None` は alert を送らずに
    /// 切断する種別（[`ProtectionError::SequenceExhausted`] のみ）を表す。
    /// 使い切った鍵のもとでは保護された alert を安全に送れる保証がないため、
    /// [`super::record::RecordError::Truncated`]／`Io` と同じ扱いにする。
    pub fn alert_description(&self) -> Option<AlertDescription> {
        match self {
            ProtectionError::UnexpectedOuterType => Some(AlertDescription::UnexpectedMessage),
            ProtectionError::BadRecordMac => Some(AlertDescription::BadRecordMac),
            ProtectionError::NoContentType => Some(AlertDescription::UnexpectedMessage),
            ProtectionError::ForbiddenInnerType(_) => Some(AlertDescription::UnexpectedMessage),
            ProtectionError::EmptyContent => Some(AlertDescription::UnexpectedMessage),
            ProtectionError::InnerOverflow => Some(AlertDescription::RecordOverflow),
            ProtectionError::SequenceExhausted => None,
            ProtectionError::InvalidTransition => Some(AlertDescription::InternalError),
            ProtectionError::SendContractViolation => Some(AlertDescription::InternalError),
        }
    }
}

/// 復号・パディング除去を経た `TLSInnerPlaintext` の中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerPlaintext {
    pub content_type: ContentType,
    pub content: Vec<u8>,
}

/// バイト値が非ゼロなら `0xFF`、ゼロなら `0x00` を、分岐なしのビット演算
/// だけで返す（`decode_inner_plaintext` の定数時間走査で使う）。
fn ct_is_nonzero(b: u8) -> u8 {
    let x = u32::from(b);
    let nz = (x | x.wrapping_neg()) >> 31; // 0 または 1
    (nz as u8).wrapping_neg() // 0x00 または 0xFF
}

/// `mask`（`0x00`／`0xFF`）に応じて `a`（mask が `0xFF`）または `b`
/// （mask が `0x00`）を分岐なしで選ぶ。
fn ct_select_u8(mask: u8, a: u8, b: u8) -> u8 {
    (a & mask) | (b & !mask)
}

/// [`ct_select_u8`] の `usize` 版。`mask` を符号拡張して all-1／all-0 の
/// `usize` マスクへ変換してから選ぶ（分岐なし）。
fn ct_select_usize(mask: u8, a: usize, b: usize) -> usize {
    let full_mask = (mask as i8 as isize) as usize; // 0xFF -> usize::MAX、0x00 -> 0
    (a & full_mask) | (b & !full_mask)
}

/// 復号済みの `TLSInnerPlaintext`（`content || content_type || 0-padding`）を
/// 定数時間で走査し、末尾の非ゼロバイト（内容型）とその手前までの `content`
/// を取り出す（RFC 8446 §5.2 の受信側処理）。
///
/// 呼び出し元（[`Opener::open`]）が担う判定（外側 content type の検査・
/// 暗号文長の検査・application epoch での内側 `Handshake` 拒否）は含まない。
/// ここで固定するのは以下のみ:
/// - 全体長が [`MAX_INNER_PLAINTEXT_LEN`] を超えない（超えれば
///   [`ProtectionError::InnerOverflow`]）
/// - 非ゼロバイトが 1 つも無い（全ゼロ）平文は
///   [`ProtectionError::NoContentType`] として拒否する
/// - 内側 content type が `{Handshake, Alert, ApplicationData}` の
///   いずれでもない場合（`ChangeCipherSpec`・未知値を含む）は
///   [`ProtectionError::ForbiddenInnerType`]
/// - `Handshake`／`Alert` で `content` が 0 長の場合は
///   [`ProtectionError::EmptyContent`]
fn decode_inner_plaintext(plaintext: &[u8]) -> Result<InnerPlaintext, ProtectionError> {
    if plaintext.len() > MAX_INNER_PLAINTEXT_LEN {
        return Err(ProtectionError::InnerOverflow);
    }

    let mut last_idx: usize = 0;
    let mut content_type_byte: u8 = 0;
    let mut found: u8 = 0;
    for (i, &b) in plaintext.iter().enumerate() {
        let nz = ct_is_nonzero(b);
        last_idx = ct_select_usize(nz, i, last_idx);
        content_type_byte = ct_select_u8(nz, b, content_type_byte);
        found |= nz;
    }
    // ここより後は「受理するか拒否するか」「切り詰め後の長さ」という、
    // どのみち公開される結果に基づく分岐のみを行う。
    if found == 0 {
        return Err(ProtectionError::NoContentType);
    }

    let content_type = match ContentType::try_from(content_type_byte) {
        Ok(ContentType::ChangeCipherSpec) => {
            return Err(ProtectionError::ForbiddenInnerType(content_type_byte));
        }
        Ok(ct) => ct,
        Err(other) => return Err(ProtectionError::ForbiddenInnerType(other)),
    };

    let content = plaintext
        .get(..last_idx)
        .ok_or(ProtectionError::NoContentType)?
        .to_vec();

    if content.is_empty() && matches!(content_type, ContentType::Handshake | ContentType::Alert) {
        return Err(ProtectionError::EmptyContent);
    }

    Ok(InnerPlaintext {
        content_type,
        content,
    })
}

/// 送信用の `TLSInnerPlaintext`（`content || content_type || 0-padding`）を
/// 組み立てる。長さ検査はすべて `checked_*` で行い、上限超過時は確保前に
/// `Err` を返す。
fn build_inner_plaintext(
    content_type: ContentType,
    content: &[u8],
    padding_len: usize,
) -> Result<Vec<u8>, ProtectionError> {
    let total = content
        .len()
        .checked_add(1)
        .and_then(|v| v.checked_add(padding_len))
        .ok_or(ProtectionError::InnerOverflow)?;
    if total > MAX_INNER_PLAINTEXT_LEN {
        return Err(ProtectionError::InnerOverflow);
    }
    if content.is_empty() && matches!(content_type, ContentType::Handshake | ContentType::Alert) {
        return Err(ProtectionError::EmptyContent);
    }
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(content);
    buf.push(content_type.as_u8());
    buf.extend(std::iter::repeat_n(0u8, padding_len));
    Ok(buf)
}

/// 送信側 `seal`／受信側 `open` が共有する AAD（外側レコードヘッダ 5 バイト）
/// の組み立て。`content_type` は常に `ApplicationData`（保護されたレコードの
/// 外側 content type）で固定する。
fn seal_aad(
    ciphertext_and_tag_len: usize,
) -> Result<[u8; record::RECORD_HEADER_LEN], ProtectionError> {
    let length =
        u16::try_from(ciphertext_and_tag_len).map_err(|_| ProtectionError::InnerOverflow)?;
    Ok(RecordHeader {
        content_type: ContentType::ApplicationData,
        legacy_version: record::LEGACY_RECORD_VERSION,
        length,
    }
    .to_bytes())
}

/// 受信側 `open` の AAD 組み立て。送信時と異なり、**受信した値のまま**
/// （`legacy_version` を正規化しない）組み立てる。`legacy_version` は
/// RFC の指示どおり検査しない（値が違えばタグ検証で失敗する）。
fn open_aad(
    record: &Record,
    ciphertext_and_tag_len: usize,
) -> Result<[u8; record::RECORD_HEADER_LEN], ProtectionError> {
    let length =
        u16::try_from(ciphertext_and_tag_len).map_err(|_| ProtectionError::BadRecordMac)?;
    Ok(RecordHeader {
        content_type: record.content_type,
        legacy_version: record.legacy_version,
        length,
    }
    .to_bytes())
}

/// 1 方向・1 epoch 分の AEAD 状態（RFC 8446 §5.3。鍵・iv・シーケンス番号）。
///
/// `Clone`／`Copy` は導出しない。`Debug` は内容を秘匿し、`Drop` で `iv` を
/// best-effort ゼロ化する（`cipher` 自身は [`Aes128Gcm`] の `Drop` が
/// 別途 `H` をゼロ化する）。
struct RecordCipher {
    cipher: Aes128Gcm,
    iv: [u8; 12],
    seq: u64,
}

impl RecordCipher {
    fn new(keys: &TrafficKeys) -> Self {
        Self {
            cipher: Aes128Gcm::new(keys.key()),
            iv: *keys.iv(),
            seq: 0,
        }
    }

    /// テスト専用: シーケンス番号を任意の値から開始する。
    /// [`MAX_RECORDS_PER_KEY`] 到達時の接続終了を、実際に 2^24 回
    /// seal／open することなく検証するために使う。
    #[cfg(test)]
    fn with_seq_for_test(keys: &TrafficKeys, seq: u64) -> Self {
        let mut cipher = Self::new(keys);
        cipher.seq = seq;
        cipher
    }

    /// `nonce = iv XOR pad(seq)`（RFC 8446 §5.3。`seq` はビッグエンディアン
    /// 64 ビットを 12 バイトの末尾へ左ゼロ詰めしたもの）。分岐なしの XOR。
    fn nonce(&self) -> [u8; 12] {
        let seq_bytes = self.seq.to_be_bytes();
        let mut padded = [0u8; 12];
        if let Some(slot) = padded.get_mut(4..12) {
            slot.copy_from_slice(&seq_bytes);
        }
        let mut out = [0u8; 12];
        for ((o, i), p) in out.iter_mut().zip(self.iv.iter()).zip(padded.iter()) {
            *o = i ^ p;
        }
        out
    }

    /// 次に使うシーケンス番号が [`MAX_RECORDS_PER_KEY`] 未満であることを
    /// 確認したうえで、現在の `nonce` を返す（消費はしない）。
    fn peek_nonce(&self) -> Result<[u8; 12], ProtectionError> {
        if self.seq >= MAX_RECORDS_PER_KEY {
            return Err(ProtectionError::SequenceExhausted);
        }
        Ok(self.nonce())
    }

    /// [`RecordCipher::peek_nonce`] で使ったシーケンス番号を実際に消費する
    /// （+1 する）。`seal` は常に呼ぶ。`open` は復号成功時のみ呼ぶ
    /// （失敗時にシーケンス番号を進めない契約）。
    fn advance(&mut self) -> Result<(), ProtectionError> {
        self.seq = self
            .seq
            .checked_add(1)
            .ok_or(ProtectionError::SequenceExhausted)?;
        Ok(())
    }
}

impl Drop for RecordCipher {
    fn drop(&mut self) {
        zeroize(&mut self.iv);
    }
}

/// [`Sealer`]／[`Opener`] が方向ごとに独立して持つ鍵状態。`Plaintext` から
/// のみ `Handshake` へ、`Handshake` からのみ `Application` へ遷移できる
/// （逆戻り・二重 install・直接遷移はいずれも [`ProtectionError::
/// InvalidTransition`]）。
enum Epoch {
    Plaintext,
    Handshake(RecordCipher),
    Application(RecordCipher),
}

impl fmt::Debug for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Epoch::Plaintext => write!(f, "Plaintext"),
            Epoch::Handshake(_) => write!(f, "Handshake(<redacted>)"),
            Epoch::Application(_) => write!(f, "Application(<redacted>)"),
        }
    }
}

/// `Plaintext` からのみ `Handshake` へ、`Handshake` からのみ `Application`
/// へ遷移する。[`Sealer`]／[`Opener`] の双方が共有する遷移本体。
fn install_handshake(epoch: &mut Epoch, keys: &TrafficKeys) -> Result<(), ProtectionError> {
    match epoch {
        Epoch::Plaintext => {
            *epoch = Epoch::Handshake(RecordCipher::new(keys));
            Ok(())
        }
        Epoch::Handshake(_) | Epoch::Application(_) => Err(ProtectionError::InvalidTransition),
    }
}

fn install_application(epoch: &mut Epoch, keys: &TrafficKeys) -> Result<(), ProtectionError> {
    match epoch {
        Epoch::Handshake(_) => {
            *epoch = Epoch::Application(RecordCipher::new(keys));
            Ok(())
        }
        Epoch::Plaintext | Epoch::Application(_) => Err(ProtectionError::InvalidTransition),
    }
}

/// 送信方向のレコード保護状態。server Finished を seal した直後に
/// [`Sealer::install_application_keys`] を呼ぶ判断は呼び出し元（#965）が
/// 担う。
#[derive(Debug)]
pub struct Sealer {
    epoch: Epoch,
}

impl Default for Sealer {
    fn default() -> Self {
        Self::new()
    }
}

impl Sealer {
    pub fn new() -> Self {
        Self {
            epoch: Epoch::Plaintext,
        }
    }

    pub fn install_handshake_keys(&mut self, keys: &TrafficKeys) -> Result<(), ProtectionError> {
        install_handshake(&mut self.epoch, keys)
    }

    pub fn install_application_keys(&mut self, keys: &TrafficKeys) -> Result<(), ProtectionError> {
        install_application(&mut self.epoch, keys)
    }

    /// 現在の epoch に応じて、呼び出し元が [`super::record::Record::
    /// serialize_into`] へ渡すべき [`RecordKind`] を返す（`Plaintext` epoch
    /// は [`RecordKind::Plaintext`]、それ以外は [`RecordKind::Ciphertext`]）。
    /// [`Opener::record_kind`] とは独立した送信側（seal 済みデータ）の値
    /// であり、両者を混同しない（#965 レビュー指摘。本実装は
    /// [`Sealer`]／[`Opener`] が常に同時に鍵切替する構成のため現状は
    /// 両者が同値になるが、将来非対称な鍵切替が入っても書き込み側の検証が
    /// 誤った epoch を参照しないよう区別する）。
    pub fn record_kind(&self) -> RecordKind {
        match self.epoch {
            Epoch::Plaintext => RecordKind::Plaintext,
            Epoch::Handshake(_) | Epoch::Application(_) => RecordKind::Ciphertext,
        }
    }

    /// `content` を 1 レコード分 seal する。`padding_len` は呼び出し側が
    /// 明示的に指定するパディング長（通常呼び出しは 0 を渡す。パディング
    /// 方針を決める仕組みはこのモジュールでは提供しない）。
    ///
    /// `Plaintext` epoch では `Handshake`／`Alert` の平文レコードだけを
    /// 許可する（`ApplicationData`・`ChangeCipherSpec` はこの epoch では
    /// 送信できない。CCS は鍵状態を持たない固定レコードのため、呼び出し
    /// 元がこの `Sealer` を経由せず直接組み立てる契約とする）。
    /// `Handshake` epoch でも `ApplicationData` は送信できない
    /// （[`ProtectionError::SendContractViolation`]。0-RTT 非対応の本実装
    /// では Finished 完了前のアプリケーションデータ送信を認めないため）。
    ///
    /// epoch ごとに送出できる内側 content type は次のとおり（CCS は
    /// `Plaintext` で `SendContractViolation`、保護 epoch で
    /// `ForbiddenInnerType`）。
    ///
    /// | epoch | 許可 | 拒否（`SendContractViolation`） |
    /// | ----- | ---- | ------------------------------ |
    /// | `Plaintext` | `Handshake`／`Alert` | `ApplicationData`・CCS |
    /// | `Handshake` | `Handshake`／`Alert` | `ApplicationData` |
    /// | `Application` | `Alert`／`ApplicationData` | `Handshake` |
    ///
    /// `Application` epoch の内側 Handshake は、受信側 [`Opener::open`] が
    /// 同 epoch の内側 Handshake を拒否する（post-handshake の
    /// NewSessionTicket・KeyUpdate 非対応）のと対称に、送信側でも拒否する。
    /// 拒否はシーケンス番号の消費・nonce 導出・暗号化より前に行うため、
    /// 拒否後も seq は進まない。
    pub fn seal(
        &mut self,
        content_type: ContentType,
        content: &[u8],
        padding_len: usize,
    ) -> Result<Record, ProtectionError> {
        match &mut self.epoch {
            Epoch::Plaintext => {
                if padding_len != 0 {
                    return Err(ProtectionError::SendContractViolation);
                }
                match content_type {
                    ContentType::Handshake | ContentType::Alert => {}
                    ContentType::ApplicationData | ContentType::ChangeCipherSpec => {
                        return Err(ProtectionError::SendContractViolation);
                    }
                }
                if content.is_empty() {
                    return Err(ProtectionError::EmptyContent);
                }
                if content.len() > record::MAX_PLAINTEXT_LEN {
                    return Err(ProtectionError::InnerOverflow);
                }
                Ok(Record {
                    content_type,
                    legacy_version: record::LEGACY_RECORD_VERSION,
                    fragment: content.to_vec(),
                })
            }
            Epoch::Handshake(cipher) => {
                // 0-RTT 非対応の本実装では、Finished 完了前の Handshake
                // epoch から ApplicationData を送信できてはならない
                // （client 認証成立前のデータをアプリケーション入力として
                // 扱う経路を作らないため）。この epoch では内側 content
                // type を Handshake／Alert に限定する。
                match content_type {
                    ContentType::Handshake | ContentType::Alert => {}
                    ContentType::ApplicationData => {
                        return Err(ProtectionError::SendContractViolation);
                    }
                    ContentType::ChangeCipherSpec => {
                        return Err(ProtectionError::ForbiddenInnerType(
                            ContentType::ChangeCipherSpec.as_u8(),
                        ));
                    }
                }
                let mut inner = build_inner_plaintext(content_type, content, padding_len)?;
                let ciphertext_and_tag_len = inner
                    .len()
                    .checked_add(TAG_LEN)
                    .ok_or(ProtectionError::InnerOverflow)?;
                let aad = seal_aad(ciphertext_and_tag_len)?;
                let nonce = cipher.peek_nonce()?;
                let sealed = cipher
                    .cipher
                    .seal(&nonce, &aad, &inner)
                    .map_err(ProtectionError::from)?;
                zeroize(&mut inner);
                cipher.advance()?;
                Ok(Record {
                    content_type: ContentType::ApplicationData,
                    legacy_version: record::LEGACY_RECORD_VERSION,
                    fragment: sealed,
                })
            }
            Epoch::Application(cipher) => {
                // post-handshake の Handshake（NewSessionTicket・KeyUpdate）
                // は非対応で、受信側 `Opener::open` が同 epoch の内側
                // Handshake を拒否するのと対称に、送信側も fail-closed で
                // 拒否する（TASK-228・WIRE-9 のポインタ）。
                match content_type {
                    ContentType::Alert | ContentType::ApplicationData => {}
                    ContentType::Handshake => {
                        return Err(ProtectionError::SendContractViolation);
                    }
                    ContentType::ChangeCipherSpec => {
                        return Err(ProtectionError::ForbiddenInnerType(
                            ContentType::ChangeCipherSpec.as_u8(),
                        ));
                    }
                }
                let mut inner = build_inner_plaintext(content_type, content, padding_len)?;
                let ciphertext_and_tag_len = inner
                    .len()
                    .checked_add(TAG_LEN)
                    .ok_or(ProtectionError::InnerOverflow)?;
                let aad = seal_aad(ciphertext_and_tag_len)?;
                let nonce = cipher.peek_nonce()?;
                let sealed = cipher
                    .cipher
                    .seal(&nonce, &aad, &inner)
                    .map_err(ProtectionError::from)?;
                zeroize(&mut inner);
                cipher.advance()?;
                Ok(Record {
                    content_type: ContentType::ApplicationData,
                    legacy_version: record::LEGACY_RECORD_VERSION,
                    fragment: sealed,
                })
            }
        }
    }

    /// `content` を現在の送信 seq で seal するが、[`RecordCipher::advance`]
    /// を呼ばずシーケンス番号を消費しない（detached seal）。RECOVER-6
    /// （緊急応答。Issue #1080）の panic フックが `TlsStream` 越しに
    /// 緊急応答を送出するため、`tls::stream::TlsStream::
    /// emergency_response_channel` がクエリ実行前に呼ぶ。
    ///
    /// # 不変条件（nonce 再利用の防止）
    ///
    /// 返した暗号文は、呼び出し時点でこの `Sealer` が次に使う seq と同じ
    /// nonce を用いる。したがって「この暗号文」と「呼び出し後に
    /// [`Sealer::seal`]／[`Sealer::seal_fragmented`] で送信される次の
    /// 通常レコード」は同じ nonce を共有し、**両方が実際に送出されると
    /// nonce 再利用（AES-GCM の機密性・完全性が崩れる）になる**。
    /// 呼び出し元は、この暗号文を送出する経路（panic フック）と通常の
    /// 送信経路が排他的であること（例: 呼び出しから送出判断までの区間で
    /// `stream` を `&mut` 排他借用し、その区間の外側でしか通常送信が
    /// 起きない・かつ両者が競合し得る箇所では必ず abort する）を担保する
    /// 責務を負う。将来この区間の panic を捕捉して処理を継続する変更を
    /// 入れると、この不変条件が壊れて nonce 再利用になる。
    ///
    /// `Application` epoch でのみ許可する（`Plaintext`／`Handshake` では
    /// [`ProtectionError::SendContractViolation`]）。内側 content type は
    /// `Alert`／`ApplicationData` のみで、`Handshake` は [`Sealer::seal`] と
    /// 同じく `SendContractViolation` で拒否する。
    pub fn seal_detached(
        &self,
        content_type: ContentType,
        content: &[u8],
    ) -> Result<Record, ProtectionError> {
        let cipher = match &self.epoch {
            Epoch::Application(cipher) => cipher,
            Epoch::Plaintext | Epoch::Handshake(_) => {
                return Err(ProtectionError::SendContractViolation);
            }
        };
        match content_type {
            ContentType::Alert | ContentType::ApplicationData => {}
            ContentType::Handshake => {
                return Err(ProtectionError::SendContractViolation);
            }
            ContentType::ChangeCipherSpec => {
                return Err(ProtectionError::ForbiddenInnerType(
                    ContentType::ChangeCipherSpec.as_u8(),
                ));
            }
        }
        let mut inner = build_inner_plaintext(content_type, content, 0)?;
        let ciphertext_and_tag_len = inner
            .len()
            .checked_add(TAG_LEN)
            .ok_or(ProtectionError::InnerOverflow)?;
        let aad = seal_aad(ciphertext_and_tag_len)?;
        let nonce = cipher.peek_nonce()?;
        let sealed = cipher
            .cipher
            .seal(&nonce, &aad, &inner)
            .map_err(ProtectionError::from)?;
        zeroize(&mut inner);
        // 意図的に advance() しない（doc の不変条件を参照）。
        Ok(Record {
            content_type: ContentType::ApplicationData,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: sealed,
        })
    }

    /// `payload` を [`MAX_INNER_PLAINTEXT_LEN`]（内側 content type 分を
    /// 差し引いた長さ）ごとに分割して seal する（`padding_len` は常に 0）。
    /// 挙動は [`record::fragment_plaintext`] と同じ規則に揃える。
    ///
    /// - `Alert` は分割禁止（RFC 8446 §5.1）。上限超過は `Err`
    /// - `ApplicationData` の空 payload は空の `Vec` を返す（0 長レコードを
    ///   送らない）
    /// - `Handshake` の空 payload は `Err`（0 長 Handshake フラグメント禁止。
    ///   epoch に関係なく `EmptyContent` を先に返す）
    ///
    /// content type の epoch 制約は [`Sealer::seal`] と同じ（`Application`
    /// epoch の `Handshake` は最初の chunk で `SendContractViolation` となり、
    /// レコードは 1 件も生成されず seq も進まない）。
    pub fn seal_fragmented(
        &mut self,
        content_type: ContentType,
        payload: &[u8],
    ) -> Result<Vec<Record>, ProtectionError> {
        let max_chunk = MAX_INNER_PLAINTEXT_LEN - 1;
        match content_type {
            ContentType::ChangeCipherSpec => Err(ProtectionError::ForbiddenInnerType(
                ContentType::ChangeCipherSpec.as_u8(),
            )),
            ContentType::Alert => {
                if payload.is_empty() {
                    return Err(ProtectionError::EmptyContent);
                }
                if payload.len() > max_chunk {
                    return Err(ProtectionError::InnerOverflow);
                }
                Ok(vec![self.seal(content_type, payload, 0)?])
            }
            ContentType::ApplicationData => {
                if payload.is_empty() {
                    return Ok(Vec::new());
                }
                payload
                    .chunks(max_chunk)
                    .map(|chunk| self.seal(content_type, chunk, 0))
                    .collect()
            }
            ContentType::Handshake => {
                if payload.is_empty() {
                    return Err(ProtectionError::EmptyContent);
                }
                payload
                    .chunks(max_chunk)
                    .map(|chunk| self.seal(content_type, chunk, 0))
                    .collect()
            }
        }
    }
}

/// 受信方向のレコード保護状態。client Finished を open した直後に
/// [`Opener::install_application_keys`] を呼ぶ判断は呼び出し元（#965）が
/// 担う。middlebox 互換の `ChangeCipherSpec` レコードは、[`Opener::open`]
/// が呼ばれる**前**に呼び出し元が読み捨てる契約とする（このレコード保護層
/// では扱わない）。
#[derive(Debug)]
pub struct Opener {
    epoch: Epoch,
}

impl Default for Opener {
    fn default() -> Self {
        Self::new()
    }
}

impl Opener {
    pub fn new() -> Self {
        Self {
            epoch: Epoch::Plaintext,
        }
    }

    pub fn install_handshake_keys(&mut self, keys: &TrafficKeys) -> Result<(), ProtectionError> {
        install_handshake(&mut self.epoch, keys)
    }

    pub fn install_application_keys(&mut self, keys: &TrafficKeys) -> Result<(), ProtectionError> {
        install_application(&mut self.epoch, keys)
    }

    /// 現在の epoch に応じて、呼び出し元が
    /// [`super::record::RecordBuffer::next_record`]／[`super::record::
    /// read_record`] へ渡すべき [`RecordKind`] を返す（`Plaintext` epoch は
    /// [`RecordKind::Plaintext`]、それ以外は [`RecordKind::Ciphertext`]）。
    pub fn record_kind(&self) -> RecordKind {
        match self.epoch {
            Epoch::Plaintext => RecordKind::Plaintext,
            Epoch::Handshake(_) | Epoch::Application(_) => RecordKind::Ciphertext,
        }
    }

    /// 受信した 1 レコードを open する。
    ///
    /// `Plaintext` epoch では外側 content type が `Handshake`／`Alert` の
    /// 場合のみそのまま通す（`ApplicationData` は鍵導入前に届き得ないため
    /// [`ProtectionError::UnexpectedOuterType`]）。`Handshake` epoch では
    /// 復号後の内側 content type を `Handshake`／`Alert` に限定し、
    /// `ApplicationData` は [`ProtectionError::ForbiddenInnerType`] として
    /// 拒否する（0-RTT 非対応の本実装では Finished 完了前の
    /// ApplicationData を受理してはならないため）。
    pub fn open(&mut self, record: &Record) -> Result<InnerPlaintext, ProtectionError> {
        match &mut self.epoch {
            Epoch::Plaintext => match record.content_type {
                ContentType::Handshake | ContentType::Alert => Ok(InnerPlaintext {
                    content_type: record.content_type,
                    content: record.fragment.clone(),
                }),
                ContentType::ApplicationData | ContentType::ChangeCipherSpec => {
                    Err(ProtectionError::UnexpectedOuterType)
                }
            },
            Epoch::Handshake(cipher) => {
                let mut inner = open_protected(cipher, record)?;
                // 0-RTT 非対応の本実装では、Finished 完了前（client 認証
                // 成立前）の Handshake epoch で ApplicationData を受理して
                // はならない（送信側の契約〔`seal`〕と対称。復号済みの
                // 平文は他の拒否経路と同様に zeroize してから破棄する）。
                if inner.content_type == ContentType::ApplicationData {
                    zeroize(&mut inner.content);
                    return Err(ProtectionError::ForbiddenInnerType(
                        ContentType::ApplicationData.as_u8(),
                    ));
                }
                Ok(inner)
            }
            Epoch::Application(cipher) => {
                let mut inner = open_protected(cipher, record)?;
                // KeyUpdate・post-handshake の Handshake メッセージは
                // application epoch で受理しない（`tls::handshake` が
                // type 24 を閉じた語彙の外として拒否済みであることと
                // あわせ、KeyUpdate を受信したら接続を終了する契約を
                // ここで確実に満たす）。復号済みの平文はこの拒否経路でも
                // 他のエラー経路と同様に zeroize してから破棄する
                // （鍵素材ではないが、モジュールの衛生方針との一貫性のため）。
                if inner.content_type == ContentType::Handshake {
                    zeroize(&mut inner.content);
                    return Err(ProtectionError::ForbiddenInnerType(
                        ContentType::Handshake.as_u8(),
                    ));
                }
                Ok(inner)
            }
        }
    }
}

/// `Handshake`／`Application` epoch 共通の open 本体。外側 content type・
/// 暗号文長の検査 → AEAD 復号 → `TLSInnerPlaintext` の復元、という順序で
/// 判定する（application epoch 固有の追加拒否は呼び出し元が行う）。
fn open_protected(
    cipher: &mut RecordCipher,
    record: &Record,
) -> Result<InnerPlaintext, ProtectionError> {
    if record.content_type != ContentType::ApplicationData {
        return Err(ProtectionError::UnexpectedOuterType);
    }
    let ciphertext_and_tag_len = record.fragment.len();
    if !(TAG_LEN + 1..=record::MAX_CIPHERTEXT_LEN).contains(&ciphertext_and_tag_len) {
        return Err(ProtectionError::BadRecordMac);
    }
    let nonce = cipher.peek_nonce()?;
    let aad = open_aad(record, ciphertext_and_tag_len)?;
    let mut plaintext = cipher
        .cipher
        .open(&nonce, &aad, &record.fragment)
        .map_err(ProtectionError::from)?;
    // タグ検証・復号のいずれかが失敗した場合はここへ到達せず、シーケンス
    // 番号も進めない（module doc・§5.5 の整理どおり）。
    cipher.advance()?;
    let result = decode_inner_plaintext(&plaintext);
    zeroize(&mut plaintext);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::hkdf::HASH_LEN;
    use crate::tls::key_schedule::EarlySecret;
    use crate::tls::x25519::EphemeralSecret;

    fn dummy_keys(key_byte: u8, iv_byte: u8) -> TrafficKeys {
        // TrafficKeys は key_schedule 経由でしか構築できないため、単体
        // テストでは実際に鍵スケジュールを 1 回通してから traffic_keys()
        // を呼ぶ（テスト専用の意図的に単純な入力）。
        let early = EarlySecret::new_without_psk();
        let server_priv = [key_byte; 32];
        let client_pub_raw = [iv_byte; 32];
        let secret = EphemeralSecret::from_bytes(server_priv);
        let shared = secret
            .diffie_hellman(&client_pub_raw)
            .expect("non-zero shared secret for distinct key/iv seed bytes");
        let hs = early.into_handshake(&shared).expect("valid HKDF params");
        let th = [0u8; HASH_LEN];
        let traffic = hs.traffic_secrets(&th).expect("valid HKDF params");
        traffic.server.traffic_keys().expect("valid HKDF params")
    }

    // ---- nonce の導出 ----

    #[test]
    fn nonce_at_seq_zero_equals_iv() {
        let keys = dummy_keys(1, 2);
        let cipher = RecordCipher::new(&keys);
        assert_eq!(cipher.nonce(), *keys.iv());
    }

    #[test]
    fn nonce_xors_only_the_low_8_bytes() {
        let keys = dummy_keys(3, 4);
        let mut cipher = RecordCipher::new(&keys);
        cipher.seq = 1;
        let nonce = cipher.nonce();
        let iv = *keys.iv();
        // 上位 4 バイトは変化しない。
        assert_eq!(&nonce[..4], &iv[..4]);
        // 下位 8 バイトは iv と (seq の be_bytes) の XOR。seq=1 の
        // be_bytes は末尾バイトのみ 1 なので、末尾だけ反転させればよい。
        let mut expected_low = [0u8; 8];
        expected_low.copy_from_slice(&iv[4..]);
        expected_low[7] ^= 1;
        assert_eq!(&nonce[4..], &expected_low[..]);
    }

    #[test]
    fn nonce_differs_for_seq_0_1_and_u64_max_neighbourhood() {
        let keys = dummy_keys(5, 6);
        let mut cipher = RecordCipher::new(&keys);
        let n0 = cipher.nonce();
        cipher.seq = 1;
        let n1 = cipher.nonce();
        assert_ne!(n0, n1);
        cipher.seq = u64::MAX - 1;
        let n_max_minus_1 = cipher.nonce();
        cipher.seq = u64::MAX;
        let n_max = cipher.nonce();
        assert_ne!(n_max_minus_1, n_max);
        assert_ne!(n0, n_max);
    }

    // ---- Sealer/Opener の遷移 ----

    #[test]
    fn install_handshake_then_application_succeeds_and_resets_seq() {
        let keys = dummy_keys(7, 8);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("first install");
        sealer
            .seal(ContentType::Handshake, b"hello", 0)
            .expect("seal under handshake keys");
        if let Epoch::Handshake(c) = &sealer.epoch {
            assert_eq!(c.seq, 1);
        } else {
            panic!("expected Handshake epoch");
        }
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        if let Epoch::Application(c) = &sealer.epoch {
            assert_eq!(c.seq, 0, "seq must reset on epoch transition");
        } else {
            panic!("expected Application epoch");
        }
    }

    #[test]
    fn double_install_handshake_is_rejected() {
        let keys = dummy_keys(9, 10);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("first install");
        assert_eq!(
            sealer.install_handshake_keys(&keys),
            Err(ProtectionError::InvalidTransition)
        );
    }

    #[test]
    fn plaintext_to_application_direct_transition_is_rejected() {
        let keys = dummy_keys(11, 12);
        let mut sealer = Sealer::new();
        assert_eq!(
            sealer.install_application_keys(&keys),
            Err(ProtectionError::InvalidTransition)
        );
    }

    #[test]
    fn application_to_handshake_backwards_transition_is_rejected() {
        let keys = dummy_keys(13, 14);
        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("first install");
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");
        assert_eq!(
            opener.install_handshake_keys(&keys),
            Err(ProtectionError::InvalidTransition)
        );
    }

    #[test]
    fn opener_record_kind_tracks_epoch() {
        let keys = dummy_keys(15, 16);
        let mut opener = Opener::new();
        assert_eq!(opener.record_kind(), RecordKind::Plaintext);
        opener.install_handshake_keys(&keys).expect("install");
        assert_eq!(opener.record_kind(), RecordKind::Ciphertext);
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");
        assert_eq!(opener.record_kind(), RecordKind::Ciphertext);
    }

    // ---- seal/open の往復 ----

    #[test]
    fn seal_then_open_roundtrips_under_handshake_keys() {
        let keys = dummy_keys(17, 18);
        let mut sealer = Sealer::new();
        let mut opener = Opener::new();
        sealer.install_handshake_keys(&keys).expect("install");
        opener.install_handshake_keys(&keys).expect("install");

        let record = sealer
            .seal(ContentType::Handshake, b"finished-message", 0)
            .expect("seal");
        assert_eq!(record.content_type, ContentType::ApplicationData);

        let inner = opener.open(&record).expect("open");
        assert_eq!(inner.content_type, ContentType::Handshake);
        assert_eq!(inner.content, b"finished-message");
    }

    #[test]
    fn seal_then_open_roundtrips_with_padding() {
        for padding_len in [0usize, 1, 64, MAX_INNER_PLAINTEXT_LEN - 1 - 3] {
            let keys = dummy_keys(19, 20);
            let mut sealer = Sealer::new();
            let mut opener = Opener::new();
            sealer.install_handshake_keys(&keys).expect("install");
            sealer
                .install_application_keys(&keys)
                .expect("handshake -> application");
            opener.install_handshake_keys(&keys).expect("install");
            opener
                .install_application_keys(&keys)
                .expect("handshake -> application");

            let record = sealer
                .seal(ContentType::ApplicationData, b"abc", padding_len)
                .expect("seal");
            let inner = opener.open(&record).expect("open");
            assert_eq!(inner.content_type, ContentType::ApplicationData);
            assert_eq!(inner.content, b"abc", "padding_len={padding_len}");
        }
    }

    #[test]
    fn seal_at_padding_plus_content_exactly_at_limit_is_accepted_one_more_is_rejected() {
        let keys = dummy_keys(21, 22);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");

        let content = vec![7u8; MAX_INNER_PLAINTEXT_LEN - 1];
        assert!(sealer
            .seal(ContentType::ApplicationData, &content, 0)
            .is_ok());

        let over_content = vec![7u8; MAX_INNER_PLAINTEXT_LEN];
        assert_eq!(
            sealer.seal(ContentType::ApplicationData, &over_content, 0),
            Err(ProtectionError::InnerOverflow)
        );

        let content_with_padding = vec![7u8; MAX_INNER_PLAINTEXT_LEN - 2];
        assert_eq!(
            sealer.seal(ContentType::ApplicationData, &content_with_padding, 2),
            Err(ProtectionError::InnerOverflow)
        );
    }

    #[test]
    fn all_zero_inner_plaintext_is_rejected_as_no_content_type() {
        assert_eq!(
            decode_inner_plaintext(&[0u8; 16]),
            Err(ProtectionError::NoContentType)
        );
    }

    #[test]
    fn inner_type_20_and_unknown_are_rejected() {
        // ChangeCipherSpec(20)。
        let mut buf = vec![0u8; 8];
        buf.push(20);
        assert!(matches!(
            decode_inner_plaintext(&buf),
            Err(ProtectionError::ForbiddenInnerType(20))
        ));

        // 未知値（KeyUpdate 24 を含む）。0 は「全ゼロ」扱いになり
        // NoContentType へ分岐するため、ここでは含めない
        // （`all_zero_inner_plaintext_is_rejected_as_no_content_type` 参照）。
        for unknown in [24u8, 200, 255] {
            let mut buf = vec![0u8; 8];
            buf.push(unknown);
            assert!(matches!(
                decode_inner_plaintext(&buf),
                Err(ProtectionError::ForbiddenInnerType(u)) if u == unknown
            ));
        }
    }

    #[test]
    fn zero_length_handshake_and_alert_inner_content_is_rejected() {
        for ct in [ContentType::Handshake.as_u8(), ContentType::Alert.as_u8()] {
            let buf = vec![ct];
            assert_eq!(
                decode_inner_plaintext(&buf),
                Err(ProtectionError::EmptyContent)
            );
        }
    }

    #[test]
    fn zero_length_application_data_inner_content_is_accepted() {
        let buf = vec![ContentType::ApplicationData.as_u8()];
        let inner = decode_inner_plaintext(&buf).expect("must accept 0-length application data");
        assert_eq!(inner.content_type, ContentType::ApplicationData);
        assert!(inner.content.is_empty());
    }

    #[test]
    fn decode_rejects_plaintext_longer_than_limit() {
        let buf = vec![1u8; MAX_INNER_PLAINTEXT_LEN + 1];
        assert_eq!(
            decode_inner_plaintext(&buf),
            Err(ProtectionError::InnerOverflow)
        );
    }

    #[test]
    fn open_rejects_outer_handshake_type_under_protected_epoch() {
        let keys = dummy_keys(23, 24);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        let mut record = sealer
            .seal(ContentType::Handshake, b"payload", 0)
            .expect("seal");
        record.content_type = ContentType::Handshake; // 外側型を偽装。

        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            opener.open(&record),
            Err(ProtectionError::UnexpectedOuterType)
        );
    }

    #[test]
    fn open_rejects_ciphertext_shorter_than_tag_plus_one() {
        let keys = dummy_keys(25, 26);
        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        let record = Record {
            content_type: ContentType::ApplicationData,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: vec![0u8; TAG_LEN],
        };
        assert_eq!(opener.open(&record), Err(ProtectionError::BadRecordMac));
    }

    #[test]
    fn application_epoch_open_rejects_inner_handshake_type() {
        let keys = dummy_keys(27, 28);

        // Sealer は application epoch で内側 Handshake を送出しないため、
        // 受信側の防御を直接暗号化した fixture で固定する（seq 0 なので
        // nonce は iv と等しい）。
        let inner = build_inner_plaintext(ContentType::Handshake, b"key-update-like", 0)
            .expect("inner plaintext");
        let aad = seal_aad(inner.len() + TAG_LEN).expect("aad within u16 range");
        let sealed = Aes128Gcm::new(keys.key())
            .seal(keys.iv(), &aad, &inner)
            .expect("seal inner handshake directly");
        let record = Record {
            content_type: ContentType::ApplicationData,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: sealed,
        };

        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");
        assert_eq!(
            opener.open(&record),
            Err(ProtectionError::ForbiddenInnerType(
                ContentType::Handshake.as_u8()
            ))
        );
    }

    fn application_pair(seed: u8) -> (Sealer, Opener) {
        let keys = dummy_keys(seed, seed + 1);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");
        (sealer, opener)
    }

    fn assert_application_seq_is_zero(sealer: &Sealer) {
        if let Epoch::Application(c) = &sealer.epoch {
            assert_eq!(c.seq, 0, "rejected seal must not consume seq");
        } else {
            panic!("expected Application epoch");
        }
    }

    #[test]
    fn application_epoch_seal_rejects_handshake_without_consuming_seq() {
        let (mut sealer, mut opener) = application_pair(101);
        assert_eq!(
            sealer.seal(ContentType::Handshake, b"x", 0),
            Err(ProtectionError::SendContractViolation)
        );
        // 判定順の固定: 空・上限超過でも content type 違反が先に返る。
        assert_eq!(
            sealer.seal(ContentType::Handshake, b"", 0),
            Err(ProtectionError::SendContractViolation)
        );
        let oversized = vec![1u8; MAX_INNER_PLAINTEXT_LEN + 1];
        assert_eq!(
            sealer.seal(ContentType::Handshake, &oversized, 0),
            Err(ProtectionError::SendContractViolation)
        );
        assert_application_seq_is_zero(&sealer);

        // seq 0 で seal されたことを受信側で確認する。
        let record = sealer
            .seal(ContentType::ApplicationData, b"after", 0)
            .expect("seal");
        let inner = opener.open(&record).expect("open at seq 0");
        assert_eq!(inner.content_type, ContentType::ApplicationData);
        assert_eq!(inner.content, b"after");
    }

    #[test]
    fn application_epoch_seal_fragmented_rejects_handshake_without_consuming_seq() {
        let (mut sealer, mut opener) = application_pair(103);
        let large = vec![2u8; MAX_INNER_PLAINTEXT_LEN * 2];
        assert_eq!(
            sealer.seal_fragmented(ContentType::Handshake, &large),
            Err(ProtectionError::SendContractViolation)
        );
        assert_eq!(
            sealer.seal_fragmented(ContentType::Handshake, b"small"),
            Err(ProtectionError::SendContractViolation)
        );
        // 空 Handshake は epoch に関係なく EmptyContent を先に返す。
        assert_eq!(
            sealer.seal_fragmented(ContentType::Handshake, b""),
            Err(ProtectionError::EmptyContent)
        );
        assert_application_seq_is_zero(&sealer);

        let records = sealer
            .seal_fragmented(ContentType::ApplicationData, b"after")
            .expect("seal");
        let first = records.first().expect("one record");
        assert_eq!(opener.open(first).expect("open at seq 0").content, b"after");
    }

    #[test]
    fn seal_detached_rejects_handshake_in_application_epoch() {
        let (sealer, _opener) = application_pair(105);
        assert_eq!(
            sealer.seal_detached(ContentType::Handshake, b"x"),
            Err(ProtectionError::SendContractViolation)
        );
        assert_eq!(
            sealer.seal_detached(ContentType::Handshake, b""),
            Err(ProtectionError::SendContractViolation)
        );
        assert_application_seq_is_zero(&sealer);
    }

    #[test]
    fn application_epoch_seal_still_allows_alert_and_application_data() {
        let (mut sealer, mut opener) = application_pair(107);
        let alert = sealer
            .seal_fragmented(ContentType::Alert, &[1, 0])
            .expect("seal alert");
        let data = sealer
            .seal_fragmented(ContentType::ApplicationData, b"payload")
            .expect("seal data");
        let detached = sealer
            .seal_detached(ContentType::ApplicationData, b"emergency")
            .expect("seal detached");
        let single = sealer.seal(ContentType::Alert, &[1, 0], 0).expect("seal");

        let a = opener
            .open(alert.first().expect("alert record"))
            .expect("open alert");
        assert_eq!(a.content_type, ContentType::Alert);
        assert_eq!(a.content, [1, 0]);
        let d = opener
            .open(data.first().expect("data record"))
            .expect("open data");
        assert_eq!(d.content_type, ContentType::ApplicationData);
        assert_eq!(d.content, b"payload");
        // detached は seq を消費せず次の通常 seal と nonce を共有するため、
        // ここでは open せず形だけ確認する。
        assert_eq!(detached.content_type, ContentType::ApplicationData);
        let s = opener.open(&single).expect("open single alert");
        assert_eq!(s.content_type, ContentType::Alert);
    }

    #[test]
    fn handshake_epoch_seal_rejects_application_data() {
        // 0-RTT 非対応の本実装では、Finished 完了前（client 認証成立前）の
        // Handshake epoch から ApplicationData を送信できてはならない。
        let keys = dummy_keys(43, 44);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            sealer.seal(ContentType::ApplicationData, b"too early", 0),
            Err(ProtectionError::SendContractViolation)
        );
    }

    #[test]
    fn handshake_epoch_open_rejects_inner_application_data_type() {
        // 送信側の契約（`handshake_epoch_seal_rejects_application_data`）と
        // 対称: 万一 Handshake epoch で ApplicationData 相当の内側 content
        // type が復号できても、そのまま呼び出し元へは返さず拒否する。
        let keys = dummy_keys(45, 46);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let record = sealer
            .seal(ContentType::ApplicationData, b"too early", 0)
            .expect("seal under application epoch for test setup");

        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            opener.open(&record),
            Err(ProtectionError::ForbiddenInnerType(
                ContentType::ApplicationData.as_u8()
            ))
        );
    }

    #[test]
    fn tampered_ciphertext_tag_and_wrong_seq_are_all_rejected_without_producing_plaintext() {
        let keys = dummy_keys(29, 30);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let record = sealer
            .seal(ContentType::ApplicationData, b"secret payload", 0)
            .expect("seal");

        // 暗号文 1 ビット反転。
        let mut flipped_ct = record.clone();
        if let Some(byte) = flipped_ct.fragment.first_mut() {
            *byte ^= 0x01;
        }
        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        assert_eq!(opener.open(&flipped_ct), Err(ProtectionError::BadRecordMac));

        // タグ末尾 1 バイト反転。
        let mut flipped_tag = record.clone();
        if let Some(byte) = flipped_tag.fragment.last_mut() {
            *byte ^= 0x80;
        }
        let mut opener2 = Opener::new();
        opener2.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            opener2.open(&flipped_tag),
            Err(ProtectionError::BadRecordMac)
        );

        // タグ切り詰め。
        let mut truncated_tag = record.clone();
        truncated_tag
            .fragment
            .truncate(truncated_tag.fragment.len() - 1);
        let mut opener3 = Opener::new();
        opener3.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            opener3.open(&truncated_tag),
            Err(ProtectionError::BadRecordMac)
        );

        // 誤った seq（seq=1 で開けようとする。まず 1 件ダミーを open して
        // 進めてから、seq=0 用にsealされた同じレコードを再度 open する）。
        let mut opener4 = Opener::new();
        opener4.install_handshake_keys(&keys).expect("install");
        opener4
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let mut sealer4 = Sealer::new();
        sealer4.install_handshake_keys(&keys).expect("install");
        sealer4
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let dummy = sealer4
            .seal(ContentType::ApplicationData, b"dummy", 0)
            .expect("seal seq=0");
        opener4.open(&dummy).expect("open seq=0 dummy");
        // opener4 は今 seq=1 を期待するが、record は seq=0 で seal されている。
        assert_eq!(opener4.open(&record), Err(ProtectionError::BadRecordMac));
    }

    #[test]
    fn seal_fragmented_alert_does_not_split_and_rejects_oversized() {
        let keys = dummy_keys(31, 32);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        let records = sealer
            .seal_fragmented(ContentType::Alert, b"\x02\x00")
            .expect("seal alert");
        assert_eq!(records.len(), 1);

        let oversized = vec![0u8; MAX_INNER_PLAINTEXT_LEN];
        assert_eq!(
            sealer.seal_fragmented(ContentType::Alert, &oversized),
            Err(ProtectionError::InnerOverflow)
        );
    }

    #[test]
    fn seal_fragmented_application_data_splits_and_roundtrips() {
        let keys = dummy_keys(33, 34);
        let mut sealer = Sealer::new();
        let mut opener = Opener::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        opener.install_handshake_keys(&keys).expect("install");
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");

        let payload = vec![9u8; (MAX_INNER_PLAINTEXT_LEN - 1) * 2 + 5];
        let records = sealer
            .seal_fragmented(ContentType::ApplicationData, &payload)
            .expect("seal fragmented");
        assert_eq!(records.len(), 3);

        let mut recovered = Vec::new();
        for record in &records {
            let inner = opener.open(record).expect("open fragment");
            assert_eq!(inner.content_type, ContentType::ApplicationData);
            recovered.extend_from_slice(&inner.content);
        }
        assert_eq!(recovered, payload);
    }

    #[test]
    fn seal_fragmented_empty_application_data_yields_no_records() {
        let keys = dummy_keys(35, 36);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        let records = sealer
            .seal_fragmented(ContentType::ApplicationData, &[])
            .expect("seal empty");
        assert!(records.is_empty());
    }

    #[test]
    fn seal_fragmented_empty_handshake_is_rejected() {
        let keys = dummy_keys(37, 38);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            sealer.seal_fragmented(ContentType::Handshake, &[]),
            Err(ProtectionError::EmptyContent)
        );
    }

    // ---- シーケンス番号の上限 ----

    #[test]
    fn sequence_exhausted_just_before_limit_rejects_seal_with_no_alert() {
        let keys = dummy_keys(39, 40);
        let cipher = RecordCipher::with_seq_for_test(&keys, MAX_RECORDS_PER_KEY - 1);
        let mut sealer = Sealer {
            epoch: Epoch::Application(cipher),
        };
        // MAX_RECORDS_PER_KEY - 1 の 1 回はまだ受理される。
        sealer
            .seal(ContentType::ApplicationData, b"last one", 0)
            .expect("last allowed record");
        // 次（MAX_RECORDS_PER_KEY 回目）は SequenceExhausted。
        let err = sealer
            .seal(ContentType::ApplicationData, b"one too many", 0)
            .expect_err("must be exhausted");
        assert_eq!(err, ProtectionError::SequenceExhausted);
        assert_eq!(err.alert_description(), None);
    }

    #[test]
    fn sequence_exhausted_also_applies_to_open() {
        let keys = dummy_keys(41, 42);
        let cipher = RecordCipher::with_seq_for_test(&keys, MAX_RECORDS_PER_KEY);
        let mut opener = Opener {
            epoch: Epoch::Handshake(cipher),
        };
        let record = Record {
            content_type: ContentType::ApplicationData,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: vec![0u8; TAG_LEN + 1],
        };
        assert_eq!(
            opener.open(&record),
            Err(ProtectionError::SequenceExhausted)
        );
    }

    #[test]
    fn u64_max_seq_wrap_is_rejected_as_sequence_exhausted() {
        let keys = dummy_keys(43, 44);
        let mut cipher = RecordCipher::with_seq_for_test(&keys, u64::MAX);
        // peek_nonce 自体は budget チェック（MAX_RECORDS_PER_KEY 未満か）で
        // 先に弾かれるため、advance 単体の checked_add 保護も直接確認する。
        assert!(cipher.seq >= MAX_RECORDS_PER_KEY);
        assert_eq!(cipher.advance(), Err(ProtectionError::SequenceExhausted));
    }

    // ---- 受信側 InnerOverflow（Issue #959 レビュー指摘 1） ----

    /// `Sealer` は送信前に `MAX_INNER_PLAINTEXT_LEN` 超過を弾くため、この
    /// 受信側分岐（`decode_inner_plaintext` の長さ検査）を経由できない。
    /// ここでは `Aes128Gcm::seal` を直接使い、`Sealer` を経由せず
    /// `MAX_INNER_PLAINTEXT_LEN` を 1 バイト超える平文を暗号化した
    /// レコードを組み立てて `Opener::open` に渡し、AEAD 認証には成功する
    /// が `record_overflow`（[`ProtectionError::InnerOverflow`]）で拒否
    /// されることを固定する。
    #[test]
    fn open_rejects_inner_plaintext_longer_than_limit_after_successful_aead_auth() {
        let keys = dummy_keys(47, 48);
        let cipher = Aes128Gcm::new(keys.key());

        let plaintext = vec![7u8; MAX_INNER_PLAINTEXT_LEN + 1];
        let ciphertext_and_tag_len = plaintext.len() + TAG_LEN;
        assert!(ciphertext_and_tag_len <= record::MAX_CIPHERTEXT_LEN);
        let aad = seal_aad(ciphertext_and_tag_len).expect("aad within u16 range");
        let sealed = cipher
            .seal(keys.iv(), &aad, &plaintext)
            .expect("seal oversized inner plaintext directly");

        let record = Record {
            content_type: ContentType::ApplicationData,
            legacy_version: record::LEGACY_RECORD_VERSION,
            fragment: sealed,
        };

        let mut opener = Opener::new();
        opener.install_handshake_keys(&keys).expect("install");
        assert_eq!(opener.open(&record), Err(ProtectionError::InnerOverflow));
    }

    // ---- Plaintext epoch の Sealer/Opener（Issue #959 レビュー指摘 2） ----

    #[test]
    fn plaintext_epoch_seal_then_open_roundtrips_handshake_and_alert() {
        for ct in [ContentType::Handshake, ContentType::Alert] {
            let mut sealer = Sealer::new();
            let mut opener = Opener::new();
            let record = sealer.seal(ct, b"clienthello-ish", 0).expect("seal");
            assert_eq!(record.content_type, ct);
            let inner = opener.open(&record).expect("open");
            assert_eq!(inner.content_type, ct);
            assert_eq!(inner.content, b"clienthello-ish");
        }
    }

    #[test]
    fn plaintext_epoch_seal_rejects_application_data_and_change_cipher_spec() {
        let mut sealer = Sealer::new();
        assert_eq!(
            sealer.seal(ContentType::ApplicationData, b"x", 0),
            Err(ProtectionError::SendContractViolation)
        );
        assert_eq!(
            sealer.seal(ContentType::ChangeCipherSpec, b"x", 0),
            Err(ProtectionError::SendContractViolation)
        );
    }

    #[test]
    fn plaintext_epoch_seal_rejects_nonzero_padding() {
        let mut sealer = Sealer::new();
        assert_eq!(
            sealer.seal(ContentType::Handshake, b"x", 1),
            Err(ProtectionError::SendContractViolation)
        );
    }

    #[test]
    fn plaintext_epoch_seal_rejects_empty_content() {
        let mut sealer = Sealer::new();
        assert_eq!(
            sealer.seal(ContentType::Handshake, b"", 0),
            Err(ProtectionError::EmptyContent)
        );
    }

    #[test]
    fn plaintext_epoch_open_rejects_application_data_and_change_cipher_spec() {
        let mut opener = Opener::new();
        for ct in [ContentType::ApplicationData, ContentType::ChangeCipherSpec] {
            let record = Record {
                content_type: ct,
                legacy_version: record::LEGACY_RECORD_VERSION,
                fragment: vec![1u8],
            };
            assert_eq!(
                opener.open(&record),
                Err(ProtectionError::UnexpectedOuterType)
            );
        }
    }

    // ---- seal_detached（RECOVER-6・Issue #1080） ----

    #[test]
    fn seal_detached_matches_seal_output_for_same_state() {
        let keys = dummy_keys(49, 50);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");

        let detached = sealer
            .seal_detached(ContentType::ApplicationData, b"emergency")
            .expect("seal_detached");
        let sealed = sealer
            .seal(ContentType::ApplicationData, b"emergency", 0)
            .expect("seal");
        // 決定的 AEAD のため、同一状態からの出力は完全一致する。
        assert_eq!(detached, sealed);
    }

    #[test]
    fn seal_detached_does_not_advance_sequence_number() {
        let keys = dummy_keys(51, 52);
        let mut sealer = Sealer::new();
        let mut opener = Opener::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        opener.install_handshake_keys(&keys).expect("install");
        opener
            .install_application_keys(&keys)
            .expect("handshake -> application");

        let _detached = sealer
            .seal_detached(ContentType::ApplicationData, b"emergency")
            .expect("seal_detached");
        // seq が消費されていなければ、続く通常 seal は seq=0 のままで、
        // 受信側も seq=0 のまま復号に成功する（往復確認）。
        let record = sealer
            .seal(ContentType::ApplicationData, b"normal", 0)
            .expect("seal after detached");
        let inner = opener.open(&record).expect("open normal record");
        assert_eq!(inner.content, b"normal");
    }

    #[test]
    fn seal_detached_rejects_plaintext_and_handshake_epoch() {
        let keys = dummy_keys(53, 54);
        let mut sealer = Sealer::new();
        assert_eq!(
            sealer.seal_detached(ContentType::ApplicationData, b"x"),
            Err(ProtectionError::SendContractViolation)
        );
        sealer.install_handshake_keys(&keys).expect("install");
        assert_eq!(
            sealer.seal_detached(ContentType::ApplicationData, b"x"),
            Err(ProtectionError::SendContractViolation)
        );
    }

    #[test]
    fn seal_detached_rejects_oversized_content() {
        let keys = dummy_keys(55, 56);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        sealer
            .install_application_keys(&keys)
            .expect("handshake -> application");
        let oversized = vec![7u8; MAX_INNER_PLAINTEXT_LEN];
        assert_eq!(
            sealer.seal_detached(ContentType::ApplicationData, &oversized),
            Err(ProtectionError::InnerOverflow)
        );
    }

    #[test]
    fn seal_detached_reports_sequence_exhausted_without_panicking() {
        let keys = dummy_keys(57, 58);
        let cipher = RecordCipher::with_seq_for_test(&keys, MAX_RECORDS_PER_KEY);
        let sealer = Sealer {
            epoch: Epoch::Application(cipher),
        };
        assert_eq!(
            sealer.seal_detached(ContentType::ApplicationData, b"x"),
            Err(ProtectionError::SequenceExhausted)
        );
    }

    // ---- Debug 秘匿 ----

    #[test]
    fn debug_output_does_not_expose_key_material() {
        let keys = dummy_keys(45, 46);
        let mut sealer = Sealer::new();
        sealer.install_handshake_keys(&keys).expect("install");
        let debug_str = format!("{sealer:?}");
        assert!(debug_str.contains("redacted"));
        // 個々のバイトの 2 桁 16 進表現は偶然 "redacted"／"Handshake" 中の
        // 文字列と衝突しうる（例: "da" は "re-d-a-cted" に現れる）ため、
        // 鍵全体の 32 桁 16 進表現という衝突しようのない単位で検査する。
        let key_hex: String = keys.key().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!debug_str.contains(&key_hex));
    }
}
