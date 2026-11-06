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
//! 実行器への接続（Issue #2120）: `rrf_k`・`window`・`weights` は [`hybrid_rrf_config`] が
//! `RrfConfig` と候補プール深さに組み立てる。`window` の範囲検証は [`check_window`] に集約し、
//! 束縛段・`EngineCore::check_bound_search`・実行時の 3 層から呼ぶ。
//!
//! BM25 への接続（Issue #2121）: `k1`・`b` は [`hybrid_bm25_params`] がクエリ時の
//! `Bm25Params` に解決し、疎検索のスコア計算だけに渡す（索引・キャッシュキー・世代は不変）。
//! これにより #2119 の暫定ゲート（`0A000`）は撤去した。
//!
//! EXPLAIN への接続（Issue #3051）: [`explain_hybrid_params`] が上記と同じ解決関数
//! （[`hybrid_bm25_params`]・[`hybrid_rrf_config`]）を呼んで実効値と指定元を求め、`core.rs` の
//! EXPLAIN 経路が `hybrid_rrf:`／`hybrid_bm25:` 行として出す。既定値は再導出しない。

use crate::hybrid::{RrfConfig, MAX_POOL_DEPTH};
use crate::json::{parse_json, JsonValue};
use crate::sql::allowlist::{NamedArgValue, SqlSurfaceError};

/// `Ranking::Hybrid` の検索語を構文（フレーズ・`or`・除外・前方一致）として解釈するかの区分
/// （Issue #2116・SEARCH-12 のポインタ。設計記録 `docs/design/fulltext-query-syntax.md` D8・D9）。
/// `BoundStatement` の `pub(crate)` フィールドで保持し、`sql::exec` の `Ranking::Hybrid` アームが
/// 実行経路を選ぶ。公開型の `Ranking::Hybrid` にはフィールドを足さない（破壊的変更の回避）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum HybridTextSyntax {
    /// 検索語を構文として解釈する（SQL の `HYBRID`・NoSQL の `hybrid.text`。既定）。
    #[default]
    WebSearch,
    /// 検索語をすべて literal の語列として扱う。`USING PLAN` が LLM 出力の `search_terms` を
    /// 連結した検索語に使い、演算子による検索結果の操作（プロンプトインジェクション）を防ぐ（D9）。
    Literal,
}

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
    /// `window` の上限・`LIMIT` との関係は実行時設定が要るため [`check_window`]
    /// （束縛段・`check_bound_search`・実行時の 3 層）が検査する。
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

/// 調整引数 `k1`／`b` をクエリ時の BM25 パラメータへ解決する（SEARCH-13・Issue #2121）。
/// 未指定の項目は索引自身の構築時の値（`index_params`。SQL 経路では既定値）で補うため、
/// 指定なしの文は従来とバイト一致する。数値安全上限を含む検証違反は `22000`
/// （固定の英語文言。入力値は含めない）。`sql::exec` の `Ranking::Hybrid` 実行から呼ばれる。
pub(crate) fn hybrid_bm25_params(
    tuning: &HybridTuning,
    index_params: crate::sparse::Bm25Params,
) -> Result<crate::sparse::Bm25Params, SqlSurfaceError> {
    crate::sparse::Bm25Params::new(
        tuning.k1().unwrap_or(index_params.k1()),
        tuning.b().unwrap_or(index_params.b()),
    )
    .map_err(|_| SqlSurfaceError::invalid_input("HYBRID BM25 arguments k1 and b are out of range"))
}

/// `Ranking::Hybrid` の `k1`／`b` を、疎索引の有無に依らず同じ基準で検証する（Issue #2121）。
/// 疎索引が無い（密のみへ縮退する）文でも不正な引数がデータ依存で受理されないよう、
/// 既定値で補った [`hybrid_bm25_params`] を通す。`core.rs::EngineCore::check_bound_search`
/// （EXPLAIN を含む全経路）と `sql::exec` の `Ranking::Hybrid` 実行から呼ばれる。
pub(crate) fn check_bm25_args(
    ranking: &crate::sql::parser::Ranking,
) -> Result<(), SqlSurfaceError> {
    let crate::sql::parser::Ranking::Hybrid { tuning, .. } = ranking else {
        return Ok(());
    };
    hybrid_bm25_params(tuning, crate::sparse::Bm25Params::default()).map(|_| ())
}

