//! 検索カーネルのリランキング層（TASK-107。対象ビヘイビア: SEARCH-6, SEARCH-7, SEARCH-8）。
//!
//! `hybrid.rs`（TASK-103）が生成する候補プール（[`crate::hybrid::HybridHit`] の列。
//! 既定 `pool_depth = 200`）を受け取り、最終上位 `final_k` 件へ再順位付けする層。
//! `hybrid.rs`・`sparse.rs` と同様に storage・catalog・policy とは結線しない純粋関数的な
//! 層として追加する。RLS 相当の可視性判定はこの層より上（`core.rs` 相当）で完結して
//! いる前提であり、[`rerank_candidates`] へ渡す `candidates`（[`RerankCandidate`] の
//! 列）は呼び出し元があらかじめ可視行のみへ縮約済みであることを契約とする。
//!
//! リランカー方式（クロスエンコーダ等）の最終選定は spec 上オーナー判断（TASK-107 は
//! 共同タスク）であり、外部 ML 推論クレートの導入は依存承認制
//! （`.claude/rules/dependency-policy.md`）に抵触するため、本モジュールでは方式を
//! 差し替え可能にする [`Reranker`] trait（object-safe。[`crate::kernel::SearchProvider`]
//! と同じ `&self` のみの制約）を公開 API の中心に据える。依存追加なしで動く決定的な
//! 参照実装を 2 種（[`IdentityReranker`]・[`LexicalOverlapReranker`]）同梱するが、
//! いずれも本命方式ではなく方式確定までの暫定実装である。
//!
//! `hybrid.rs` の provider・index 結果検証と同じ理由で、[`Reranker`] trait object は
//! 型で「入力候補 id 集合の部分集合のみを返す」ことを強制できない別個のオブジェクトの
//! ため、[`rerank_candidates`] は出力を検証し、契約違反（候補外 id・重複・順序契約
//! 違反・非有限スコア・件数超過）を 1 件でも検知したら部分的に受理せず
//! [`RerankError`] で検索全体を拒否する（fail-closed。事後フィルタは件数差から
//! 不可視データの有無が漏れる経路になるため使わない）。
//!
//! SEARCH-6（候補プール品質の前提）・SEARCH-7（リランキング層本体）は本モジュールが
//! 対応する。SEARCH-8（効果測定の追跡）は TASK-108（Issue #39）が担当し、本モジュールは
//! その効果を測定・主張しない（`crates/engine/tests/rerank.rs` の `//!` も参照）。
//! `VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄でありここでは扱わない。

use std::fmt;

use crate::sparse::tokenize;

/// リランキング設定の上限。`hybrid.rs::MAX_POOL_DEPTH` と同桁を採用し、未検証の
/// 巨大な値がそのままアロケーションサイズへ伝播することを防ぐ
/// （coding-rust.md「無制限確保禁止」）。
const MAX_POOL_DEPTH: usize = 10_000;

/// `query_text` のバイト長上限。[`LexicalOverlapReranker::rerank`] は `query_text` を
/// `tokenize()`（`String`・`BTreeSet` を確保する）へ渡すため、`sparse.rs::MAX_QUERY_BYTES`
/// と同じ理由・同じ値で `tokenize()` を呼ぶ前に `query_text.len()`（アロケーション不要）
/// で判定し、[`rerank_candidates`] の入口で fail-closed に拒否する。
const MAX_QUERY_TEXT_BYTES: usize = 16 * 1024;

/// 候補 1 件（`RerankCandidate::text`）のバイト長上限。`sparse.rs::MAX_DOC_BYTES` と
/// 同じ理由・同じ値を採用する。候補件数自体は [`MAX_POOL_DEPTH`]（`cfg.pool_depth()`）で
/// 別途上限があるため、この上限と合わせて候補側の走査・アロケーションコストの総量を
/// 有界に保つ（[`MAX_CANDIDATE_TEXT_BYTES`] は 1 件あたり、[`MAX_POOL_DEPTH`] は件数の
/// 上限で、互いに独立な検証のため両方が必要）。
pub(crate) const MAX_CANDIDATE_TEXT_BYTES: usize = 1024 * 1024;

/// 候補テキスト（`RerankCandidate::text`）の合計バイト長上限。`sparse.rs::MAX_CORPUS_BYTES`
/// と同じ理由: [`MAX_CANDIDATE_TEXT_BYTES`]（1 件あたり）・[`MAX_POOL_DEPTH`]（件数）は
/// それぞれ独立な上限のため、両方の上限値ちょうどの入力（1 MiB × 10,000 件 ≈ 10 GiB）を
/// 同時に許すと `tokenize()` の総コストが無制限に近い規模へ増幅しうる（CPU DoS）。
/// `sparse.rs` の値をそのまま踏襲する。
pub(crate) const MAX_TOTAL_CANDIDATE_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// リランク対象の候補 1 件（[`crate::hybrid::HybridHit`] から融合スコアと文書テキストを
/// 添えて渡す入力表現）。`text` はリランカーの素性計算専用であり、SQL・プラン文字列の
/// 組み立てには使わない（coding-rust.md「untrusted 入力の扱い」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankCandidate<'a> {
    pub id: u64,
    pub fused_score: f64,
    pub text: &'a str,
    /// 候補の埋め込み（Issue #2143。[`MmrReranker`] だけが読む。字句一致・クロスエンコーダ・
    /// 恒等リランカーは参照しない）。実行器は RLS 適用後の可視アリーナ由来の値だけを渡す。
    pub embedding: Option<&'a [f32]>,
}

/// 再順位付け後の検索結果 1 件。[`crate::hybrid::HybridHit`] とスコア尺度が異なるため
/// 型を分ける（`hybrid.rs` が `kernel::CandidateHit`/`sparse::ScoredDoc` と型を分けた方針と
/// 同じ）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankedHit {
    pub id: u64,
    pub score: f64,
}

/// リランキング層の設定（本モジュールの既定値。関連: TASK-107）。
///
/// - `pool_depth`: 受理する候補件数の上限（既定 200 = `hybrid::RrfConfig::default()` の
///   `pool_depth` と整合）。
/// - `final_k`: 再順位付け後に返す件数（既定 20）。
///
/// フィールドは非 `pub`（private）とし、[`RerankConfig::new`] による検証済み構築のみを
/// 許可する（`hybrid::RrfConfig` と同じ構築パターン）。構造体リテラルでの直接構築を
/// 許すと検証を迂回でき、`final_k = 0` や `pool_depth` 超過の `final_k` が黙って通る
/// fail-open な経路になりうる（security.md「不安全な設計」）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankConfig {
    pool_depth: usize,
    final_k: usize,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            pool_depth: 200,
            final_k: 20,
        }
    }
}

impl RerankConfig {
    /// 検証付きコンストラクタ。`pool_depth` は `1..=MAX_POOL_DEPTH`、`final_k` は
    /// `1..=pool_depth` であることを構築時に検証し、違反は `Err`（fail-closed）。
    pub fn new(pool_depth: usize, final_k: usize) -> Result<Self, RerankError> {
        if pool_depth == 0 || pool_depth > MAX_POOL_DEPTH {
            return Err(RerankError::InvalidConfig);
        }
        if final_k == 0 || final_k > pool_depth {
            return Err(RerankError::InvalidConfig);
        }
        Ok(Self {
            pool_depth,
            final_k,
        })
    }

    /// 受理する候補件数の上限（検証済み: `1..=MAX_POOL_DEPTH`）。
    pub fn pool_depth(&self) -> usize {
        self.pool_depth
    }

    /// 再順位付け後に返す件数（検証済み: `1..=pool_depth`）。
    pub fn final_k(&self) -> usize {
        self.final_k
    }
}

/// [`rerank_candidates`]・[`Reranker`] 実装が返すエラー。fail-closed（曖昧な場合は
/// 拒否側に倒す。coding-rust.md）。
#[derive(Debug, Clone, PartialEq)]
pub enum RerankError {
    /// [`RerankConfig::new`] の検証違反。
    InvalidConfig,
    /// [`rerank_candidates`] に渡された `final_k` が `0` または `cfg.pool_depth()` 超過。
    InvalidK,
    /// 入力候補（`candidates`）の件数が `cfg.pool_depth()` を超えていた。
    /// `hybrid.rs::HybridError::TooManyCandidates` と同じ理由で、有限性・順序・重複の
    /// 検証（いずれもアロケーションを伴いうる）より先に検証する。
    TooManyCandidates { len: usize, max: usize },
    /// 入力候補に同一 id が複数回出現した（呼び出し元の契約違反）。
    DuplicateId,
    /// 入力候補が融合スコア降順・同点 id 昇順の順位契約に従っていなかった
    /// （[`crate::hybrid::HybridHit`] の順序契約と同じ）。
    UnsortedInput,
    /// 入力候補の `fused_score` が非有限（NaN・Inf）だった。
    NonFiniteScore,
    /// [`Reranker`] 実装（trait object）が、渡した candidates の id 集合に含まれない
    /// id を 1 件でも返した（契約違反）。`hybrid.rs::HybridError::ProviderResultRejected`
    /// と同じ理由で、事後フィルタではなく検索全体を拒否する（部分除外は件数差から
    /// 候補外データの存在情報が漏れる経路になる）。
    ForeignId,
    /// [`Reranker`] 実装が出力内で同一 id を複数回返した。
    DuplicateOutputId,
    /// [`Reranker`] 実装の出力が非有限スコアを含む、またはスコア降順・同点 id 昇順の
    /// 順序契約に違反していた。
    InvalidOutputOrder,
    /// [`Reranker`] 実装の出力件数が要求した `final_k` を超えていた。
    OversizedResult { len: usize, max: usize },
    /// `query_text` のバイト長が [`MAX_QUERY_TEXT_BYTES`] を超える。`reranker.rerank()`
    /// （字句一致系実装は `tokenize()` を呼ぶ）へ渡す前に、[`rerank_candidates`] の
    /// 入口で `query_text.len()`（アロケーション不要）により判定し fail-closed に
    /// 拒否する（`sparse.rs::SparseError::QueryTooLong` と同じ理由）。
    QueryTextTooLong { len: usize, max: usize },
    /// 候補（`RerankCandidate::text`）のバイト長が [`MAX_CANDIDATE_TEXT_BYTES`] を
    /// 超える。`reranker.rerank()` を呼ぶ前に候補の走査（`text.len()`。アロケーション
    /// 不要）で判定し fail-closed に拒否する（`sparse.rs::SparseError::DocTooLong` と
    /// 同じ理由）。
    CandidateTextTooLong { id: u64, len: usize, max: usize },
    /// 候補テキスト（`RerankCandidate::text`）の合計バイト長が
    /// [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`] を超える。`reranker.rerank()`
    /// （`tokenize()` を候補ごとに呼びうる）を呼ぶ前に `checked_add` による累計で
    /// 判定し fail-closed に拒否する（`sparse.rs::SparseError::CorpusTooLarge` と
    /// 同じ理由: 候補ごとの上限だけでは合計コストが無制限に増幅しうる）。
    TotalCandidateTextTooLong { total: usize, max: usize },
    /// [`CrossEncoderReranker`]（Issue #333・SEARCH-7 方式変更）が
    /// [`CrossEncoderBackend`] 実装の呼び出しで失敗した。バックエンド固有の理由は
    /// [`CrossEncoderError`] へ委譲し、ここでは種別のみを保持する。`RerankError` は
    /// `#[non_exhaustive]` を持たない既公開 enum であり、`docs/design/
    /// error-enum-non-exhaustive-policy.md` の方針（既公開エラー enum への
    /// `#[non_exhaustive]` 後付け不採用）どおり、この variant 追加はソース非互換な
    /// **破壊的変更**として扱う（コミットの `BREAKING CHANGE:` フッタ・PR 本文の
    /// Breaking changes 節で明示する）。
    CrossEncoder(CrossEncoderError),
    /// [`MmrReranker`]（Issue #2143）が受けた候補の埋め込みが使えない（欠落・次元不一致・
    /// ノルムが 0 または非有限・類似度が NaN）。`RerankError` は `#[non_exhaustive]` を
    /// 持たない既公開 enum のため、この variant 追加も [`RerankError::CrossEncoder`] と同じく
    /// ソース非互換な**破壊的変更**として扱う。
    InvalidEmbedding { id: u64 },
}

impl fmt::Display for RerankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RerankError::InvalidConfig => write!(f, "invalid rerank config"),
            RerankError::InvalidK => write!(f, "invalid final_k for reranking"),
            RerankError::TooManyCandidates { len, max } => write!(
                f,
                "rerank candidate list too long: {len} candidates (max {max})"
            ),
            RerankError::DuplicateId => write!(f, "duplicate id in rerank candidate input"),
            RerankError::UnsortedInput => {
                write!(f, "rerank candidate input is not sorted by rank contract")
            }
            RerankError::NonFiniteScore => {
                write!(f, "rerank candidate input contains a non-finite score")
            }
            RerankError::ForeignId => {
                write!(f, "reranker returned an id outside the candidate id set")
            }
            RerankError::DuplicateOutputId => {
                write!(f, "reranker returned a duplicate id in its output")
            }
            RerankError::InvalidOutputOrder => write!(
                f,
                "reranker output violates the score-descending id-ascending order contract"
            ),
            RerankError::OversizedResult { len, max } => {
                write!(f, "reranker returned too many hits: {len} hits (max {max})")
            }
            RerankError::QueryTextTooLong { len, max } => {
                write!(f, "rerank query_text too long: {len} bytes (max {max})")
            }
            RerankError::CandidateTextTooLong { id, len, max } => write!(
                f,
                "rerank candidate text too long: id={id} {len} bytes (max {max})"
            ),
            RerankError::TotalCandidateTextTooLong { total, max } => write!(
                f,
                "rerank candidate text total too long: {total} bytes (max {max})"
            ),
            RerankError::CrossEncoder(e) => write!(f, "cross-encoder reranker error: {e}"),
            RerankError::InvalidEmbedding { id } => {
                write!(f, "rerank candidate has an unusable embedding: id={id}")
            }
        }
    }
}

impl std::error::Error for RerankError {}

impl From<CrossEncoderError> for RerankError {
    fn from(e: CrossEncoderError) -> Self {
        RerankError::CrossEncoder(e)
    }
}

