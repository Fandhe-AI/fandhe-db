//! 検索結果の同点順序キーと共通比較関数（Issue #2195。ビヘイビア: TABLE-23・SEARCH-1・
//! SQL-4。設計記録: `docs/design/rowid-primary-key-separation.md` の D9）。
//!
//! 責務境界: 「行の識別（[`RowId`]）」と「同点順序キー（[`TieKey`]）」を型で分ける。
//! kernel の Top-k 選出・結果整列など、スコアが同点のときの順序を決める比較は本モジュールを
//! 経由する。順序キーの具体的な意味は spec のポインタ（TABLE-23・SEARCH-1・SQL-4）を正とし、
//! 本コメントには記載しない。
//!
//! 差し込み点: [`tie_key_for`] は従来方式（`RowIdentity::LegacyId`）の表でのみ到達する
//! ため、現状は rowid の生値の昇順を順序キーとする（移行前の「同点は id 昇順」と一致）。
//! 採番方式の表のゲートを外す後続 sub-issue（#2197・#2199）が、この関数の実装を差し替える。
//!
//! 不変条件: 整列は安定ソートのみを使う（`scripts/check_sort_determinism.sh` が検査）。

use std::cmp::Ordering;

use crate::storage::rowid::RowId;

/// 同点順序キー。`Ord` は「昇順に並べたとき先に来るほうが小さい」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TieKey(u64);

/// rowid から同点順序キーを得る（差し込み点。モジュール doc 参照）。
pub(crate) const fn tie_key_for(rowid: RowId) -> TieKey {
    TieKey(rowid.legacy_id())
}

/// 公開型へ詰め済みの応答 `id`（従来方式の表では rowid の生値と同一）から順序キーを得る。
/// kernel の `CandidateHit.id` のように rowid へ戻せない位置の比較で使う。
pub(crate) const fn tie_key_for_response_id(id: u64) -> TieKey {
    tie_key_for(RowId::from_legacy_id(id))
}

/// 「スコア降順 → 同点は順序キー昇順」の全順序（`total_cmp` で NaN も含め決定的）。
pub(crate) fn cmp_score_desc_then_tie(
    a_score: f32,
    a_tie: TieKey,
    b_score: f32,
    b_tie: TieKey,
) -> Ordering {
    b_score.total_cmp(&a_score).then(a_tie.cmp(&b_tie))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tie_key_order_matches_legacy_id_order() {
        let a = tie_key_for(RowId::from_legacy_id(1));
        let b = tie_key_for(RowId::from_legacy_id(2));
        assert!(a < b);
        assert_eq!(
            tie_key_for_response_id(5),
            tie_key_for(RowId::from_legacy_id(5))
        );
    }

    #[test]
    fn score_desc_then_tie_ascending() {
        let k1 = tie_key_for_response_id(1);
        let k2 = tie_key_for_response_id(2);
        assert_eq!(cmp_score_desc_then_tie(0.9, k2, 0.5, k1), Ordering::Less);
        assert_eq!(cmp_score_desc_then_tie(0.5, k1, 0.5, k2), Ordering::Less);
        assert_eq!(cmp_score_desc_then_tie(0.5, k2, 0.5, k1), Ordering::Greater);
    }
}
