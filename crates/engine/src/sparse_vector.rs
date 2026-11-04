//! `SPARSEVEC(N)` 列の値表現・テキスト入出力・入力検証（TABLE-47 (1)(2)(5)・Issue #2025）。
//!
//! 役割: 非ゼロ要素（位置・値）だけを位置の昇順で保持する疎ベクトル値を定義し、SQL リテラル・
//! `$n` テキスト束縛・NoSQL 文字列入力が共有する**唯一の検証付き構築経路**を提供する。
//! BM25 の転置索引用 [`crate::sparse`] とは独立（用途も表現も別）で、混同を避けるため別名にした。
//!
//! 呼び出し文脈:
//! - `sql::parser` の束縛（INSERT／UPDATE SET／UPSERT）が [`SparseVector::parse_text`] を呼ぶ
//! - `row_codec` の decode 経路が [`SparseVector::from_sorted_parts`] を呼び、永続データの
//!   不整合を fail-closed（`RowCodecError::Invalid`）にする
//! - wire-server の結果整形が [`SparseVector::format_text`] を呼ぶ
//!
//! 上限の扱い（Issue #2027 への引き継ぎ点）: 書き込み経路は設定可能な上限
//! （現状は [`DEFAULT_MAX_SPARSEVEC_NNZ`]）で、decode 経路は [`HARD_MAX_SPARSEVEC_NNZ`] だけで
//! 検査する。こうすることで上限を下げても既存行を読める。書き込み側の上限参照は
//! [`SparseVector::parse_text`] / [`SparseVector::try_from_pairs`] の引数 `max_nnz` に集約しており、
//! CLI 設定化はその呼び出し側（`sql::parser`）の値を差し替えるだけで済む。
//!
//! エラー文言は固定の英語で、入力値を含めない（テナント境界・情報漏えい対策）。

use std::fmt::Write as _;

use crate::json::parse_f32_text;

/// 非ゼロ要素数の既定上限（TABLE-47 (2)）。書き込み経路の検査に使う。
pub const DEFAULT_MAX_SPARSEVEC_NNZ: usize = 16_000;

/// 非ゼロ要素数のハード上限。decode 経路はこの値だけで検査する（現時点では既定値と同値）。
pub const HARD_MAX_SPARSEVEC_NNZ: usize = 16_000;

/// 宣言次元の上限（TABLE-47 (1)）。疎表現なので次元そのものはメモリを消費しない。
pub const MAX_SPARSEVEC_DIM: u32 = 1_000_000_000;

/// テキスト入力のバイト長上限。wire の 1 MiB メッセージ上限に収まり、16,000 要素
/// （1 要素あたり最大 27 バイト程度）を許容する。解析前に検査する。
pub const MAX_SPARSEVEC_LITERAL_BYTES: usize = 512 * 1024;

/// 検証エラーの分類。呼び出し側が SQLSTATE（22P02／22000／54000）へ写す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparseVectorErrorKind {
    /// テキスト表現の形式不正（`22P02`）。
    InvalidText,
    /// 形式は正しいが値が不正（位置範囲外・重複・非有限・次元不一致。`22000`）。
    InvalidValue,
    /// 上限超過（リテラル長・次元・非ゼロ数。`54000`）。
    TooLarge,
}

/// 疎ベクトルの検証エラー。`detail` は固定文言で入力値を含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseVectorError {
    /// SQLSTATE への写像に使う分類。
    pub kind: SparseVectorErrorKind,
    /// 英語の固定説明。
    pub detail: &'static str,
}

fn err(kind: SparseVectorErrorKind, detail: &'static str) -> SparseVectorError {
    SparseVectorError { kind, detail }
}

/// 疎ベクトル値。indices は 0 始まりで厳密昇順かつ `< dim`、values は有限かつ非ゼロ。
/// フィールドは非公開で、構築は検証付きの関数だけを通す。
#[derive(Debug, Clone, PartialEq)]
pub struct SparseVector {
    dim: u32,
    indices: Vec<u32>,
    values: Vec<f32>,
}

impl SparseVector {
    /// 宣言次元。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 非ゼロ要素の位置（0 始まり・厳密昇順）。
    pub fn indices(&self) -> &[u32] {
        &self.indices
    }

