//! ベクトルの「形」を扱うスカラー関数（`vector_dims`・`l2_normalize`・`subvector`・
//! `binary_quantize`）の値レベルの純関数群（対象ビヘイビア: SQL-72 (1)(2)・TABLE-48 (3)。
//! Issue #2012・#2023）。
//!
//! 呼び出し元は `sql::udf_call::apply_builtin`（束縛済み式木の評価）のみ。NULL の伝播
//! （strict）は呼び出し側の一律ガードが担うため、ここへは非 NULL の値だけが届く。
//! 結果ベクトルの確保は `try_reserve_exact` で fail-closed にし、引数の検証は
//! `numeric_fn::trunc2` と同じ規約（非有限・非整数は `22000`、`i32` 範囲外は `22003`）に従う。
//! `vector_norm` は既存の `vec_norm` の別名として `udf_call` 側で解決する。

use super::allowlist::SqlSurfaceError;

/// 要素数（`vector_dims`）。`f64` で返すのは式評価器のスカラー表現に合わせるため。
pub(crate) fn dims(v: &[f32]) -> f64 {
    v.len() as f64
}

/// 結果用バッファを fail-closed に確保する（確保失敗は `54000`）。
fn alloc(n: usize, who: &str) -> Result<Vec<f32>, SqlSurfaceError> {
    let mut out: Vec<f32> = Vec::new();
    out.try_reserve_exact(n).map_err(|_| {
        SqlSurfaceError::payload_too_large(format!("{who} result exceeds available memory"))
    })?;
    Ok(out)
}

/// L2 正規化（`l2_normalize`）。ノルムは f64 で累積し、各要素は `(x as f64 / norm) as f32`。
/// ノルムが 0 のときは同じ次元の全要素 `+0.0` のベクトルを返す。f32 化で非有限になる場合は `22003`。
pub(crate) fn l2_normalize(v: &[f32]) -> Result<Vec<f32>, SqlSurfaceError> {
    let norm = v
        .iter()
        .map(|&x| (x as f64) * (x as f64))
        .sum::<f64>()
        .sqrt();
    let mut out = alloc(v.len(), "l2_normalize")?;
    if norm > 0.0 {
        for &x in v {
            let r = ((x as f64) / norm) as f32;
            if !r.is_finite() {
                return Err(SqlSurfaceError::numeric_out_of_range(
                    "l2_normalize: result is not finite",
                ));
            }
            out.push(r);
        }
    } else {
        out.resize(v.len(), 0.0);
    }
    Ok(out)
}

/// 二値量子化（`binary_quantize`。TABLE-48 (3)・Issue #2023）。正の成分を 1、それ以外
/// （0・`-0.0`・負値・NaN）を 0 とする（pgvector と同じ `x > 0`）。ビットは MSB 先頭で並べ、
/// 詰め物ビットは 0。結果長は入力の次元数で、空ベクトルは到達しない（`22000`）。確保失敗は `54000`。
/// 呼び出し元は `udf_call::apply_builtin` のみ（結果は `ExprValue::Bit` として投影・代入へ流れる）。
pub(crate) fn binary_quantize(v: &[f32]) -> Result<crate::bit_string::BitString, SqlSurfaceError> {
    let too_large = || SqlSurfaceError::payload_too_large("binary_quantize result is too large");
    let len = u32::try_from(v.len()).map_err(|_| too_large())?;
    if len == 0 {
        return Err(SqlSurfaceError::invalid_input(
            "vector must have at least 1 dimension",
        ));
    }
    let n = crate::bit_string::byte_len_for_bits(len).ok_or_else(too_large)?;
    let mut bytes: Vec<u8> = Vec::new();
    bytes.try_reserve_exact(n).map_err(|_| too_large())?;
    for chunk in v.chunks(8) {
        let mut b = 0u8;
        for (i, &x) in chunk.iter().enumerate() {
            if x > 0.0 {
                b |= 0x80 >> i;
            }
        }
        bytes.push(b);
    }
    crate::bit_string::BitString::from_parts(len, bytes).map_err(|_| SqlSurfaceError::Internal {
        detail: "binary_quantize produced an invalid bit string".to_string(),
    })
}

/// 位置引数（`start`／`count`）を検証して `i64` にする。非有限・非整数は `22000`、
/// `i32` 範囲外は `22003`。
fn int_arg(x: f64, what: &str) -> Result<i64, SqlSurfaceError> {
    if !x.is_finite() || x.fract() != 0.0 {
        return Err(SqlSurfaceError::invalid_input(format!(
            "subvector: {what} must be an integer"
        )));
    }
    if x < (i32::MIN as f64) || x > (i32::MAX as f64) {
        return Err(SqlSurfaceError::numeric_out_of_range(format!(
            "subvector: {what} is out of range"
        )));
    }
    Ok(x as i64)
}

