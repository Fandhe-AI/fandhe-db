//! `precision` モードの実行契約（TASK-162、対象ビヘイビア: SEARCH-9。ポインタ:
//! `docs/spec/05-tasks.md` TASK-162・`docs/spec/04-behavior/search.md` SEARCH-9）。
//!
//! 責務境界: 候補生成（`sql::exec::execute_statement` の DISTANCE 段。`recall` と
//! 共通の dense／hybrid RRF）が返した順位付き候補列に対して、確信度判定
//! （「ゲート」）を適用する純粋関数群を提供する。ゲートは
//! - 確信度が閾値以上なら上位少数件（既定 Top-1）を返す
//! - 閾値未満なら**空集合**（エラーではなく通常応答の 0 行）を返す（fail-closed）
//!
//! を行う。呼び出し元は `sql::exec::execute_statement` のみで、DISTANCE 段（＋
//! `HINT ORDER` で先行する場合は SCALAR 事後フィルタ）の**後**、`RlsSafetyNet::apply`
//! の**前**に適用する（`sql::exec` モジュールドキュメント「PRECISION ゲート段」参照）。
//! `RlsSafetyNet` は行を「減らす」ことしかしないため、ゲート通過後に安全網が行を
//! 落としても「確信のない行が増える」方向にはならず fail-closed が保たれる。
//!
//! リランキング層（`rerank.rs`。SEARCH-7）は `sql::exec` のハイブリッド経路の後段へ結線済み
//! （Issue #2142・SEARCH-15。設計記録: `docs/design/rerank-wiring-policy.md` D3）だが、
//! `precision` モードではリランキングを適用しない。確信度閾値（[`ConfidenceThresholds`]）は
//! dense／RRF の尺度で校正されており、別尺度のスコアへ当てると誤った空集合や誤った確信を
//! 生むためである。よってゲートが見る「最終スコア」は常に候補生成スコア
//! （dense の cosine 類似度・hybrid の正規化 RRF スコア）であり、リランキング後スコアから
//! 確信度を計算する経路は持たない。
//!
//! **fail-open 経路を持たない**ことが本モジュールの中心的な設計制約
//! （security.md「不安全な設計」）: [`PrecisionPolicy`] は `EngineCore` が保持する
//! サーバー側の設定値であり、`SessionState`・`BoundStatement`・SQL 構文のいずれにも
//! 対応するフィールド・句を持たない。外部入力（クエリ・セッション変数）から閾値を
//! 変更する経路は構造的に存在しない。加えて [`ConfidenceThresholds::new`]／
//! [`PrecisionPolicy::new`] は閾値 0・負値・非有限値を型レベルで拒否する
//! （「閾値 0」＝実質 fail-open になるため）。

use std::fmt;

/// dense／hybrid いずれかのランキング方式に対する確信度閾値。フィールドは非公開とし、
/// [`Self::new`] の検証（有限・厳密に正）を経ずに構築する経路を持たない
/// （fail-open な `0.0` 相当の値が構造体リテラルで紛れ込むのを型で防ぐ）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConfidenceThresholds {
    min_top1: f64,
    min_margin: f64,
}

impl ConfidenceThresholds {
    /// `min_top1`・`min_margin` は共に有限かつ**厳密に正**であることを要求する
    /// （`0.0` は「常に通過」＝fail-open と等価になるため拒否する）。
    pub fn new(min_top1: f64, min_margin: f64) -> Result<Self, PrecisionError> {
        if !min_top1.is_finite() || min_top1 <= 0.0 {
            return Err(PrecisionError::InvalidThreshold {
                detail: "min_top1 must be finite and strictly positive".to_string(),
            });
        }
        if !min_margin.is_finite() || min_margin <= 0.0 {
            return Err(PrecisionError::InvalidThreshold {
                detail: "min_margin must be finite and strictly positive".to_string(),
            });
        }
        Ok(Self {
            min_top1,
            min_margin,
        })
    }

    pub fn min_top1(&self) -> f64 {
        self.min_top1
    }

    pub fn min_margin(&self) -> f64 {
        self.min_margin
    }
}

