//! WHERE 内の距離式（`<VECTOR 列> <距離演算子> <ベクトルリテラル>`）の値を求める純関数群
//! （SQL-74 (1)・Issue #2036）。
//!
//! `udf_call::eval_with_scalars` の `BoundExpr::VectorDistance` 腕から行ごとに呼ばれ、float8
//! 相当の `f64` を返す。pgvector と同じく f32 で累積し f64 で返す。コサインは ADR
//! `docs/design/cosine-distance-method.md` 第 6 節の手順（arena を経由しない経路）に従う。
//! ORDER BY 側（`<=>` は #1991 で `kernel::cosine_distance` に接続）とは、有限・通常範囲の入力で
//! ビット一致する式を使う（下の単体テストで固定）。f32 の溢れ・アンダーフロー時の inf と NaN の
//! 扱いの統一は #2006 の担当で、本モジュールは WHERE の評価専用。
//!
//! 関数形（`l2_distance` 等 4 種。SQL-72 (1)・Issue #2011）の値計算 [`distance_fn`] も置く。
//! 演算子と同じカーネル・同じ加算順なのでビット一致する（単体テストで固定）。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::DistanceKind;
use crate::sql::udf_call::BuiltinFn;

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
            // ゼロノルム（行・クエリ）は距離 NaN。0/0 の NaN は符号ビットが ISA 依存
            // （x86 は負、aarch64 は正）なので、正規の NaN へ揃えて投影時のバイナリ表現を
            // 環境非依存にする（Issue #2240・SQL-33 (3)(7)）。
            if na * nb == 0.0 {
                return Ok(f64::NAN);
            }
            let s = dot / (na * nb).sqrt();
            let d = 1.0 - s.clamp(-1.0, 1.0);
            // `f64::clamp` は NaN をそのまま返すので、非有限入力由来の NaN もここで正規化する。
            if d.is_nan() {
                f64::NAN
            } else {
                d
            }
        }
    })
}

/// 距離関数の名前（小文字）から [`BuiltinFn`] を引く（SQL-72 (1)・Issue #2011）。
/// `udf_call::bind_call` が汎用の組み込み解決より前に振り分けるために使う。
pub(crate) fn distance_fn_from_name(lower: &str) -> Option<BuiltinFn> {
    match lower {
        "l2_distance" => Some(BuiltinFn::L2Distance),
        "cosine_distance" => Some(BuiltinFn::CosineDistance),
        "inner_product" => Some(BuiltinFn::InnerProduct),
        "l1_distance" => Some(BuiltinFn::L1Distance),
        _ => None,
    }
}