/// `window` の範囲検証（SEARCH-13・Issue #2120）。`window` 未指定なら何もしない。
/// 受理条件は `fetch_count`（`LIMIT + OFFSET`）以上かつ `max_result_rows` 以下で、
/// 範囲外はクランプ・黙った件数削減をせず `22000` で拒否する（fail-closed）。`LIMIT` 省略
/// （`result_cap` が `Some`）の文は取得窓が受理範囲を常に超えるため専用文言で拒否する。
/// `sql::parser` の束縛段と `core.rs::EngineCore::check_bound_search`（外部で組み立てた計画への
/// 多層防御）から呼ばれる。エラー文言は固定の英語で、入力値・件数を含めない。
pub(crate) fn check_window(
    ranking: &crate::sql::parser::Ranking,
    fetch_count: usize,
    result_cap: Option<usize>,
    max_result_rows: usize,
    mode: crate::sql::mode::SearchMode,
) -> Result<(), SqlSurfaceError> {
    let crate::sql::parser::Ranking::Hybrid { tuning, .. } = ranking else {
        return Ok(());
    };
    let Some(w) = tuning.window() else {
        return Ok(());
    };
    // precision は `k_eff` が可視件数（SCALAR 先行時は `fetch_count().max(2)`）に依存し、束縛段で
    // `window` との大小を決められないため、データ依存の実行時 22000 を避けて組み合わせ自体を
    // 明示的に拒否する（fail-closed。文言は固定で値を含めない）。
    if mode == crate::sql::mode::SearchMode::Precision {
        return Err(SqlSurfaceError::feature_not_supported(
            "HYBRID argument window cannot be combined with precision mode",
        ));
    }
    if w > max_result_rows {
        return Err(range_error("window"));
    }
    if result_cap.is_some() {
        return Err(SqlSurfaceError::invalid_input(
            "HYBRID argument window requires an explicit LIMIT",
        ));
    }
    if w < fetch_count {
        return Err(SqlSurfaceError::invalid_input(
            "HYBRID argument window must be at least LIMIT plus OFFSET",
        ));
    }
    Ok(())
}

/// 調整引数から RRF 設定を組み立てる（SEARCH-13・Issue #2120）。`sql::exec` の hybrid 融合と
/// precision 正規化が同じ値を使うよう、組み立てをここに 1 本化する。
///
/// - `rrf_k`・`weights` 未指定は [`RrfConfig::default`] の値（60・等重み）。
/// - `window` 未指定の `pool_depth` は従来どおり `k_eff.max(既定深さ)`（無指定文の結果を
///   バイト一致させる）。指定時は `window` そのもので、`k_eff > window` は `22000`
///   （precision／SCALAR 事後フィルタ経路は `k_eff` がデータ件数に依存するため、実行時の
///   最終防衛。`hybrid_search_boosted` の `InvalidK` が `XX000` に化けるのを防ぐ。拒否条件は
///   自テナントの可視件数のみに依存し、文言は固定で件数を含めない）。
pub(crate) fn hybrid_rrf_config(
    tuning: &HybridTuning,
    k_eff: usize,
) -> Result<RrfConfig, SqlSurfaceError> {
    let default = RrfConfig::default();
    let pool_depth = match tuning.window() {
        None => k_eff.max(default.pool_depth()),
        Some(w) => {
            if k_eff > w {
                return Err(SqlSurfaceError::invalid_input(
                    "HYBRID argument window must be at least the number of candidates requested",
                ));
            }
            w
        }
    };
    let (dense_weight, sparse_weight) = tuning
        .weights()
        .unwrap_or((default.dense_weight(), default.sparse_weight()));
    RrfConfig::new(
        tuning.rrf_k().unwrap_or(default.k_const()),
        dense_weight,
        sparse_weight,
        pool_depth,
    )
    .map_err(|_| SqlSurfaceError::Internal {
        detail: "invalid hybrid RRF config".to_string(),
    })
}

