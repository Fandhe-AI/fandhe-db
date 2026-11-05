//! スカラー `ORDER BY` のキーセット述語の型・位置の照合・封緘（Issue #2093。ポインタ:
//! NOSQL-23・SQL-25・TASK-253。設計は `docs/design/scalar-order-by-scan.md` の
//! 「キーセットモード」節）。
//!
//! 責務境界: 広域取得（[`crate::sql::scan`]）に「直前ページの最終行より後ろ」を指定するための
//! 土台を engine 側に置く。(1) 実行計画 [`crate::sql::parser::BoundScan`] が持つ
//! [`BoundKeyset`]（利用者キー数と位置）、(2) ページ境界の位置 [`KeysetPosition`]（順序キー値列
//! ＋ `tenant_id`）、(3) 位置の形・型の照合、(4) 位置を呼び出し元へ渡す不透明バイト列への封緘
//! （[`KeysetPosition::seal`]／[`KeysetPosition::open`]）、(5) 実行結果 [`ScanPage`] を提供する。
//!
//! 呼び出し元: wire-server の NoSQL 表層（Issue #2094。`after`／`next_cursor`）が
//! `BoundScan::with_after` と `EngineCore::execute_bound_scan_page_in_session` を呼ぶ。
//! 封緘鍵の生成・保管・ローテーション、AAD（テナント・認証主体・表・`sort` の束縛）の構成、
//! base64 化は #2094 の責務で、ここは暗号と改ざん検知を単一実装として提供するだけにする
//! （呼び出し側で二重実装しない）。
//!
//! 比較規約は既存の `ORDER BY` と同一（[`crate::sql::order_value`] の `compare_nullable` への
//! 委譲。昇降順・複数キー・NULL 位置は構造上一致する）。RLS 適用は呼び出し元（`sql::scan`）の
//! 責務で、本モジュールは行の可視性を判断しない。受信データ経路（[`KeysetPosition::open`]）では
//! `unwrap`／`expect`／添字アクセスを使わず、長さは確保前に上限検査する。

use crate::crypto::hmac_sha256::hmac_sha256;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::QueryResult;
use crate::sql::order_value::OrderValue;
use crate::sql::parser::{BoundOrderKey, BoundScan, OrderKind};

/// 封緘済み位置のバイト長上限（確保前に検査する）。TEXT／JSONB のソート値を含みうるため
/// 余裕を持たせつつ、受信データ起因の確保量を有界にする。
pub const MAX_KEYSET_SEALED_BYTES: usize = 64 * 1024;

/// 平文の正準形の版。
const PLAINTEXT_VERSION: u8 = 1;
/// 封緘済みバイト列の版（先頭 1 バイト）。
const SEALED_VERSION: u8 = 1;
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 32;
/// 版 1 byte ＋ nonce ＋ tag。
const SEALED_OVERHEAD: usize = 1 + NONCE_LEN + TAG_LEN;

const TAG_ID: u8 = 1;
const TAG_BYTES: u8 = 2;
const TAG_COLLATED: u8 = 3;
const TAG_SIGNED_INT: u8 = 4;
const TAG_FLOAT: u8 = 5;
const TAG_BOOL: u8 = 6;
const TAG_NUMERIC: u8 = 7;
const TAG_UUID: u8 = 8;
const TAG_ENUM: u8 = 9;
const TAG_JSONB: u8 = 10;
const TAG_INTERVAL: u8 = 11;

/// 位置の不整合・改ざん・版違いを報告する固定文言の `22000`（値・テナント名・rowid を含めない）。
fn invalid_position() -> SqlSurfaceError {
    SqlSurfaceError::invalid_input("invalid keyset position")
}

/// 実行計画に載るキーセット指定（`BoundScan::keyset`）。`BoundScan::order_by` の末尾
/// `order_by.len() - user_key_count` 件が末尾キー（PK 列／物理 rowid）という不変条件を持つ。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundKeyset {
    /// 利用者が指定した `sort` キーの件数（末尾キーを足す前）。
    pub(crate) user_key_count: usize,
    /// 直前ページの最終行の位置。`None` はキーセットの 1 ページ目（述語なし）。
    pub(crate) after: Option<KeysetPosition>,
}