    /// 非ゼロ要素の値（`indices` と同順）。
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// 非ゼロ要素数。
    pub fn nnz(&self) -> usize {
        self.indices.len()
    }

    /// 行 codec 上のフレーム長（presence(1) + dim(4) + nnz(4) + nnz × 8）。overflow は `None`。
    pub fn encoded_len(&self) -> Option<usize> {
        frame_len(self.indices.len())
    }

    /// 位置・値の組から構築する唯一の検証付き構築子。位置は 0 始まり・順不同可。
    /// 位置の範囲外・重複（ゼロ要素を除く前に検査）・非有限値・次元不正・非ゼロ数が
    /// `max_nnz` 超過はエラー。値が 0（`-0.0` を含む）の要素は保存しない。
    pub fn try_from_pairs(
        dim: u32,
        mut pairs: Vec<(u32, f32)>,
        max_nnz: usize,
    ) -> Result<Self, SparseVectorError> {
        check_dim(dim)?;
        for (idx, val) in &pairs {
            if *idx >= dim {
                return Err(err(
                    SparseVectorErrorKind::InvalidValue,
                    "sparsevec index out of range",
                ));
            }
            if !val.is_finite() {
                return Err(err(
                    SparseVectorErrorKind::InvalidValue,
                    "sparsevec value must be finite",
                ));
            }
        }
        // 位置が同じ要素の相対順は、直後の重複検査でエラーにするため結果に影響しない。
        pairs.sort_unstable_by_key(|(idx, _)| *idx); // sort-determinism: allow 同一キーは重複エラーで棄却
        if pairs.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(err(
                SparseVectorErrorKind::InvalidValue,
                "sparsevec index is duplicated",
            ));
        }
        let nnz = pairs.iter().filter(|(_, v)| *v != 0.0).count();
        if nnz > max_nnz {
            return Err(err(
                SparseVectorErrorKind::TooLarge,
                "sparsevec non-zero element count exceeds limit",
            ));
        }
        let mut indices = Vec::new();
        let mut values = Vec::new();
        reserve(&mut indices, nnz)?;
        reserve(&mut values, nnz)?;
        for (idx, val) in pairs {
            if val != 0.0 {
                indices.push(idx);
                values.push(val);
            }
        }
        Ok(Self {
            dim,
            indices,
            values,
        })
    }

    /// 既に正規化済みの列（行 codec の decode 経路）から構築する。位置は厳密昇順かつ
    /// `< dim`、値は有限かつ非ゼロ、`nnz ≤ min(max_nnz, dim)` でなければエラー。
    pub fn from_sorted_parts(
        dim: u32,
        indices: Vec<u32>,
        values: Vec<f32>,
        max_nnz: usize,
    ) -> Result<Self, SparseVectorError> {
        check_dim(dim)?;
        if indices.len() != values.len() {
            return Err(err(
                SparseVectorErrorKind::InvalidValue,
                "sparsevec indices and values length mismatch",
            ));
        }
        if indices.len() > max_nnz || indices.len() as u64 > u64::from(dim) {
            return Err(err(
                SparseVectorErrorKind::TooLarge,
                "sparsevec non-zero element count exceeds limit",
            ));
        }
        let mut prev: Option<u32> = None;
        for (idx, val) in indices.iter().zip(values.iter()) {
            if *idx >= dim || prev.is_some_and(|p| p >= *idx) {
                return Err(err(
                    SparseVectorErrorKind::InvalidValue,
                    "sparsevec indices must be strictly ascending and within the dimension",
                ));
            }
            if !val.is_finite() || *val == 0.0 {
                return Err(err(
                    SparseVectorErrorKind::InvalidValue,
                    "sparsevec stored value must be finite and non-zero",
                ));
            }
            prev = Some(*idx);
        }
        Ok(Self {
            dim,
            indices,
            values,
        })
    }

    /// テキスト表現 `{idx:val,...}/N`（位置は 1 始まり）を解析する。SQL リテラル・`$n`・NoSQL
    /// の文字列入力が共有する。検査順: リテラル長（解析前）→ `/N` の次元 → 要素。
    pub fn parse_text(
        literal: &str,
        expected_dim: u32,
        max_nnz: usize,
    ) -> Result<Self, SparseVectorError> {
        if literal.len() > MAX_SPARSEVEC_LITERAL_BYTES {
            return Err(err(
                SparseVectorErrorKind::TooLarge,
                "sparsevec literal length exceeds limit",
            ));
        }
        let malformed = || {
            err(
                SparseVectorErrorKind::InvalidText,
                "sparsevec literal must be of the form {idx:val,...}/dim",
            )
        };
        let text = literal.trim();
        let body = text.strip_prefix('{').ok_or_else(malformed)?;
        let close = body.rfind('}').ok_or_else(malformed)?;
        let inner = body.get(..close).ok_or_else(malformed)?;
        let dim_text = body
            .get(close + 1..)
            .and_then(|s| s.trim_start().strip_prefix('/'))
            .ok_or_else(malformed)?
            .trim();
        let dim = parse_dim_text(dim_text, malformed())?;
        if dim != expected_dim {
            return Err(err(
                SparseVectorErrorKind::InvalidValue,
                "sparsevec dimension does not match the column type",
            ));
        }
        let mut pairs: Vec<(u32, f32)> = Vec::new();
        if !inner.trim().is_empty() {
            let slots = inner
                .bytes()
                .filter(|b| *b == b',')
                .count()
                .saturating_add(1);
            pairs
                .try_reserve_exact(slots)
                .map_err(|_| too_much_memory())?;
            let mut nonzero = 0usize;
            for part in inner.split(',') {
                let (idx_text, val_text) = part.split_once(':').ok_or_else(malformed)?;
                let idx_text = idx_text.trim();
                let val_text = val_text.trim();
                let digits = idx_text
                    .strip_prefix('-')
                    .or_else(|| idx_text.strip_prefix('+'))
                    .unwrap_or(idx_text);
                if digits.is_empty()
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                    || val_text.is_empty()
                {
                    return Err(malformed());
                }
                let value = parse_f32_text(val_text).map_err(|_| malformed())?;
                // 数字列として正しいので、i64 に収まらない位置は範囲外（22000）。
                let idx1: i64 = idx_text.parse().map_err(|_| {
                    err(
                        SparseVectorErrorKind::InvalidValue,
                        "sparsevec index out of range",
                    )
                })?;
                if idx1 < 1 || idx1 > i64::from(dim) {
                    return Err(err(
                        SparseVectorErrorKind::InvalidValue,
                        "sparsevec index out of range",
                    ));
                }
                if !value.is_finite() {
                    return Err(err(
                        SparseVectorErrorKind::InvalidValue,
                        "sparsevec value must be finite",
                    ));
                }
                if value != 0.0 {
                    nonzero += 1;
                    if nonzero > max_nnz {
                        return Err(err(
                            SparseVectorErrorKind::TooLarge,
                            "sparsevec non-zero element count exceeds limit",
                        ));
                    }
                }
                // idx1 は 1..=dim（u32 範囲）を検証済み。
                let idx0 = u32::try_from(idx1 - 1).map_err(|_| malformed())?;
                pairs.push((idx0, value));
            }
        }
        Self::try_from_pairs(dim, pairs, max_nnz)
    }

    /// テキスト表現 `{idx:val,...}/N`（位置は 1 始まり、値は `f32::to_string`）へ整形する。
    /// [`Self::parse_text`] と往復で一致する。
    pub fn format_text(&self) -> String {
        let mut out =
            String::with_capacity(self.indices.len().saturating_mul(12).saturating_add(16));
        out.push('{');
        for (i, (idx, val)) in self.indices.iter().zip(self.values.iter()).enumerate() {
            if i > 0 {
                out.push(',');
            }
            // 位置は 0 始まりから 1 始まりへ（u64 で加算し overflow を避ける）。
            let pos = u64::from(*idx) + 1;
            // 極小・極大の有限値は Display だと数十桁へ展開され入力上限を超えうるため、
            // 往復可能な指数表記へ切り替える（f32 の `LowerExp` は最短往復表現）。
            let mag = val.abs();
            if mag != 0.0 && !(1e-4..1e16).contains(&mag) {
                let _ = write!(out, "{pos}:{val:e}");
            } else {
                let _ = write!(out, "{pos}:{val}");
            }
        }
        let _ = write!(out, "}}/{}", self.dim);
        out
    }
}

