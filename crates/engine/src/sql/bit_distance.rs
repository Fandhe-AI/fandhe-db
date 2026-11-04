//! bit 列の距離関数（`hamming_distance`・`jaccard_distance`）と演算子 `<~>`・`<%>` の
//! 被演算子検証・評価ヘルパー（対象ビヘイビア: TABLE-48 (2)。Issue #2970）。
//!
//! 責務境界: 構文段（`allowlist::Parser::parse_json_access_rest`）は `<~>`／`<%>` を
//! 2 関数の呼び出しへ脱糖し、束縛段（`udf_call::bind_bit_distance_fn`）が被演算子の型を決める。
//! 本モジュールはその束縛段・評価段（`udf_call::apply_builtin`）から呼ばれ、(1) 文字列リテラルの
//! fail-closed な解析（確保前の長さ上限検査 → 文字種検査）、(2) 長さ不一致の SQLSTATE 写像
//! （`22000`）、(3) 長さ検証付きのカーネル呼び出し（`kernel::bit::{hamming_distance, jaccard_distance}`）
//! を受け持つ。行ごとの距離は scan 経路の可視行（RLS・行ポリシー適用後）だけに対して評価される。
//!
//! エラー文言は固定の英語で、リテラルの値・他テナントの情報を含めない。

use super::allowlist::SqlSurfaceError;
use super::udf_call::{BuiltinFn, ExprValue};
use crate::bit_string::{BitString, MAX_BIT_LENGTH};

/// 長さの違う bit 列どうしの距離（`22000`。pgvector と同じ分類で、`22026` ではない）。
pub(crate) fn length_mismatch_error() -> SqlSurfaceError {
    SqlSurfaceError::invalid_input("different bit lengths")
}

/// 文字列リテラルを長さ任意の bit 値として解析する。
///
/// 検証順（fail-closed）: (1) 確保前に文字数が [`MAX_BIT_LENGTH`] 以内か（超過は `54000`）、
/// (2) 空でないか、(3) `0`・`1` 以外の文字（`22P02`）。長さの不一致はここでは判定しない。
pub(crate) fn parse_bit_operand_literal(lit: &str) -> Result<BitString, SqlSurfaceError> {
    let n = u32::try_from(lit.len())
        .ok()
        .filter(|n| *n <= MAX_BIT_LENGTH)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("bit string literal is too long"))?;
    if n == 0 {
        return Err(SqlSurfaceError::invalid_input(
            "bit string must have at least 1 bit",
        ));
    }
    BitString::parse_text(lit, n).map_err(|e| match e {
        crate::bit_string::BitError::InvalidDigit => {
            SqlSurfaceError::invalid_text_representation(e.message())
        }
        _ => SqlSurfaceError::invalid_input(e.message()),
    })
}

/// 2 引数の bit 距離を評価する（`udf_call::apply_builtin` の `HammingDistance`／`JaccardDistance` 腕）。
///
/// NULL は呼び出し元の strict ガードで処理済み。`bit_len` の一致を必ず先に検査する
/// （カーネルの `DimMismatch` はバイト長しか見ず、9 と 10 ビットのような同一バイト長の組を
/// 見逃すため）。カーネルの `DimMismatch` も最後の砦として `22000` に写す。
pub(crate) fn bit_distance_fn(
    f: BuiltinFn,
    args: &mut [Option<ExprValue<'_>>],
) -> Result<f64, SqlSurfaceError> {
    let internal = || SqlSurfaceError::Internal {
        detail: "bit distance argument type mismatch at evaluation time".to_string(),
    };
    let a = match args.get_mut(0).and_then(Option::take) {
        Some(ExprValue::Bit(b)) => b,
        _ => return Err(internal()),
    };
    let b = match args.get_mut(1).and_then(Option::take) {
        Some(ExprValue::Bit(b)) => b,
        _ => return Err(internal()),
    };
    if a.bit_len() != b.bit_len() {
        return Err(length_mismatch_error());
    }
    let result = match f {
        BuiltinFn::HammingDistance => {
            crate::kernel::hamming_distance(a.as_bytes(), b.as_bytes(), a.bit_len())
        }
        BuiltinFn::JaccardDistance => {
            crate::kernel::jaccard_distance(a.as_bytes(), b.as_bytes(), a.bit_len())
        }
        _ => return Err(internal()),
    };
    result.map_err(|_| length_mismatch_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(s: &str) -> ExprValue<'static> {
        ExprValue::Bit(parse_bit_operand_literal(s).unwrap())
    }

    fn run(f: BuiltinFn, a: &str, b: &str) -> Result<f64, SqlSurfaceError> {
        bit_distance_fn(f, &mut [Some(bit(a)), Some(bit(b))])
    }

    #[test]
    fn hamming_matches_pgvector_values() {
        for (b, want) in [("111", 0.0), ("110", 1.0), ("100", 2.0), ("000", 3.0)] {
            assert_eq!(run(BuiltinFn::HammingDistance, "111", b).unwrap(), want);
        }
    }

    #[test]
    fn jaccard_matches_pgvector_values() {
        for (b, want) in [
            ("1111", 0.0),
            ("1110", 0.25),
            ("1100", 0.5),
            ("1000", 0.75),
            ("0000", 1.0),
        ] {
            assert_eq!(run(BuiltinFn::JaccardDistance, "1111", b).unwrap(), want);
        }
        assert_eq!(
            run(BuiltinFn::JaccardDistance, "0000", "0000").unwrap(),
            1.0
        );
    }

    #[test]
    fn same_byte_length_but_different_bit_length_is_rejected() {
        let err = run(BuiltinFn::HammingDistance, "111111111", "1111111111").unwrap_err();
        assert!(matches!(err, SqlSurfaceError::InvalidInput { .. }));
    }

    #[test]
    fn literal_validation_is_fail_closed() {
        assert!(matches!(
            parse_bit_operand_literal("10x1"),
            Err(SqlSurfaceError::InvalidTextRepresentation { .. })
        ));
        assert!(parse_bit_operand_literal("").is_err());
        let too_long = "1".repeat(MAX_BIT_LENGTH as usize + 1);
        assert!(matches!(
            parse_bit_operand_literal(&too_long),
            Err(SqlSurfaceError::PayloadTooLarge { .. })
        ));
    }
}