/// ページ境界の位置（出力順で 1 行を一意に決める値）。順序仕様（利用者キー ＋ 末尾キー）と同数の
/// キー値と、最終同点に使う `tenant_id` を持つ。クレート外からフィールドを読めない
/// （rowid・テナントを呼び出し元へ見せない）。外へ出すときは [`Self::seal`] の不透明バイト列だけ。
#[derive(Debug, Clone, PartialEq)]
pub struct KeysetPosition {
    pub(crate) keys: Vec<Option<OrderValue>>,
    pub(crate) tenant_id: String,
}

/// キーセット付き広域取得の 1 ページ分。`next` が `Some` なら続きの行が存在する。
#[derive(Debug)]
pub struct ScanPage {
    result: QueryResult,
    next: Option<KeysetPosition>,
}

impl ScanPage {
    pub(crate) fn new(result: QueryResult, next: Option<KeysetPosition>) -> Self {
        Self { result, next }
    }

    /// このページの結果行。
    pub fn result(&self) -> &QueryResult {
        &self.result
    }

    /// 次ページの位置（最終ページは `None`）。
    pub fn next(&self) -> Option<&KeysetPosition> {
        self.next.as_ref()
    }

    /// 結果行と次位置に分解する。
    pub fn into_parts(self) -> (QueryResult, Option<KeysetPosition>) {
        (self.result, self.next)
    }
}

/// 位置の封緘・開封に使う 32 バイトの秘密鍵。`Debug` は伏字。生成・保管・ローテーションは
/// 呼び出し元（wire-server。Issue #2094）の責務。
#[derive(Clone)]
pub struct KeysetSealKey([u8; 32]);

impl KeysetSealKey {
    /// 32 バイトの鍵素材から構築する。
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for KeysetSealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeysetSealKey(<redacted>)")
    }
}

/// キーセットとの併用を認めない指定（`offset`・ウィンドウ・`WITH TIES`・暗黙行数上限）を
/// `42601` で拒否する。呼び出し元（wire-server）が先に同じコードを返す前提の多層防御で、
/// `BoundScan::with_after` 時と実行時（`with_offset` が後から呼ばれうる）の両方で呼ぶ。
pub(crate) fn check_combination(bound: &BoundScan) -> Result<(), SqlSurfaceError> {
    // 次位置は「最後に返した行」から作るため、1 件も返さない `LIMIT 0` は位置を定義できない。
    if bound.limit == 0 {
        return Err(SqlSurfaceError::invalid_input(
            "keyset paging requires a LIMIT of at least 1",
        ));
    }
    if bound.offset > 0 {
        return Err(SqlSurfaceError::unsupported(
            "keyset paging cannot be combined with OFFSET",
        ));
    }
    if !bound.windows.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "keyset paging cannot be combined with window functions",
        ));
    }
    if bound.ties_row_cap.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "keyset paging cannot be combined with WITH TIES",
        ));
    }
    if bound.result_cap.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "keyset paging cannot be combined with an implicit row limit",
        ));
    }
    Ok(())
}

/// `value`（`Some`）が順序キー `key` の型・照合に合致するか。
fn value_matches_key(value: &OrderValue, key: &BoundOrderKey) -> bool {
    match (key.kind, value) {
        (OrderKind::Id, OrderValue::Id(_)) => true,
        (OrderKind::Bytes | OrderKind::Bpchar, OrderValue::Bytes(_)) => {
            key.collation.is_byte_order()
        }
        (OrderKind::Bytes | OrderKind::Bpchar, OrderValue::Collated { collation, .. }) => {
            !key.collation.is_byte_order() && *collation == key.collation
        }
        (OrderKind::SignedInt, OrderValue::SignedInt(_))
        | (OrderKind::Float, OrderValue::Float(_))
        | (OrderKind::Bool, OrderValue::Bool(_))
        | (OrderKind::Numeric, OrderValue::Numeric(_))
        | (OrderKind::Uuid, OrderValue::Uuid(_))
        | (OrderKind::Enum, OrderValue::EnumOrdinal(_))
        | (OrderKind::Jsonb, OrderValue::JsonbKey(_))
        | (OrderKind::Interval, OrderValue::Interval(_)) => true,
        _ => false,
    }
}

