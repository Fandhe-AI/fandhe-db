//! 位置つき転置索引によるフレーズ一致の基本関数（Issue #2114・親 #2111。対象ビヘイビア
//! SEARCH-12 のポインタ: `docs/spec/04-behavior/search.md`、RLS-8 のポインタ: 可視集合の縮約。
//! 設計の根拠は公開済みの `docs/design/fulltext-query-syntax.md` の D4・D7・D10）。
//!
//! 責務境界: 構文木のフレーズ（[`Phrase`]）を「必須トークンと相対位置」の
//! [`PhrasePattern`] へ変換し、[`SparseIndex`] の位置配列から連続して一致する箇所を数える。
//! グループの評価・BM25 との統合・前方一致の展開・`wire_code` への写像は #2115、
//! SQL／NoSQL への結線と EXPLAIN は #2116 の責務で、本モジュールはそれらが呼ぶ下回りである。
//! 近接演算子（D10）は対象外で、位置は隣接の評価にだけ使う。
//!
//! 必須トークン規則（D4）: ASCII 単語はその語、長さ 2 以上の CJK の連続はバイグラムだけ
//! （並び中のユニグラムは使わない）、バイグラムに含まれない CJK 1 文字はユニグラム。
//! ユニグラムが抑止された単独のストップワード文字は「穴」（位置を消費し、任意の 1 単位に
//! 一致する）。先頭と末尾の穴は落とす。
//!
//! 計算量と上限: posting の結合は `O(Σ|postings(t)|)`、文書ごとの位置の照合は
//! `O(L × tf × log tf)`（`L` は必須トークン数）。同じ語の繰り返しで `L × MAX_CORPUS_TOKENS`
//! まで膨らむのを防ぐため、`L` をデータに依存しない定数 [`MAX_PHRASE_REQUIRED_TOKENS`]
//! で拒否する（切り詰めず `Err`。fail-closed）。
//!
//! テナント境界: [`SparseIndex::phrase_matches_within`] は可視集合の文書だけを照合・計数し、
//! 不可視文書は結果にも数にも反映しない。エラーはパターンの中身を含めない。

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use super::text_query::Phrase;
use super::{
    is_ascii_word_char, validate_query_bounds, walk_analyzed_tokens_positioned, DocId, SparseError,
    SparseIndex, TermId, TextAnalysis, VisibleBitmap,
};

/// パターンの必須トークン数の上限（暫定値。D7）。
///
/// 位置の照合コストが `必須トークン数 × tf` に比例するため、データに依存しない定数で
/// 拒否する。`wire_code`（`54000` を想定）への写像と調整可能化は #2115 が担う。
pub const MAX_PHRASE_REQUIRED_TOKENS: usize = 64;

/// [`PhrasePattern`] の作成エラー。内容（フレーズ本文）は含めない。`wire_code` への写像は #2115。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PhraseError {
    /// 入力のバイト長が `MAX_QUERY_BYTES` を超える。
    QueryTooLong {
        /// 入力のバイト長。
        len: usize,
        /// 上限。
        max: usize,
    },
    /// 入力の一意語数が `MAX_QUERY_TERMS` を超える。
    TooManyQueryTerms {
        /// 一意語数。
        unique_terms: usize,
        /// 上限。
        max: usize,
    },
    /// 必須トークン数が [`MAX_PHRASE_REQUIRED_TOKENS`] を超える。
    PatternTooLong {
        /// 必須トークン数。
        len: usize,
        /// 上限。
        max: usize,
    },
}

impl fmt::Display for PhraseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhraseError::QueryTooLong { len, max } => {
                write!(f, "phrase source too long: {len} bytes (max {max})")
            }
            PhraseError::TooManyQueryTerms { unique_terms, max } => {
                write!(
                    f,
                    "phrase has too many unique terms: {unique_terms} (max {max})"
                )
            }
            PhraseError::PatternTooLong { len, max } => {
                write!(f, "phrase has too many required tokens: {len} (max {max})")
            }
        }
    }
}

impl std::error::Error for PhraseError {}

/// 作成済みのフレーズパターン。`(相対オフセット, 語)` の昇順の列で、最初の要素のオフセットは 0。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhrasePattern {
    entries: Vec<(u32, String)>,
    /// 作成時のテキスト処理設定の世代 ID（Issue #2130。既定は 0）。索引の設定と違うときは
    /// 一致を返さない（[`SparseIndex::phrase_matches`] 参照）。
    analysis_id: u64,
}

/// トークンが CJK のバイグラム（先頭が ASCII 単語文字でなく 2 文字）か。
fn is_bigram_token(t: &str) -> bool {
    t.chars().next().is_some_and(|c| !is_ascii_word_char(c)) && t.chars().count() == 2
}

impl PhrasePattern {
    /// 構文木のフレーズから作る。#2115 のグループ評価が呼ぶ入口。
    pub fn from_phrase(phrase: &Phrase) -> Result<Self, PhraseError> {
        Self::compile(phrase.source())
    }

