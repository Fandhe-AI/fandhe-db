//! 構文解析済みの全文検索クエリ（除外語・前方一致・フレーズ・`or`）を可視集合の BM25
//! スコアリングへ結線する評価経路（Issue #2115・親 #2111。対象ビヘイビア SEARCH-12 の
//! ポインタ: `docs/spec/04-behavior/search.md`、RLS-8 のポインタ: 可視集合の縮約。設計の
//! 根拠は公開済みの `docs/design/fulltext-query-syntax.md` の D2・D5・D6・D7）。
//!
//! 責務境界: [`SparseIndex::score_text_query_within`]（構文木入口）と
//! [`SparseIndex::score_text_query_str_within`]（文字列入口。`parse_text_query` を先に呼ぶ）を
//! 提供する。既存の文字列 API（`search`・`score_within`・`score_within_with_params`）は変えない。
//! HYBRID／NoSQL 表層からの呼び出しは #2116 で結線済み（`hybrid::hybrid_search_with_text_query`・
//! `sql::exec` の `map_hybrid_text_error`）。本モジュールは [`TextSearchError::wire_code`] までを定義する。
//!
//! 評価（D2・D5）: `or` で分けたグループごとに、正の語（通常語・前方一致の展開語・フレーズの
//! 構成トークン）を既存の `score_pass`（可視集合の統計）で採点し、フレーズ一致と除外で
//! 文書を絞ってから、グループの入力順に文書ごとへ加算する（和集合）。除外・フレーズは
//! フィルタであり、統計（N・avgdl・df）には影響しない。結果は [`SparseScored`] で、
//! スコア降順・同点 `doc_id` 昇順の全順序は `Candidate::Ord` が構造的に保証する。
//!
//! 上限（D7）: 検査順は既存の長さ・一意語数（`22000`）→ 項数（`54000`）→ フレーズ長
//! （`54000`）→ 前方一致の展開数（`54000`）。項数・展開数はグループごとに数えて合算し、
//! 同じ語や語幹を多数のグループで繰り返す入力による走査量の増幅を上限に含める。
//! 展開数は可視集合で df > 0 の語だけを数え、不可視テナントの語の存在を上限超過の有無から
//! 推測させない（RLS-8）。時間差のサイドチャネルは既知の制約として設計記録に残す。
//!
//! 計算量: 前方一致の展開は語幹の範囲の和集合を 1 回だけ走査し `O(V_ascii + Σ|postings|)`
//! （語幹の数に比例して増えない）。グループの評価は同時に `ScorePass` を 1 つだけ保持する。

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::phrase::{PhraseError, PhrasePattern};
use super::text_query::{
    parse_text_query, Clause, Excluded, QueryGroup, TextQuery, TextQueryError,
};
use super::{
    tokenize_analyzed, Bm25Params, Candidate, DocId, ScoreScope, SparseIndex, SparseScored, TermId,
    TextAnalysis, VisibleBitmap,
};
use crate::text_query_limits::TextQueryLimits;

/// [`SparseIndex::score_text_query_within`] のエラー。内容は件数と上限値だけで、検索語・語幹・
/// 展開語・テナント情報は含めない（fail-closed）。`wire_code` は [`Self::wire_code`]。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum TextSearchError {
    /// 検索語のバイト長が既存上限を超える（`22000`）。
    QueryTooLong {
        /// 入力のバイト長。
        len: usize,
        /// 上限。
        max: usize,
    },
    /// 検索語の一意語数が既存上限を超える（`22000`）。
    TooManyQueryTerms {
        /// 一意語数。
        unique_terms: usize,
        /// 上限。
        max: usize,
    },
    /// グループごとの一意な語の総和が項数の上限を超える（`54000`）。
    TooManyTerms {
        /// 語の総数。
        count: usize,
        /// 上限。
        max: usize,
    },
    /// 前方一致の展開語数が上限を超える（`54000`）。件数は打ち切るため上限のみ持つ。
    TooManyPrefixExpansions {
        /// 上限。
        max: usize,
    },
    /// フレーズの必須トークン数が上限を超える（`54000`）。
    PhraseTooLong {
        /// 必須トークン数。
        len: usize,
        /// 上限。
        max: usize,
    },
    /// BM25 係数が不正、またはスコアが有限でない（`22000`）。
    InvalidParams {
        /// 係数 `k1`。
        k1: f64,
        /// 係数 `b`。
        b: f64,
    },
}