/// リランキング方式を差し替え可能にする trait（object-safe・`&self` のみ。
/// [`crate::kernel::SearchProvider`] と同じ制約）。クロスエンコーダ等の本命方式は
/// この trait を実装して差し替える想定（オーナー判断・依存承認後に追加実装）。
///
/// 実装契約: `candidates` に含まれる id 以外を返してはならず、返す件数は `final_k`
/// 以下、スコアは有限、かつ出力は実装が算出した独自スコアの降順・同点 id 昇順で
/// 返さなければならない（[`rerank_candidates`] が [`RerankError::InvalidOutputOrder`]
/// で検証する。呼び出し元側での再ソートは行わない）。
pub trait Reranker: Send + Sync {
    /// `query_text` と `candidates`（高々 `final_k` 呼び出し元が要求する以上の件数を
    /// 含みうる）から再順位付け後の上位 `final_k` 件を返す。
    fn rerank(
        &self,
        query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError>;
}

/// [`rerank_candidates`] の入力検証ヘルパ。融合スコア降順・同点 id 昇順の順位契約
/// （`hybrid.rs::is_sorted_desc_id_asc` と同じ判定ロジック）に従っているかを判定する。
fn is_sorted_desc_id_asc(items: impl Iterator<Item = (f64, u64)>) -> bool {
    let mut prev: Option<(f64, u64)> = None;
    for (score, id) in items {
        if let Some((prev_score, prev_id)) = prev {
            let score_order = score.total_cmp(&prev_score);
            let violates = score_order == std::cmp::Ordering::Greater
                || (score_order == std::cmp::Ordering::Equal && id < prev_id);
            if violates {
                return false;
            }
        }
        prev = Some((score, id));
    }
    true
}

/// `query_text`・`candidates` のバイト長検証ヘルパ（[`MAX_QUERY_TEXT_BYTES`]・
/// [`MAX_CANDIDATE_TEXT_BYTES`]・[`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]）。[`rerank_candidates`]
/// だけでなく [`LexicalOverlapReranker::rerank`] からも直接呼ぶ（公開 trait
/// [`Reranker`] は `rerank_candidates` を経由せず直接呼び出すことも型上できてしまうため、
/// `rerank_candidates` の入口検証だけでは trait 実装を直接呼ぶ経路で未検証の巨大な
/// テキストがそのまま `tokenize()` へ渡りうる。`tokenize()` を呼ぶ実装自身がこの
/// 検証を内部で行うことで、迂回できない構造にする）。件数は呼び出し元
/// （[`rerank_candidates`] の `TooManyCandidates` 検証、または [`MAX_POOL_DEPTH`] 自体）で
/// 別途上限があるため、ここでの走査コスト自体は有界。
fn validate_text_lengths(
    query_text: &str,
    candidates: &[RerankCandidate<'_>],
) -> Result<(), RerankError> {
    if query_text.len() > MAX_QUERY_TEXT_BYTES {
        return Err(RerankError::QueryTextTooLong {
            len: query_text.len(),
            max: MAX_QUERY_TEXT_BYTES,
        });
    }

    if let Some(c) = candidates
        .iter()
        .find(|c| c.text.len() > MAX_CANDIDATE_TEXT_BYTES)
    {
        return Err(RerankError::CandidateTextTooLong {
            id: c.id,
            len: c.text.len(),
            max: MAX_CANDIDATE_TEXT_BYTES,
        });
    }

    // 候補ごとの上限だけでは合計コストが無制限に増幅しうるため（[`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]
    // のコメント参照）、`checked_add` で合計を求めて検証する。`usize` オーバーフローは
    // 未定義動作にせず、オーバーフローした時点で fail-closed に拒否する
    // （coding-rust.md「整数演算は checked_* / saturating_* を使う」）。
    let mut total: usize = 0;
    for c in candidates {
        total = total
            .checked_add(c.text.len())
            .ok_or(RerankError::TotalCandidateTextTooLong {
                total: usize::MAX,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            })?;
        if total > MAX_TOTAL_CANDIDATE_TEXT_BYTES {
            return Err(RerankError::TotalCandidateTextTooLong {
                total,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            });
        }
    }

    Ok(())
}

/// [`rerank_candidates`] の入力検証ヘルパ。`ids` の列に同一 id が複数回出現するかを
/// 判定する（`hybrid.rs::has_duplicate_id` と同じロジック）。
fn has_duplicate_id(ids: impl Iterator<Item = u64>) -> bool {
    let mut seen: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return true;
        }
    }
    false
}