/// 位置を拡張後の順序仕様 `spec`（利用者キー ＋ 末尾キー）と照合する。キー数・各値の型と
/// 照合の一致・末尾キーが NULL でないことを確認し、不一致は固定文言の `22000`
/// （別表・別 `sort` 用の位置の持ち込み、封緘前の組み立てミスを fail-closed にする）。
pub(crate) fn check_position_shape(
    spec: &[BoundOrderKey],
    user_key_count: usize,
    position: &KeysetPosition,
) -> Result<(), SqlSurfaceError> {
    if position.keys.len() != spec.len() {
        return Err(invalid_position());
    }
    for (idx, (key, value)) in spec.iter().zip(position.keys.iter()).enumerate() {
        match value {
            None if idx >= user_key_count => return Err(invalid_position()),
            None => {}
            Some(v) if !value_matches_key(v, key) => return Err(invalid_position()),
            Some(_) => {}
        }
    }
    Ok(())
}

// --- 正準バイト形式（版 1）の書き出し ------------------------------------------------

fn alloc_err(e: std::collections::TryReserveError) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("failed to reserve keyset position buffer: {e}"),
    }
}

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("keyset position is too large")
}

/// `out` へ追記する（上限超過・確保失敗は `Err`）。
fn put(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    if out.len().saturating_add(bytes.len()) > MAX_KEYSET_SEALED_BYTES - SEALED_OVERHEAD {
        return Err(too_large());
    }
    out.try_reserve(bytes.len()).map_err(alloc_err)?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len()).map_err(|_| too_large())?;
    put(out, &len.to_be_bytes())?;
    put(out, bytes)
}

fn collation_id(c: crate::collation::Collation) -> u8 {
    match c {
        crate::collation::Collation::C => 0,
        crate::collation::Collation::Posix => 1,
        crate::collation::Collation::UndIcu => 2,
        crate::collation::Collation::JaIcu => 3,
    }
}

fn collation_from_id(id: u8) -> Option<crate::collation::Collation> {
    match id {
        0 => Some(crate::collation::Collation::C),
        1 => Some(crate::collation::Collation::Posix),
        2 => Some(crate::collation::Collation::UndIcu),
        3 => Some(crate::collation::Collation::JaIcu),
        _ => None,
    }
}

fn encode_value(out: &mut Vec<u8>, value: &OrderValue) -> Result<(), SqlSurfaceError> {
    match value {
        OrderValue::Id(v) => {
            put(out, &[TAG_ID])?;
            put(out, &v.to_be_bytes())
        }
        OrderValue::Bytes(b) => {
            put(out, &[TAG_BYTES])?;
            put_len_prefixed(out, b)
        }
        OrderValue::Collated { collation, text } => {
            put(out, &[TAG_COLLATED, collation_id(*collation)])?;
            put_len_prefixed(out, text)
        }
        OrderValue::SignedInt(v) => {
            put(out, &[TAG_SIGNED_INT])?;
            put(out, &v.to_be_bytes())
        }
        OrderValue::Float(v) => {
            put(out, &[TAG_FLOAT])?;
            put(out, &v.to_bits().to_be_bytes())
        }
        OrderValue::Bool(v) => put(out, &[TAG_BOOL, u8::from(*v)]),
        OrderValue::Numeric(d) => {
            put(out, &[TAG_NUMERIC])?;
            put(out, &d.unscaled().to_be_bytes())?;
            put(out, &[d.scale()])
        }
        OrderValue::Uuid(u) => {
            put(out, &[TAG_UUID])?;
            put(out, u.as_bytes())
        }
        OrderValue::EnumOrdinal(v) => {
            let v = u32::try_from(*v).map_err(|_| too_large())?;
            put(out, &[TAG_ENUM])?;
            put(out, &v.to_be_bytes())
        }
        OrderValue::JsonbKey(b) => {
            put(out, &[TAG_JSONB])?;
            put_len_prefixed(out, b)
        }
        OrderValue::Interval(i) => {
            put(out, &[TAG_INTERVAL])?;
            put(out, &i.months.to_be_bytes())?;
            put(out, &i.days.to_be_bytes())?;
            put(out, &i.micros.to_be_bytes())
        }
    }
}