/// `precision` モードの実行契約を制御するサーバー側設定値（TASK-162）。
///
/// 保持場所は `core::EngineCore`（フィールド `precision_policy`）で、差し替えは
/// [`crate::core::EngineCore::with_precision_policy`]（ビルダー）のみを経由する。
/// クエリ・セッション変数からこの値へ到達する経路は存在しない（本モジュール
/// ドキュメントの fail-open 不在の設計制約を参照）。
///
/// 既定値（[`Self::default`]）は TASK-163（評価基準の実測・目標値確定）までの
/// **仮置き**であり、[`DEFAULT_DENSE_MIN_TOP1`] 等の名前付き定数にまとめてある
/// （変更時に定数名を検索すれば影響箇所が追える）。既定値自体が [`Self::new`] の
/// 検証を通ることを単体テストで固定し、仮置き値の改変が fail-open な値へ漂流するのを
/// 防ぐ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrecisionPolicy {
    dense: ConfidenceThresholds,
    hybrid: ConfidenceThresholds,
    max_results: usize,
}

/// `precision` モードが返してよい最大件数の上限（DoS 対策。確信度計算・ゲート後の
/// truncate 両方をこの件数に閉じ込める）。`core::MAX_SEARCH_K` 以下であることを
/// 下記 `const _` で固定する。
pub const MAX_PRECISION_RESULTS: usize = 100;

const _: () = assert!(
    MAX_PRECISION_RESULTS <= crate::core::MAX_SEARCH_K,
    "precision::MAX_PRECISION_RESULTS must not exceed core::MAX_SEARCH_K"
);

/// dense ランキングの既定確信度閾値（仮置き。TASK-163 で実測確定）。
pub const DEFAULT_DENSE_MIN_TOP1: f64 = 0.80;
/// dense ランキングの既定マージン閾値（仮置き。TASK-163 で実測確定）。
pub const DEFAULT_DENSE_MIN_MARGIN: f64 = 0.05;
/// hybrid（正規化 RRF）ランキングの既定確信度閾値（仮置き。TASK-163 で実測確定）。
/// 「両リストとも 1 位」= 1.0、「両リストとも 2 位」≈ 0.984 となる正規化スコールの
/// 値域を踏まえ、単一検索器のみが 1 位に置いた候補（最大 0.5 相当）を意図的に
/// 通過させない値を採用する（モジュールドキュメント参照）。
pub const DEFAULT_HYBRID_MIN_TOP1: f64 = 0.98;
/// hybrid ランキングの既定マージン閾値（仮置き。TASK-163 で実測確定）。1 位・2 位が
/// 同点（マージン 0）の場合は空集合に倒れる値。
pub const DEFAULT_HYBRID_MIN_MARGIN: f64 = 0.005;
/// `precision` モードの既定返却件数上限（仮置き。TASK-163 で実測確定）。
pub const DEFAULT_MAX_RESULTS: usize = 1;

impl Default for PrecisionPolicy {
    fn default() -> Self {
        // 既定値は必ず `ConfidenceThresholds::new`／`PrecisionPolicy::new` の検証を
        // 通過する定数のみで構築する（`precision_policy_default_passes_validation`
        // が検証を固定する）。ここで `unwrap` を使うのは untrusted 入力経路ではなく
        // コンパイル時定数に対してのみであり、AGENTS.md P0 の対象外（`coding-rust.md`
        // 「受信データ経路では unwrap を禁止」は wire 入力経路が対象）。
        Self::new(
            DEFAULT_DENSE_MIN_TOP1,
            DEFAULT_DENSE_MIN_MARGIN,
            DEFAULT_HYBRID_MIN_TOP1,
            DEFAULT_HYBRID_MIN_MARGIN,
            DEFAULT_MAX_RESULTS,
        )
        .expect("PrecisionPolicy default constants must pass PrecisionPolicy::new validation")
    }
}