/// リランカーを呼び出し候補を再順位付けする入口（`hybrid_search` の出力を受け取り
/// `core.rs` 相当の呼び出し元へ最終上位を返す統合点。TASK-107）。
///
/// 入力検証（`reranker` 呼び出し前）: 候補件数 ≤ `cfg.pool_depth()`
/// （[`RerankError::TooManyCandidates`]）・`query_text` のバイト長 ≤
/// [`MAX_QUERY_TEXT_BYTES`]（[`RerankError::QueryTextTooLong`]）・候補ごとのテキストの
/// バイト長 ≤ [`MAX_CANDIDATE_TEXT_BYTES`]（[`RerankError::CandidateTextTooLong`]）・
/// 候補テキスト合計のバイト長 ≤ [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]
/// （[`RerankError::TotalCandidateTextTooLong`]）・スコア有限性
/// （[`RerankError::NonFiniteScore`]）・融合スコア降順同点 id 昇順
/// （[`RerankError::UnsortedInput`]）・id 重複なし（[`RerankError::DuplicateId`]）の順に
/// 検証する（`hybrid.rs::rrf_fuse` と同じ検証順序: 長さ→有限性→順序→重複。長さを最初に
/// 検証してから初めて残りの走査を許すのは coding-rust.md「長さフィールドは上限検証して
/// からアロケーションに使う」に従う。`query_text`・候補テキストの長さ検証は
/// [`validate_text_lengths`] を通じて `reranker.rerank()`（[`LexicalOverlapReranker`] 等の
/// 実装が `tokenize()` へ渡す）を呼ぶ前に行い、`sparse.rs::MAX_QUERY_BYTES`・
/// `MAX_DOC_BYTES`・`MAX_CORPUS_BYTES` と同じ理由で無制限なアロケーション・走査コストの
/// 増幅を防ぐ。[`Reranker`] は公開 trait のためこの `rerank_candidates` を経由しない
/// 直接呼び出しも型上ありうる。そのため同じ検証を [`LexicalOverlapReranker::rerank`]
/// 自身も内部で行い、直接呼び出しでも迂回できない構造にしている）。
///
/// `final_k` は `1..=cfg.pool_depth()` を検証し、`0` または超過は [`RerankError::InvalidK`]
/// （`hybrid.rs::hybrid_search` が `k` を `cfg.pool_depth()` 基準で検証するのと同じ理由:
/// `final_k` が `cfg.pool_depth()` を超えると候補プール自体がその件数を満たせないまま
/// 静かに縮退しうるため）。
///
/// 出力検証（`reranker` 呼び出し後）: `reranker.rerank()` は trait object であり
/// 「候補 id 集合の部分集合のみを返す」ことは型で強制されない別個のオブジェクトのため、
/// 出力の id が入力候補の id 集合に含まれない場合は 1 件でも
/// [`RerankError::ForeignId`] で拒否する（`hybrid.rs` の provider 結果検証と同じ
/// fail-closed の方向。事後フィルタは件数差から候補外データの存在情報が漏れる経路に
/// なるため使わない）。あわせて出力内の id 重複（[`RerankError::DuplicateOutputId`]）・
/// スコア非有限または順序契約違反（[`RerankError::InvalidOutputOrder`]）・件数が
/// `final_k` を超過（[`RerankError::OversizedResult`]）をそれぞれ検証する。
pub fn rerank_candidates(
    reranker: &dyn Reranker,
    query_text: &str,
    candidates: &[RerankCandidate<'_>],
    cfg: &RerankConfig,
) -> Result<Vec<RerankedHit>, RerankError> {
    if cfg.final_k() == 0 || cfg.final_k() > cfg.pool_depth() {
        return Err(RerankError::InvalidK);
    }

    // 長さ検証を他のどの検証よりも先に行う（[`RerankError::TooManyCandidates`] の
    // ドキュメント参照）。以降の検証（有限性・ソート順・重複）はいずれも入力を線形
    // 走査するため、長さを検証せずに通すと契約違反の呼び出し元が `cfg.pool_depth()`
    // を大きく超える件数を渡した場合に無制限な走査コストを払うことになる。
    if candidates.len() > cfg.pool_depth() {
        return Err(RerankError::TooManyCandidates {
            len: candidates.len(),
            max: cfg.pool_depth(),
        });
    }

    // `query_text`・候補ごとのテキスト・候補テキスト合計のバイト長を `reranker.rerank()`
    // を呼ぶ（実装が `tokenize()` へ渡し `String`・`BTreeSet` を確保しうる）前に検証する
    // （[`validate_text_lengths`] のドキュメント参照。候補件数は直前の
    // `TooManyCandidates` 検証により `cfg.pool_depth()`（`MAX_POOL_DEPTH` 以下）に
    // 収まっているため、この走査コスト自体は有界）。
    validate_text_lengths(query_text, candidates)?;

    // スコアの有限性を順序検証より先に確認する（`hybrid.rs::rrf_fuse` と同じ理由:
    // `f64::total_cmp` は NaN にもビットパターン依存の全順序を与えるため、有限性を
    // 確認しないまま順序検証だけに頼ると NaN が偶然順序契約を満たすビットパターンで
    // 紛れ込んだ場合に検出できない）。
    if candidates.iter().any(|c| !c.fused_score.is_finite()) {
        return Err(RerankError::NonFiniteScore);
    }

    if !is_sorted_desc_id_asc(candidates.iter().map(|c| (c.fused_score, c.id))) {
        return Err(RerankError::UnsortedInput);
    }

    if has_duplicate_id(candidates.iter().map(|c| c.id)) {
        return Err(RerankError::DuplicateId);
    }

    let visible_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();

    let hits = reranker.rerank(query_text, candidates, cfg.final_k())?;

    // 長さ検証を可視性走査より先に行う（`hybrid.rs::hybrid_search` と同じ理由: 契約
    // 違反の reranker が `final_k` を大きく超える件数を返した場合に、直後の走査で
    // 不要な O(n) コストを払わずに済む）。
    if hits.len() > cfg.final_k() {
        return Err(RerankError::OversizedResult {
            len: hits.len(),
            max: cfg.final_k(),
        });
    }

    if hits.iter().any(|h| !h.score.is_finite()) {
        return Err(RerankError::InvalidOutputOrder);
    }
    if !is_sorted_desc_id_asc(hits.iter().map(|h| (h.score, h.id))) {
        return Err(RerankError::InvalidOutputOrder);
    }
    if has_duplicate_id(hits.iter().map(|h| h.id)) {
        return Err(RerankError::DuplicateOutputId);
    }
    // 事後フィルタ（候補外 id だけを黙って除外する）はしない: 候補外 id が
    // `final_k` の枠を占有していた場合、フィルタ後に正しい候補ヒットを復元できず、
    // 結果件数の差から候補外データの存在情報が外部へ漏れうる（`hybrid.rs` の
    // `ProviderResultRejected` と同じ理由）。1 件でも候補外 id が含まれていたら
    // 検索全体を拒否する（fail-closed）。
    if hits.iter().any(|h| !visible_ids.contains(&h.id)) {
        return Err(RerankError::ForeignId);
    }

    Ok(hits)
}

/// ベースラインの参照実装: 候補の入力順序（融合スコア降順）をそのまま保持して
/// `final_k` 件へ切り詰めるだけの恒等リランカー。TASK-108（Issue #39）における
/// 「リランキングなし構成」との前後比較の基準実装を兼ねる。
///
/// Issue #320 codex-review P1 指摘は [`LexicalOverlapReranker`] の隣接グループ化
/// （fused_score 降順を前提にした走査）に対するものであり、この
/// `IdentityReranker` は候補配列を位置順にそのまま透過するだけで、そのような
/// 隣接前提のグループ化ロジックを持たない。そのため未検証の入力に対して
/// [`RerankError::UnsortedInput`] 等を能動的に検出する意味のある不変条件は
/// 無く、本対応のスコープ外とする（`rerank_candidates` 経由の呼び出しでは
/// 引き続き入口検証で保護される）。
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityReranker;

impl Reranker for IdentityReranker {
    fn rerank(
        &self,
        _query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        Ok(candidates
            .iter()
            .take(final_k)
            .map(|c| RerankedHit {
                id: c.id,
                score: c.fused_score,
            })
            .collect())
    }
}

/// RRF の順位ベース融合（`weight / (k_const + rank)`）と同じ形式で、字句一致順位
/// （`query_text` と `candidate.text` のトークン重なり件数の多い順）を融合スコアの
/// 順位と再結合する参照実装。方式確定までの暫定実装（クロスエンコーダ等の本命方式は
/// オーナー判断・依存承認後に [`Reranker`] の別実装として追加する想定。SEARCH-8 の
/// 効果測定は TASK-108 = Issue #39 の管轄）。
///
/// 字句一致件数そのもの（尺度が候補ごとに不安定なスコア）ではなく順位のみを使う点は
/// `hybrid.rs` が RRF で採った設計判断（元スコアではなく順位ベースで統合する）を
/// 踏襲する。字句トークン化は [`crate::sparse::tokenize`]（TASK-102 実装済みの簡易
/// トークナイザ）を再利用する。
///
/// 既定重み（[`LexicalOverlapReranker::default`]）は `fused_weight:lexical_weight
/// = 3.0:1.0`（fused 優位。Issue #310 対応で変更）。大規模段実測（`crates/engine/
/// tests/rerank_recall.rs`）で `after_hits20`（389）≥ `baseline_hits20`（387）の
/// 非劣化を満たす（389 は Issue #330 対応後の実測値。詳細は
/// `docs/design/rerank-recall-regression.md` 参照）。
#[derive(Debug, Clone, Copy)]
pub struct LexicalOverlapReranker {
    /// RRF 型融合のランク減衰定数（`hybrid::RrfConfig::k_const` と同じ役割）。
    k_const: f64,
    /// 融合スコアの順位（rank_by_fused_score）に対する重み。
    fused_weight: f64,
    /// 字句一致順位（rank_by_lexical_overlap）に対する重み。
    lexical_weight: f64,
}

impl Default for LexicalOverlapReranker {
    fn default() -> Self {
        // 既定重み 3.0:1.0（fused 優位）の採用根拠は本 struct のドキュメンテー
        // ションコメント（Issue #310）を参照。
        Self {
            k_const: 60.0,
            fused_weight: 3.0,
            lexical_weight: 1.0,
        }
    }
}

impl LexicalOverlapReranker {
    /// 検証付きコンストラクタ。`k_const`・重みが有限かつ正であることを検証する
    /// （`hybrid::RrfConfig::new` と同じ検証方針）。
    pub fn new(k_const: f64, fused_weight: f64, lexical_weight: f64) -> Result<Self, RerankError> {
        if !k_const.is_finite() || k_const <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        if !fused_weight.is_finite() || fused_weight <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        if !lexical_weight.is_finite() || lexical_weight <= 0.0 {
            return Err(RerankError::InvalidConfig);
        }
        Ok(Self {
            k_const,
            fused_weight,
            lexical_weight,
        })
    }
}

impl Reranker for LexicalOverlapReranker {
    fn rerank(
        &self,
        query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        // [`Reranker`] は公開 trait のため、[`rerank_candidates`] を経由せずこの
        // `rerank()` を直接呼び出す経路が型上ありうる（`rerank_candidates` の入口検証は
        // 迂回可能）。空文字列・短文の候補を大量に渡すとバイト長検証だけでは
        // 上限を超えないまま件数だけが無制限に増幅しうるため、`tokenize()`
        // （`String`・`BTreeSet` を確保する）や `Vec` 確保（`overlap_ranked`）より
        // 前に、この実装自身でも件数を [`MAX_POOL_DEPTH`] で検証する
        // （`rerank_candidates` の `TooManyCandidates` 検証と同じ契約。直接呼び出し
        // 経路では `cfg.pool_depth()` を持たないため [`MAX_POOL_DEPTH`] 自体を上限に
        // 使う）。
        if candidates.len() > MAX_POOL_DEPTH {
            return Err(RerankError::TooManyCandidates {
                len: candidates.len(),
                max: MAX_POOL_DEPTH,
            });
        }

        // この実装自身でも [`validate_text_lengths`] により長さを検証し、未検証の
        // 巨大な `query_text`・候補テキストがそのまま `tokenize()` へ渡る経路を構造的に
        // 防ぐ（`rerank_candidates` 経由の呼び出しでは二重検証になるが、コストは
        // 候補件数に対して線形かつ [`MAX_POOL_DEPTH`] で有界なため許容する）。
        validate_text_lengths(query_text, candidates)?;

        // 直後の `rank_fused_by_idx` グループ化（Issue #320 codex-review P1 指摘対応）は
        // 「`candidates` が融合スコア降順・同点 id 昇順で隣接グループを成す」ことを
        // 前提に走査する。この前提は本来 `rerank_candidates` の入口検証
        // （[`is_sorted_desc_id_asc`]・[`has_duplicate_id`]・有限性検証）で保証される
        // が、[`Reranker`] は公開 trait のため `rerank_candidates` を経由しないこの
        // `rerank()` の直接呼び出しも型上ありうる。同点集合が非連続に渡された場合に
        // 隣接走査だけでは検出できず、同一同点集合へ異なる順位を割り当てて
        // `hybrid.rs::TieRank::GroupEnd` の順位契約を静かに破りうるため、
        // グループ化より前にこの実装自身でも `rerank_candidates` と同じ検証
        // （有限性→順序→重複の順。理由は [`rerank_candidates`] のドキュメント参照）を
        // 行い、迂回できない構造にする（`validate_text_lengths` と同じ設計判断）。
        if candidates.iter().any(|c| !c.fused_score.is_finite()) {
            return Err(RerankError::NonFiniteScore);
        }
        if !is_sorted_desc_id_asc(candidates.iter().map(|c| (c.fused_score, c.id))) {
            return Err(RerankError::UnsortedInput);
        }
        if has_duplicate_id(candidates.iter().map(|c| c.id)) {
            return Err(RerankError::DuplicateId);
        }

        // クエリ側トークンは候補ごとの重なり計算で使い回すため一度だけ計算する。
        let query_tokens: std::collections::BTreeSet<String> =
            tokenize(query_text).into_iter().collect();

        // `rank_fused`（1-based）は候補配列の位置ではなく、`hybrid.rs::TieRank::
        // GroupEnd`（既定の同点順位規約）と同じグループ末尾順位で求める
        // （Issue #320 codex-review P1 指摘対応）。`candidates` は `fused_score` の
        // 同点集合を持ちうる（`hybrid.rs` の融合結果由来のため）。この隣接走査での
        // グループ化が安全なのは、直前の `is_sorted_desc_id_asc` 検証〔`rerank_candidates`
        // 経由・この `rerank()` への直接呼び出しのいずれでも本関数自身が検証済み〕が
        // 「fused_score 降順」を保証しており、降順制約下では等スコアの要素同士は
        // 必ず隣接する（間に異なるスコアを挟むと降順が破綻する）という不変条件が
        // 成り立つため。位置順位（`idx + 1`）のまま扱うと同点グループ内で融合側と異なる順位規約に
        // なり、`hybrid.rs` 側の GroupEnd 化（Issue #310）の効果がこの層で部分的に
        // 相殺される。グループ内の全メンバーへグループ末尾の 1-based 位置を割り当てる
        // （`hybrid.rs::accumulate_ranked` の `TieRank::GroupEnd` 分岐と同じ走査）。
        let mut rank_fused_by_idx = vec![0usize; candidates.len()];
        let mut group_idx = 0usize;
        while group_idx < candidates.len() {
            let group_score = candidates[group_idx].fused_score;
            let mut group_end = group_idx + 1;
            while group_end < candidates.len()
                && candidates[group_end].fused_score.total_cmp(&group_score)
                    == std::cmp::Ordering::Equal
            {
                group_end += 1;
            }
            for slot in rank_fused_by_idx.iter_mut().take(group_end).skip(group_idx) {
                *slot = group_end;
            }
            group_idx = group_end;
        }

        // (id, 融合スコア順位 rank_fused, 字句重なり件数) を候補順（＝融合スコア
        // 降順。`rerank_candidates` が事前検証済み）に集める。
        // `idx` は候補配列の長さ（`rerank_candidates` により高々 `pool_depth`
        // （`MAX_POOL_DEPTH` 以下）に上限がある）に収まるため `as f64` 変換は
        // 精度を失わない。
        let mut overlap_ranked: Vec<(u64, usize, usize)> = candidates
            .iter()
            .enumerate()
            .map(|(idx, c)| {
                let rank_fused = rank_fused_by_idx[idx];
                let doc_tokens: std::collections::BTreeSet<String> =
                    tokenize(c.text).into_iter().collect();
                let overlap = query_tokens.intersection(&doc_tokens).count();
                (c.id, rank_fused, overlap)
            })
            .collect();

        // 字句重なり件数の多い順（同点は候補順＝融合スコア降順を保つ安定ソート）で
        // 並べ替える。
        //
        // Issue #330（SEARCH-7 大規模段 improvement@20 ゲート未達の是正）対応:
        // rank_lexical は並べ替え後の位置（enumerate の rank_idx + 1、以下「位置順位」）
        // ではなく、`rank_fused_by_idx` と同じグループ末尾順位（GroupEnd）で求める。
        // 位置順位のままだと、overlap（字句重なり件数）が同点の候補グループ内でも
        // ソート前の並び順（＝ソート安定性により融合スコア降順）がそのまま順位差に
        // なってしまい、`fused_weight/(k+rank_fused)` の寄与と二重に効く非対称が
        // 生じていた。`rank_fused` 側は Issue #310 で `hybrid.rs::TieRank::GroupEnd`
        // （同点グループは末尾の共通順位）へ統一済みのため、字句側もグループ内の
        // 全メンバーへ同一順位（グループ末尾の 1-based 位置）を割り当てて同じ規約に
        // 揃える。詳細・実測値は `docs/design/rerank-recall-regression.md`
        // 「Issue #330」節参照。
        overlap_ranked.sort_by_key(|entry| std::cmp::Reverse(entry.2));

        // 添字アクセスを避けるため `chunk_by` で overlap 同点グループを連続部分
        // スライスへ分割し、各グループへグループ末尾の 1-based 順位を一括付与する
        // （`rerank_candidates` は wire/SQL 表層から到達しうる経路であり、
        // coding-rust.md の untrusted 入力経路での添字アクセス禁止に合わせる）。
        let mut rank_lexical_by_idx: Vec<usize> = Vec::with_capacity(overlap_ranked.len());
        let mut consumed = 0usize;
        for group in overlap_ranked.chunk_by(|a, b| a.2 == b.2) {
            consumed += group.len();
            rank_lexical_by_idx.extend(std::iter::repeat_n(consumed, group.len()));
        }

        let mut scores: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
        for ((id, rank_fused, _), rank_lexical) in
            overlap_ranked.iter().zip(rank_lexical_by_idx.iter())
        {
            let contribution_fused = self.fused_weight / (self.k_const + *rank_fused as f64);
            let contribution_lexical = self.lexical_weight / (self.k_const + *rank_lexical as f64);
            let entry = scores.entry(*id).or_insert(0.0);
            *entry += contribution_fused + contribution_lexical;
        }

        if scores.values().any(|score| !score.is_finite()) {
            return Err(RerankError::NonFiniteScore);
        }

        let mut out: Vec<RerankedHit> = scores
            .into_iter()
            .map(|(id, score)| RerankedHit { id, score })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        out.truncate(final_k);
        Ok(out)
    }
}

// ---------- MMR リランカー（Issue #2143・SEARCH-15。設計記録:
// `docs/design/rerank-wiring-policy.md` D2・D3・D4・D8） ----------

/// `--rerank-mmr-lambda` の既定値（関連度と多様性の中立点）。
pub const DEFAULT_MMR_LAMBDA: f64 = 0.5;

/// MMR の計算量予算（ADR D4）。`窓幅 W × min(k_eff, W)` の上限で、貪欲選択の
/// 内積計算回数（約 `n × final_k`）を有界にする。
pub const MMR_PAIR_BUDGET: usize = 1_000_000;

/// MMR の λ を検証する（有限かつ `0.0..=1.0`）。wire-server の起動引数検証（`rerank_opt`）から
/// 呼ばれ、範囲の判定を engine に一本化する。範囲外は [`RerankError::InvalidConfig`]。
pub fn validate_mmr_lambda(lambda: f64) -> Result<f64, RerankError> {
    if !lambda.is_finite() || !(0.0..=1.0).contains(&lambda) {
        return Err(RerankError::InvalidConfig);
    }
    Ok(lambda)
}

/// 窓幅 `window` と実効件数 `k_eff` が MMR の計算量予算内か（`window × min(k_eff, window)`
/// が [`MMR_PAIR_BUDGET`] 以下）。オーバーフローは予算超過として偽を返す。実行器が取得の前に
/// 設定値だけで判定する（表の内容に依存しない。ADR D4）。
pub fn mmr_budget_allows(window: usize, k_eff: usize) -> bool {
    window
        .checked_mul(k_eff.min(window))
        .is_some_and(|pairs| pairs <= MMR_PAIR_BUDGET)
}

/// MMR（Maximal Marginal Relevance）リランカー。関連度と、既に選んだ結果との類似度を λ で
/// 釣り合わせて貪欲に選ぶ。
///
/// - 関連度は入力位置から導く順位ベースの値（`1 - i/(n-1)`、`n = 1` は 1.0）。入力の並び
///   （融合スコア降順・同点 id 昇順）をそのまま反映するため、λ=1 は元の順位を厳密に再現する。
/// - 類似度は埋め込み同士のコサイン類似度（`kernel` の内積・コサイン距離カーネルで計算する）。
/// - 1 件目は入力先頭を選ぶ。2 件目以降は `λ·rel − (1−λ)·max_sim` が最大の候補で、同点は
///   id 昇順（ADR D8）。
/// - 出力スコアは位置由来の狭義単調減少値（[`rerank_candidates`] の出力検証を通すため）。
///   実行器は融合スコア（または元の距離）のまま応答するので、この値は外へ出ない。
///
/// 構築は [`MmrReranker::new`] だけ（フィールド非公開。λ の検証迂回を防ぐ）。
#[derive(Debug, Clone, Copy)]
pub struct MmrReranker {
    lambda: f64,
}

impl Default for MmrReranker {
    fn default() -> Self {
        Self {
            lambda: DEFAULT_MMR_LAMBDA,
        }
    }
}

impl MmrReranker {
    /// 検証付きコンストラクタ（λ は [`validate_mmr_lambda`]）。
    pub fn new(lambda: f64) -> Result<Self, RerankError> {
        Ok(Self {
            lambda: validate_mmr_lambda(lambda)?,
        })
    }

    /// 検証済みの λ。
    pub fn lambda(&self) -> f64 {
        self.lambda
    }
}

/// [`MmrReranker::rerank`] の貪欲選択の作業状態（候補 1 件分）。
struct MmrItem<'a> {
    id: u64,
    rel: f64,
    emb: &'a [f32],
    norm_sq: f32,
    selected: bool,
    max_sim: f64,
}

impl Reranker for MmrReranker {
    fn rerank(
        &self,
        _query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        // trait を直接呼ぶ経路でも迂回できないよう、件数・予算・入力契約をこの実装自身で
        // 検証する（`LexicalOverlapReranker` と同じ設計判断）。確保より前に行う。
        let n = candidates.len();
        if n > MAX_POOL_DEPTH {
            return Err(RerankError::TooManyCandidates {
                len: n,
                max: MAX_POOL_DEPTH,
            });
        }
        if final_k == 0 || final_k > n {
            return Err(RerankError::InvalidK);
        }
        if n.checked_mul(final_k)
            .is_none_or(|pairs| pairs > MMR_PAIR_BUDGET)
        {
            return Err(RerankError::TooManyCandidates {
                len: n,
                max: MMR_PAIR_BUDGET / final_k,
            });
        }
        if candidates.iter().any(|c| !c.fused_score.is_finite()) {
            return Err(RerankError::NonFiniteScore);
        }
        if !is_sorted_desc_id_asc(candidates.iter().map(|c| (c.fused_score, c.id))) {
            return Err(RerankError::UnsortedInput);
        }
        if has_duplicate_id(candidates.iter().map(|c| c.id)) {
            return Err(RerankError::DuplicateId);
        }

        // 埋め込みの検証とノルム² の前計算（候補ごとに 1 回）。
        let dim = candidates
            .first()
            .and_then(|c| c.embedding)
            .map_or(0, <[f32]>::len);
        let denom = n.saturating_sub(1).max(1) as f64;
        let mut items: Vec<MmrItem<'_>> = Vec::with_capacity(n);
        for (i, c) in candidates.iter().enumerate() {
            let invalid = RerankError::InvalidEmbedding { id: c.id };
            let emb = c.embedding.ok_or_else(|| invalid.clone())?;
            if dim == 0 || emb.len() != dim {
                return Err(invalid);
            }
            let norm_sq = crate::kernel::dot(emb, emb);
            if !norm_sq.is_finite() || norm_sq <= 0.0 {
                return Err(invalid);
            }
            let rel = if n == 1 {
                1.0
            } else {
                1.0 - (i as f64) / denom
            };
            items.push(MmrItem {
                id: c.id,
                rel,
                emb,
                norm_sq,
                selected: false,
                max_sim: f64::NEG_INFINITY,
            });
        }

        let lambda = self.lambda;
        let mut out: Vec<RerankedHit> = Vec::with_capacity(final_k);
        for pos in 0..final_k {
            // 1 件目は入力先頭（選択集合が空の段は類似度項が全候補で 0 になるため固定する）。
            let pick = if pos == 0 {
                0
            } else {
                let mut best: Option<(usize, f64, u64)> = None;
                for (j, it) in items.iter().enumerate().filter(|(_, it)| !it.selected) {
                    let score = lambda * it.rel - (1.0 - lambda) * it.max_sim;
                    let better = match best {
                        None => true,
                        Some((_, b_score, b_id)) => match score.total_cmp(&b_score) {
                            std::cmp::Ordering::Greater => true,
                            std::cmp::Ordering::Equal => it.id < b_id,
                            std::cmp::Ordering::Less => false,
                        },
                    };
                    if better {
                        best = Some((j, score, it.id));
                    }
                }
                best.map(|b| b.0).ok_or(RerankError::InvalidK)?
            };
            let (pick_id, pick_emb, pick_norm) = {
                let it = items.get_mut(pick).ok_or(RerankError::InvalidK)?;
                it.selected = true;
                (it.id, it.emb, it.norm_sq)
            };
            out.push(RerankedHit {
                id: pick_id,
                score: (final_k - pos) as f64,
            });
            if pos + 1 < final_k {
                for it in items.iter_mut().filter(|it| !it.selected) {
                    let d = crate::kernel::cosine_distance_from_parts(
                        crate::kernel::dot(it.emb, pick_emb),
                        it.norm_sq,
                        pick_norm,
                    );
                    if d.is_nan() {
                        return Err(RerankError::InvalidEmbedding { id: it.id });
                    }
                    let sim = 1.0 - d;
                    if sim > it.max_sim {
                        it.max_sim = sim;
                    }
                }
            }
        }
        Ok(out)
    }
}