/// 位置を正準バイト列（版 1）へ書き出す。
fn encode_plaintext(position: &KeysetPosition) -> Result<Vec<u8>, SqlSurfaceError> {
    let count = u8::try_from(position.keys.len()).map_err(|_| too_large())?;
    let mut out = Vec::new();
    put(&mut out, &[PLAINTEXT_VERSION, count])?;
    for key in &position.keys {
        match key {
            None => put(&mut out, &[0])?,
            Some(v) => encode_value(&mut out, v)?,
        }
    }
    put_len_prefixed(&mut out, position.tenant_id.as_bytes())?;
    Ok(out)
}

// --- 厳密パース（受信データ経路。添字アクセス・unwrap を使わない） -----------------------

/// 残りバイト列を前から消費する読み取り器。
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SqlSurfaceError> {
        let head = self.rest.get(..n).ok_or_else(invalid_position)?;
        self.rest = self.rest.get(n..).ok_or_else(invalid_position)?;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], SqlSurfaceError> {
        self.take(N)?.try_into().map_err(|_| invalid_position())
    }

    fn u8(&mut self) -> Result<u8, SqlSurfaceError> {
        Ok(u8::from_be_bytes(self.array::<1>()?))
    }

    /// 長さ前置の可変長バイト列。長さは残り長で上限検査してから複製する。
    fn len_prefixed(&mut self) -> Result<Vec<u8>, SqlSurfaceError> {
        let len = usize::try_from(u32::from_be_bytes(self.array::<4>()?))
            .map_err(|_| invalid_position())?;
        let bytes = self.take(len)?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(bytes.len()).map_err(alloc_err)?;
        owned.extend_from_slice(bytes);
        Ok(owned)
    }
}

fn decode_value(r: &mut Reader<'_>, tag: u8) -> Result<OrderValue, SqlSurfaceError> {
    Ok(match tag {
        TAG_ID => OrderValue::Id(u64::from_be_bytes(r.array::<8>()?)),
        TAG_BYTES => OrderValue::Bytes(r.len_prefixed()?),
        TAG_COLLATED => {
            let collation = collation_from_id(r.u8()?).ok_or_else(invalid_position)?;
            OrderValue::Collated {
                collation,
                text: r.len_prefixed()?,
            }
        }
        TAG_SIGNED_INT => OrderValue::SignedInt(i64::from_be_bytes(r.array::<8>()?)),
        TAG_FLOAT => OrderValue::Float(f64::from_bits(u64::from_be_bytes(r.array::<8>()?))),
        TAG_BOOL => match r.u8()? {
            0 => OrderValue::Bool(false),
            1 => OrderValue::Bool(true),
            _ => return Err(invalid_position()),
        },
        TAG_NUMERIC => {
            let unscaled = i128::from_be_bytes(r.array::<16>()?);
            let scale = r.u8()?;
            OrderValue::Numeric(
                crate::numeric::Decimal::from_parts(unscaled, scale)
                    .map_err(|_| invalid_position())?,
            )
        }
        TAG_UUID => OrderValue::Uuid(crate::uuid::Uuid::from_bytes(r.array::<16>()?)),
        TAG_ENUM => OrderValue::EnumOrdinal(
            usize::try_from(u32::from_be_bytes(r.array::<4>()?)).map_err(|_| invalid_position())?,
        ),
        TAG_JSONB => OrderValue::JsonbKey(r.len_prefixed()?),
        TAG_INTERVAL => {
            let interval = crate::datetime::Interval {
                months: i32::from_be_bytes(r.array::<4>()?),
                days: i32::from_be_bytes(r.array::<4>()?),
                micros: i64::from_be_bytes(r.array::<8>()?),
            };
            if !crate::datetime::validate_interval(&interval) {
                return Err(invalid_position());
            }
            OrderValue::Interval(interval)
        }
        _ => return Err(invalid_position()),
    })
}