impl TextSearchError {
    /// SQLSTATE 風の `wire_code`。上限超過（検索語が複雑すぎる）は `54000`、入力そのものの
    /// 不正は `22000`（D7。既存上限の超過は現行どおり `22000` のまま）。
    pub fn wire_code(&self) -> &'static str {
        match self {
            Self::QueryTooLong { .. }
            | Self::TooManyQueryTerms { .. }
            | Self::InvalidParams { .. } => "22000",
            Self::TooManyTerms { .. }
            | Self::TooManyPrefixExpansions { .. }
            | Self::PhraseTooLong { .. } => "54000",
        }
    }
}

impl fmt::Display for TextSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueryTooLong { len, max } => {
                write!(f, "query is too long: {len} bytes (max {max})")
            }
            Self::TooManyQueryTerms { unique_terms, max } => {
                write!(f, "too many query terms: {unique_terms} (max {max})")
            }
            Self::TooManyTerms { count, max } => {
                write!(f, "query has too many terms: {count} (max {max})")
            }
            Self::TooManyPrefixExpansions { max } => {
                write!(f, "prefix expansion exceeds the limit (max {max})")
            }
            Self::PhraseTooLong { len, max } => {
                write!(f, "phrase has too many required tokens: {len} (max {max})")
            }
            Self::InvalidParams { k1, b } => {
                write!(f, "invalid BM25 params or non-finite score: k1={k1}, b={b}")
            }
        }
    }
}

impl std::error::Error for TextSearchError {}

impl From<TextQueryError> for TextSearchError {
    fn from(e: TextQueryError) -> Self {
        match e {
            TextQueryError::QueryTooLong { len, max } => Self::QueryTooLong { len, max },
            TextQueryError::TooManyQueryTerms { unique_terms, max } => {
                Self::TooManyQueryTerms { unique_terms, max }
            }
        }
    }
}

impl From<PhraseError> for TextSearchError {
    fn from(e: PhraseError) -> Self {
        match e {
            PhraseError::PatternTooLong { len, max } => Self::PhraseTooLong { len, max },
            // 文字列入口では先に弾かれ到達しないが、到達しても 22000 側（fail-closed）へ倒す。
            PhraseError::QueryTooLong { len, max } => Self::QueryTooLong { len, max },
            PhraseError::TooManyQueryTerms { unique_terms, max } => {
                Self::TooManyQueryTerms { unique_terms, max }
            }
        }
    }
}

/// グループ 1 つ分の評価計画（フレーズはコンパイル済み）。
struct GroupPlan<'q> {
    group: &'q QueryGroup,
    /// 全てに一致する必要があるフレーズ（正）。
    required: Vec<PhrasePattern>,
    /// どれかに一致したら落とすフレーズ（除外。1 語の除外は長さ 1）。
    excluded: Vec<PhrasePattern>,
    /// このグループが使う前方一致の語幹（正・除外）。
    /// 英語ステミング有効時は語幹もステミング後の形（Issue #3050。索引語と同じ形にそろえる）。
    stems: BTreeSet<Cow<'q, str>>,
}

/// グループ内の一意な語（通常語・語幹・フレーズ／除外の構成トークン）の集合。
fn group_unique_words(group: &QueryGroup) -> BTreeSet<&str> {
    let mut set: BTreeSet<&str> = BTreeSet::new();
    for c in group.clauses() {
        match c {
            Clause::Term(t) | Clause::Prefix(t) | Clause::Exclude(Excluded::Prefix(t)) => {
                set.insert(t.as_str());
            }
            Clause::Phrase(p) | Clause::Exclude(Excluded::Phrase(p)) => {
                set.extend(p.tokens().iter().map(String::as_str));
            }
        }
    }
    set
}