// ---------- クロスエンコーダ型リランカー（Issue #333・SEARCH-7 方式変更） ----------
//
// `LexicalOverlapReranker`（字句一致順位の RRF 型融合）は Issue #330 で構造上限
// （ratio ≈ 0.22）に達しており、SEARCH-7 の相対基準（ratio ≥ 0.6。
// `docs/design/rerank-recall-regression.md`「Issue #330」節）には方式変更なしでは
// 到達できない。ここでは推論バックエンドを [`CrossEncoderBackend`] trait で抽象化し、
// 実 ONNX 推論（`cross_encoder_onnx` サブモジュール。`cross-encoder` feature 限定・
// オーナー承認済み依存 `ort`/`tokenizers`）とテスト用スタブの双方から
// [`CrossEncoderReranker`] を構築できるようにする。これにより受け入れ条件 2
// （スタブ推論での契約テスト: 順序決定性・有界化・エラー契約）は feature 無効の
// 通常ビルドでも常時 `cargo test` の対象にできる。

/// [`CrossEncoderReranker`] が使う推論バックエンドの抽象化（`(query, passage)` ペアの
/// 関連度スコアを返す）。ONNX 実推論（`cross_encoder_onnx::OnnxCrossEncoderBackend`。
/// `cross-encoder` feature 限定）とテスト用の決定的スタブの双方がこの trait を実装する。
/// [`Reranker`] と同じ object-safe・`&self` のみの制約。
pub trait CrossEncoderBackend: Send + Sync {
    /// `query` と `passages` の各要素とのペアに対する関連度スコアを、`passages` と
    /// 同じ長さ・同じ順序で返す。返却長が一致しない場合や非有限値を含む場合の検証は
    /// 呼び出し元（[`CrossEncoderReranker::rerank`]）が行う（バックエンド実装自身に
    /// 検証を要求しない。fail-closed の判定点を一箇所に集約するため）。
    fn score_pairs(&self, query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError>;

    /// バックエンドが実際にトークナイザへ設定した最大シーケンス長。
    /// [`CrossEncoderReranker::new`] がこの値と [`CrossEncoderConfig::max_seq_len`]
    /// の一致を構築時に検証し、不一致を [`CrossEncoderError::BackendSeqLenMismatch`]
    /// で拒否する（Issue #333 codex-review 指摘への対応。設定値が推論経路の実際の
    /// truncation 上限と無関係に構築できてしまうと、DoS／メモリ上限の有界化と
    /// [`CrossEncoderError::TruncationFailed`] の fail-closed 契約を保証できないため）。
    fn max_seq_len(&self) -> usize;
}

/// [`CrossEncoderReranker`] が [`Reranker`] を実装する際に返すエラー
/// （[`RerankError::CrossEncoder`] へ `From` 変換で包む）。fail-closed:
/// バックエンド呼び出しの失敗・出力契約違反はいずれも部分受理せず検索全体を拒否する
/// （`rerank_candidates` の既存 fail-closed 方針と同じ）。
#[derive(Debug, Clone, PartialEq)]
pub enum CrossEncoderError {
    /// [`CrossEncoderConfig::new`] の検証違反。
    InvalidConfig,
    /// バックエンド初期化（`cross_encoder_onnx::OnnxCrossEncoderBackend::from_files`
    /// 等）の失敗。理由文字列にはバックエンド固有のエラー文言のみを含め、候補
    /// テキスト・id 集合は含めない（security.md「エラー・ログ経由で情報を漏らさない」）。
    Backend(String),
    /// [`CrossEncoderBackend::score_pairs`] が渡した `passages` と異なる長さの
    /// スコア列を返した。
    LengthMismatch { expected: usize, got: usize },
    /// [`CrossEncoderBackend::score_pairs`] が非有限（NaN・Inf）スコアを返した。
    NonFiniteScore,
    /// 候補件数が [`CrossEncoderConfig::max_candidates`] を超えた
    /// （`rerank_candidates` の [`RerankError::TooManyCandidates`] と同じ理由で、
    /// [`Reranker`] を経由しない直接呼び出しでもこの実装自身が検証する）。
    TooManyCandidates { len: usize, max: usize },
    /// トークナイザの truncation 設定に失敗した（`cross_encoder_onnx` 限定）。
    /// 3.2 節の契約どおり、この失敗は `from_files` 段階（ロード時）で検出され、
    /// 推論時に無制限シーケンス長へ伸びる経路を構造的に塞ぐ。
    TruncationFailed,
    /// [`CrossEncoderReranker::new`] の構築時検証違反: バックエンドが実際に
    /// トークナイザへ設定した最大シーケンス長（[`CrossEncoderBackend::max_seq_len`]）
    /// と設定値（[`CrossEncoderConfig::max_seq_len`]）が一致しない。不一致のまま
    /// 構築を許すと、推論経路の実際の truncation 上限と無関係な設定値を掲げること
    /// になり、DoS／メモリ上限の有界化を保証できない（Issue #333 codex-review 指摘）。
    BackendSeqLenMismatch { backend: usize, configured: usize },
    /// codex-review 指摘（PR #336 P1）: `query` のバイト長が [`MAX_QUERY_TEXT_BYTES`]
    /// を超える。`CrossEncoderReranker` を介さず [`CrossEncoderBackend::score_pairs`]
    /// を直接呼ぶ利用者に対しても、`rerank_candidates`（`validate_text_lengths`）と
    /// 同じ上限をトークナイズ前に強制し、巨大な原文の走査・中間割り当てを
    /// 有界化する（fail-closed）。
    QueryTextTooLong { len: usize, max: usize },
    /// 同上。`passages` 中の 1 件が [`MAX_CANDIDATE_TEXT_BYTES`] を超える。
    CandidateTextTooLong { len: usize, max: usize },
    /// 同上。`passages` 全体の合計バイト長が [`MAX_TOTAL_CANDIDATE_TEXT_BYTES`]
    /// を超える（checked 加算。オーバーフローも同じエラーへ丸めて拒否する）。
    TotalCandidateTextTooLong { total: usize, max: usize },
    /// 推論の期限（[`CrossEncoderReranker::with_timeout`]）を超過した（Issue #2144）。
    /// 検索実行器はこれを検索拒否ではなく「リランキング前の順位への縮退」として扱う
    /// （`sql::rerank_stage::degrade_reason` が `RerankDegradeReason::Timeout` へ写す）。
    Timeout,
    /// モデル／トークナイザファイルの事前検証違反（Issue #2144。`read_bounded_model_file`）。
    /// パスやファイル内容は含めず、閉じた固定語の `reason` だけを持つ（情報漏えい防止）。
    ModelFile {
        file: CrossEncoderFile,
        reason: &'static str,
    },
}

/// [`CrossEncoderError::ModelFile`] が指す対象ファイルの種別（CLI フラグ名への対応づけ用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossEncoderFile {
    /// ONNX モデル本体。
    Model,
    /// `tokenizer.json`。
    Tokenizer,
}

impl CrossEncoderFile {
    /// ログ・エラー文に出す固定語。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Tokenizer => "tokenizer",
        }
    }
}

impl fmt::Display for CrossEncoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrossEncoderError::InvalidConfig => write!(f, "invalid cross-encoder config"),
            CrossEncoderError::Backend(msg) => write!(f, "cross-encoder backend error: {msg}"),
            CrossEncoderError::LengthMismatch { expected, got } => write!(
                f,
                "cross-encoder backend returned {got} scores (expected {expected})"
            ),
            CrossEncoderError::NonFiniteScore => {
                write!(f, "cross-encoder backend returned a non-finite score")
            }
            CrossEncoderError::TooManyCandidates { len, max } => write!(
                f,
                "cross-encoder candidate list too long: {len} candidates (max {max})"
            ),
            CrossEncoderError::TruncationFailed => {
                write!(f, "cross-encoder tokenizer truncation setup failed")
            }
            CrossEncoderError::BackendSeqLenMismatch {
                backend,
                configured,
            } => write!(
                f,
                "cross-encoder backend max_seq_len ({backend}) does not match configured max_seq_len ({configured})"
            ),
            CrossEncoderError::Timeout => write!(f, "cross-encoder inference timed out"),
            CrossEncoderError::ModelFile { file, reason } => write!(
                f,
                "cross-encoder {} file rejected: {reason}",
                file.as_str()
            ),
            CrossEncoderError::QueryTextTooLong { len, max } => write!(
                f,
                "cross-encoder query text too long: {len} bytes (max {max})"
            ),
            CrossEncoderError::CandidateTextTooLong { len, max } => write!(
                f,
                "cross-encoder candidate text too long: {len} bytes (max {max})"
            ),
            CrossEncoderError::TotalCandidateTextTooLong { total, max } => write!(
                f,
                "cross-encoder total candidate text too long: {total} bytes (max {max})"
            ),
        }
    }
}

impl std::error::Error for CrossEncoderError {}

/// [`CrossEncoderReranker`] の設定（[`RerankConfig`] と同じ「非 `pub` フィールド +
/// 検証付き `new`」構築パターン）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossEncoderConfig {
    /// 1 回の `score_pairs` 呼び出しへ渡す passage 件数の上限（DoS・メモリ上限の
    /// 有界化。バックエンド実装がこれを超えるバッチを一括でテンソル化しない
    /// ことを保証するのは呼び出し元 = [`CrossEncoderReranker::rerank`] の責務）。
    batch_size: usize,
    /// 受理する候補件数の上限（[`MAX_POOL_DEPTH`] 以下）。
    max_candidates: usize,
    /// バックエンドへ伝える最大シーケンス長（情報提供用の記録値。truncation の
    /// 実施自体はバックエンド実装の責務）。
    max_seq_len: usize,
}

/// バッチサイズの実装上の上限（`wgpu` バッチ経路等と異なりモデル推論の
/// メモリ使用量に直結するため、小さめの値で頭打ちする）。
const MAX_CROSS_ENCODER_BATCH_SIZE: usize = 256;

/// `max_seq_len` の実装上の上限（トークン長。一般的な事前学習済みクロスエンコーダの
/// 実用域を大きく超える値を拒否し、テンソル確保コストを有界にする）。
const MAX_CROSS_ENCODER_SEQ_LEN: usize = 8192;

impl CrossEncoderConfig {
    /// 検証付きコンストラクタ。`batch_size`・`max_candidates`・`max_seq_len` が
    /// いずれも正かつ実装上限以下であることを検証する。
    pub fn new(
        batch_size: usize,
        max_candidates: usize,
        max_seq_len: usize,
    ) -> Result<Self, CrossEncoderError> {
        if batch_size == 0 || batch_size > MAX_CROSS_ENCODER_BATCH_SIZE {
            return Err(CrossEncoderError::InvalidConfig);
        }
        if max_candidates == 0 || max_candidates > MAX_POOL_DEPTH {
            return Err(CrossEncoderError::InvalidConfig);
        }
        if max_seq_len == 0 || max_seq_len > MAX_CROSS_ENCODER_SEQ_LEN {
            return Err(CrossEncoderError::InvalidConfig);
        }
        Ok(Self {
            batch_size,
            max_candidates,
            max_seq_len,
        })
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn max_candidates(&self) -> usize {
        self.max_candidates
    }

    /// [`CrossEncoderReranker::new`] が [`CrossEncoderBackend::max_seq_len`] との
    /// 一致検証に使う（同メソッドのドキュメント参照）。
    pub fn max_seq_len(&self) -> usize {
        self.max_seq_len
    }
}

/// クロスエンコーダ型リランカー本体（[`Reranker`] 実装）。[`CrossEncoderBackend`]
/// （ONNX 実推論またはテスト用スタブ）へ委譲し、候補を `batch_size` 件ずつバッチ化
/// して推論、スコア降順・同点 id 昇順の決定的順序で上位 `final_k` 件を返す。
///
/// fail-closed 契約: バックエンド呼び出しの失敗・返却長不一致・非有限スコアは
/// いずれも部分受理せず [`RerankError::CrossEncoder`] で検索全体を拒否する
/// （`rerank_candidates` の既存方針と同じ。事後フィルタは件数差から情報が漏れる
/// 経路になるため使わない）。
pub struct CrossEncoderReranker<B: CrossEncoderBackend> {
    backend: B,
    cfg: CrossEncoderConfig,
    /// 推論全体の期限（Issue #2144）。`None` は無期限（従来挙動）。
    timeout: Option<std::time::Duration>,
}

impl<B: CrossEncoderBackend> CrossEncoderReranker<B> {
    /// 検証付きコンストラクタ。`backend.max_seq_len()`（バックエンドが実際に
    /// トークナイザへ設定した最大シーケンス長）と `cfg.max_seq_len()`（設定値）の
    /// 一致を検証し、不一致は [`CrossEncoderError::BackendSeqLenMismatch`] で拒否する
    /// （Issue #333 codex-review 指摘: この検証なしでは設定値が推論経路の実際の
    /// truncation 上限と無関係になり、DoS／メモリ上限の有界化を保証できない）。
    pub fn new(backend: B, cfg: CrossEncoderConfig) -> Result<Self, CrossEncoderError> {
        if backend.max_seq_len() != cfg.max_seq_len() {
            return Err(CrossEncoderError::BackendSeqLenMismatch {
                backend: backend.max_seq_len(),
                configured: cfg.max_seq_len(),
            });
        }
        Ok(Self {
            backend,
            cfg,
            timeout: None,
        })
    }

