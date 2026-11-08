//! アロケータ状態表（redb のシステム表 `allocator_state`）を決定的に壊す試験専用ヘルパー
//! （RECOVER-13 のポインタ・Issue #3101）。
//!
//! std だけに依存し engine の型を使わないため、engine の単体試験と wire-server の結合試験の双方から
//! `#[path]` で同一ソースを取り込める。redb のリーフページ形式（`[0]=1`・`[2..4]=件数`・固定幅 5 バイトの
//! キー領域）を前提に、キー列 `Region(i)・RegionTracker・TransactionId` が並ぶリーフを探して書き換える。
//! redb は旧版のページをファイル内に残しうるため、一致した箇所はすべて書き換える。

/// 壊し方。
#[derive(Debug, Clone, Copy)]
pub enum Corruption {
    /// TransactionId キーの tag を未知の値にする（キー復号の `unreachable!` を踏む想定）。
    InvalidTransactionIdTag,
    /// RegionTracker キーの tag を廃止済みの値にして、必須キーを見えなくする。
    HiddenRegionTracker,
}

fn u16_at(bytes: &[u8], p: usize) -> Option<usize> {
    Some(u16::from_le_bytes([*bytes.get(p)?, *bytes.get(p + 1)?]) as usize)
}

fn u32_at(bytes: &[u8], p: usize) -> Option<usize> {
    let b = bytes.get(p..p + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
}

/// `p` のページが「RegionTracker と TransactionId を持つ表のリーフ」なら、2 つのキーの位置
/// `(RegionTracker, TransactionId)` を返す。キーは固定幅 5 バイトで値だけ可変長のため、リーフは
/// `[1, _, 件数(u16)]`・値の終端オフセット（u32 × 件数）・キー列の順に並ぶ。TransactionId の値は 8 バイト。
fn tracker_and_txid_keys(bytes: &[u8], p: usize) -> Option<(usize, usize)> {
    if *bytes.get(p)? != 1 {
        return None;
    }
    let n = u16_at(bytes, p + 2)?;
    if n < 2 {
        return None;
    }
    let keys = p + 4 + 4 * n;
    let tracker = keys + 5 * (n - 2);
    let txid = tracker + 5;
    if bytes.get(tracker..tracker + 10)? != [4, 0, 0, 0, 0, 5, 0, 0, 0, 0] {
        return None;
    }
    let end_last = u32_at(bytes, p + 4 + 4 * (n - 1))?;
    let end_prev = u32_at(bytes, p + 4 + 4 * (n - 2))?;
    (end_last.checked_sub(end_prev)? == 8).then_some((tracker, txid))
}

/// 書き換えた箇所の数を返す。0 のときは対象のリーフが見つからなかった。
pub fn patch_allocator_state_keys(bytes: &mut [u8], how: Corruption) -> usize {
    let page = bytes
        .get(12..16)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .filter(|s| *s >= 512)
        .unwrap_or(4096);
    let mut patched = 0;
    let mut p = 0usize;
    while p < bytes.len() {
        if let Some((tracker, txid)) = tracker_and_txid_keys(bytes, p) {
            let (at, tag) = match how {
                Corruption::InvalidTransactionIdTag => (txid, 9u8),
                Corruption::HiddenRegionTracker => (tracker, 0u8),
            };
            if let Some(b) = bytes.get_mut(at) {
                *b = tag;
                patched += 1;
            }
        }
        p += page;
    }
    patched
}