/// EXPLAIN の `hybrid_rrf:`／`hybrid_bm25:` 行の入力を解決する（SEARCH-13・Issue #3051）。実行器と
/// 同じ [`hybrid_bm25_params`]・[`hybrid_rrf_config`] を呼ぶため既定値を二重管理しない。
/// `static_k_eff` は `sql::exec::static_fetch_k` の結果で、`None`（precision かつ SCALAR 先行でない
/// ＝取得件数が可視件数に依存）のとき既定の `window` は数値を出さず `Dynamic` にする
/// （テナントのデータ量を EXPLAIN に漏らさない）。この場合に `window` が指定済みの組み合わせは束縛段が
/// `0A000` で拒否済みで到達しないが、多層防御として `Internal` で fail-closed にする。
/// `core.rs::search_explain_from_bound` から呼ばれ、解決失敗は実行時と同じ `wire_code` になる。
pub(crate) fn explain_hybrid_params(
    tuning: &HybridTuning,
    static_k_eff: Option<usize>,
) -> Result<crate::sql::explain::ExplainHybridParams, SqlSurfaceError> {
    use crate::sql::explain::{ExplainHybridParams, ExplainHybridWindow, ParamSource};
    let source = |specified: bool| {
        if specified {
            ParamSource::Specified
        } else {
            ParamSource::Default
        }
    };
    let bm25 = hybrid_bm25_params(tuning, crate::sparse::Bm25Params::default())?;
    let (rrf, window) = match static_k_eff {
        Some(k) => {
            let rrf = hybrid_rrf_config(tuning, k)?;
            let window = ExplainHybridWindow::Fixed(rrf.pool_depth());
            (rrf, window)
        }
        None => {
            if tuning.window().is_some() {
                return Err(SqlSurfaceError::Internal {
                    detail: "unexpected HYBRID window with data-dependent fetch size".to_string(),
                });
            }
            (hybrid_rrf_config(tuning, 1)?, ExplainHybridWindow::Dynamic)
        }
    };
    Ok(ExplainHybridParams {
        rrf_k: rrf.k_const(),
        rrf_k_source: source(tuning.rrf_k().is_some()),
        window,
        window_source: source(tuning.window().is_some()),
        dense_weight: rrf.dense_weight(),
        sparse_weight: rrf.sparse_weight(),
        weights_source: source(tuning.weights().is_some()),
        k1: bm25.k1(),
        k1_source: source(tuning.k1().is_some()),
        b: bm25.b(),
        b_source: source(tuning.b().is_some()),
    })
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

    fn tuning(
        k1: Option<f64>,
        b: Option<f64>,
        rrf_k: Option<f64>,
        window: Option<usize>,
        weights: Option<(f64, f64)>,
    ) -> HybridTuning {
        HybridTuning::new(k1, b, rrf_k, window, weights).unwrap()
    }

    /// Issue #3051: 未指定は既定値と `Default`、指定は `Specified`（既定値と等しくても）。
    #[test]
    fn explain_params_defaults_and_sources() {
        use crate::sql::explain::{ExplainHybridWindow, ParamSource};
        let p = explain_hybrid_params(&HybridTuning::default(), Some(10)).unwrap();
        assert_eq!((p.rrf_k, p.dense_weight, p.sparse_weight), (60.0, 1.0, 1.0));
        assert_eq!((p.k1, p.b), (1.2, 0.75));
        assert_eq!(p.window, ExplainHybridWindow::Fixed(200));
        for s in [
            p.rrf_k_source,
            p.window_source,
            p.weights_source,
            p.k1_source,
            p.b_source,
        ] {
            assert_eq!(s, ParamSource::Default);
        }
        let p = explain_hybrid_params(
            &tuning(Some(1.2), None, Some(60.0), None, Some((2.0, 1.0))),
            Some(10),
        )
        .unwrap();
        assert_eq!(p.k1_source, ParamSource::Specified);
        assert_eq!(p.b_source, ParamSource::Default);
        assert_eq!(p.rrf_k_source, ParamSource::Specified);
        assert_eq!(p.weights_source, ParamSource::Specified);
        assert_eq!((p.dense_weight, p.sparse_weight), (2.0, 1.0));
    }

    /// Issue #3051: 既定 window は `k_eff` に追従し、静的に決まらないときは `Dynamic`。
    #[test]
    fn explain_params_window_resolution() {
        use crate::sql::explain::ExplainHybridWindow;
        let t = HybridTuning::default();
        assert_eq!(
            explain_hybrid_params(&t, Some(260)).unwrap().window,
            ExplainHybridWindow::Fixed(260)
        );
        assert_eq!(
            explain_hybrid_params(&t, None).unwrap().window,
            ExplainHybridWindow::Dynamic
        );
        let w = tuning(None, None, None, Some(5), None);
        assert_eq!(
            explain_hybrid_params(&w, Some(3)).unwrap().window,
            ExplainHybridWindow::Fixed(5)
        );
        assert_eq!(
            explain_hybrid_params(&w, None).unwrap_err().wire_code(),
            "XX000"
        );
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

    fn hybrid_ranking(tuning: HybridTuning) -> crate::sql::parser::Ranking {
        crate::sql::parser::Ranking::Hybrid {
            query: vec![1.0],
            text_column_index: 0,
            query_text: "q".to_string(),
            tuning,
        }
    }

    /// Issue #2121: 未指定は索引の値、部分指定は残りを補完、上限超えは 22000。
    #[test]
    fn bm25_params_resolution() {
        let idx = crate::sparse::Bm25Params::new(1.5, 0.4).unwrap();
        let t = HybridTuning::default();
        assert_eq!(hybrid_bm25_params(&t, idx).unwrap(), idx);
        let t = HybridTuning::new(Some(2.0), None, None, None, None).unwrap();
        let p = hybrid_bm25_params(&t, idx).unwrap();
        assert_eq!((p.k1(), p.b()), (2.0, 0.4));
        let t = HybridTuning::new(None, Some(1.0), None, None, None).unwrap();
        let p = hybrid_bm25_params(&t, idx).unwrap();
        assert_eq!((p.k1(), p.b()), (1.5, 1.0));
        let t = HybridTuning::new(Some(1e308), None, None, None, None).unwrap();
        assert_eq!(
            hybrid_bm25_params(&t, idx).unwrap_err().wire_code(),
            "22000"
        );
    }

    /// Issue #2120: `window` の境界（`LIMIT + OFFSET` 以上・上限以下・`LIMIT` 省略は拒否）。
    #[test]
    fn check_window_boundaries() {
        use crate::sql::mode::SearchMode;
        const M: SearchMode = SearchMode::Recall;
        let r =
            |w: usize| hybrid_ranking(HybridTuning::new(None, None, None, Some(w), None).unwrap());
        assert!(check_window(&r(5), 5, None, 100, M).is_ok());
        assert_eq!(
            check_window(&r(4), 5, None, 100, M)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert!(check_window(&r(100), 5, None, 100, M).is_ok());
        assert_eq!(
            check_window(&r(101), 5, None, 100, M)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        assert_eq!(
            check_window(&r(50), 101, Some(100), 100, M)
                .unwrap_err()
                .wire_code(),
            "22000"
        );
        // window 未指定・非 hybrid は対象外。
        assert!(check_window(&hybrid_ranking(HybridTuning::default()), 500, None, 100, M).is_ok());
        // precision と window の組み合わせは束縛段で明示的に 0A000 拒否（k_eff がデータ依存のため）。
        for w in [1, 5, 100] {
            assert_eq!(
                check_window(&r(w), 1, None, 100, SearchMode::Precision)
                    .unwrap_err()
                    .wire_code(),
                "0A000"
            );
        }
        // window 未指定の precision は対象外。
        assert!(check_window(
            &hybrid_ranking(HybridTuning::default()),
            1,
            None,
            100,
            SearchMode::Precision
        )
        .is_ok());
    }

    /// Issue #2120: `window` 未指定は従来の `k_eff.max(200)`、指定時はその値。
    #[test]
    fn rrf_config_assembly() {
        let d = HybridTuning::default();
        let c = hybrid_rrf_config(&d, 10).unwrap();
        assert_eq!(
            (
                c.k_const(),
                c.dense_weight(),
                c.sparse_weight(),
                c.pool_depth()
            ),
            (60.0, 1.0, 1.0, 200)
        );
        assert_eq!(hybrid_rrf_config(&d, 350).unwrap().pool_depth(), 350);
        let t = HybridTuning::new(None, None, Some(7.0), Some(20), Some((2.0, 0.0))).unwrap();
        let c = hybrid_rrf_config(&t, 20).unwrap();
        assert_eq!(
            (
                c.k_const(),
                c.dense_weight(),
                c.sparse_weight(),
                c.pool_depth()
            ),
            (7.0, 2.0, 0.0, 20)
        );
        assert_eq!(hybrid_rrf_config(&t, 21).unwrap_err().wire_code(), "22000");
    }
}