/// 距離関数 4 種の値（SQL-72 (1)(2)・Issue #2011）。`udf_call::apply_builtin` から呼ばれる。
///
/// 値は演算子と同じ [`distance`]／カーネルで求める。`inner_product` は `<#>` の符号を反転した
/// 正の内積。次元不一致は `22000`（式の引数は束縛時に次元が決まらないため評価時に照合する）。
/// 非有限値は [`distance`] と同じくそのまま返す（コサインのゼロノルムは NaN）。
pub(crate) fn distance_fn(f: BuiltinFn, a: &[f32], b: &[f32]) -> Result<f64, SqlSurfaceError> {
    if a.len() != b.len() {
        return Err(SqlSurfaceError::invalid_input(
            "different vector dimensions",
        ));
    }
    match f {
        BuiltinFn::L2Distance => distance(DistanceKind::L2, a, b),
        BuiltinFn::CosineDistance => distance(DistanceKind::Cosine, a, b),
        BuiltinFn::L1Distance => distance(DistanceKind::L1, a, b),
        BuiltinFn::InnerProduct => Ok(f64::from(crate::kernel::dot(a, b))),
        _ => Err(SqlSurfaceError::Internal {
            detail: "not a distance function".to_string(),
        }),
    }
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
        for (r, q) in [
            ([0.0f32, 0.0], [1.0f32, 0.0]),
            ([1.0, 0.0], [0.0, 0.0]),
            ([0.0, 0.0], [0.0, 0.0]),
        ] {
            let d = distance(DistanceKind::Cosine, &r, &q).unwrap();
            assert!(d.is_nan());
            assert_eq!(d.to_bits(), f64::NAN.to_bits());
        }
    }

    #[test]
    fn dimension_mismatch_is_internal() {
        let e = distance(DistanceKind::L2, &[1.0], &[1.0, 2.0]).unwrap_err();
        assert!(matches!(e, SqlSurfaceError::Internal { .. }));
    }

    /// Issue #1991: ORDER BY 側（`kernel::cosine_distance`）と WHERE 側で、通常範囲の入力の
    /// コサイン距離がビット一致する（順位と投影・比較の値が食い違わない）。
    #[test]
    fn cosine_matches_kernel_cosine_distance_bitwise() {
        let rows: [[f32; 3]; 4] = [
            [1.0, 2.0, 3.0],
            [-0.5, 0.25, 8.0],
            [10.0, 10.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        let q = [0.3_f32, -1.0, 2.5];
        for r in &rows {
            let a = distance(DistanceKind::Cosine, r, &q).unwrap();
            let b = crate::kernel::cosine_distance(r, &q);
            if a.is_nan() {
                assert!(b.is_nan());
            } else {
                assert_eq!(a.to_bits(), b.to_bits(), "{r:?}");
            }
        }
    }

    /// SQL-72 (1)(2)・Issue #2011: 関数形は演算子形と同じ値（ビット一致）。`inner_product` は
    /// `<#>` の符号反転で、`-0.0`／`+0.0` の境界も含めて一致する。
    #[test]
    fn distance_fn_matches_operator_forms_bitwise() {
        let rows: [[f32; 3]; 4] = [
            [1.0, 2.0, 3.0],
            [-0.5, 0.25, 8.0],
            [10.0, 10.0, 0.0],
            [0.0, 0.0, 0.0],
        ];
        let q = [0.3_f32, -1.0, 2.5];
        for r in &rows {
            for (f, k) in [
                (BuiltinFn::L2Distance, DistanceKind::L2),
                (BuiltinFn::CosineDistance, DistanceKind::Cosine),
                (BuiltinFn::L1Distance, DistanceKind::L1),
            ] {
                let a = distance_fn(f, r, &q).unwrap();
                let b = distance(k, r, &q).unwrap();
                assert_eq!(a.to_bits(), b.to_bits(), "{f:?} {r:?}");
            }
            let ip = distance_fn(BuiltinFn::InnerProduct, r, &q).unwrap();
            let neg = distance(DistanceKind::NegativeInnerProduct, r, &q).unwrap();
            assert_eq!((-neg).to_bits(), ip.to_bits(), "{r:?}");
        }
        let ip = distance_fn(BuiltinFn::InnerProduct, &[0.0], &[0.0]).unwrap();
        assert_eq!(ip.to_bits(), 0.0f64.to_bits());
    }

    /// 次元不一致は `22000`（`Internal` ではない）。ゼロノルムのコサインは正規の NaN。
    #[test]
    fn distance_fn_dimension_mismatch_and_zero_norm() {
        for f in [
            BuiltinFn::L2Distance,
            BuiltinFn::CosineDistance,
            BuiltinFn::InnerProduct,
            BuiltinFn::L1Distance,
        ] {
            let e = distance_fn(f, &[1.0], &[1.0, 2.0]).unwrap_err();
            assert_eq!(e.wire_code(), "22000", "{f:?}");
        }
        let d = distance_fn(BuiltinFn::CosineDistance, &[0.0, 0.0], &[1.0, 0.0]).unwrap();
        assert_eq!(d.to_bits(), f64::NAN.to_bits());
    }
}
