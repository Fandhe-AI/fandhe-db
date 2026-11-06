//! ハイブリッド検索の投影で系統別順位と融合スコアを返す引数なし関数
//! （`hybrid_dense_rank()`・`hybrid_sparse_rank()`・`hybrid_score()`。SEARCH-17・Issue #2137）。
//!
//! 役割: 関数名と結果列（名前・型）の対応、および「使えない場所」の `42601` 拒否文言を
//! 1 箇所に集約する。構文段（`sql::allowlist`）・束縛段（`sql::parser`）・汎用の式束縛器
//! （`sql::udf_call`）が同じ名前判定と文言を共有する。値の生成は実行器（`sql::exec`）が
//! 融合段で退避した系統別順位の対応表から行い、本モジュールは値を持たない。

use crate::catalog::ColumnType;
use crate::sql::allowlist::SqlSurfaceError;

/// 系統別順位・融合スコアの種別（`ProjectedColumn::HybridDetail` の `kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridDetailKind {
    /// 密側の 1 始まりの実効順位（候補に入っていなければ NULL）。
    DenseRank,
    /// 疎側の 1 始まりの実効順位（候補に入っていなければ NULL）。
    SparseRank,
    /// 融合スコア（`Σ 重み / (rrf_k + 順位)`）。
    Score,
}

impl HybridDetailKind {
    /// 3 つの種別を NoSQL 応答の列順（密順位・疎順位・スコア）で並べた配列。
    pub const ALL: [Self; 3] = [Self::DenseRank, Self::SparseRank, Self::Score];

    /// 関数名（大文字小文字を区別しない）から種別を引く。
    pub fn from_function_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "hybrid_dense_rank" => Some(Self::DenseRank),
            "hybrid_sparse_rank" => Some(Self::SparseRank),
            "hybrid_score" => Some(Self::Score),
            _ => None,
        }
    }

    /// 既定の結果列名（小文字の関数名。`AS` 別名が無いとき、および NoSQL の追加列名）。
    pub fn column_name(self) -> &'static str {
        match self {
            Self::DenseRank => "hybrid_dense_rank",
            Self::SparseRank => "hybrid_sparse_rank",
            Self::Score => "hybrid_score",
        }
    }

    /// 結果列の型（順位は int8、スコアは float8）。
    pub fn result_type(self) -> ColumnType {
        match self {
            Self::DenseRank | Self::SparseRank => ColumnType::BigInt,
            Self::Score => ColumnType::Double,
        }
    }
}

/// 名前が 3 関数のいずれかか（大文字小文字を区別しない）。
pub fn is_hybrid_detail_function_name(name: &str) -> bool {
    HybridDetailKind::from_function_name(name).is_some()
}

/// ハイブリッド検索の SELECT リスト最上位以外での使用を拒否する `42601`（固定文言。
/// 利用者の入力は含めない）。
pub(crate) fn misuse_error() -> SqlSurfaceError {
    SqlSurfaceError::unsupported(
        "hybrid_dense_rank(), hybrid_sparse_rank() and hybrid_score() are only allowed \
         in the select list of a hybrid search",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_case_insensitive_and_typed() {
        assert_eq!(
            HybridDetailKind::from_function_name("HYBRID_Score"),
            Some(HybridDetailKind::Score)
        );
        assert_eq!(HybridDetailKind::from_function_name("hybrid"), None);
        assert_eq!(
            HybridDetailKind::DenseRank.result_type(),
            ColumnType::BigInt
        );
        assert_eq!(HybridDetailKind::Score.result_type(), ColumnType::Double);
        for k in HybridDetailKind::ALL {
            assert_eq!(
                HybridDetailKind::from_function_name(k.column_name()),
                Some(k)
            );
        }
    }
}