    /// 推論全体の期限を設定する（Issue #2144。`--rerank-timeout-ms` の注入点）。
    /// `rerank` の入口から計測し、各バッチ完了後に超過を検査して
    /// [`CrossEncoderError::Timeout`] を返す（スレッドは作らない。超過幅は最大 1 バッチ分）。
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl<B: CrossEncoderBackend> Reranker for CrossEncoderReranker<B> {
    fn rerank(
        &self,
        query_text: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        // [`Reranker`] は公開 trait のため `rerank_candidates` を経由しない直接
        // 呼び出しが型上ありうる（`LexicalOverlapReranker::rerank` と同じ理由）。
        // この実装自身でも件数・テキスト長を検証してから推論バックエンドへ渡す。
        if candidates.len() > self.cfg.max_candidates() {
            return Err(RerankError::CrossEncoder(
                CrossEncoderError::TooManyCandidates {
                    len: candidates.len(),
                    max: self.cfg.max_candidates(),
                },
            ));
        }
        validate_text_lengths(query_text, candidates)?;

        if candidates.iter().any(|c| !c.fused_score.is_finite()) {
            return Err(RerankError::NonFiniteScore);
        }
        if !is_sorted_desc_id_asc(candidates.iter().map(|c| (c.fused_score, c.id))) {
            return Err(RerankError::UnsortedInput);
        }
        if has_duplicate_id(candidates.iter().map(|c| c.id)) {
            return Err(RerankError::DuplicateId);
        }

        // 候補を `batch_size` 件ずつバックエンドへ渡す。バッチ間で id → score の
        // 対応が崩れないよう `scores` は候補順（＝ `candidates` の順）にそのまま
        // 積み上げる（`Vec` の位置対応。バックエンドは長さを保つ契約
        // [`CrossEncoderError::LengthMismatch`] で検証済み）。
        let started = std::time::Instant::now();
        let mut scores: Vec<f64> = Vec::with_capacity(candidates.len());
        for chunk in candidates.chunks(self.cfg.batch_size()) {
            let passages: Vec<&str> = chunk.iter().map(|c| c.text).collect();
            let chunk_scores = self
                .backend
                .score_pairs(query_text, &passages)
                .map_err(RerankError::CrossEncoder)?;
            if chunk_scores.len() != passages.len() {
                return Err(RerankError::CrossEncoder(
                    CrossEncoderError::LengthMismatch {
                        expected: passages.len(),
                        got: chunk_scores.len(),
                    },
                ));
            }
            if chunk_scores.iter().any(|s| !s.is_finite()) {
                return Err(RerankError::CrossEncoder(CrossEncoderError::NonFiniteScore));
            }
            scores.extend(chunk_scores);
            // 期限検査はバッチ境界で行う。超過は検索拒否ではなく縮退として扱われる
            // （`sql::rerank_stage::degrade_reason`）。
            if let Some(limit) = self.timeout {
                if started.elapsed() >= limit {
                    return Err(RerankError::CrossEncoder(CrossEncoderError::Timeout));
                }
            }
        }

        // スコア降順・同点 id 昇順の決定的順序（`rerank_candidates` の出力契約と
        // 同一。`total_cmp` は浮動小数の全順序を与える）で安定させるため、
        // まず (id, score) のペアへ変換してから比較ソートする。
        let mut out: Vec<RerankedHit> = candidates
            .iter()
            .zip(scores.iter())
            .map(|(c, &score)| RerankedHit { id: c.id, score })
            .collect();
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        out.truncate(final_k);
        Ok(out)
    }
}

// `OnnxCrossEncoderBackend`（実 ONNX 推論。`ort`/`tokenizers` 依存。
// オーナー承認済み 2026-08-30・Issue #333 再 open コメント）を
// `cross-encoder` feature 限定のサブモジュールとして接続する。上記
// [`CrossEncoderBackend`] trait・[`CrossEncoderReranker`] 自体は変更しない
// （feature 無効時は決定的スタブ経由の契約テスト＝下記 `tests` モジュールのみが
// 対象。feature 有効時のみ `cross_encoder_onnx::OnnxCrossEncoderBackend` が
// 追加で利用可能になる）。
#[cfg(feature = "cross-encoder")]
pub mod cross_encoder_onnx;

// ---------------------------------------------------------------------------
// 検索実行器への結線用の設定・統計型（Issue #2142。設計記録:
// `docs/design/rerank-wiring-policy.md`。ビヘイビア: SEARCH-15 をポインタで参照）
// ---------------------------------------------------------------------------

/// `--rerank-window` の既定値。`RerankConfig::default()`・ハイブリッド既定 pool と揃える。
pub const DEFAULT_RERANK_WINDOW: usize = 200;

/// リランク窓の幅 `W` を検証する（`1..=MAX_POOL_DEPTH`）。wire-server の起動引数検証
/// （`rerank_opt`）から呼ばれ、範囲の判定を engine 側に一本化する（`MAX_POOL_DEPTH` は
/// 非公開のまま。`validate_statement_timeout_ms` と同形）。範囲外は
/// [`RerankError::InvalidConfig`]（fail-closed）。
pub fn validate_rerank_window(window: usize) -> Result<usize, RerankError> {
    if window == 0 || window > MAX_POOL_DEPTH {
        return Err(RerankError::InvalidConfig);
    }
    Ok(window)
}

/// `--rerank-timeout-ms` の既定値（ミリ秒。Issue #2144。設計記録 ADR D5）。
pub const DEFAULT_RERANK_TIMEOUT_MS: u64 = 1000;

/// `--rerank-timeout-ms` の上限（ミリ秒）。
const MAX_RERANK_TIMEOUT_MS: u64 = 60_000;

/// 推論期限（ミリ秒）を検証して [`std::time::Duration`] にする（`1..=60000`）。
/// wire-server の起動引数検証（`rerank_opt`）から呼ばれ、範囲判定を engine に一本化する
/// （`validate_rerank_window` と同形）。範囲外は [`RerankError::InvalidConfig`]。
pub fn validate_rerank_timeout_ms(ms: u64) -> Result<std::time::Duration, RerankError> {
    if ms == 0 || ms > MAX_RERANK_TIMEOUT_MS {
        return Err(RerankError::InvalidConfig);
    }
    Ok(std::time::Duration::from_millis(ms))
}

/// クロスエンコーダ推論の 1 バッチ件数（固定。CLI には出さない）。
pub const DEFAULT_CROSS_ENCODER_BATCH_SIZE: usize = 32;

/// クロスエンコーダの最大シーケンス長（固定。BERT 系の標準。CLI には出さない）。
pub const DEFAULT_CROSS_ENCODER_MAX_SEQ_LEN: usize = 512;

/// ONNX モデルファイルの読み込み上限（バイト。protobuf 単一ファイルの 2 GiB 上限に合わせる。
/// 外部データ形式の ONNX は非対応）。自動運転下の暫定値で、人間による確認が必要
/// （`docs/design/rerank-wiring-policy.md` 参照）。
pub const MAX_CROSS_ENCODER_MODEL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// `tokenizer.json` の読み込み上限（バイト）。上記と同様に暫定値。
pub const MAX_CROSS_ENCODER_TOKENIZER_BYTES: u64 = 64 * 1024 * 1024;

/// 本ビルドが `cross-encoder` feature 付きかどうか。wire-server は feature を持たないため、
/// この関数で実行時に分岐する（feature 無効のビルドでは `cross_encoder` 指定を拒否する）。
pub fn cross_encoder_available() -> bool {
    cfg!(feature = "cross-encoder")
}

/// モデル／トークナイザファイルを上限付きで読む（Issue #2144。ADR D2 の持ち越し事項）。
///
/// 検証順序: (1) パスに `..` を含めば拒否（パストラバーサル対策）、(2) 通常ファイルでなければ
/// open 前に拒否（FIFO でのブロック回避）、(3) open 後にハンドルでも通常ファイルとサイズ上限を
/// 確認（パス差し替え対策）、(4) `take(max + 1)` で読み上限超過を拒否（TOCTOU・無限ストリーム対策）。
/// エラーはパスを含まない閉じた固定語のみ。feature の有無に関わらずコンパイルされ、
/// `onnx_cross_encoder_method` から呼ばれる。
pub fn read_bounded_model_file(
    path: &std::path::Path,
    file: CrossEncoderFile,
    max: u64,
) -> Result<Vec<u8>, CrossEncoderError> {
    use std::io::Read;
    let fail = |reason: &'static str| CrossEncoderError::ModelFile { file, reason };
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(fail("parent_dir_component"));
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(fail("not_regular_file")),
        Err(_) => return Err(fail("unreadable")),
    }
    let f = std::fs::File::open(path).map_err(|_| fail("unreadable"))?;
    let meta = f.metadata().map_err(|_| fail("unreadable"))?;
    if !meta.is_file() {
        return Err(fail("not_regular_file"));
    }
    if meta.len() > max {
        return Err(fail("too_large"));
    }
    let mut buf = Vec::new();
    f.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|_| fail("unreadable"))?;
    if buf.len() as u64 > max {
        return Err(fail("too_large"));
    }
    Ok(buf)
}

/// ONNX クロスエンコーダ方式を構築する（`--reranker cross_encoder` の実体。Issue #2144）。
/// 2 ファイルを上限付きで読み、バックエンド構築・設定検証・期限設定まで行う。失敗は
/// すべて起動エラーとして wire-server が扱う。`cross-encoder` feature 無効のビルドでは
/// ファイルに触れず固定文言の `Err` を返す（多層防御）。
#[cfg(feature = "cross-encoder")]
pub fn onnx_cross_encoder_method(
    model_path: &std::path::Path,
    tokenizer_path: &std::path::Path,
    window: usize,
    timeout: std::time::Duration,
) -> Result<RerankMethod, CrossEncoderError> {
    let model = read_bounded_model_file(
        model_path,
        CrossEncoderFile::Model,
        MAX_CROSS_ENCODER_MODEL_BYTES,
    )?;
    let tokenizer = read_bounded_model_file(
        tokenizer_path,
        CrossEncoderFile::Tokenizer,
        MAX_CROSS_ENCODER_TOKENIZER_BYTES,
    )?;
    let backend = cross_encoder_onnx::OnnxCrossEncoderBackend::from_bytes(
        &model,
        &tokenizer,
        DEFAULT_CROSS_ENCODER_MAX_SEQ_LEN,
    )?;
    drop(model);
    drop(tokenizer);
    let cfg = CrossEncoderConfig::new(
        DEFAULT_CROSS_ENCODER_BATCH_SIZE,
        window,
        DEFAULT_CROSS_ENCODER_MAX_SEQ_LEN,
    )?;
    let reranker = CrossEncoderReranker::new(backend, cfg)?.with_timeout(timeout);
    Ok(RerankMethod::cross_encoder(reranker))
}

/// `cross-encoder` feature 無効ビルド用。ファイルに触れずに拒否する。
#[cfg(not(feature = "cross-encoder"))]
pub fn onnx_cross_encoder_method(
    _model_path: &std::path::Path,
    _tokenizer_path: &std::path::Path,
    _window: usize,
    _timeout: std::time::Duration,
) -> Result<RerankMethod, CrossEncoderError> {
    Err(CrossEncoderError::Backend(
        "cross-encoder feature is disabled in this build".to_string(),
    ))
}

/// 検索実行器に結線するリランク方式。`EngineCore::with_rerank` で起動時に 1 回だけ注入
/// する（セッション・SQL・NoSQL からは変更できない。ADR D1）。`Identity`・`Custom` は
/// Rust API 専用で CLI からは到達できない。
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum RerankMethod {
    /// リランクしない（既定。従来経路をそのまま通る）。
    #[default]
    Off,
    /// 恒等リランカー（テスト用。順位を変えない）。
    Identity,
    /// 字句一致リランカー（`--reranker lexical`）。
    Lexical(LexicalOverlapReranker),
    /// 任意のリランカー（契約違反の注入テスト用。出力は実行器が検証する）。
    Custom(std::sync::Arc<dyn Reranker>),
    /// クロスエンコーダ（`--reranker cross_encoder`。Issue #2144）。
    CrossEncoder(std::sync::Arc<dyn Reranker>),
    /// MMR リランカー（`--reranker mmr`。Issue #2143）。埋め込みを読む。
    Mmr(MmrReranker),
    /// 埋め込みを読む任意のリランカー（密経路の縮退・再検索の注入テスト用。Rust API 専用）。
    CustomEmbedding(std::sync::Arc<dyn Reranker>),
}

/// リランカーが要求する入力の種別（実行器が窓内の行から何を取り出すかを決める）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RerankInput {
    /// 行のテキスト列（ハイブリッドのみ適用）。
    Text,
    /// 行の埋め込み（ハイブリッド・密のみの両方に適用）。
    Embedding,
}

impl fmt::Debug for RerankMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind_name())
    }
}

impl RerankMethod {
    /// ログ・統計に出す閉じた語彙の種別名。
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Identity => "identity",
            Self::Lexical(_) => "lexical",
            Self::CrossEncoder(_) => "cross_encoder",
            Self::Custom(_) | Self::CustomEmbedding(_) => "custom",
            Self::Mmr(_) => "mmr",
        }
    }

    /// 方式が要求する入力（ADR D3 の適用形状表）。`Off` は使われない。
    pub(crate) fn required_input(&self) -> RerankInput {
        match self {
            Self::Mmr(_) | Self::CustomEmbedding(_) => RerankInput::Embedding,
            _ => RerankInput::Text,
        }
    }

    /// クロスエンコーダ方式を構築する（スタブバックエンドでも ONNX でも共通。Issue #2144）。
    pub fn cross_encoder<B: CrossEncoderBackend + 'static>(r: CrossEncoderReranker<B>) -> Self {
        Self::CrossEncoder(std::sync::Arc::new(r))
    }

    /// 実行器が呼ぶリランカー。`Off` は `None`。
    pub(crate) fn reranker(&self) -> Option<&dyn Reranker> {
        match self {
            Self::Off => None,
            Self::Identity => Some(&IdentityReranker),
            Self::Lexical(r) => Some(r),
            Self::Custom(r) | Self::CustomEmbedding(r) | Self::CrossEncoder(r) => Some(r.as_ref()),
            Self::Mmr(r) => Some(r),
        }
    }
}

