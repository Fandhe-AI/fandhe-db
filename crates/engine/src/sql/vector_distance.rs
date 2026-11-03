//! WHERE 内の距離式（`<VECTOR 列> <距離演算子> <ベクトルリテラル>`）の値を求める純関数群
//! （SQL-74 (1)・Issue #2036）。
//!
//! `udf_call::eval_with_scalars` の `BoundExpr::VectorDistance` 腕から行ごとに呼ばれ、float8
//! 相当の `f64` を返す。pgvector と同じく f32 で累積し f64 で返す。コサインは ADR
//! `docs/design/cosine-distance-method.md` 第 6 節の手順（arena を経由しない経路）に従う。
//! ORDER BY 側のスコアとの統合は #1987・#1991 の担当で、本モジュールは WHERE の評価専用。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::DistanceKind;

/// 行ベクトルとクエリベクトルの距離を返す（長さ不一致は fail-closed の `Internal`。値は含めない）。
///
/// 行側の値は格納時に有限値検証済みで、f32 累積が溢れた場合の inf は pgvector と同じくそのまま返す。
/// ノルム 0 のコサインは NaN になる（呼び出し側の比較が PG の float8 順序で扱う）。
pub(crate) fn distance(
    kind: DistanceKind,
    row: &[f32],
    query: &[f32],
) -> Result<f64, SqlSurfaceError> {
    if row.len() != query.len() {
        return Err(SqlSurfaceError::Internal {
            detail: "distance operand dimension mismatch".to_string(),
        });
    }
    Ok(match kind {
        DistanceKind::L2 => {
            let mut sum = 0.0f32;
            for (a, b) in row.iter().zip(query.iter()) {
                let d = a - b;
                sum += d * d;
            }
            f64::from(sum).sqrt()
        }
        DistanceKind::NegativeInnerProduct => -f64::from(crate::kernel::dot(row, query)),
        DistanceKind::L1 => f64::from(crate::kernel::l1(row, query)),
        DistanceKind::Cosine => {
            let dot = f64::from(crate::kernel::dot(row, query));
            let na = f64::from(crate::kernel::dot(row, row));
            let nb = f64::from(crate::kernel::dot(query, query));
            let s = dot / (na * nb).sqrt();
            // `f64::clamp` は NaN をそのまま返す（ゼロノルムは NaN のまま比較側へ渡る）。
            1.0 - s.clamp(-1.0, 1.0)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_computed_values() {
        let r = [2.0f32, 0.0];
        let q = [1.0f32, 0.0];
        assert_eq!(distance(DistanceKind::Cosine, &r, &q).unwrap(), 0.0);
        assert_eq!(distance(DistanceKind::L2, &r, &q).unwrap(), 1.0);
        assert_eq!(
            distance(DistanceKind::NegativeInnerProduct, &r, &q).unwrap(),
            -2.0
        );
        assert_eq!(distance(DistanceKind::L1, &r, &q).unwrap(), 1.0);
        let z = [0.0f32, 0.0, 0.0];
        let p = [3.0f32, 4.0, 0.0];
        assert_eq!(distance(DistanceKind::L2, &z, &p).unwrap(), 5.0);
        assert_eq!(
            distance(DistanceKind::Cosine, &[1.0, 0.0], &[0.0, 1.0]).unwrap(),
            1.0
        );
    }

    #[test]
    fn zero_norm_cosine_is_nan() {
        assert!(distance(DistanceKind::Cosine, &[0.0, 0.0], &[1.0, 0.0])
            .unwrap()
            .is_nan());
    }

    #[test]
    fn dimension_mismatch_is_internal() {
        let e = distance(DistanceKind::L2, &[1.0], &[1.0, 2.0]).unwrap_err();
        assert!(matches!(e, SqlSurfaceError::Internal { .. }));
    }
}