/// 語幹ごとの展開結果。`visible` は可視集合で df > 0 の語の語彙添字（昇順）、`spans` は
/// 語幹から `visible` 内の半開区間。区間で持つので語幹が重なっても複製しない。
struct Expansion<'q> {
    visible: Vec<usize>,
    spans: BTreeMap<Cow<'q, str>, (usize, usize)>,
}

impl Expansion<'_> {
    /// 語幹の展開に含まれる語彙添字。
    fn indices(&self, stem: &str) -> &[usize] {
        self.spans
            .get(stem)
            .and_then(|&(a, b)| self.visible.get(a..b))
            .unwrap_or(&[])
    }
}

/// データ非依存の入口検証（項数 `54000`・フレーズ長 `54000`）を行い、グループごとの評価計画
/// （フレーズはコンパイル済み）を作る。[`SparseIndex::score_text_query_within`] と
/// [`validate_text_query_static`] が共有し、検証ロジックを複製しない。
fn compile_group_plans<'q>(
    query: &'q TextQuery,
    analysis: &TextAnalysis,
    limits: &TextQueryLimits,
) -> Result<Vec<GroupPlan<'q>>, TextSearchError> {
    let mut total_words: usize = 0;
    for g in query.groups() {
        total_words = total_words.saturating_add(group_unique_words(g).len());
    }
    if total_words > limits.max_terms() {
        return Err(TextSearchError::TooManyTerms {
            count: total_words,
            max: limits.max_terms(),
        });
    }
    let mut plans: Vec<GroupPlan<'_>> = Vec::with_capacity(query.groups().len());
    for g in query.groups() {
        let mut required = Vec::new();
        let mut excluded = Vec::new();
        let mut stems: BTreeSet<Cow<'_, str>> = BTreeSet::new();
        for c in g.clauses() {
            match c {
                Clause::Term(_) => {}
                Clause::Prefix(s) | Clause::Exclude(Excluded::Prefix(s)) => {
                    stems.insert(analysis.stem_prefix(s.as_str()));
                }
                // 同一フィルタは評価前に一意化する（重複入力で走査量・中間結果を
                // 増幅させない。max_terms は一意語で数えるため、ここで揃える）。
                Clause::Phrase(p) => {
                    let pat = PhrasePattern::from_phrase_with(p, analysis)?;
                    if !required.contains(&pat) {
                        required.push(pat);
                    }
                }
                Clause::Exclude(Excluded::Phrase(p)) => {
                    let pat = PhrasePattern::from_phrase_with(p, analysis)?;
                    if !excluded.contains(&pat) {
                        excluded.push(pat);
                    }
                }
            }
        }
        plans.push(GroupPlan {
            group: g,
            required,
            excluded,
            stems,
        });
    }
    Ok(plans)
}

/// 索引・可視集合を必要としない検索語の静的検証（SEARCH-12。Issue #2116）。項数とフレーズ長の
/// 上限（`54000`）を [`SparseIndex::score_text_query_within`] の入口検証と同じ実装で行う。
/// `sql::exec` が「可視行に本文が 1 件もなく疎索引を作れず密のみへ縮退する」文でも呼び、
/// 結果が表の内容次第で変わらないようにする（fail-closed）。展開数の検査はデータ依存のため
/// 含まない（可視集合が空なら評価経路も展開前に空を返すので非対称にならない）。
pub(crate) fn validate_text_query_static(
    query: &TextQuery,
    analysis: &TextAnalysis,
    limits: &TextQueryLimits,
) -> Result<(), TextSearchError> {
    compile_group_plans(query, analysis, limits).map(|_| ())
}