/// 部分ベクトル（`subvector(v, start, count)`）。`start` は 1 始まり。終端はクランプ前の
/// `start` から決め、要素が 1 つも残らない場合は `22000`（pgvector と同じ順序）。
pub(crate) fn subvector(v: &[f32], start: f64, count: f64) -> Result<Vec<f32>, SqlSurfaceError> {
    let empty = || SqlSurfaceError::invalid_input("vector must have at least 1 dimension");
    let start = int_arg(start, "start")?;
    let count = int_arg(count, "count")?;
    let dim = v.len() as i64;
    if count < 1 {
        return Err(empty());
    }
    let end = if start > dim - count {
        dim + 1
    } else {
        start + count
    };
    let start = if start < 1 {
        1
    } else if start > dim {
        return Err(empty());
    } else {
        start
    };
    if end - start < 1 {
        return Err(empty());
    }
    // 0 始まりの半開区間。上の検査で 1 <= start <= dim, start < end <= dim + 1。
    let slice = v
        .get((start - 1) as usize..(end - 1) as usize)
        .ok_or_else(empty)?;
    let mut out = alloc(slice.len(), "subvector")?;
    out.extend_from_slice(slice);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: [f32; 3] = [1.0, 2.0, 3.0];

    fn sub(s: f64, c: f64) -> Result<Vec<f32>, SqlSurfaceError> {
        subvector(&V, s, c)
    }

    #[test]
    fn subvector_boundaries() {
        assert_eq!(sub(1.0, 3.0).unwrap(), vec![1.0, 2.0, 3.0]);
        assert_eq!(sub(3.0, 1.0).unwrap(), vec![3.0]);
        assert_eq!(sub(2.0, 5.0).unwrap(), vec![2.0, 3.0]);
        assert_eq!(sub(0.0, 2.0).unwrap(), vec![1.0]);
        assert_eq!(sub(-1.0, 3.0).unwrap(), vec![1.0]);
        for (s, c) in [(-1.0, 2.0), (4.0, 1.0), (1.0, 0.0), (1.0, -1.0)] {
            assert_eq!(sub(s, c).unwrap_err().wire_code(), "22000", "{s} {c}");
        }
        let (mn, mx) = (i32::MIN as f64, i32::MAX as f64);
        assert_eq!(sub(mn, mx).unwrap_err().wire_code(), "22000");
        assert_eq!(sub(mx, mx).unwrap_err().wire_code(), "22000");
        assert_eq!(sub(mx, 1.0).unwrap_err().wire_code(), "22000");
        assert_eq!(sub(mn, 1.0).unwrap_err().wire_code(), "22000");
    }

    #[test]
    fn subvector_rejects_bad_arguments() {
        for bad in [f64::NAN, f64::INFINITY, 1.5] {
            assert_eq!(sub(bad, 1.0).unwrap_err().wire_code(), "22000");
            assert_eq!(sub(1.0, bad).unwrap_err().wire_code(), "22000");
        }
        assert_eq!(sub(3e9, 1.0).unwrap_err().wire_code(), "22003");
        assert_eq!(sub(1.0, -3e9).unwrap_err().wire_code(), "22003");
    }

    #[test]
    fn l2_normalize_values() {
        let r = l2_normalize(&[3.0, 4.0]).unwrap();
        assert_eq!(r[0].to_bits(), ((3.0f64 / 5.0) as f32).to_bits());
        assert_eq!(r[1].to_bits(), ((4.0f64 / 5.0) as f32).to_bits());
        let z = l2_normalize(&[-0.0, 0.0]).unwrap();
        assert!(z.iter().all(|x| x.to_bits() == 0.0f32.to_bits()));
        assert_eq!(l2_normalize(&[0.0, 0.0, 0.0]).unwrap().len(), 3);
        let n = l2_normalize(&[1.0, 2.0, 3.0]).unwrap();
        let norm: f64 = n
            .iter()
            .map(|&x| (x as f64) * (x as f64))
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    fn bq(v: &[f32]) -> String {
        binary_quantize(v).unwrap().format_text()
    }

    #[test]
    fn binary_quantize_matches_pgvector_rule() {
        assert_eq!(bq(&[1.0, 0.0, -1.0]), "100");
        assert_eq!(
            bq(&[0.0, 0.1, -0.2, -0.3, 0.4, 0.5, 0.6, -0.7, 0.8, -0.9, 1.0]),
            "01001110101"
        );
        assert_eq!(bq(&[-0.0, 0.0, f32::NAN, f32::NEG_INFINITY]), "0000");
        assert_eq!(bq(&[f32::INFINITY]), "1");
    }

    #[test]
    fn binary_quantize_byte_boundaries() {
        let eight = binary_quantize(&[1.0; 8]).unwrap();
        assert_eq!(eight.as_bytes(), &[0xFF]);
        let nine = binary_quantize(&[1.0; 9]).unwrap();
        assert_eq!(nine.as_bytes(), &[0xFF, 0x80]);
        assert_eq!(nine.bit_len(), 9);
        assert_eq!(binary_quantize(&[]).unwrap_err().wire_code(), "22000");
    }

    #[test]
    fn dims_counts_elements() {
        assert_eq!(dims(&V), 3.0);
    }
}
