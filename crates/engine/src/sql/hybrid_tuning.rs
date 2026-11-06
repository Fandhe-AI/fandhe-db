//! `HYBRID`／`hybrid_rrf` の調整引数（`k1`・`b`・`rrf_k`・`window`・`weights`）の束縛結果と検証
//! （SEARCH-13・Issue #2119。ADR は `docs/design/hybrid-tuning-parameters.md`）。
//!
//! 役割: `sql::allowlist` の構文段が受理した名前付き引数（[`NamedArgValue`]）を、`sql::parser`
//! の `bind_ranking` が値の範囲と型の検証付きで [`HybridTuning`] に束ねる。未指定の引数は
//! `None` のまま保持し、既定値の解決は実行器の責務とする（#2120 が `RrfConfig` と取得窓を、
//! #2121 が BM25 の `k1`／`b` を組み立てる）。NoSQL 表層（#2122）も [`HybridTuning::new`] を
//! 呼んで同じ検証を共有する。
//!
//! エラー契約: 範囲外・型の不一致は `22000`（`invalid_input`）、未知の引数名は `42601`
//! （`unsupported`）。エラー文は固定の英語文と引数名に留め、入力値の全文を含めない。
//!
//! 暫定ゲート: 本 Issue の時点では実行器が束縛値を使わないため、既定以外の [`HybridTuning`] を
//! 持つ文は実行・EXPLAIN 経路で `0A000` により拒否される（[`HybridTuning::is_default`]。
//! 指定が黙って無視される fail-open を避ける。#2120／#2121 で撤去する）。

use crate::hybrid::MAX_POOL_DEPTH;
use crate::json::{parse_json, JsonValue};
use crate::sql::allowlist::{NamedArgValue, SqlSurfaceError};

/// 名前付き調整引数として受理する引数名の種類数（重複を別途拒否するため、個数上限を兼ねる）。
pub(crate) const MAX_NAMED_TUNING_ARGS: usize = 5;

/// `HYBRID` の調整引数の束縛結果。フィールドはすべて「指定されたときだけ `Some`」。
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct HybridTuning {
    k1: Option<f64>,
    b: Option<f64>,
    rrf_k: Option<f64>,
    window: Option<usize>,
    weights: Option<(f64, f64)>,
}