    /// [`Self::from_phrase`] のテキスト処理設定付き版（Issue #2130）。索引と同じ設定で
    /// ストップワード・同義語をかけるので、除かれた語は位置の欠けとして扱われる。
    pub fn from_phrase_with(phrase: &Phrase, analysis: &TextAnalysis) -> Result<Self, PhraseError> {
        Self::compile_with(phrase.source(), analysis)
    }

    /// フレーズの元の部分文字列から作る。入口で長さ・一意語数を再検証する（多層防御）。
    pub fn compile(source: &str) -> Result<Self, PhraseError> {
        Self::compile_with(source, &TextAnalysis::default())
    }

    /// [`Self::compile`] のテキスト処理設定付き版（Issue #2130）。
    pub fn compile_with(source: &str, analysis: &TextAnalysis) -> Result<Self, PhraseError> {
        validate_query_bounds(source).map_err(|e| match e {
            SparseError::QueryTooLong { len, max } => PhraseError::QueryTooLong { len, max },
            SparseError::TooManyQueryTerms { unique_terms, max } => {
                PhraseError::TooManyQueryTerms { unique_terms, max }
            }
            // `validate_query_bounds` が返さない variant。fail-closed で長さ超過扱いにする。
            _ => PhraseError::QueryTooLong {
                len: source.len(),
                max: 0,
            },
        })?;

        let mut toks: Vec<(String, u32)> = Vec::new();
        walk_analyzed_tokens_positioned(source, analysis, |t, _span, pos| {
            toks.push((t.to_owned(), pos));
            true
        });
        // バイグラムが覆う位置（先頭の位置と次の位置）。
        let mut covered: HashSet<u32> = HashSet::new();
        for (t, pos) in &toks {
            if is_bigram_token(t) {
                covered.insert(*pos);
                covered.insert(pos.saturating_add(1));
            }
        }
        let mut required: Vec<(u32, String)> = Vec::new();
        for (t, pos) in toks {
            let is_ascii = t.chars().next().is_some_and(is_ascii_word_char);
            if is_ascii || is_bigram_token(&t) || !covered.contains(&pos) {
                required.push((pos, t));
            }
        }
        required.sort();
        required.dedup();
        if required.len() > MAX_PHRASE_REQUIRED_TOKENS {
            return Err(PhraseError::PatternTooLong {
                len: required.len(),
                max: MAX_PHRASE_REQUIRED_TOKENS,
            });
        }
        // 先頭の必須トークンからの相対位置へ正規化する（先頭の穴は落ちる）。
        let base = required.first().map(|(p, _)| *p).unwrap_or(0);
        let entries = required
            .into_iter()
            .map(|(p, t)| (p.saturating_sub(base), t))
            .collect();
        Ok(Self {
            entries,
            analysis_id: analysis.analysis_id(),
        })
    }

    /// 必須トークン数。0 のパターンは何にも一致しない。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 必須トークンが 0 個か。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(相対オフセット, 語)` の列（オフセット昇順）。
    pub fn entries(&self) -> &[(u32, String)] {
        &self.entries
    }
}

/// フレーズ一致した文書と、その文書内の一致箇所数（重なりを含む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhraseHit {
    doc_id: DocId,
    count: u32,
}

impl PhraseHit {
    /// 一致した文書の ID。
    pub fn doc_id(&self) -> DocId {
        self.doc_id
    }

    /// 一致箇所数（1 以上）。
    pub fn count(&self) -> u32 {
        self.count
    }
}

/// 一意語ごとの posting 走査状態。`pos_off` は `positions[t]` 内の現在の posting 要素の開始。
struct Cursor<'a> {
    postings: &'a [(u32, u32)],
    positions: &'a [u32],
    i: usize,
    pos_off: usize,
}

impl Cursor<'_> {
    fn doc(&self) -> Option<u32> {
        self.postings.get(self.i).map(|&(d, _)| d)
    }

    fn advance(&mut self) {
        if let Some(&(_, tf)) = self.postings.get(self.i) {
            self.pos_off = self.pos_off.saturating_add(tf as usize);
            self.i = self.i.saturating_add(1);
        }
    }

    /// 現在の文書の位置列（昇順）。範囲外は空（fail-closed）。
    fn slice(&self) -> &[u32] {
        let tf = self.postings.get(self.i).map_or(0, |&(_, tf)| tf as usize);
        let end = self.pos_off.saturating_add(tf);
        self.positions.get(self.pos_off..end).unwrap_or(&[])
    }
}

impl SparseIndex {
    /// 全文書を対象にフレーズ一致を数える。結果は `DocId` 昇順。
    ///
    /// 可視集合を持たない呼び出し（テスト・管理用途）向け。テナント境界が必要な経路は
    /// [`SparseIndex::phrase_matches_within`] を使うこと。
    ///
    /// パターンと索引のテキスト処理設定が違う場合は空を返す（Issue #2130。fail-closed。
    /// 設定が違うと位置の欠けや語が食い違い、誤った一致を返しうるため）。
    pub fn phrase_matches(&self, pattern: &PhrasePattern) -> Vec<PhraseHit> {
        self.phrase_hits(pattern, None)
    }