/// リランク設定（方式と窓幅 `W`）。検証付きコンストラクタ [`RerankSettings::new`] のみで
/// 構築する（フィールド非公開。検証迂回による fail-open を防ぐ）。
#[derive(Debug, Clone)]
pub struct RerankSettings {
    method: RerankMethod,
    window: usize,
}

impl Default for RerankSettings {
    fn default() -> Self {
        Self::off()
    }
}

impl RerankSettings {
    /// リランク無効の設定（既定）。
    pub fn off() -> Self {
        Self {
            method: RerankMethod::Off,
            window: DEFAULT_RERANK_WINDOW,
        }
    }

    /// 検証付きコンストラクタ。`window` は `1..=10_000`（[`validate_rerank_window`]）。
    /// `Off` のとき窓幅は使われないが、値は検証する。
    pub fn new(method: RerankMethod, window: usize) -> Result<Self, RerankError> {
        let window = validate_rerank_window(window)?;
        Ok(Self { method, window })
    }

    /// 方式。
    pub fn method(&self) -> &RerankMethod {
        &self.method
    }

    /// 窓幅 `W`。
    pub fn window(&self) -> usize {
        self.window
    }

    /// リランクが有効（`Off` 以外）か。
    pub fn is_enabled(&self) -> bool {
        !matches!(self.method, RerankMethod::Off)
    }
}

/// 縮退（リランキング前の順位で応答すること）の理由。ログ・統計で使う閉じた語彙
/// （ADR D7）。`Timeout` は #2144 で、`MmrBudget` は後続 Issue（#2143）で到達する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RerankDegradeReason {
    /// 推論の期限超過。
    Timeout,
    /// バックエンド（クロスエンコーダ等）の失敗。
    BackendError,
    /// リランカーの入出力契約違反。
    ContractViolation,
    /// テキスト長などの入力上限超過。
    InputLimit,
    /// MMR の計算量予算超過。
    MmrBudget,
}

impl RerankDegradeReason {
    /// ログ・統計に出す固定の語彙。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::BackendError => "backend_error",
            Self::ContractViolation => "contract_violation",
            Self::InputLimit => "input_limit",
            Self::MmrBudget => "mmr_budget",
        }
    }
}

/// リランクの観測用統計（`EngineCore::rerank_stats` が返す累計のスナップショット）。
/// 件数だけを持ち、id・本文・テナント情報は含まない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RerankStats {
    /// リランクを適用して応答した文の数。
    pub applied: u64,
    /// 文の形（密のみ等）により適用外として素通しした文の数。
    pub not_applicable: u64,
    /// 縮退: 期限超過。
    pub degraded_timeout: u64,
    /// 縮退: バックエンド失敗。
    pub degraded_backend_error: u64,
    /// 縮退: 契約違反。
    pub degraded_contract_violation: u64,
    /// 縮退: 入力上限超過。
    pub degraded_input_limit: u64,
    /// 縮退: MMR 予算超過。
    pub degraded_mmr_budget: u64,
}

impl RerankStats {
    /// 全理由の縮退件数の合計。
    pub fn degraded_total(&self) -> u64 {
        self.degraded_timeout
            .saturating_add(self.degraded_backend_error)
            .saturating_add(self.degraded_contract_violation)
            .saturating_add(self.degraded_input_limit)
            .saturating_add(self.degraded_mmr_budget)
    }
}

/// [`RerankStats`] の実体（`EngineCore` が保持し、実行器が更新する crate 内部型）。
#[derive(Debug, Default)]
pub(crate) struct RerankCounters {
    applied: std::sync::atomic::AtomicU64,
    not_applicable: std::sync::atomic::AtomicU64,
    timeout: std::sync::atomic::AtomicU64,
    backend_error: std::sync::atomic::AtomicU64,
    contract_violation: std::sync::atomic::AtomicU64,
    input_limit: std::sync::atomic::AtomicU64,
    mmr_budget: std::sync::atomic::AtomicU64,
}