impl PrecisionPolicy {
    /// 検証付きコンストラクタ。各閾値は [`ConfidenceThresholds::new`] の検証を経て、
    /// `max_results` は `1..=MAX_PRECISION_RESULTS` の範囲であることを要求する。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dense_min_top1: f64,
        dense_min_margin: f64,
        hybrid_min_top1: f64,
        hybrid_min_margin: f64,
        max_results: usize,
    ) -> Result<Self, PrecisionError> {
        let dense = ConfidenceThresholds::new(dense_min_top1, dense_min_margin)?;
        let hybrid = ConfidenceThresholds::new(hybrid_min_top1, hybrid_min_margin)?;
        if max_results == 0 || max_results > MAX_PRECISION_RESULTS {
            return Err(PrecisionError::InvalidMaxResults { max_results });
        }
        Ok(Self {
            dense,
            hybrid,
            max_results,
        })
    }

    pub fn dense(&self) -> ConfidenceThresholds {
        self.dense
    }

    pub fn hybrid(&self) -> ConfidenceThresholds {
        self.hybrid
    }

    pub fn max_results(&self) -> usize {
        self.max_results
    }
}

/// `precision.rs` が返すエラー。契約違反（非有限確信度）のみを表し、`sql::exec` 側で
/// `SqlSurfaceError::Internal`（`XX000`）へ写像する（黙って通さない。閾値未達自体は
/// エラーではなく空集合の通常応答として扱うため、本 enum の対象外）。
#[derive(Debug, Clone, PartialEq)]
pub enum PrecisionError {
    /// [`ConfidenceThresholds::new`] の検証失敗（非有限・0 以下）。
    InvalidThreshold { detail: String },
    /// [`PrecisionPolicy::new`] の `max_results` 検証失敗。
    InvalidMaxResults { max_results: usize },
    /// ゲート適用時に非有限（NaN／Inf）な確信度を検出した（contract 違反。
    /// `cosine_similarity`／`rrf_normalized` はノルム 0 等を `None` に変換して
    /// 呼び出し元へ「確信なし」を伝える設計のため、ここへ到達するのは呼び出し元が
    /// 検証を経ずに非有限値を渡した場合のみ）。
    NonFiniteConfidence,
}

impl fmt::Display for PrecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrecisionError::InvalidThreshold { detail } => {
                write!(f, "invalid precision confidence threshold: {detail}")
            }
            PrecisionError::InvalidMaxResults { max_results } => write!(
                f,
                "invalid precision max_results: {max_results} (must be 1..={MAX_PRECISION_RESULTS})"
            ),
            PrecisionError::NonFiniteConfidence => {
                write!(f, "precision gate received a non-finite confidence value")
            }
        }
    }
}

impl std::error::Error for PrecisionError {}

/// 確信度判定（ゲート）本体。`conf` は DISTANCE 段（＋事後 SCALAR フィルタ）が確定した
/// 順位順の確信度列（先頭が Top-1）。戻り値は `hits` から先頭何件を残すか（`hits.truncate(n)`
/// で使う）。
///
/// 規則（順に評価）:
/// 1. `conf` が空 → `0`
/// 2. `conf[0]` が非有限 → [`PrecisionError::NonFiniteConfidence`]（黙って通さない）
/// 3. `conf[0] < thresholds.min_top1()` → `0`
/// 4. `conf.len() >= 2` かつ `conf[0] - conf[1] < thresholds.min_margin()` → `0`
///    （Top-2 が存在しない場合はマージン条件を「満たす」扱いとするが、規則 3 の
///    絶対閾値は Top-2 の有無に関係なく常に適用する）
/// 5. それ以外 → `min(limit, max_results, 先頭から連続して min_top1 を満たす件数)`
///
/// 順位そのものは変更しない（ゲートは既存の順位順 `conf` をそのまま読むだけで、
/// 独自の再ソートは行わない。`.claude/rules/coding-rust.md` の安定ソート方針に加え、
/// 候補生成側〔dense/hybrid〕が確定した順序を最終出力まで保つのが本モジュールの
/// 契約であるため）。
pub fn apply_gate(
    conf: &[f64],
    thresholds: &ConfidenceThresholds,
    limit: usize,
    max_results: usize,
) -> Result<usize, PrecisionError> {
    let Some(&top1) = conf.first() else {
        return Ok(0);
    };
    if !top1.is_finite() {
        return Err(PrecisionError::NonFiniteConfidence);
    }
    if top1 < thresholds.min_top1() {
        return Ok(0);
    }
    if let Some(&top2) = conf.get(1) {
        if !top2.is_finite() {
            return Err(PrecisionError::NonFiniteConfidence);
        }
        if top1 - top2 < thresholds.min_margin() {
            return Ok(0);
        }
    }
    // 先頭から連続して `min_top1` を満たす件数を数える（非有限値に当たった場合も
    // 契約違反として扱う。`conf` は候補生成側が確定した有界な列のため、この走査は
    // O(min(limit, max_results, conf.len())) に収まる）。
    let cap = limit.min(max_results);
    let mut n = 0usize;
    for &c in conf.iter().take(cap) {
        if !c.is_finite() {
            return Err(PrecisionError::NonFiniteConfidence);
        }
        if c < thresholds.min_top1() {
            break;
        }
        n += 1;
    }
    Ok(n)
}