impl HybridTuning {
    /// 値の範囲を検証して構築する（SEARCH-13）。`k1` は有限かつ 0 以上、`b` は有限かつ 0〜1、
    /// `rrf_k` は有限かつ正、`window` は `1..=MAX_POOL_DEPTH`、`weights`（密・疎）は各値が
    /// 有限かつ 0 以上で両方 0 は不可。違反は `22000`。`--max-result-rows` に依存する
    /// `window` の上限は実行時設定が要るため #2120 の実行器が検査する。
    pub fn new(
        k1: Option<f64>,
        b: Option<f64>,
        rrf_k: Option<f64>,
        window: Option<usize>,
        weights: Option<(f64, f64)>,
    ) -> Result<Self, SqlSurfaceError> {
        if let Some(v) = k1 {
            if !v.is_finite() || v < 0.0 {
                return Err(range_error("k1"));
            }
        }
        if let Some(v) = b {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(range_error("b"));
            }
        }
        if let Some(v) = rrf_k {
            if !v.is_finite() || v <= 0.0 {
                return Err(range_error("rrf_k"));
            }
        }
        if let Some(v) = window {
            if v == 0 || v > MAX_POOL_DEPTH {
                return Err(range_error("window"));
            }
        }
        if let Some((d, s)) = weights {
            if !d.is_finite() || !s.is_finite() || d < 0.0 || s < 0.0 || (d == 0.0 && s == 0.0) {
                return Err(range_error("weights"));
            }
        }
        Ok(Self {
            k1,
            b,
            rrf_k,
            window,
            weights,
        })
    }

    /// 名前付き引数（`sql::allowlist` が受理した `name => value` 列）から束縛する。未知の名前は
    /// `42601`、値の型・範囲の違反は `22000`。名前は大文字小文字を区別しない。
    pub(crate) fn from_named_args(
        named: &[(String, NamedArgValue)],
    ) -> Result<Self, SqlSurfaceError> {
        let mut k1 = None;
        let mut b = None;
        let mut rrf_k = None;
        let mut window = None;
        let mut weights = None;
        for (name, value) in named {
            match name.to_ascii_lowercase().as_str() {
                "k1" => k1 = Some(number_value("k1", value)?),
                "b" => b = Some(number_value("b", value)?),
                "rrf_k" => rrf_k = Some(number_value("rrf_k", value)?),
                "window" => window = Some(window_value(value)?),
                "weights" => weights = Some(weights_value(value)?),
                _ => {
                    return Err(SqlSurfaceError::unsupported(format!(
                        "unsupported HYBRID tuning argument: {name}"
                    )))
                }
            }
        }
        Self::new(k1, b, rrf_k, window, weights)
    }

    /// 何も指定されていない（既定の挙動）かどうか。実行器が調整引数を使うまでの暫定ゲートが使う。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// BM25 の `k1`（指定時のみ）。
    pub fn k1(&self) -> Option<f64> {
        self.k1
    }

    /// BM25 の `b`（指定時のみ）。
    pub fn b(&self) -> Option<f64> {
        self.b
    }

    /// RRF の定数 `rrf_k`（指定時のみ）。
    pub fn rrf_k(&self) -> Option<f64> {
        self.rrf_k
    }

    /// 各リストの取得窓（指定時のみ）。
    pub fn window(&self) -> Option<usize> {
        self.window
    }

    /// 重み（密, 疎）（指定時のみ）。
    pub fn weights(&self) -> Option<(f64, f64)> {
        self.weights
    }
}

/// 暫定ゲート（fail-closed）: 調整引数を持つ `Ranking::Hybrid` の実行・EXPLAIN を `0A000` で拒否する。
/// 実行器（`sql::exec` の hybrid 腕）が束縛値を使うようになるまで、指定が既定値のまま黙って
/// 無視されることを防ぐ。`sql::exec`（実行）と `core.rs` の EXPLAIN 経路から呼ばれる。
/// #2120（`rrf_k`・`window`・`weights`）／#2121（`k1`・`b`）で撤去する。
pub(crate) fn ensure_tuning_applied(
    ranking: &crate::sql::parser::Ranking,
) -> Result<(), SqlSurfaceError> {
    match ranking {
        crate::sql::parser::Ranking::Hybrid { tuning, .. } if !tuning.is_default() => Err(
            SqlSurfaceError::feature_not_supported("HYBRID tuning arguments are not supported yet"),
        ),
        _ => Ok(()),
    }
}

fn range_error(name: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_input(format!("HYBRID argument {name} is out of range"))
}

fn type_error(name: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_input(format!("HYBRID argument {name} has an invalid type"))
}

/// 数値引数。文字列リテラルは型の不一致（暗黙変換はしない。fail-closed）。
fn number_value(name: &str, value: &NamedArgValue) -> Result<f64, SqlSurfaceError> {
    match value {
        NamedArgValue::Number(text) => text.parse::<f64>().map_err(|_| type_error(name)),
        NamedArgValue::StringLiteral(_) => Err(type_error(name)),
    }
}

/// `window`: 整数のみ（小数点・指数・符号付き負数は型の不一致）。桁あふれは範囲外。いずれも `22000`。
fn window_value(value: &NamedArgValue) -> Result<usize, SqlSurfaceError> {
    match value {
        NamedArgValue::Number(text) => {
            if text.is_empty() || !text.bytes().all(|c| c.is_ascii_digit()) {
                return Err(type_error("window"));
            }
            text.parse::<usize>().map_err(|_| range_error("window"))
        }
        NamedArgValue::StringLiteral(_) => Err(type_error("window")),
    }
}

