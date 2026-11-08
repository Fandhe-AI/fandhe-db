//! 復元をまたぐ世代の高水位を持つ状態ファイルの純粋なコーデック（Issue #3105・RECOVER-17 のポインタ。
//! 設計は `docs/design/restore-generation.md`）。
//!
//! 責務境界: バイト列と状態の相互変換だけを持ち、I/O・ロック・パスの検査は呼び出し元の wire-server
//! （`restore --generation-state`）が担う。`Storage::advance_restore_generation_above` に渡す高水位を
//! [`decode_high_water`] で読み、新しい世代を [`encode_next_slot`] で書く。
//!
//! # 形式（固定長 64 バイト = 32 バイトのスロット × 2。A/B を交互に書く）
//!
//! 各スロットは `magic(8) | seq(u64 LE) | value(u64 LE) | checksum(8)`。checksum は先頭 24 バイトの
//! SHA-256 の先頭 8 バイトで、書きかけ・破損の検出用（認証ではない）。
//!
//! # 読み取りの規則（fail-closed）
//!
//! - 長さ 0 と全 0 の 64 バイトは新規（高水位 0。作成直後・初回書き込みの途中で止まった状態）
//! - それ以外で長さが 64 でなければ拒否する
//! - 両側が有効なら `seq` が大きい方を採る。片側が全 0（未書き込み）なのは、もう一方が `seq == 1`
//!   （初回の書き込みだけが済んだ状態）のときに限り許す
//! - 片側でも全 0 以外の無効なスロット（checksum 不正・書きかけ・破損）があれば拒否する。公開済みの最新の
//!   スロットが壊れた場合と書きかけで止まった場合を区別できず、古い側を採ると公開済みの世代を再利用して
//!   しまうため（復元の公開は記録の fsync の後なので、書きかけで止まった場合も運用者の確認を要する）
//! - 検査を通ったスロットが無い（他のファイルを誤って指定した場合を含む）・値が上限超過・両側の `seq` が
//!   同じなら拒否する。読み取りは何も書かないので、誤指定したファイルの内容を壊さない

use crate::crypto::sha256;
use crate::storage::{Result, StorageError};

use super::MAX_RESTORE_GENERATION;

/// 状態ファイルの長さ（バイト）。読み取りの上限でもある。
pub const HIGH_WATER_FILE_LEN: usize = 64;
const SLOT_LEN: usize = 32;
const MAGIC: &[u8; 8] = b"FDRGHW01";

/// 状態ファイルから読んだ高水位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighWaterState {
    /// これまでに発行した世代の最大値（新規は 0）。
    pub value: u64,
    seq: u64,
    active_slot: usize,
}

fn invalid() -> StorageError {
    StorageError::Codec("invalid restore generation state".to_string())
}

fn checksum(head: &[u8]) -> [u8; 8] {
    let d = sha256::digest(head);
    let mut out = [0u8; 8];
    out.copy_from_slice(&d[..8]);
    out
}

/// スロットが全 0（未書き込み）か。
fn is_zero_slot(slot: &[u8]) -> bool {
    slot.len() == SLOT_LEN && slot.iter().all(|b| *b == 0)
}

/// スロット 1 つを検査して `(seq, value)` にする。形式が合わなければ `None`。
fn parse_slot(slot: &[u8]) -> Option<(u64, u64)> {
    let head = slot.get(..24)?;
    let sum = slot.get(24..32)?;
    if head.get(..8)? != MAGIC || checksum(head) != sum {
        return None;
    }
    let seq = u64::from_le_bytes(head.get(8..16)?.try_into().ok()?);
    let value = u64::from_le_bytes(head.get(16..24)?.try_into().ok()?);
    Some((seq, value))
}

/// 状態ファイルの全内容から高水位を読む（規則はモジュール doc）。
pub fn decode_high_water(bytes: &[u8]) -> Result<HighWaterState> {
    if bytes.is_empty() || (bytes.len() == HIGH_WATER_FILE_LEN && bytes.iter().all(|b| *b == 0)) {
        // 次の書き込みはスロット 0 へ向かう（有効側を 1 と見なす）。
        return Ok(HighWaterState {
            value: 0,
            seq: 0,
            active_slot: 1,
        });
    }
    if bytes.len() != HIGH_WATER_FILE_LEN {
        return Err(invalid());
    }
    let raw_a = bytes.get(..SLOT_LEN).ok_or_else(invalid)?;
    let raw_b = bytes.get(SLOT_LEN..).ok_or_else(invalid)?;
    let a = parse_slot(raw_a);
    let b = parse_slot(raw_b);
    // 全 0 以外の無効なスロットは、最新の公開済みスロットの破損かもしれないので古い側へ戻さず拒否する。
    if (a.is_none() && !is_zero_slot(raw_a)) || (b.is_none() && !is_zero_slot(raw_b)) {
        return Err(invalid());
    }
    let (active_slot, (seq, value)) = match (a, b) {
        (Some(x), Some(y)) if x.0 == y.0 => return Err(invalid()),
        (Some(x), Some(y)) => {
            if x.0 > y.0 {
                (0, x)
            } else {
                (1, y)
            }
        }
        // 未書き込みの側があってよいのは初回の書き込みだけが済んだ状態（seq == 1）のとき。
        (Some(x), None) if x.0 == 1 => (0, x),
        (None, Some(y)) if y.0 == 1 => (1, y),
        (Some(_), None) | (None, Some(_)) => return Err(invalid()),
        (None, None) => return Err(invalid()),
    };
    // 検査を通った値が範囲外なら、古い側へ巻き戻さず拒否する（高水位を下げない）。
    if value > MAX_RESTORE_GENERATION {
        return Err(invalid());
    }
    Ok(HighWaterState {
        value,
        seq,
        active_slot,
    })
}

/// `next` を高水位として書くスロットの `(ファイル内オフセット, 32 バイト)` を作る。有効な側とは逆側へ
/// `seq + 1` で書くので、書きかけで止まっても直前の値が残る。`next` が現在値以下・上限超過なら `Err`。
pub fn encode_next_slot(state: &HighWaterState, next: u64) -> Result<(u64, [u8; SLOT_LEN])> {
    if next <= state.value || next > MAX_RESTORE_GENERATION {
        return Err(invalid());
    }
    let seq = state.seq.checked_add(1).ok_or_else(invalid)?;
    let slot_index: u64 = if state.active_slot == 0 { 1 } else { 0 };
    let mut out = [0u8; SLOT_LEN];
    let (head, tail) = out.split_at_mut(24);
    head[..8].copy_from_slice(MAGIC);
    head[8..16].copy_from_slice(&seq.to_le_bytes());
    head[16..24].copy_from_slice(&next.to_le_bytes());
    tail.copy_from_slice(&checksum(head));
    Ok((slot_index * SLOT_LEN as u64, out))
}

#[cfg(test)]
mod tests;