impl SparseIndex {
    /// 構文解析済みの検索語を可視集合内で評価する（SEARCH-12・RLS-8 ポインタ）。
    ///
    /// HYBRID の全文検索側（`hybrid::hybrid_search_with_text_query`。Issue #2116 で結線済み）が呼ぶ。統計（N・avgdl・df）・フレーズ・除外・
    /// 前方一致の展開数は `visible_ids` に限定し、不可視文書は結果にも上限判定にも影響しない。
    /// 演算子を含まない検索語は `score_within_with_params` とビット一致する（D8）。
    /// `params` が索引自身の値でなければ `Bm25Params::new` で再検証する。
    pub fn score_text_query_within(
        &self,
        query: &TextQuery,
        visible_ids: &BTreeSet<DocId>,
        params: Bm25Params,
        limits: &TextQueryLimits,
    ) -> Result<SparseScored, TextSearchError> {
        let params = if params == self.params() {
            params
        } else {
            Bm25Params::new(params.k1(), params.b()).map_err(|_| {
                TextSearchError::InvalidParams {
                    k1: params.k1(),
                    b: params.b(),
                }
            })?
        };

        // 入口検証（データ非依存。visible_ids が空でも実行する）。
        let plans = compile_group_plans(query, &self.analysis, limits)?;

        let empty = || SparseScored {
            candidates: Vec::new(),
            sorted_len: 0,
        };
        if plans.is_empty() || visible_ids.is_empty() {
            return Ok(empty());
        }
        let bitmap = VisibleBitmap::build(self, visible_ids);
        if bitmap.n == 0 {
            return Ok(empty());
        }

        // 前方一致の展開（データ依存。可視集合の語だけを数える）。
        let all_stems: BTreeSet<Cow<'_, str>> =
            plans.iter().flat_map(|p| p.stems.iter().cloned()).collect();
        let expansion = self.expand_stems(&all_stems, &bitmap, limits.max_prefix_expansions())?;
        let mut total_expansions: usize = 0;
        for p in &plans {
            for stem in &p.stems {
                total_expansions = total_expansions.saturating_add(expansion.indices(stem).len());
                if total_expansions > limits.max_prefix_expansions() {
                    return Err(TextSearchError::TooManyPrefixExpansions {
                        max: limits.max_prefix_expansions(),
                    });
                }
            }
        }

        // グループの入力順に逐次評価し、文書ごとに加算する（加算順が決定的）。
        let mut acc: BTreeMap<u32, f64> = BTreeMap::new();
        for plan in &plans {
            for (doc_idx, score) in self.eval_group(plan, &expansion, &bitmap, params)? {
                *acc.entry(doc_idx).or_insert(0.0) += score;
            }
        }
        let mut candidates = Vec::with_capacity(acc.len());
        for (doc_idx, score) in acc {
            if !score.is_finite() {
                return Err(TextSearchError::InvalidParams {
                    k1: params.k1(),
                    b: params.b(),
                });
            }
            if let Some(&doc_id) = self.doc_ids.get(doc_idx as usize) {
                candidates.push(Candidate { score, doc_id });
            }
        }
        Ok(SparseScored {
            candidates,
            sorted_len: 0,
        })
    }

    /// 文字列入口。`parse_text_query` の上限検証（`22000`）を先に行ってから
    /// [`Self::score_text_query_within`] を呼ぶ。
    pub fn score_text_query_str_within(
        &self,
        query: &str,
        visible_ids: &BTreeSet<DocId>,
        params: Bm25Params,
        limits: &TextQueryLimits,
    ) -> Result<SparseScored, TextSearchError> {
        let parsed = parse_text_query(query)?;
        self.score_text_query_within(&parsed, visible_ids, params, limits)
    }