/// `weights`: 文字列リテラル `'{"dense": <数>, "sparse": <数>}'`（キーはこの 2 つちょうど）。
fn weights_value(value: &NamedArgValue) -> Result<(f64, f64), SqlSurfaceError> {
    let NamedArgValue::StringLiteral(text) = value else {
        return Err(type_error("weights"));
    };
    let JsonValue::Object(map) = parse_json(text).map_err(|_| type_error("weights"))? else {
        return Err(type_error("weights"));
    };
    if map.len() != 2 {
        return Err(type_error("weights"));
    }
    let pick = |key: &str| match map.get(key) {
        Some(JsonValue::Number(n)) => Ok(n.as_f64()),
        _ => Err(type_error("weights")),
    };
    Ok((pick("dense")?, pick("sparse")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str) -> NamedArgValue {
        NamedArgValue::Number(s.to_string())
    }
    fn st(s: &str) -> NamedArgValue {
        NamedArgValue::StringLiteral(s.to_string())
    }
    fn one(name: &str, v: NamedArgValue) -> Result<HybridTuning, SqlSurfaceError> {
        HybridTuning::from_named_args(&[(name.to_string(), v)])
    }
    fn code(r: Result<HybridTuning, SqlSurfaceError>) -> &'static str {
        r.expect_err("must be rejected").wire_code()
    }

    /// SEARCH-13: 受理（境界値を含む）。
    #[test]
    fn accepts_boundaries() {
        assert_eq!(one("k1", num("0")).unwrap().k1(), Some(0.0));
        assert_eq!(one("K1", num("1.2")).unwrap().k1(), Some(1.2));
        assert_eq!(one("b", num("0")).unwrap().b(), Some(0.0));
        assert_eq!(one("b", num("1")).unwrap().b(), Some(1.0));
        assert_eq!(one("rrf_k", num("10")).unwrap().rrf_k(), Some(10.0));
        assert_eq!(one("window", num("1")).unwrap().window(), Some(1));
        let max = MAX_POOL_DEPTH.to_string();
        assert_eq!(
            one("window", num(&max)).unwrap().window(),
            Some(MAX_POOL_DEPTH)
        );
        let w = one("weights", st(r#"{"dense": 0, "sparse": 2.5}"#)).unwrap();
        assert_eq!(w.weights(), Some((0.0, 2.5)));
        assert!(!w.is_default());
        assert!(HybridTuning::from_named_args(&[]).unwrap().is_default());
    }

    /// SEARCH-13: 範囲外・型の不一致は 22000。
    #[test]
    fn rejects_out_of_range_and_type_mismatch() {
        for (n, v) in [
            ("k1", num("-0.1")),
            ("k1", num("1e999")),
            ("k1", st("1")),
            ("b", num("1.0001")),
            ("b", num("-1")),
            ("rrf_k", num("0")),
            ("rrf_k", num("-5")),
            ("rrf_k", num("1e999")),
            ("window", num("0")),
            ("window", num("1.5")),
            ("window", num("1e3")),
            ("window", num("-1")),
            ("window", num(&(MAX_POOL_DEPTH + 1).to_string())),
            ("window", num("99999999999999999999999999")),
            ("window", st("5")),
            ("weights", num("1")),
            ("weights", st("not json")),
            ("weights", st(r#"{"dense": 1}"#)),
            ("weights", st(r#"{"dense": 1, "sparse": 1, "x": 1}"#)),
            ("weights", st(r#"{"dense": 1, "sparse": "a"}"#)),
            ("weights", st(r#"{"dense": -1, "sparse": 1}"#)),
            ("weights", st(r#"{"dense": 0, "sparse": 0}"#)),
            ("weights", st(r#"{"dense": 1, "weight": 1}"#)),
        ] {
            assert_eq!(code(one(n, v.clone())), "22000", "{n} {v:?}");
        }
    }

    /// SEARCH-13: 未知の引数名は 42601。
    #[test]
    fn rejects_unknown_name() {
        for n in ["pool_depth", "dense_weight", "k2"] {
            assert_eq!(code(one(n, num("1"))), "42601", "{n}");
        }
    }
}