impl RerankCounters {
    fn bump(c: &std::sync::atomic::AtomicU64) {
        c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn record_applied(&self) {
        Self::bump(&self.applied);
    }

    pub(crate) fn record_not_applicable(&self) {
        Self::bump(&self.not_applicable);
    }

    pub(crate) fn record_degraded(&self, reason: RerankDegradeReason) {
        match reason {
            RerankDegradeReason::Timeout => Self::bump(&self.timeout),
            RerankDegradeReason::BackendError => Self::bump(&self.backend_error),
            RerankDegradeReason::ContractViolation => Self::bump(&self.contract_violation),
            RerankDegradeReason::InputLimit => Self::bump(&self.input_limit),
            RerankDegradeReason::MmrBudget => Self::bump(&self.mmr_budget),
        }
    }

    pub(crate) fn snapshot(&self) -> RerankStats {
        use std::sync::atomic::Ordering::Relaxed;
        RerankStats {
            applied: self.applied.load(Relaxed),
            not_applicable: self.not_applicable.load(Relaxed),
            degraded_timeout: self.timeout.load(Relaxed),
            degraded_backend_error: self.backend_error.load(Relaxed),
            degraded_contract_violation: self.contract_violation.load(Relaxed),
            degraded_input_limit: self.input_limit.load(Relaxed),
            degraded_mmr_budget: self.mmr_budget.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod wiring_tests {
    use super::*;

    #[test]
    fn window_bounds() {
        assert!(validate_rerank_window(0).is_err());
        assert_eq!(validate_rerank_window(1), Ok(1));
        assert_eq!(validate_rerank_window(10_000), Ok(10_000));
        assert!(validate_rerank_window(10_001).is_err());
    }

    #[test]
    fn settings_default_is_off_and_validates_window() {
        let s = RerankSettings::default();
        assert!(!s.is_enabled());
        assert_eq!(s.window(), DEFAULT_RERANK_WINDOW);
        assert!(RerankSettings::new(RerankMethod::Identity, 0).is_err());
        let s = RerankSettings::new(RerankMethod::Identity, 50).unwrap();
        assert!(s.is_enabled());
        assert_eq!(s.window(), 50);
        assert_eq!(s.method().kind_name(), "identity");
    }

    #[test]
    fn degrade_reason_vocabulary_is_closed() {
        let all = [
            RerankDegradeReason::Timeout,
            RerankDegradeReason::BackendError,
            RerankDegradeReason::ContractViolation,
            RerankDegradeReason::InputLimit,
            RerankDegradeReason::MmrBudget,
        ];
        let names: Vec<&str> = all.iter().map(RerankDegradeReason::as_str).collect();
        assert_eq!(
            names,
            [
                "timeout",
                "backend_error",
                "contract_violation",
                "input_limit",
                "mmr_budget"
            ]
        );
    }

    #[test]
    fn counters_snapshot_tracks_each_reason() {
        let c = RerankCounters::default();
        c.record_applied();
        c.record_not_applicable();
        c.record_degraded(RerankDegradeReason::InputLimit);
        c.record_degraded(RerankDegradeReason::ContractViolation);
        let s = c.snapshot();
        assert_eq!(s.applied, 1);
        assert_eq!(s.not_applicable, 1);
        assert_eq!(s.degraded_input_limit, 1);
        assert_eq!(s.degraded_contract_violation, 1);
        assert_eq!(s.degraded_total(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(id: u64, fused_score: f64, text: &'a str) -> RerankCandidate<'a> {
        RerankCandidate {
            id,
            fused_score,
            text,
            embedding: None,
        }
    }

    #[test]
    fn rerank_config_default_matches_expected_values() {
        let cfg = RerankConfig::default();
        assert_eq!(cfg.pool_depth(), 200);
        assert_eq!(cfg.final_k(), 20);
    }

    #[test]
    fn rerank_config_rejects_invalid_values() {
        assert_eq!(
            RerankConfig::new(0, 1).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(MAX_POOL_DEPTH + 1, 1).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(10, 0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            RerankConfig::new(10, 11).unwrap_err(),
            RerankError::InvalidConfig
        );
    }

    #[test]
    fn lexical_overlap_reranker_rejects_invalid_values() {
        assert_eq!(
            LexicalOverlapReranker::new(0.0, 1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(f64::NAN, 1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(60.0, -1.0, 1.0).unwrap_err(),
            RerankError::InvalidConfig
        );
        assert_eq!(
            LexicalOverlapReranker::new(60.0, 1.0, f64::INFINITY).unwrap_err(),
            RerankError::InvalidConfig
        );
    }

    #[test]
    fn identity_reranker_preserves_order_and_truncates() {
        let cfg = RerankConfig::new(10, 2).unwrap();
        let candidates = [cand(1, 3.0, "a"), cand(2, 2.0, "b"), cand(3, 1.0, "c")];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(
            hits,
            vec![
                RerankedHit { id: 1, score: 3.0 },
                RerankedHit { id: 2, score: 2.0 },
            ]
        );
    }

    #[test]
    fn lexical_overlap_reranker_surfaces_matching_document_to_top() {
        // 融合スコアでは 1 位の候補（id=1）を、クエリと字句一致する文書（id=2）が
        // 最終的に上回ることを確認する（SEARCH-7: 再順位付けの動作）。既定重み
        // （[`LexicalOverlapReranker::default`]。Issue #310 対応で fused 優位
        // 3.0:1.0 へ変更済み）でも融合順位 1 位の優位が大きく拮抗しうるため、
        // 字句一致信号を優勢にする重み構成（`lexical_weight` を大きく取る）で
        // 検証する。
        let cfg = RerankConfig::new(10, 3).unwrap();
        let candidates = [
            cand(1, 3.0, "unrelated content about nothing"),
            cand(2, 2.0, "vector search kernel reranking"),
            cand(3, 1.0, "also unrelated filler text"),
        ];
        let reranker = LexicalOverlapReranker::new(60.0, 1.0, 5.0).unwrap();
        let hits =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        assert_eq!(hits[0].id, 2, "lexically matching doc must surface to top");
    }

    #[test]
    fn lexical_overlap_reranker_is_deterministic() {
        let cfg = RerankConfig::new(10, 3).unwrap();
        let candidates = [
            cand(1, 3.0, "vector search kernel"),
            cand(2, 2.0, "vector search kernel"),
            cand(3, 1.0, "unrelated"),
        ];
        let reranker = LexicalOverlapReranker::default();
        let hits_a =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        let hits_b =
            rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
        assert_eq!(hits_a, hits_b);
    }

    #[test]
    fn lexical_overlap_reranker_tie_breaks_by_id_ascending() {
        // 真の同点スコアを作るため、rank_fused と rank_lexical が入れ替わる構成にする。
        // 既定（[`LexicalOverlapReranker::default`]。Issue #310 対応で fused 優位
        // 3.0:1.0）ではこの入れ替えだけで真の同点は作れないため、本テストの関心
        // （タイブレーク分岐そのもの）に絞って等重み（fused_weight = lexical_weight
        // = 1.0）を明示的に構築する。
        // id=5: rank_fused=1（融合スコア降順で先頭）・rank_lexical=2（字句重なり少）
        // id=6: rank_fused=2                       ・rank_lexical=1（字句重なり多）
        // スコア = weight/(k+rank_fused) + weight/(k+rank_lexical) は rank の組が
        // 入れ替わっているだけなので id=5・id=6 で厳密に一致し、同点タイブレーク
        // （id 昇順）分岐（本ファイル `LexicalOverlapReranker::rerank` の
        // `.then(a.id.cmp(&b.id))`）を実際に通過する。
        let cfg = RerankConfig::new(10, 2).unwrap();
        let candidates = [cand(5, 3.0, "alpha"), cand(6, 2.0, "alpha bravo")];
        let reranker = LexicalOverlapReranker::new(60.0, 1.0, 1.0).unwrap();
        let hits = rerank_candidates(&reranker, "alpha bravo", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].score, hits[1].score, "must be a true score tie");
        assert_eq!(hits[0].id, 5, "tie must break by ascending id");
        assert_eq!(hits[1].id, 6);
    }

    #[test]
    fn lexical_overlap_reranker_lexical_rank_uses_group_end_for_tied_overlap() {
        // Issue #330: overlap（字句重なり件数）が同点の候補グループには、グループ内の
        // 全メンバーへ同一の rank_lexical（グループ末尾の 1-based 位置。GroupEnd）を
        // 割り当てる。位置順位（並べ替え後の enumerate 位置）のままだと、この 3 件は
        // いずれも overlap=1（"alpha" のみ一致）で同点のはずが id=1・id=2・id=3 の
        // 順に rank_lexical=1,2,3 と異なる値になり、`contribution_lexical` が
        // fused_score 降順（＝元の候補順）とは無関係な id ごとの寄与差を生む。
        // GroupEnd では 3 件とも rank_lexical=3（グループ末尾）を共有するため、
        // 3 候補間のスコア差は `contribution_fused`（rank_fused の差）のみに由来する
        // ことを検証する。
        let cfg = RerankConfig::new(10, 3).unwrap();
        // fused_score は互いに異なる（rank_fused は 1・2・3 で確定）が、text は
        // すべて "alpha" のみを含み overlap は 3 件とも 1 で同点にする。
        let candidates = [
            cand(1, 3.0, "alpha unrelated"),
            cand(2, 2.0, "alpha filler"),
            cand(3, 1.0, "alpha noise"),
        ];
        let reranker = LexicalOverlapReranker::new(60.0, 1.0, 1.0).unwrap();
        let hits = rerank_candidates(&reranker, "alpha", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 3);

        // GroupEnd では 3 件とも rank_lexical=3 を共有するため、期待スコアは
        // fused_weight/(k+rank_fused) + lexical_weight/(k+3) で計算できる
        // （rank_fused は候補順そのまま 1・2・3）。
        let k = 60.0;
        let expected = |rank_fused: f64| 1.0 / (k + rank_fused) + 1.0 / (k + 3.0);
        let by_id = |id: u64| hits.iter().find(|h| h.id == id).expect("id present").score;
        assert!(
            (by_id(1) - expected(1.0)).abs() < 1e-9,
            "id=1 score must match GroupEnd-based expected value"
        );
        assert!(
            (by_id(2) - expected(2.0)).abs() < 1e-9,
            "id=2 score must match GroupEnd-based expected value"
        );
        assert!(
            (by_id(3) - expected(3.0)).abs() < 1e-9,
            "id=3 score must match GroupEnd-based expected value"
        );
        // 出力順は fused_score 降順のまま保たれる（rank_lexical が全員同一のため
        // 逆転は起きない）。
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 2);
        assert_eq!(hits[2].id, 3);
    }

    #[test]
    fn rerank_candidates_rejects_k_zero_and_over_pool_depth() {
        let cfg_zero = RerankConfig::new(10, 1).unwrap();
        // final_k=0 は RerankConfig::new 自体が拒否するため、InvalidK の経路は
        // final_k > pool_depth のケースで確認する（RerankConfig::new は
        // 1..=pool_depth を検証するため、直接それを超える cfg は構築できない。
        // よって InvalidK は rerank_candidates 内の防御的二重検証として存在し、
        // ここでは RerankConfig::new 自体の拒否を確認する）。
        assert_eq!(
            RerankConfig::new(1, 0).unwrap_err(),
            RerankError::InvalidConfig
        );
        let candidates = [cand(1, 1.0, "a")];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg_zero).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_input_exceeding_pool_depth() {
        let cfg = RerankConfig::new(1, 1).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::TooManyCandidates { len: 2, max: 1 });
    }

    #[test]
    fn rerank_candidates_rejects_query_text_exceeding_max_bytes() {
        let cfg = RerankConfig::default();
        let oversized_query = "a".repeat(MAX_QUERY_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, "a")];
        let err =
            rerank_candidates(&IdentityReranker, &oversized_query, &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::QueryTextTooLong {
                len: MAX_QUERY_TEXT_BYTES + 1,
                max: MAX_QUERY_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn rerank_candidates_accepts_query_text_at_max_bytes_boundary() {
        let cfg = RerankConfig::default();
        let boundary_query = "a".repeat(MAX_QUERY_TEXT_BYTES);
        let candidates = [cand(1, 1.0, "a")];
        let hits =
            rerank_candidates(&IdentityReranker, &boundary_query, &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_candidate_text_exceeding_max_bytes() {
        let cfg = RerankConfig::default();
        let oversized_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, oversized_text.as_str())];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::CandidateTextTooLong {
                id: 1,
                len: MAX_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn rerank_candidates_accepts_candidate_text_at_max_bytes_boundary() {
        let cfg = RerankConfig::default();
        let boundary_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let candidates = [cand(1, 1.0, boundary_text.as_str())];
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_accepts_total_candidate_text_at_max_bytes_boundary() {
        // MAX_CANDIDATE_TEXT_BYTES 上限の候補を MAX_TOTAL_CANDIDATE_TEXT_BYTES ちょうどに
        // なる件数だけ並べても受理される（境界値。整数演算は各要素が上限一杯でも
        // オーバーフローしないことも合わせて確認する）。
        let doc_count = MAX_TOTAL_CANDIDATE_TEXT_BYTES / MAX_CANDIDATE_TEXT_BYTES;
        let cfg = RerankConfig::new(doc_count, 1).unwrap();
        let text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let candidates: Vec<RerankCandidate<'_>> = (0..doc_count as u64)
            .map(|i| cand(i, (doc_count - i as usize) as f64, text.as_str()))
            .collect();
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn rerank_candidates_rejects_total_candidate_text_exceeding_max_bytes() {
        // 候補 1 件あたりは MAX_CANDIDATE_TEXT_BYTES 以内でも、合計が
        // MAX_TOTAL_CANDIDATE_TEXT_BYTES を超えれば拒否する（sparse.rs::CorpusTooLarge
        // 相当。1 件あたりの上限のみでは合計コストが無制限に増幅しうるため）。
        let doc_count = MAX_TOTAL_CANDIDATE_TEXT_BYTES / MAX_CANDIDATE_TEXT_BYTES;
        let cfg = RerankConfig::new(doc_count + 1, 1).unwrap();
        let text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES);
        let mut candidates: Vec<RerankCandidate<'_>> = (0..doc_count as u64)
            .map(|i| cand(i, (doc_count - i as usize) as f64, text.as_str()))
            .collect();
        // 合計をちょうど MAX_TOTAL_CANDIDATE_TEXT_BYTES + 1 バイトにする 1 バイト候補を
        // 融合スコア最小として追加する。
        candidates.push(cand(doc_count as u64, -1.0, "a"));
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(
            err,
            RerankError::TotalCandidateTextTooLong {
                total: MAX_TOTAL_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_TOTAL_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_oversized_query_text() {
        // codex-review P1 指摘対応: 公開 trait Reranker::rerank を rerank_candidates を
        // 経由せず直接呼び出しても、実装内部の validate_text_lengths 呼び出しにより
        // MAX_QUERY_TEXT_BYTES 検証が迂回されないことを確認する。
        let reranker = LexicalOverlapReranker::default();
        let oversized_query = "a".repeat(MAX_QUERY_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, "a")];
        let err = reranker
            .rerank(&oversized_query, &candidates, 1)
            .unwrap_err();
        assert_eq!(
            err,
            RerankError::QueryTextTooLong {
                len: MAX_QUERY_TEXT_BYTES + 1,
                max: MAX_QUERY_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_oversized_candidate_text() {
        // 同上（候補テキスト側）。tokenize() が呼ばれる前に拒否されることを確認する。
        let reranker = LexicalOverlapReranker::default();
        let oversized_text = "a".repeat(MAX_CANDIDATE_TEXT_BYTES + 1);
        let candidates = [cand(1, 1.0, oversized_text.as_str())];
        let err = reranker.rerank("q", &candidates, 1).unwrap_err();
        assert_eq!(
            err,
            RerankError::CandidateTextTooLong {
                id: 1,
                len: MAX_CANDIDATE_TEXT_BYTES + 1,
                max: MAX_CANDIDATE_TEXT_BYTES,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_candidate_count_exceeding_max_pool_depth() {
        // codex-review P1 / Bugbot Medium 指摘対応: 公開 trait Reranker::rerank を
        // rerank_candidates を経由せず直接呼び出した場合、合計バイト長が
        // MAX_TOTAL_CANDIDATE_TEXT_BYTES を超えない短文候補を MAX_POOL_DEPTH 件超
        // 渡しても、tokenize()・Vec 確保（overlap_ranked）より前に件数超過として
        // 拒否されることを確認する。
        let reranker = LexicalOverlapReranker::default();
        let text = "a";
        let candidates: Vec<RerankCandidate<'_>> = (0..(MAX_POOL_DEPTH as u64 + 1))
            .map(|i| cand(i, -(i as f64), text))
            .collect();
        let err = reranker.rerank("q", &candidates, 1).unwrap_err();
        assert_eq!(
            err,
            RerankError::TooManyCandidates {
                len: MAX_POOL_DEPTH + 1,
                max: MAX_POOL_DEPTH,
            }
        );
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_non_finite_score() {
        // Issue #320 codex-review P1 指摘対応: rerank_candidates を経由しない直接
        // 呼び出しでも、隣接グループ化（rank_fused_by_idx）の前に有限性を検証し
        // 拒否することを確認する。
        let reranker = LexicalOverlapReranker::default();
        let candidates = [cand(1, f64::NAN, "a")];
        let err = reranker.rerank("q", &candidates, 1).unwrap_err();
        assert_eq!(err, RerankError::NonFiniteScore);
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_descending_order_violation() {
        // 同上（降順違反）。融合スコアが昇順（本来は降順であるべき契約に違反）。
        let reranker = LexicalOverlapReranker::default();
        let candidates = [cand(1, 1.0, "a"), cand(2, 2.0, "b")];
        let err = reranker.rerank("q", &candidates, 2).unwrap_err();
        assert_eq!(err, RerankError::UnsortedInput);
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_tie_group_violating_id_ascending() {
        // Issue #320 codex-review P1 指摘の核心ケース: 直後の `rank_fused_by_idx`
        // グループ化は「fused_score 降順であれば同点集合は必ず連続する」という
        // 不変条件（降順制約下では等スコアの要素は隣接せざるを得ない）に依拠して
        // 隣接走査のみでグループを求める。この不変条件が成立するのは降順性が
        // 保証されている場合に限るため、降順性は保ちつつ同点グループ内部の
        // id 昇順（`is_sorted_desc_id_asc` の `score_order == Equal && id < prev_id`
        // 分岐）にだけ違反する入力（id=2 の直後に同点 id=1 が来る）で、この
        // 実装自身が UnsortedInput を返すことを確認する（隣接グループ化に
        // 静かに委ねない）。
        let reranker = LexicalOverlapReranker::default();
        let candidates = [cand(2, 2.0, "a"), cand(1, 2.0, "b"), cand(3, 1.0, "c")];
        let err = reranker.rerank("q", &candidates, 3).unwrap_err();
        assert_eq!(err, RerankError::UnsortedInput);
    }

    #[test]
    fn lexical_overlap_reranker_direct_call_rejects_duplicate_id() {
        // 同上（重複 id）。
        let reranker = LexicalOverlapReranker::default();
        let candidates = [cand(1, 2.0, "a"), cand(1, 1.0, "b")];
        let err = reranker.rerank("q", &candidates, 2).unwrap_err();
        assert_eq!(err, RerankError::DuplicateId);
    }

    #[test]
    fn rerank_candidates_rejects_non_finite_score() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, f64::NAN, "a")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::NonFiniteScore);
    }

    #[test]
    fn rerank_candidates_rejects_unsorted_input() {
        let cfg = RerankConfig::default();
        // スコア昇順（本来は降順であるべき契約に違反）。
        let candidates = [cand(1, 1.0, "a"), cand(2, 2.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::UnsortedInput);
    }

    #[test]
    fn rerank_candidates_rejects_duplicate_input_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 2.0, "a"), cand(1, 1.0, "b")];
        let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::DuplicateId);
    }

    /// [`Reranker`] の契約違反（候補外 id を返す実装バグ）を模したモック。
    struct LeakyReranker;
    impl Reranker for LeakyReranker {
        fn rerank(
            &self,
            _query_text: &str,
            _candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            Ok(vec![RerankedHit {
                id: 999,
                score: 1.0,
            }])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_returning_foreign_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&LeakyReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::ForeignId);
    }

    /// [`Reranker`] の契約違反（出力内で id を重複させる実装バグ）を模したモック。
    struct DuplicatingReranker;
    impl Reranker for DuplicatingReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            let id = candidates.first().map(|c| c.id).unwrap_or(0);
            Ok(vec![
                RerankedHit { id, score: 2.0 },
                RerankedHit { id, score: 1.0 },
            ])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_returning_duplicate_output_id() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&DuplicatingReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::DuplicateOutputId);
    }

    /// [`Reranker`] の契約違反（要求した final_k を超える件数を返す実装バグ）を
    /// 模したモック。
    struct OverflowingReranker;
    impl Reranker for OverflowingReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            Ok(candidates
                .iter()
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect())
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_exceeding_final_k() {
        let cfg = RerankConfig::new(10, 1).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&OverflowingReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::OversizedResult { len: 2, max: 1 });
    }

    /// [`Reranker`] の契約違反（非有限スコアを返す実装バグ）を模したモック。
    struct NonFiniteOutputReranker;
    impl Reranker for NonFiniteOutputReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            let id = candidates.first().map(|c| c.id).unwrap_or(0);
            Ok(vec![RerankedHit {
                id,
                score: f64::NAN,
            }])
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_non_finite_output_score() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 1.0, "a")];
        let err = rerank_candidates(&NonFiniteOutputReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::InvalidOutputOrder);
    }

    /// [`Reranker`] の契約違反（出力の順序契約に違反する実装バグ）を模したモック。
    struct UnsortedOutputReranker;
    impl Reranker for UnsortedOutputReranker {
        fn rerank(
            &self,
            _query_text: &str,
            candidates: &[RerankCandidate<'_>],
            _final_k: usize,
        ) -> Result<Vec<RerankedHit>, RerankError> {
            // 候補は融合スコア降順で渡ってくるが、ここではあえてスコア昇順で返す。
            Ok(candidates
                .iter()
                .rev()
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect())
        }
    }

    #[test]
    fn rerank_candidates_rejects_reranker_unsorted_output() {
        let cfg = RerankConfig::default();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err = rerank_candidates(&UnsortedOutputReranker, "q", &candidates, &cfg).unwrap_err();
        assert_eq!(err, RerankError::InvalidOutputOrder);
    }

    #[test]
    fn rerank_candidates_output_is_subset_of_input_pool() {
        // SEARCH-6 対応: リランカーはプールを拡張しない（出力 ⊆ 入力）ことの単体
        // レベルでの固定。
        let cfg = RerankConfig::new(200, 20).unwrap();
        assert_eq!(cfg.pool_depth(), 200);
        let candidates: Vec<RerankCandidate<'_>> =
            (0..5).map(|i| cand(i, (5 - i) as f64, "x")).collect();
        let hits = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).expect("ok");
        let input_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();
        assert!(hits.iter().all(|h| input_ids.contains(&h.id)));
    }

    // ---------- CrossEncoderReranker（Issue #333）: 決定的スタブによる契約テスト ----------
    //
    // `crates/engine/tests/rerank_cross_encoder.rs`（feature 非依存・常時 CI 対象）が
    // より広い契約（バッチ分割呼び出し数の検証等）を担うため、ここでは
    // `CrossEncoderReranker::rerank` 自身の内部検証（件数上限・バックエンド委譲・
    // fail-closed）に絞った最小限のスタブテストのみを置く。

    /// 決定的スタブ: passage の長さをスコアとして返す（順序が決定的になるだけの
    /// 単純な合成信号。実モデルの近似ではない）。
    struct LengthScoreBackend;

    impl CrossEncoderBackend for LengthScoreBackend {
        fn score_pairs(
            &self,
            _query: &str,
            passages: &[&str],
        ) -> Result<Vec<f64>, CrossEncoderError> {
            Ok(passages.iter().map(|p| p.len() as f64).collect())
        }

        fn max_seq_len(&self) -> usize {
            512
        }
    }

    #[test]
    fn cross_encoder_reranker_orders_by_backend_score_descending() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthScoreBackend, cfg).unwrap();
        let candidates = [
            cand(1, 3.0, "aa"),
            cand(2, 2.0, "aaaaaa"),
            cand(3, 1.0, "aaaa"),
        ];
        let hits =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).expect("ok");
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 3, 1]);
    }

    #[test]
    fn cross_encoder_reranker_is_deterministic_across_calls() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthScoreBackend, cfg).unwrap();
        let candidates = [cand(1, 3.0, "aa"), cand(2, 2.0, "aaaaaa")];
        let first =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).expect("ok");
        let second =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).expect("ok");
        assert_eq!(first, second);
    }

    struct LengthMismatchBackend;

    impl CrossEncoderBackend for LengthMismatchBackend {
        fn score_pairs(
            &self,
            _query: &str,
            passages: &[&str],
        ) -> Result<Vec<f64>, CrossEncoderError> {
            Ok(vec![0.0; passages.len().saturating_sub(1)])
        }

        fn max_seq_len(&self) -> usize {
            512
        }
    }

    #[test]
    fn cross_encoder_reranker_rejects_backend_length_mismatch() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthMismatchBackend, cfg).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let err =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            RerankError::CrossEncoder(CrossEncoderError::LengthMismatch { .. })
        ));
    }

    struct NonFiniteScoreBackend;

    impl CrossEncoderBackend for NonFiniteScoreBackend {
        fn score_pairs(
            &self,
            _query: &str,
            passages: &[&str],
        ) -> Result<Vec<f64>, CrossEncoderError> {
            Ok(vec![f64::NAN; passages.len()])
        }

        fn max_seq_len(&self) -> usize {
            512
        }
    }

    #[test]
    fn cross_encoder_reranker_rejects_backend_non_finite_score() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(NonFiniteScoreBackend, cfg).unwrap();
        let candidates = [cand(1, 2.0, "a")];
        let err =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            RerankError::CrossEncoder(CrossEncoderError::NonFiniteScore)
        ));
    }

    struct FailingBackend;

    impl CrossEncoderBackend for FailingBackend {
        fn score_pairs(
            &self,
            _query: &str,
            _passages: &[&str],
        ) -> Result<Vec<f64>, CrossEncoderError> {
            Err(CrossEncoderError::Backend("stub failure".to_string()))
        }

        fn max_seq_len(&self) -> usize {
            512
        }
    }

    #[test]
    fn cross_encoder_reranker_propagates_backend_error() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(FailingBackend, cfg).unwrap();
        let candidates = [cand(1, 2.0, "a")];
        let err =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            RerankError::CrossEncoder(CrossEncoderError::Backend(_))
        ));
    }

    #[test]
    fn cross_encoder_reranker_rejects_too_many_candidates() {
        // `max_candidates` を `pool_depth` より小さく設定し、`rerank_candidates` の
        // `TooManyCandidates`（入口検証）を通過しても `CrossEncoderReranker::rerank`
        // 自身が候補数上限を検証することを固定する。
        let cfg = CrossEncoderConfig::new(32, 1, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthScoreBackend, cfg).unwrap();
        let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
        let rerank_cfg = RerankConfig::new(200, 20).unwrap();
        let err = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).unwrap_err();
        assert!(matches!(
            err,
            RerankError::CrossEncoder(CrossEncoderError::TooManyCandidates { len: 2, max: 1 })
        ));
    }

    #[test]
    fn cross_encoder_config_rejects_invalid_values() {
        assert_eq!(
            CrossEncoderConfig::new(0, 200, 512).unwrap_err(),
            CrossEncoderError::InvalidConfig
        );
        assert_eq!(
            CrossEncoderConfig::new(32, 0, 512).unwrap_err(),
            CrossEncoderError::InvalidConfig
        );
        assert_eq!(
            CrossEncoderConfig::new(32, 200, 0).unwrap_err(),
            CrossEncoderError::InvalidConfig
        );
        assert_eq!(
            CrossEncoderConfig::new(32, MAX_POOL_DEPTH + 1, 512).unwrap_err(),
            CrossEncoderError::InvalidConfig
        );
    }

    // Issue #333 codex-review 指摘: `CrossEncoderConfig::max_seq_len` の検証済み値が
    // 推論経路（`CrossEncoderBackend::max_seq_len`）と無関係に構築できてしまうと、
    // DoS／メモリ上限の有界化を保証できない。`CrossEncoderReranker::new` が構築時に
    // 両者の一致を検証し、不一致を拒否することを固定する。
    #[test]
    fn cross_encoder_reranker_new_rejects_backend_seq_len_mismatch() {
        // `LengthScoreBackend::max_seq_len` は 512 固定。`cfg` 側を異なる値
        // （256）にすることで不一致を作り、構築拒否を固定する。
        let cfg = CrossEncoderConfig::new(32, 200, 256).unwrap();
        let result = CrossEncoderReranker::new(LengthScoreBackend, cfg);
        assert!(matches!(
            result,
            Err(CrossEncoderError::BackendSeqLenMismatch {
                backend: 512,
                configured: 256,
            })
        ));
    }

    // ---------- Issue #2144: 推論期限・モデルファイル検証・方式結線 ----------

    #[test]
    fn cross_encoder_zero_timeout_returns_timeout_after_first_batch() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthScoreBackend, cfg)
            .unwrap()
            .with_timeout(std::time::Duration::ZERO);
        let candidates = [cand(1, 2.0, "aa"), cand(2, 1.0, "aaaa")];
        let err =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            RerankError::CrossEncoder(CrossEncoderError::Timeout)
        ));
    }

    #[test]
    fn cross_encoder_generous_timeout_keeps_output() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let reranker = CrossEncoderReranker::new(LengthScoreBackend, cfg)
            .unwrap()
            .with_timeout(std::time::Duration::from_secs(3600));
        let candidates = [cand(1, 2.0, "aa"), cand(2, 1.0, "aaaa")];
        let hits =
            rerank_candidates(&reranker, "q", &candidates, &RerankConfig::default()).expect("ok");
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn validate_rerank_timeout_ms_boundaries() {
        assert!(validate_rerank_timeout_ms(0).is_err());
        assert_eq!(
            validate_rerank_timeout_ms(1).unwrap(),
            std::time::Duration::from_millis(1)
        );
        assert!(validate_rerank_timeout_ms(60_000).is_ok());
        assert!(validate_rerank_timeout_ms(60_001).is_err());
        assert!(validate_rerank_timeout_ms(DEFAULT_RERANK_TIMEOUT_MS).is_ok());
    }

    #[test]
    fn cross_encoder_method_kind_name() {
        let cfg = CrossEncoderConfig::new(32, 200, 512).unwrap();
        let m = RerankMethod::cross_encoder(
            CrossEncoderReranker::new(LengthScoreBackend, cfg).unwrap(),
        );
        assert_eq!(m.kind_name(), "cross_encoder");
        assert!(m.reranker().is_some());
    }

    /// 一意な一時ディレクトリ（Drop で削除。tmp-leak-check 対策）。
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "fandhe-ce-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn reason(e: CrossEncoderError) -> &'static str {
        match e {
            CrossEncoderError::ModelFile { reason, .. } => reason,
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn read_bounded_model_file_checks() {
        let d = TmpDir::new("bounded");
        let f = d.0.join("m.bin");
        std::fs::write(&f, b"12345").unwrap();
        let file = CrossEncoderFile::Model;
        assert_eq!(read_bounded_model_file(&f, file, 5).unwrap(), b"12345");
        assert_eq!(
            reason(read_bounded_model_file(&f, file, 4).unwrap_err()),
            "too_large"
        );
        assert_eq!(
            reason(read_bounded_model_file(&d.0, file, 100).unwrap_err()),
            "not_regular_file"
        );
        assert_eq!(
            reason(read_bounded_model_file(&d.0.join("none"), file, 100).unwrap_err()),
            "unreadable"
        );
        let traversal = d.0.join("..").join("m.bin");
        let e = read_bounded_model_file(&traversal, file, 100).unwrap_err();
        assert!(!e.to_string().contains("fandhe-ce"));
        assert_eq!(reason(e), "parent_dir_component");
    }

    #[cfg(feature = "cross-encoder")]
    #[test]
    fn onnx_method_with_missing_model_returns_model_file_error() {
        let d = TmpDir::new("onnx-missing");
        let err = onnx_cross_encoder_method(
            &d.0.join("no-model"),
            &d.0.join("no-tok"),
            10,
            std::time::Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(reason(err), "unreadable");
    }
}
#[cfg(test)]
mod mmr_tests {
    use super::*;

    /// `(id, 埋め込み)` から融合スコア降順（先頭が最大）の候補列を作る。
    fn cands(rows: &[(u64, Vec<f32>)]) -> Vec<RerankCandidate<'_>> {
        rows.iter()
            .enumerate()
            .map(|(i, (id, e))| RerankCandidate {
                id: *id,
                fused_score: 100.0 - i as f64,
                text: "",
                embedding: Some(e.as_slice()),
            })
            .collect()
    }

    fn ids(hits: &[RerankedHit]) -> Vec<u64> {
        hits.iter().map(|h| h.id).collect()
    }

    fn cfg(n: usize, k: usize) -> RerankConfig {
        RerankConfig::new(n, k).unwrap()
    }

    /// A が最上位、B は A とほぼ同じ向き、C は A と直交。
    fn abc() -> Vec<(u64, Vec<f32>)> {
        vec![
            (1, vec![1.0, 0.0]),
            (2, vec![0.99, 0.1]),
            (3, vec![0.0, 1.0]),
        ]
    }

    #[test]
    fn lambda_one_reproduces_the_input_order() {
        let rows = abc();
        let c = cands(&rows);
        let r = MmrReranker::new(1.0).unwrap();
        let out = rerank_candidates(&r, "", &c, &cfg(3, 3)).unwrap();
        assert_eq!(ids(&out), vec![1, 2, 3]);
        let out = rerank_candidates(&r, "", &c, &cfg(3, 2)).unwrap();
        assert_eq!(ids(&out), vec![1, 2]);
    }

    #[test]
    fn lambda_one_keeps_order_for_ties_and_single_candidate() {
        // 融合スコアが同点（id 昇順）でも入力位置どおり。
        let rows = abc();
        let mut c = cands(&rows);
        for x in &mut c {
            x.fused_score = 1.0;
        }
        let r = MmrReranker::new(1.0).unwrap();
        assert_eq!(
            ids(&rerank_candidates(&r, "", &c, &cfg(3, 3)).unwrap()),
            vec![1, 2, 3]
        );
        let one = &c[..1];
        assert_eq!(
            ids(&rerank_candidates(&r, "", one, &cfg(1, 1)).unwrap()),
            vec![1]
        );
    }

    #[test]
    fn lambda_zero_maximizes_diversity_but_starts_from_the_top() {
        let rows = abc();
        let c = cands(&rows);
        let r = MmrReranker::new(0.0).unwrap();
        let out = rerank_candidates(&r, "", &c, &cfg(3, 3)).unwrap();
        assert_eq!(ids(&out), vec![1, 3, 2]);
    }

    #[test]
    fn ties_break_by_id_ascending_and_runs_are_deterministic() {
        // 2 件目の候補 9 と 5 は A と同じ類似度（ともに直交）。
        let rows = vec![
            (1, vec![1.0, 0.0]),
            (9, vec![0.0, 1.0]),
            (5, vec![0.0, -1.0]),
        ];
        let mut c = cands(&rows);
        // 入力契約（スコア降順・同点 id 昇順）を満たすよう、9 と 5 を同点にして id 昇順へ並べる。
        c[1].fused_score = 50.0;
        c[2].fused_score = 50.0;
        c.swap(1, 2);
        let r = MmrReranker::new(0.0).unwrap();
        let a = rerank_candidates(&r, "", &c, &cfg(3, 3)).unwrap();
        let b = rerank_candidates(&r, "", &c, &cfg(3, 3)).unwrap();
        assert_eq!(a, b);
        assert_eq!(ids(&a), vec![1, 5, 9]);
    }

    #[test]
    fn lambda_validation() {
        for ok in [0.0, 0.5, 1.0, -0.0] {
            assert!(validate_mmr_lambda(ok).is_ok(), "{ok}");
        }
        for bad in [
            -0.1,
            1.000_000_1,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert_eq!(validate_mmr_lambda(bad), Err(RerankError::InvalidConfig));
            assert!(MmrReranker::new(bad).is_err());
        }
        assert_eq!(MmrReranker::default().lambda(), DEFAULT_MMR_LAMBDA);
    }

    #[test]
    fn unusable_embeddings_are_rejected() {
        let r = MmrReranker::new(0.5).unwrap();
        let good = vec![(1, vec![1.0, 0.0]), (2, vec![0.0, 1.0])];
        let mut c = cands(&good);
        c[1].embedding = None;
        assert_eq!(
            r.rerank("", &c, 2),
            Err(RerankError::InvalidEmbedding { id: 2 })
        );
        for bad in [
            vec![1.0],
            vec![0.0, 0.0],
            vec![f32::NAN, 1.0],
            vec![f32::INFINITY, 0.0],
        ] {
            let rows = vec![(1, vec![1.0, 0.0]), (2, bad)];
            let c = cands(&rows);
            assert_eq!(
                r.rerank("", &c, 2),
                Err(RerankError::InvalidEmbedding { id: 2 })
            );
        }
    }

    #[test]
    fn invalid_k_and_direct_call_contracts() {
        let r = MmrReranker::new(0.5).unwrap();
        let rows = abc();
        let c = cands(&rows);
        assert_eq!(r.rerank("", &c, 0), Err(RerankError::InvalidK));
        assert_eq!(r.rerank("", &c, 4), Err(RerankError::InvalidK));
        let mut unsorted = cands(&rows);
        unsorted.swap(0, 1);
        assert_eq!(r.rerank("", &unsorted, 2), Err(RerankError::UnsortedInput));
    }

    #[test]
    fn budget_bounds() {
        assert!(mmr_budget_allows(1000, 1000));
        assert!(!mmr_budget_allows(1001, 1000));
        assert!(!mmr_budget_allows(10_000, 101));
        assert!(mmr_budget_allows(10_000, 100));
        // k_eff が窓を超えても W × W で判定する。
        assert!(mmr_budget_allows(1000, 5000));
        assert!(!mmr_budget_allows(usize::MAX, usize::MAX));
    }

    #[test]
    fn direct_call_over_budget_is_rejected() {
        let r = MmrReranker::new(0.5).unwrap();
        // 1001 件 × final_k 1000 = 1,001,000 > 予算。埋め込みの中身に入る前に拒否される。
        let rows: Vec<(u64, Vec<f32>)> = (0..1001u64).map(|i| (i, vec![1.0, 0.0])).collect();
        let c = cands(&rows);
        assert!(matches!(
            r.rerank("", &c, 1000),
            Err(RerankError::TooManyCandidates { .. })
        ));
    }

    #[test]
    fn method_metadata() {
        let m = RerankMethod::Mmr(MmrReranker::default());
        assert_eq!(m.kind_name(), "mmr");
        assert_eq!(m.required_input(), RerankInput::Embedding);
        assert!(m.reranker().is_some());
        let c = RerankMethod::CustomEmbedding(std::sync::Arc::new(IdentityReranker));
        assert_eq!(c.kind_name(), "custom");
        assert_eq!(c.required_input(), RerankInput::Embedding);
        assert_eq!(RerankMethod::Identity.required_input(), RerankInput::Text);
    }
}