    /// 語幹（昇順・一意）ごとに、順序付き語彙の範囲から可視集合で df > 0 の語を求める。
    /// 範囲の和集合を 1 回だけ走査する（前方一致の範囲は入れ子か互いに素）。
    ///
    /// 一意な可視展開語が `max` を超えた時点で `TooManyPrefixExpansions` を返して走査を打ち切る
    /// （上限超過クエリで全該当語を走査・確保しない。一意数 <= 加算数なので判定は後段と整合する）。
    fn expand_stems<'q>(
        &self,
        stems: &BTreeSet<Cow<'q, str>>,
        bitmap: &VisibleBitmap,
        max: usize,
    ) -> Result<Expansion<'q>, TextSearchError> {
        let mut visible: Vec<usize> = Vec::new();
        let mut spans: BTreeMap<Cow<'q, str>, (usize, usize)> = BTreeMap::new();
        let mut scanned_to = 0usize;
        for stem in stems {
            let stem_str: &str = stem;
            let lo = self.prefix_vocab.partition_point(|(t, _)| &**t < stem_str);
            let tail = self.prefix_vocab.get(lo..).unwrap_or(&[]);
            let hi = lo.saturating_add(tail.partition_point(|(t, _)| t.starts_with(stem_str)));
            for i in lo.max(scanned_to)..hi {
                if let Some(&(_, tid)) = self.prefix_vocab.get(i) {
                    if self.has_visible_posting(tid, bitmap) {
                        visible.push(i);
                        if visible.len() > max {
                            return Err(TextSearchError::TooManyPrefixExpansions { max });
                        }
                    }
                }
            }
            scanned_to = scanned_to.max(hi);
            let a = visible.partition_point(|&i| i < lo);
            let b = visible.partition_point(|&i| i < hi);
            spans.insert(stem.clone(), (a, b));
        }
        Ok(Expansion { visible, spans })
    }

    /// 構文木の語 1 つを索引の `TextAnalysis` で正規化した語列にする（同義語置換・
    /// ストップワード除去。`score_within_with_params` の `tokenize_analyzed` と同じ処理）。
    /// 設定が恒等なら入力をそのまま返し、従来経路とビット一致を保つ（D8）。
    fn analyzed_words(&self, word: &str) -> Vec<String> {
        if self.analysis.is_empty() {
            return vec![word.to_owned()];
        }
        tokenize_analyzed(word, &self.analysis)
    }

    /// 語の posting に可視文書が 1 つでもあるか（最初のヒットで打ち切る）。
    fn has_visible_posting(&self, tid: TermId, bitmap: &VisibleBitmap) -> bool {
        self.postings
            .get(tid.0 as usize)
            .is_some_and(|list| list.iter().any(|&(d, _)| bitmap.contains(d)))
    }

    /// グループ 1 つを評価し、一致した `(doc_idx, スコア)` を返す。
    fn eval_group(
        &self,
        plan: &GroupPlan<'_>,
        expansion: &Expansion<'_>,
        bitmap: &VisibleBitmap,
        params: Bm25Params,
    ) -> Result<Vec<(u32, f64)>, TextSearchError> {
        // 除外だけのグループは常に不一致（D2）。
        if !plan.group.has_positive() {
            return Ok(Vec::new());
        }

        // 必須フレーズ: 逐次交差で絞り込み、フレーズ数×文書数の中間配列を作らない。
        // 1 つでも一致 0 件（交差が空）ならグループは空。
        let mut required: Option<Vec<u32>> = None;
        for pat in &plan.required {
            let hits: Vec<u32> = self
                .phrase_match_doc_idx(pat, Some(bitmap))
                .into_iter()
                .map(|(d, _)| d)
                .collect();
            let next = match required.take() {
                None => hits,
                Some(prev) => prev
                    .into_iter()
                    .filter(|d| hits.binary_search(d).is_ok())
                    .collect(),
            };
            if next.is_empty() {
                return Ok(Vec::new());
            }
            required = Some(next);
        }

        // 除外する文書（フレーズ一致と前方一致の展開語を含む文書）。
        // 各源の後に整列・重複排除して、保持量を文書数以下に抑える。
        let mut excluded_docs: Vec<u32> = Vec::new();
        for pat in &plan.excluded {
            excluded_docs.extend(
                self.phrase_match_doc_idx(pat, Some(bitmap))
                    .into_iter()
                    .map(|(d, _)| d),
            );
            excluded_docs.sort_unstable();
            excluded_docs.dedup();
        }
        // 前方一致の除外は展開語ごとに文書番号を積まず、ビットセットへ直接立てる
        // （保持量を展開語数ではなく文書数に比例させる）。
        let mut excluded_bits: Vec<u64> = vec![0; self.doc_ids.len().div_ceil(64)];
        let mut excluded_stems: BTreeSet<Cow<'_, str>> = BTreeSet::new();
        for c in plan.group.clauses() {
            if let Clause::Exclude(Excluded::Prefix(stem)) = c {
                excluded_stems.insert(self.analysis.stem_prefix(stem.as_str()));
            }
        }
        for stem in excluded_stems {
            for &vi in expansion.indices(&stem) {
                let Some(&(_, tid)) = self.prefix_vocab.get(vi) else {
                    continue;
                };
                if let Some(list) = self.postings.get(tid.0 as usize) {
                    for &(d, _) in list {
                        if bitmap.contains(d) {
                            set_bit(&mut excluded_bits, d);
                        }
                    }
                }
            }
        }
        for &d in &excluded_docs {
            set_bit(&mut excluded_bits, d);
        }
        drop(excluded_docs);

        // 正の語を文字列の辞書順で一意化して TermId へ引く（`score_within` と同じ走査順）。
        let mut words: BTreeMap<String, TermId> = BTreeMap::new();
        for c in plan.group.clauses() {
            match c {
                Clause::Term(t) => {
                    // 索引と同じ設定（同義語・ストップワード）で正規化してから引く。
                    for w in self.analyzed_words(t) {
                        if let Some(tid) = self.terms.lookup(&w) {
                            words.insert(w, tid);
                        }
                    }
                }
                Clause::Prefix(stem) => {
                    for &vi in expansion.indices(&self.analysis.stem_prefix(stem)) {
                        if let Some((t, tid)) = self.prefix_vocab.get(vi) {
                            words.insert((**t).to_owned(), *tid);
                        }
                    }
                }
                Clause::Phrase(p) => {
                    for t in p.tokens() {
                        for w in self.analyzed_words(t) {
                            if let Some(tid) = self.terms.lookup(&w) {
                                words.insert(w, tid);
                            }
                        }
                    }
                }
                Clause::Exclude(_) => {}
            }
        }
        let query_ids: Vec<TermId> = words.values().copied().collect();
        if query_ids.is_empty() {
            return Ok(Vec::new());
        }

        let pass = self.score_pass(&query_ids, ScoreScope::Visible(bitmap), params);
        let acc = pass.acc();
        let mut out: Vec<(u32, f64)> = Vec::with_capacity(pass.touched().len());
        for &doc_idx in pass.touched() {
            let Some(&score) = acc.get(doc_idx as usize) else {
                continue;
            };
            if !score.is_finite() {
                return Err(TextSearchError::InvalidParams {
                    k1: params.k1(),
                    b: params.b(),
                });
            }
            if score <= 0.0 {
                continue;
            }
            if test_bit(&excluded_bits, doc_idx) {
                continue;
            }
            let all_required = required
                .as_ref()
                .is_none_or(|docs| docs.binary_search(&doc_idx).is_ok());
            if all_required {
                out.push((doc_idx, score));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;

/// 文書番号 `d` のビットを立てる（範囲外は無視。ビットセットは文書数で確保済み）。
fn set_bit(bits: &mut [u64], d: u32) {
    if let Some(w) = bits.get_mut((d / 64) as usize) {
        *w |= 1u64 << (d % 64);
    }
}

/// 文書番号 `d` のビットが立っているか。
fn test_bit(bits: &[u64], d: u32) -> bool {
    bits.get((d / 64) as usize)
        .is_some_and(|w| w & (1u64 << (d % 64)) != 0)
}