/// フレーム長 `9 + nnz * 8`（checked）。
pub(crate) fn frame_len(nnz: usize) -> Option<usize> {
    nnz.checked_mul(8)?.checked_add(9)
}

fn too_much_memory() -> SparseVectorError {
    err(
        SparseVectorErrorKind::TooLarge,
        "sparsevec payload exceeds available memory",
    )
}

fn reserve<T>(v: &mut Vec<T>, n: usize) -> Result<(), SparseVectorError> {
    v.try_reserve_exact(n).map_err(|_| too_much_memory())
}

fn check_dim(dim: u32) -> Result<(), SparseVectorError> {
    if dim > MAX_SPARSEVEC_DIM {
        return Err(err(
            SparseVectorErrorKind::TooLarge,
            "sparsevec dimension exceeds limit",
        ));
    }
    if dim < 1 {
        return Err(err(
            SparseVectorErrorKind::InvalidValue,
            "sparsevec dimension must be at least 1",
        ));
    }
    Ok(())
}

/// `/N` の N を解析する。数字列でなければ `malformed`（22P02）、`u32` に収まらない・
/// 上限超過は 54000、0 は 22000。
fn parse_dim_text(text: &str, malformed: SparseVectorError) -> Result<u32, SparseVectorError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed);
    }
    let dim: u32 = text.parse().map_err(|_| {
        err(
            SparseVectorErrorKind::TooLarge,
            "sparsevec dimension exceeds limit",
        )
    })?;
    check_dim(dim)?;
    Ok(dim)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: usize = DEFAULT_MAX_SPARSEVEC_NNZ;

    fn kind(literal: &str, dim: u32) -> SparseVectorErrorKind {
        SparseVector::parse_text(literal, dim, D)
            .expect_err(literal)
            .kind
    }

    /// 極小値を上限いっぱい並べても出力が入力上限内に収まり、往復で一致する。
    #[test]
    fn format_text_tiny_values_stay_within_literal_limit() {
        let dim = 16_000u32;
        let mut lit = String::from("{");
        for i in 1..=dim {
            if i > 1 {
                lit.push(',');
            }
            lit.push_str(&format!("{i}:1e-40"));
        }
        lit.push_str("}/16000");
        let v = SparseVector::parse_text(&lit, dim, D).unwrap();
        let out = v.format_text();
        assert!(out.len() <= MAX_SPARSEVEC_LITERAL_BYTES);
        let again = SparseVector::parse_text(&out, dim, D).unwrap();
        assert_eq!(v, again);
        let big = SparseVector::parse_text("{1:3e38}/2", 2, D).unwrap();
        assert_eq!(
            SparseVector::parse_text(&big.format_text(), 2, D).unwrap(),
            big
        );
    }

    /// TABLE-47 (1): 入力順が違っても昇順で保存され、テキストが往復で一致する。
    #[test]
    fn parse_sorts_and_roundtrips() {
        let v = SparseVector::parse_text("{3:2,1:1}/5", 5, D).unwrap();
        assert_eq!(v.indices(), &[0, 2]);
        assert_eq!(v.values(), &[1.0, 2.0]);
        assert_eq!(v.format_text(), "{1:1,3:2}/5");
        let again = SparseVector::parse_text(&v.format_text(), 5, D).unwrap();
        assert_eq!(v, again);
        let empty = SparseVector::parse_text("{}/4", 4, D).unwrap();
        assert_eq!(empty.format_text(), "{}/4");
        let frac = SparseVector::parse_text(" { 2 : 0.5 , 4 : -1.25 } / 4 ", 4, D).unwrap();
        assert_eq!(frac.format_text(), "{2:0.5,4:-1.25}/4");
    }

    /// TABLE-47 (1): ゼロ（-0 を含む）は保存しない。
    #[test]
    fn zero_elements_are_dropped() {
        let v = SparseVector::parse_text("{1:0,2:-0,3:4}/3", 3, D).unwrap();
        assert_eq!(v.indices(), &[2]);
        assert_eq!(v.nnz(), 1);
    }

    /// 重複の検査はゼロ要素を除く前に行う。
    #[test]
    fn duplicate_is_checked_before_zero_removal() {
        assert_eq!(kind("{1:0,1:1}/5", 5), SparseVectorErrorKind::InvalidValue);
        assert_eq!(kind("{2:1,2:1}/5", 5), SparseVectorErrorKind::InvalidValue);
    }

    /// 形式の崩れは 22P02 相当。
    #[test]
    fn malformed_text_is_invalid_text() {
        for raw in [
            "",
            "1:1/5",
            "{1:1}",
            "{1:1}5",
            "{1:1}/",
            "{1:1}/x",
            "{1}/5",
            "{1:}/5",
            "{:1}/5",
            "{a:1}/5",
            "{1:abc}/5",
            "{1:1,}/5",
            "{,1:1}/5",
            "{1.5:1}/5",
            "{1:1}/-5",
            "{1:1}/5x",
        ] {
            assert_eq!(kind(raw, 5), SparseVectorErrorKind::InvalidText, "{raw}");
        }
    }

    /// 位置範囲外・非有限・次元不一致・N<1 は 22000 相当。
    #[test]
    fn invalid_values_are_invalid_value() {
        for raw in [
            "{0:1}/5",
            "{6:1}/5",
            "{-1:1}/5",
            "{99999999999999999999:1}/5",
            "{1:nan}/5",
            "{1:inf}/5",
            "{1:1e999}/5",
            "{1:1}/4",
            "{1:1}/6",
        ] {
            assert_eq!(kind(raw, 5), SparseVectorErrorKind::InvalidValue, "{raw}");
        }
        assert_eq!(kind("{}/0", 5), SparseVectorErrorKind::InvalidValue);
    }

    /// 次元・リテラル長・非ゼロ数の上限は 54000 相当。上限ちょうどは受理、+1 は拒否。
    #[test]
    fn limits_are_too_large() {
        assert_eq!(
            kind("{}/1000000001", 1_000_000_001),
            SparseVectorErrorKind::TooLarge
        );
        assert_eq!(kind("{}/99999999999", 5), SparseVectorErrorKind::TooLarge);
        assert!(SparseVector::parse_text("{}/1000000000", 1_000_000_000, D).is_ok());

        let body = |n: usize| {
            let mut s = String::from("{");
            for i in 1..=n {
                if i > 1 {
                    s.push(',');
                }
                s.push_str(&format!("{i}:1"));
            }
            s.push_str("}/20000");
            s
        };
        assert!(SparseVector::parse_text(&body(D), 20_000, D).is_ok());
        assert_eq!(
            SparseVector::parse_text(&body(D + 1), 20_000, D)
                .unwrap_err()
                .kind,
            SparseVectorErrorKind::TooLarge
        );
        // ゼロ要素は非ゼロ数に数えない。
        assert!(SparseVector::parse_text("{1:0,2:1}/3", 3, 1).is_ok());

        let huge = " ".repeat(MAX_SPARSEVEC_LITERAL_BYTES + 1);
        assert_eq!(
            SparseVector::parse_text(&huge, 5, D).unwrap_err().kind,
            SparseVectorErrorKind::TooLarge
        );
    }

    /// エラー文言に入力値を含めない。
    #[test]
    fn error_detail_does_not_echo_input() {
        let e = SparseVector::parse_text("{1:secretvalue}/5", 5, D).unwrap_err();
        assert!(!e.detail.contains("secretvalue"));
    }

    /// decode 用の構築子は非昇順・範囲外・ゼロ・非有限・nnz 過大を拒否する。
    #[test]
    fn from_sorted_parts_validates() {
        assert!(SparseVector::from_sorted_parts(5, vec![0, 3], vec![1.0, 2.0], D).is_ok());
        for (idx, val) in [
            (vec![3, 1], vec![1.0, 1.0]),
            (vec![1, 1], vec![1.0, 1.0]),
            (vec![5], vec![1.0]),
            (vec![1], vec![0.0]),
            (vec![1], vec![f32::NAN]),
            (vec![1], vec![f32::INFINITY]),
            (vec![1, 2], vec![1.0]),
        ] {
            assert!(SparseVector::from_sorted_parts(5, idx, val, D).is_err());
        }
        assert!(SparseVector::from_sorted_parts(1, vec![0, 1], vec![1.0, 1.0], D).is_err());
        assert!(SparseVector::from_sorted_parts(5, vec![0, 1], vec![1.0, 1.0], 1).is_err());
        assert_eq!(frame_len(2), Some(25));
    }
}