/// クエリベクトルと候補 embedding の cosine 類似度を `f64` で計算する。いずれかの
/// ノルムが 0（有限だが非正）、または内積・ノルムの計算結果が非有限になる場合は
/// `None`（「確信なし」として呼び出し元がゲートを空集合へ倒す）を返す。次元不一致
/// （`a.len() != b.len()`）も同様に `None` とする（束縛段で次元検証済みのため通常
/// 到達しないが、契約違反を `panic` させず fail-closed に倒す）。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Option<f64> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let xf = x as f64;
        let yf = y as f64;
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    if !dot.is_finite() || !norm_a.is_finite() || !norm_b.is_finite() {
        return None;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if !denom.is_finite() || denom <= 0.0 {
        return None;
    }
    let sim = dot / denom;
    if !sim.is_finite() {
        return None;
    }
    Some(sim)
}

/// RRF 融合スコアを `[0, 1]` 近傍の共通尺度へ正規化する。理論最大値は
/// `(dense_weight + sparse_weight) / (k_const + 1)`（両リストで 1 位を獲得した場合の
/// スコア。`hybrid::RrfConfig` のドキュメント参照）。理論最大値が非有限・0 以下、
/// または結果が非有限になる場合は `None` を返す（呼び出し元は「確信なし」として
/// 空集合へ倒す）。
pub fn rrf_normalized(score: f64, cfg: &crate::hybrid::RrfConfig) -> Option<f64> {
    if !score.is_finite() {
        return None;
    }
    // 重みの和を先に取ると巨大重み（例: 各 1e308）で中間オーバーフローし、有限な理論最大値が
    // 失われるため、項別に分母で割ってから加算する。
    let denom = cfg.k_const() + 1.0;
    let max_score = cfg.dense_weight() / denom + cfg.sparse_weight() / denom;
    if !max_score.is_finite() || max_score <= 0.0 {
        return None;
    }
    let normalized = score / max_score;
    if !normalized.is_finite() {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ConfidenceThresholds::new ---

    #[test]
    fn confidence_thresholds_new_accepts_positive_finite_values() {
        assert!(ConfidenceThresholds::new(0.5, 0.1).is_ok());
    }

    #[test]
    fn confidence_thresholds_new_rejects_zero_min_top1() {
        assert!(matches!(
            ConfidenceThresholds::new(0.0, 0.1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_negative_min_top1() {
        assert!(matches!(
            ConfidenceThresholds::new(-0.1, 0.1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_nan_min_top1() {
        assert!(matches!(
            ConfidenceThresholds::new(f64::NAN, 0.1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_infinite_min_top1() {
        assert!(matches!(
            ConfidenceThresholds::new(f64::INFINITY, 0.1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_zero_min_margin() {
        assert!(matches!(
            ConfidenceThresholds::new(0.5, 0.0),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_negative_min_margin() {
        assert!(matches!(
            ConfidenceThresholds::new(0.5, -0.1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn confidence_thresholds_new_rejects_nonfinite_min_margin() {
        assert!(matches!(
            ConfidenceThresholds::new(0.5, f64::NAN),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
        assert!(matches!(
            ConfidenceThresholds::new(0.5, f64::INFINITY),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    // --- PrecisionPolicy::new / Default ---

    #[test]
    fn precision_policy_default_passes_validation() {
        let policy = PrecisionPolicy::default();
        assert_eq!(policy.max_results(), DEFAULT_MAX_RESULTS);
        // `Default` 自体が `new` の検証を経て構築される（`unwrap`／`expect` は
        // コンパイル時定数に対してのみ）。ここでは値の再検証で固定する。
        assert!(PrecisionPolicy::new(
            policy.dense().min_top1(),
            policy.dense().min_margin(),
            policy.hybrid().min_top1(),
            policy.hybrid().min_margin(),
            policy.max_results(),
        )
        .is_ok());
    }

    #[test]
    fn precision_policy_new_rejects_zero_max_results() {
        assert!(matches!(
            PrecisionPolicy::new(0.5, 0.1, 0.5, 0.1, 0),
            Err(PrecisionError::InvalidMaxResults { max_results: 0 })
        ));
    }

    #[test]
    fn precision_policy_new_rejects_max_results_over_cap() {
        assert!(matches!(
            PrecisionPolicy::new(0.5, 0.1, 0.5, 0.1, MAX_PRECISION_RESULTS + 1),
            Err(PrecisionError::InvalidMaxResults { .. })
        ));
    }

    #[test]
    fn precision_policy_new_accepts_max_results_at_cap() {
        assert!(PrecisionPolicy::new(0.5, 0.1, 0.5, 0.1, MAX_PRECISION_RESULTS).is_ok());
    }

    #[test]
    fn precision_policy_new_propagates_dense_threshold_error() {
        assert!(matches!(
            PrecisionPolicy::new(0.0, 0.1, 0.5, 0.1, 1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    #[test]
    fn precision_policy_new_propagates_hybrid_threshold_error() {
        assert!(matches!(
            PrecisionPolicy::new(0.5, 0.1, 0.0, 0.1, 1),
            Err(PrecisionError::InvalidThreshold { .. })
        ));
    }

    // --- apply_gate ---

    fn thresholds() -> ConfidenceThresholds {
        ConfidenceThresholds::new(0.8, 0.05).expect("valid thresholds")
    }

    #[test]
    fn apply_gate_returns_zero_for_empty_confidence() {
        assert_eq!(apply_gate(&[], &thresholds(), 10, 10).unwrap(), 0);
    }

    #[test]
    fn apply_gate_returns_zero_when_top1_below_threshold() {
        assert_eq!(apply_gate(&[0.5], &thresholds(), 10, 10).unwrap(), 0);
    }

    #[test]
    fn apply_gate_returns_one_when_single_hit_meets_threshold() {
        assert_eq!(apply_gate(&[0.9], &thresholds(), 10, 10).unwrap(), 1);
    }

    #[test]
    fn apply_gate_returns_zero_when_margin_insufficient() {
        // top1=0.9, top2=0.88: 差 0.02 < min_margin 0.05
        assert_eq!(apply_gate(&[0.9, 0.88], &thresholds(), 10, 10).unwrap(), 0);
    }

    #[test]
    fn apply_gate_returns_zero_when_margin_negative() {
        // top1 < top2 は margin が負になり、常に閾値未満として空集合になる
        assert_eq!(apply_gate(&[0.9, 0.95], &thresholds(), 10, 10).unwrap(), 0);
    }

    #[test]
    fn apply_gate_passes_when_margin_sufficient() {
        // top1=0.95, top2=0.80: 差 0.15 >= 0.05。両方とも min_top1 (0.8) 以上のため
        // 規則 5 の連続プレフィックス判定は 2 件とも満たす。
        assert_eq!(apply_gate(&[0.95, 0.80], &thresholds(), 10, 10).unwrap(), 2);
    }

    #[test]
    fn apply_gate_absolute_threshold_applies_even_without_top2() {
        // Top-2 が存在しない場合はマージン条件は「満たす」扱いだが、絶対閾値
        // （規則 3）は常に適用される。
        assert_eq!(apply_gate(&[0.5], &thresholds(), 10, 10).unwrap(), 0);
        assert_eq!(apply_gate(&[0.9], &thresholds(), 10, 10).unwrap(), 1);
    }

    #[test]
    fn apply_gate_respects_limit_cap() {
        // top1/top2 のマージンを十分に取り（0.99-0.90=0.09 >= 0.05）、cap（limit=2）が
        // 効くことだけを検査する。
        assert_eq!(
            apply_gate(&[0.99, 0.90, 0.89, 0.88], &thresholds(), 2, 10).unwrap(),
            2
        );
    }

    #[test]
    fn apply_gate_respects_max_results_cap() {
        assert_eq!(
            apply_gate(&[0.99, 0.90, 0.89, 0.88], &thresholds(), 10, 2).unwrap(),
            2
        );
    }

    #[test]
    fn apply_gate_stops_at_first_confidence_below_top1_threshold() {
        // top1/top2 のマージンを満たした上で、3 番目が min_top1 未満のため
        // そこで連続プレフィックス判定が止まる。
        assert_eq!(
            apply_gate(&[0.99, 0.90, 0.5, 0.99], &thresholds(), 10, 10).unwrap(),
            2
        );
    }

    #[test]
    fn apply_gate_rejects_nonfinite_top1() {
        assert_eq!(
            apply_gate(&[f64::NAN], &thresholds(), 10, 10),
            Err(PrecisionError::NonFiniteConfidence)
        );
        assert_eq!(
            apply_gate(&[f64::INFINITY], &thresholds(), 10, 10),
            Err(PrecisionError::NonFiniteConfidence)
        );
    }

    #[test]
    fn apply_gate_rejects_nonfinite_top2() {
        assert_eq!(
            apply_gate(&[0.9, f64::NAN], &thresholds(), 10, 10),
            Err(PrecisionError::NonFiniteConfidence)
        );
    }

    // --- cosine_similarity ---

    #[test]
    fn cosine_similarity_identical_vectors_is_one() {
        let v = [1.0f32, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v).expect("must be Some");
        assert!((sim - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors_is_zero() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        let sim = cosine_similarity(&a, &b).expect("must be Some");
        assert!(sim.abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_zero_norm_is_none() {
        let a = [0.0f32, 0.0];
        let b = [1.0f32, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);
    }

    #[test]
    fn cosine_similarity_dimension_mismatch_is_none() {
        let a = [1.0f32, 0.0];
        let b = [1.0f32, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), None);
    }

    #[test]
    fn cosine_similarity_empty_is_none() {
        assert_eq!(cosine_similarity(&[], &[]), None);
    }

    // --- rrf_normalized ---

    #[test]
    fn rrf_normalized_max_score_is_one() {
        let cfg = crate::hybrid::RrfConfig::new(60.0, 1.0, 1.0, 10).expect("valid cfg");
        // 両リストとも 1 位（rank=1）のスコア: weight/(k+1) + weight/(k+1)
        // 重みの和を先に取ると巨大重み（例: 各 1e308）で中間オーバーフローし、有限な理論最大値が
        // 失われるため、項別に分母で割ってから加算する。
        let denom = cfg.k_const() + 1.0;
        let max_score = cfg.dense_weight() / denom + cfg.sparse_weight() / denom;
        let normalized = rrf_normalized(max_score, &cfg).expect("must be Some");
        assert!((normalized - 1.0).abs() < 1e-9);
    }

    #[test]
    fn rrf_normalized_rejects_nonfinite_score() {
        let cfg = crate::hybrid::RrfConfig::new(60.0, 1.0, 1.0, 10).expect("valid cfg");
        assert_eq!(rrf_normalized(f64::NAN, &cfg), None);
        assert_eq!(rrf_normalized(f64::INFINITY, &cfg), None);
    }

    #[test]
    fn rrf_normalized_survives_huge_weights() {
        let cfg = crate::hybrid::RrfConfig::new(60.0, 1e308, 1e308, 10).expect("cfg");
        let normalized = rrf_normalized(1.0, &cfg).expect("must be Some");
        assert!(normalized.is_finite() && normalized > 0.0);
    }

    #[test]
    fn rrf_normalized_zero_score_is_zero() {
        let cfg = crate::hybrid::RrfConfig::new(60.0, 1.0, 1.0, 10).expect("valid cfg");
        let normalized = rrf_normalized(0.0, &cfg).expect("must be Some");
        assert_eq!(normalized, 0.0);
    }
}