/// 正準バイト列（版 1）を厳密にパースする（未知の版・タグ・余剰バイト・不正 UTF-8 は拒否）。
fn decode_plaintext(bytes: &[u8]) -> Result<KeysetPosition, SqlSurfaceError> {
    let mut r = Reader { rest: bytes };
    if r.u8()? != PLAINTEXT_VERSION {
        return Err(invalid_position());
    }
    let count = usize::from(r.u8()?);
    // 各キーは最低 1 バイトを消費するため、残り長を超える件数は確保前に拒否する。
    if count > r.rest.len() {
        return Err(invalid_position());
    }
    let mut keys = Vec::new();
    keys.try_reserve_exact(count).map_err(alloc_err)?;
    for _ in 0..count {
        let tag = r.u8()?;
        keys.push(if tag == 0 {
            None
        } else {
            Some(decode_value(&mut r, tag)?)
        });
    }
    let tenant_id = String::from_utf8(r.len_prefixed()?).map_err(|_| invalid_position())?;
    if !r.rest.is_empty() {
        return Err(invalid_position());
    }
    Ok(KeysetPosition { keys, tenant_id })
}

// --- 封緘（encrypt-then-MAC。依存追加なし・既存の HMAC-SHA256 のみ） ---------------------

fn derive_subkey(key: &KeysetSealKey, label: &[u8]) -> [u8; 32] {
    hmac_sha256(&key.0, label)
}

/// `nonce ‖ カウンタ` を HMAC に通した keystream を `data` へ XOR する（暗号化・復号共通）。
fn apply_keystream(enc_key: &[u8; 32], nonce: &[u8], data: &mut [u8]) {
    for (block_index, chunk) in data.chunks_mut(32).enumerate() {
        let counter = u32::try_from(block_index).unwrap_or(u32::MAX);
        let mut message = Vec::with_capacity(nonce.len() + 4);
        message.extend_from_slice(nonce);
        message.extend_from_slice(&counter.to_be_bytes());
        let block = hmac_sha256(enc_key, &message);
        for (byte, k) in chunk.iter_mut().zip(block.iter()) {
            *byte ^= k;
        }
    }
}