    /// 可視集合（RLS 相当）に限定してフレーズ一致を数える。結果は `DocId` 昇順。
    ///
    /// 不可視の文書は照合も計数もしない。構築時のコーパスに無い ID は無視する。
    pub fn phrase_matches_within(
        &self,
        pattern: &PhrasePattern,
        visible_ids: &BTreeSet<DocId>,
    ) -> Vec<PhraseHit> {
        let visible = VisibleBitmap::build(self, visible_ids);
        if visible.n == 0 {
            return Vec::new();
        }
        self.phrase_hits(pattern, Some(&visible))
    }

    fn phrase_hits(
        &self,
        pattern: &PhrasePattern,
        visible: Option<&VisibleBitmap>,
    ) -> Vec<PhraseHit> {
        if pattern.analysis_id != self.analysis_id() {
            return Vec::new();
        }
        let mut hits: Vec<PhraseHit> = self
            .phrase_match_doc_idx(pattern, visible)
            .into_iter()
            .filter_map(|(idx, count)| {
                self.doc_ids
                    .get(idx as usize)
                    .map(|&doc_id| PhraseHit { doc_id, count })
            })
            .collect();
        hits.sort_by_key(|h| h.doc_id);
        hits
    }

    /// `doc_idx` 空間での中核。`(doc_idx, 一致箇所数)` を `doc_idx` 昇順で返す。
    pub(super) fn phrase_match_doc_idx(
        &self,
        pattern: &PhrasePattern,
        visible: Option<&VisibleBitmap>,
    ) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        if pattern.is_empty() {
            return out;
        }
        // 語を TermId へ解決し、一意語ごとに 1 つのカーソルを持つ。未知語があれば空。
        let mut uniq: Vec<TermId> = Vec::new();
        // (オフセット, カーソル添字)
        let mut entries: Vec<(u32, usize)> = Vec::with_capacity(pattern.len());
        for (off, term) in pattern.entries() {
            let Some(tid) = self.terms.lookup(term) else {
                return out;
            };
            let ci = match uniq.iter().position(|&u| u == tid) {
                Some(ci) => ci,
                None => {
                    uniq.push(tid);
                    uniq.len().saturating_sub(1)
                }
            };
            entries.push((*off, ci));
        }
        let mut cursors: Vec<Cursor<'_>> = Vec::with_capacity(uniq.len());
        for tid in &uniq {
            let (Some(p), Some(q)) = (
                self.postings.get(tid.0 as usize),
                self.positions.get(tid.0 as usize),
            ) else {
                return out;
            };
            cursors.push(Cursor {
                postings: p,
                positions: q,
                i: 0,
                pos_off: 0,
            });
        }

        let mut starts: Vec<u32> = Vec::new();
        'merge: loop {
            // 全カーソルの現在 doc_idx の最大値へ揃える。
            let mut target = 0u32;
            for c in &cursors {
                match c.doc() {
                    Some(d) => target = target.max(d),
                    None => break 'merge,
                }
            }
            let mut aligned = true;
            for c in &mut cursors {
                while let Some(d) = c.doc() {
                    if d >= target {
                        break;
                    }
                    c.advance();
                }
                match c.doc() {
                    Some(d) if d == target => {}
                    Some(_) => aligned = false,
                    None => break 'merge,
                }
            }
            if !aligned {
                continue;
            }
            let is_visible = visible.is_none_or(|v| v.contains(target));
            if is_visible {
                let count = Self::count_in_doc(&cursors, &entries, &mut starts);
                if count > 0 {
                    out.push((target, count));
                }
            }
            for c in &mut cursors {
                c.advance();
            }
        }
        out
    }

    /// 候補文書 1 件での開始位置の数。tf 最小の項目から開始位置の候補を作り、残りで絞る。
    fn count_in_doc(
        cursors: &[Cursor<'_>],
        entries: &[(u32, usize)],
        starts: &mut Vec<u32>,
    ) -> u32 {
        let slice_of = |ci: usize| cursors.get(ci).map_or(&[][..], Cursor::slice);
        let mut order: Vec<(u32, usize)> = entries.to_vec();
        order.sort_by_key(|&(_, ci)| slice_of(ci).len());
        let Some(&(base_off, base_ci)) = order.first() else {
            return 0;
        };
        starts.clear();
        starts.extend(
            slice_of(base_ci)
                .iter()
                .filter_map(|&p| p.checked_sub(base_off)),
        );
        for &(off, ci) in order.iter().skip(1) {
            if starts.is_empty() {
                break;
            }
            let sl = slice_of(ci);
            starts.retain(|&s| {
                s.checked_add(off)
                    .is_some_and(|q| sl.binary_search(&q).is_ok())
            });
        }
        u32::try_from(starts.len()).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests;
