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

/// 「距離昇順 → 同点は順序キー昇順」の全順序（コサイン距離カーネル `kernel/cosine.rs` の
/// Top-k 選出が使う。Issue #1987・#2240）。距離は f64 のまま比較する。
///
/// NaN（ゼロノルム等）は符号・ペイロードに依らず全数値より後ろ（遠い側）に置き、
/// NaN 同士は順序キー昇順で並べる（pgvector の float8 順序と同じ。ポインタ: SQL-33 (3)）。
/// `total_cmp` は負の NaN を -inf より前に置くため、NaN は明示分岐で扱う。
pub(crate) fn cmp_distance_asc_then_tie(
    a_dist: f64,
    a_tie: TieKey,
    b_dist: f64,
    b_tie: TieKey,
) -> Ordering {
    match (a_dist.is_nan(), b_dist.is_nan()) {
        (true, true) => a_tie.cmp(&b_tie),
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a_dist.total_cmp(&b_dist).then(a_tie.cmp(&b_tie)),
    }
}

/// 「遠い順（`DESC`）→ 同点は順序キー昇順」の全順序（Issue #2009・SQL-33 (2)）。
///
/// スコアは [`cmp_score_desc_then_tie`] と同じ向き（大きいほど近い）で受け取り、スコア昇順
/// （＝近さの低い順）に並べる。NaN は全数値より遠いものとして先頭に置き、NaN 同士は順序キー
/// 昇順にする。スコア部は昇順経路の `total_cmp` を反転しただけなので、`±0.0` を含め `DESC` は
/// 昇順のスコア順を厳密に逆にした並びになる（同点の順序キーだけは昇順のまま）。
pub(crate) fn cmp_far_first_then_tie(
    a_score: f32,
    a_tie: TieKey,
    b_score: f32,
    b_tie: TieKey,
) -> Ordering {
    match (a_score.is_nan(), b_score.is_nan()) {
        (true, true) => a_tie.cmp(&b_tie),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a_score.total_cmp(&b_score).then(a_tie.cmp(&b_tie)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_asc_then_tie_ascending() {
        let k1 = tie_key_for_response_id(1);
        let k2 = tie_key_for_response_id(2);
        assert_eq!(cmp_distance_asc_then_tie(0.1, k2, 0.5, k1), Ordering::Less);
        assert_eq!(cmp_distance_asc_then_tie(0.5, k1, 0.5, k2), Ordering::Less);
        assert_eq!(
            cmp_distance_asc_then_tie(0.5, k2, 0.5, k1),
            Ordering::Greater
        );
        assert_eq!(cmp_distance_asc_then_tie(0.5, k1, 0.5, k1), Ordering::Equal);
    }

    #[test]
    fn distance_asc_puts_nan_last_regardless_of_sign() {
        let k1 = tie_key_for_response_id(1);
        let k2 = tie_key_for_response_id(2);
        let neg_nan = f64::from_bits(0xFFF8_0000_0000_0000);
        assert!(neg_nan.is_nan());
        for nan in [f64::NAN, neg_nan] {
            for num in [0.0, 2.0, f64::INFINITY, f64::NEG_INFINITY] {
                // NaN 側の順序キーが小さくても数値より後ろ。
                assert_eq!(
                    cmp_distance_asc_then_tie(nan, k1, num, k2),
                    Ordering::Greater
                );
                assert_eq!(cmp_distance_asc_then_tie(num, k2, nan, k1), Ordering::Less);
            }
        }
        // NaN 同士は符号に依らず順序キーで決まる。
        assert_eq!(
            cmp_distance_asc_then_tie(f64::NAN, k2, neg_nan, k1),
            Ordering::Greater
        );
        assert_eq!(
            cmp_distance_asc_then_tie(neg_nan, k1, f64::NAN, k2),
            Ordering::Less
        );
        assert_eq!(
            cmp_distance_asc_then_tie(neg_nan, k1, f64::NAN, k1),
            Ordering::Equal
        );
    }

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

    #[test]
    fn far_first_is_reverse_of_score_order_with_tie_ascending() {
        let k1 = tie_key_for_response_id(1);
        let k2 = tie_key_for_response_id(2);
        // 低スコア（遠い）が先
        assert_eq!(cmp_far_first_then_tie(0.1, k2, 0.9, k1), Ordering::Less);
        // 同点は順序キー昇順
        assert_eq!(cmp_far_first_then_tie(0.5, k1, 0.5, k2), Ordering::Less);
        assert_eq!(cmp_far_first_then_tie(0.5, k2, 0.5, k1), Ordering::Greater);
        // -0.0 は +0.0 より遠い（昇順では +0.0 が先なので厳密に逆）
        assert_eq!(cmp_far_first_then_tie(-0.0, k2, 0.0, k1), Ordering::Less);
        assert_eq!(cmp_score_desc_then_tie(0.0, k2, -0.0, k1), Ordering::Less);
    }

    #[test]
    fn far_first_puts_nan_first() {
        let k1 = tie_key_for_response_id(1);
        let k2 = tie_key_for_response_id(2);
        assert_eq!(
            cmp_far_first_then_tie(f32::NAN, k2, -1e30, k1),
            Ordering::Less
        );
        assert_eq!(
            cmp_far_first_then_tie(1e30, k1, f32::NAN, k2),
            Ordering::Greater
        );
        assert_eq!(
            cmp_far_first_then_tie(f32::NAN, k1, f32::NAN, k2),
            Ordering::Less
        );
    }
}