/// MAC の入力: 版 ‖ nonce ‖ len(暗号文) ‖ 暗号文 ‖ len(aad) ‖ aad（長さは固定幅 u32 BE。
/// 暗号文と AAD の境界が曖昧にならない）。
fn mac_input(nonce: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, SqlSurfaceError> {
    let ct_len = u32::try_from(ciphertext.len()).map_err(|_| too_large())?;
    let aad_len = u32::try_from(aad.len()).map_err(|_| too_large())?;
    let mut input = Vec::new();
    input
        .try_reserve_exact(1 + nonce.len() + 8 + ciphertext.len() + aad.len())
        .map_err(alloc_err)?;
    input.push(SEALED_VERSION);
    input.extend_from_slice(nonce);
    input.extend_from_slice(&ct_len.to_be_bytes());
    input.extend_from_slice(ciphertext);
    input.extend_from_slice(&aad_len.to_be_bytes());
    input.extend_from_slice(aad);
    Ok(input)
}

impl KeysetPosition {
    /// 位置を不透明なバイト列へ封緘する（版 ‖ nonce ‖ 暗号文 ‖ tag）。ペイロード全体
    /// （キー値・rowid・テナント）を暗号化するため、rowid・テナントを平文で含まない。`aad` は
    /// 開封側が同じ値を渡したときだけ通る認証済み付加データ（呼び出し元がテナント・認証主体・表・
    /// `sort` 等へ束縛する）。nonce は OS の CSPRNG で、読めなければ fail-closed（`XX000`）。
    pub fn seal(&self, key: &KeysetSealKey, aad: &[u8]) -> Result<Vec<u8>, SqlSurfaceError> {
        let mut data = encode_plaintext(self)?;
        let nonce = crate::crypto::random::read_urandom(NONCE_LEN).map_err(|_| {
            SqlSurfaceError::Internal {
                detail: "failed to read random bytes for the keyset position".to_string(),
            }
        })?;
        let enc_key = derive_subkey(key, b"fandhe-db keyset enc v1");
        let mac_key = derive_subkey(key, b"fandhe-db keyset mac v1");
        apply_keystream(&enc_key, &nonce, &mut data);
        let tag = hmac_sha256(&mac_key, &mac_input(&nonce, &data, aad)?);
        let mut out = Vec::new();
        out.try_reserve_exact(1 + nonce.len() + data.len() + TAG_LEN)
            .map_err(alloc_err)?;
        out.push(SEALED_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&data);
        out.extend_from_slice(&tag);
        Ok(out)
    }

    /// [`Self::seal`] で封緘したバイト列を開封する。長さ上限を確保前に検査し、MAC を定数時間で
    /// 検証してから復号・厳密パースする。改ざん・切り詰め・別鍵・別 `aad`・版違い・形式不正は
    /// すべて同一の固定文言 `22000`（区別を観測させない）。位置が現在の表・`sort` に合うかの照合は
    /// `BoundScan::with_after` が行う。
    pub fn open(
        key: &KeysetSealKey,
        aad: &[u8],
        sealed: &[u8],
    ) -> Result<KeysetPosition, SqlSurfaceError> {
        if sealed.len() > MAX_KEYSET_SEALED_BYTES || sealed.len() < SEALED_OVERHEAD {
            return Err(invalid_position());
        }
        let mut r = Reader { rest: sealed };
        if r.u8()? != SEALED_VERSION {
            return Err(invalid_position());
        }
        let nonce = r.take(NONCE_LEN)?;
        let body_len = sealed
            .len()
            .checked_sub(SEALED_OVERHEAD)
            .ok_or_else(invalid_position)?;
        let ciphertext = r.take(body_len)?;
        let tag = r.take(TAG_LEN)?;
        let mac_key = derive_subkey(key, b"fandhe-db keyset mac v1");
        let expected = hmac_sha256(&mac_key, &mac_input(nonce, ciphertext, aad)?);
        if !crate::crypto::constant_time_eq(&expected, tag) {
            return Err(invalid_position());
        }
        let mut plaintext = Vec::new();
        plaintext
            .try_reserve_exact(ciphertext.len())
            .map_err(alloc_err)?;
        plaintext.extend_from_slice(ciphertext);
        let enc_key = derive_subkey(key, b"fandhe-db keyset enc v1");
        apply_keystream(&enc_key, nonce, &mut plaintext);
        decode_plaintext(&plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collation::Collation;

    fn sample() -> KeysetPosition {
        KeysetPosition {
            keys: vec![
                Some(OrderValue::Bytes(b"abc".to_vec())),
                None,
                Some(OrderValue::SignedInt(-7)),
                Some(OrderValue::Float(f64::NAN)),
                Some(OrderValue::Bool(true)),
                Some(OrderValue::Numeric(
                    crate::numeric::Decimal::from_parts(-12345, 2).expect("decimal"),
                )),
                Some(OrderValue::Uuid(crate::uuid::Uuid::from_bytes([7; 16]))),
                Some(OrderValue::EnumOrdinal(3)),
                Some(OrderValue::JsonbKey(vec![1, 2, 3])),
                Some(OrderValue::Interval(crate::datetime::Interval {
                    months: 1,
                    days: -2,
                    micros: 3,
                })),
                Some(OrderValue::Collated {
                    collation: Collation::UndIcu,
                    text: b"xyz".to_vec(),
                }),
                Some(OrderValue::Id(0x0102_0304_0506_0708)),
            ],
            tenant_id: "tenant-a".to_string(),
        }
    }

    fn key(b: u8) -> KeysetSealKey {
        KeysetSealKey::from_bytes([b; 32])
    }

    fn assert_invalid(r: Result<KeysetPosition, SqlSurfaceError>) {
        match r {
            Err(SqlSurfaceError::InvalidInput { detail }) => {
                assert_eq!(detail, "invalid keyset position");
            }
            other => panic!("expected invalid keyset position, got {other:?}"),
        }
    }

    #[test]
    fn plaintext_round_trips_every_variant() {
        let pos = sample();
        let bytes = encode_plaintext(&pos).expect("encode");
        let back = decode_plaintext(&bytes).expect("decode");
        // NaN は PartialEq で等しくならないため、ビット表現で比較する。
        assert_eq!(back.tenant_id, pos.tenant_id);
        assert_eq!(back.keys.len(), pos.keys.len());
        for (a, b) in back.keys.iter().zip(pos.keys.iter()) {
            match (a, b) {
                (Some(OrderValue::Float(x)), Some(OrderValue::Float(y))) => {
                    assert_eq!(x.to_bits(), y.to_bits());
                }
                _ => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn seal_open_round_trip() {
        let pos = KeysetPosition {
            keys: vec![Some(OrderValue::SignedInt(5)), Some(OrderValue::Id(42))],
            tenant_id: "acme".to_string(),
        };
        let sealed = pos.seal(&key(1), b"aad").expect("seal");
        let opened = KeysetPosition::open(&key(1), b"aad", &sealed).expect("open");
        assert_eq!(opened, pos);
    }

    #[test]
    fn sealed_bytes_do_not_expose_rowid_or_tenant() {
        let rowid: u64 = 0x1122_3344_5566_7788;
        let pos = KeysetPosition {
            keys: vec![Some(OrderValue::Id(rowid))],
            tenant_id: "visible-tenant-name".to_string(),
        };
        let sealed = pos.seal(&key(2), b"").expect("seal");
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        assert!(!contains(&sealed, &rowid.to_be_bytes()));
        assert!(!contains(&sealed, &rowid.to_le_bytes()));
        assert!(!contains(&sealed, b"visible-tenant-name"));
    }

    #[test]
    fn open_rejects_tampering_truncation_wrong_key_and_wrong_aad() {
        let pos = sample();
        let sealed = pos.seal(&key(1), b"aad").expect("seal");
        for i in 0..sealed.len() {
            let mut t = sealed.clone();
            t[i] ^= 0x01;
            assert_invalid(KeysetPosition::open(&key(1), b"aad", &t));
        }
        for len in 0..sealed.len() {
            assert_invalid(KeysetPosition::open(&key(1), b"aad", &sealed[..len]));
        }
        assert_invalid(KeysetPosition::open(&key(9), b"aad", &sealed));
        assert_invalid(KeysetPosition::open(&key(1), b"other", &sealed));
        let mut extended = sealed.clone();
        extended.push(0);
        assert_invalid(KeysetPosition::open(&key(1), b"aad", &extended));
    }

    #[test]
    fn open_rejects_oversized_input_before_parsing() {
        let big = vec![0u8; MAX_KEYSET_SEALED_BYTES + 1];
        assert_invalid(KeysetPosition::open(&key(1), b"", &big));
    }

    #[test]
    fn seal_rejects_oversized_position() {
        let pos = KeysetPosition {
            keys: vec![Some(OrderValue::Bytes(vec![0; MAX_KEYSET_SEALED_BYTES]))],
            tenant_id: "t".to_string(),
        };
        assert!(matches!(
            pos.seal(&key(1), b""),
            Err(SqlSurfaceError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn decode_rejects_unknown_tag_trailing_bytes_and_bad_version() {
        let pos = KeysetPosition {
            keys: vec![Some(OrderValue::Id(1))],
            tenant_id: "t".to_string(),
        };
        let good = encode_plaintext(&pos).expect("encode");
        assert!(decode_plaintext(&good).is_ok());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_plaintext(&trailing).is_err());
        let mut bad_version = good.clone();
        bad_version[0] = 2;
        assert!(decode_plaintext(&bad_version).is_err());
        let mut bad_tag = good.clone();
        bad_tag[2] = 0xEE;
        assert!(decode_plaintext(&bad_tag).is_err());
        // 長さフィールドが残り長を超える。
        let mut huge_len = encode_plaintext(&KeysetPosition {
            keys: vec![Some(OrderValue::Bytes(b"a".to_vec()))],
            tenant_id: "t".to_string(),
        })
        .expect("encode");
        huge_len[3..7].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_plaintext(&huge_len).is_err());
        assert!(decode_plaintext(&[]).is_err());
    }

    /// 末尾キーの束縛: 単一 PK・複合 PK（宣言順）・BYTEA PK（バイト列順の専用束縛）・PK なし（物理 id）。
    #[test]
    fn tail_binding_follows_primary_key_declaration() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::sql::parser::{bind_keyset_tail, BoundOrderTarget};
        let cols = || {
            vec![
                ColumnDef::new("a", ColumnType::Text, false),
                ColumnDef::new("b", ColumnType::Integer, false),
                ColumnDef::new("c", ColumnType::Bytea, false),
            ]
        };
        let none = bind_keyset_tail(&TableSchema::new("t", cols())).expect("tail");
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].target, BoundOrderTarget::Id);
        assert_eq!(none[0].kind, OrderKind::Id);

        let single = TableSchema::new("t", cols()).with_primary_key(vec!["b".to_string()]);
        let tail = bind_keyset_tail(&single).expect("tail");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].target, BoundOrderTarget::Column(1));
        assert_eq!(tail[0].kind, OrderKind::SignedInt);

        let composite =
            TableSchema::new("t", cols()).with_primary_key(vec!["b".to_string(), "a".to_string()]);
        let tail = bind_keyset_tail(&composite).expect("tail");
        let targets: Vec<_> = tail.iter().map(|k| k.target).collect();
        assert_eq!(
            targets,
            vec![BoundOrderTarget::Column(1), BoundOrderTarget::Column(0)]
        );
        assert!(tail.iter().all(|k| !k.descending));

        let bytea = TableSchema::new("t", cols()).with_primary_key(vec!["c".to_string()]);
        let tail = bind_keyset_tail(&bytea).expect("tail");
        assert_eq!(tail[0].target, BoundOrderTarget::Column(2));
        assert_eq!(tail[0].kind, OrderKind::Bytes);
    }

    /// 末尾専用の BYTEA 抽出腕: バイト列をそのまま借用キーにする。
    #[test]
    fn bytea_tail_value_is_extracted_as_byte_order_key() {
        use crate::catalog::ColumnType;
        use crate::sql::order_value::{extract_column_order_value_ref, ScalarKeyRef};
        use crate::sql::parser::BoundOrderTarget;
        let key = BoundOrderKey {
            target: BoundOrderTarget::Column(0),
            kind: OrderKind::Bytes,
            descending: false,
            nulls: crate::sql::order_value::NullPlacement::Last,
            collation: Collation::DEFAULT,
        };
        let scanned = [Some(crate::row_codec::ScalarRef::Bytes(&[1u8, 2, 3]))];
        let got =
            extract_column_order_value_ref(&ColumnType::Bytea, 0, &key, &scanned).expect("extract");
        assert!(matches!(got, Some(ScalarKeyRef::Bytes(b)) if b == [1u8, 2, 3]));
    }

    #[test]
    fn seal_key_debug_is_redacted() {
        assert_eq!(format!("{:?}", key(7)), "KeysetSealKey(<redacted>)");
    }
}
