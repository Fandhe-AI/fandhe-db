//! 疎検索（BM25）のテキスト処理設定（ストップワード・同義語。Issue #2130。対象ビヘイビア
//! SEARCH-16 のポインタ: `docs/spec/04-behavior/search.md`、タスクのポインタ: TASK-254）。
//!
//! 責務境界: 運用者が起動時に渡す 2 つの一覧（ストップワード・同義語）を厳格に検証して
//! [`TextAnalysis`] へ変換し、[`super::walk_analyzed_tokens_positioned`] の emit フィルタとして
//! 1 トークンずつ適用する。ファイルの読み込み（上限付き I/O）と CLI フラグは wire-server の
//! `text_analysis_opt` が担い、ここは文字列の検証だけを行う（I/O を持たない）。
//!
//! 処理順（PostgreSQL の辞書チェーンに合わせる）: 同義語を 1 対 1 で 1 回だけ引き（連鎖しない。
//! 置き換え後は最終形）、当たらなかった語だけをストップワードと照合する。除いたトークンの
//! 位置は消費されたままになる（walker が emit の有無によらず位置を進めるため。位置の欠け）。
//!
//! 対称性: [`super::SparseIndex`] は構築時にこの設定を固定し、クエリ側にも同じ設定を使う。
//! 世代の失効は [`TextAnalysis::analysis_id`] を `sql::sparse_cache` が照合して行う。
//!
//! エラー文は種別・行番号・理由だけを持ち、**ファイルの語や内容を含めない**（誤って秘密を
//! 含むファイルを指定された場合に起動ログへ漏らさないため）。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use super::is_ascii_word_char;

/// 1 ファイルの最大バイト数（読み込み側と検証側で共通の唯一の基準）。
pub const MAX_TEXT_ANALYSIS_FILE_BYTES: usize = 1_048_576;
/// 1 ファイルの最大項目数。
pub const MAX_TEXT_ANALYSIS_ENTRIES: usize = 65_536;
/// 1 語の最大バイト数（ASCII に限るため文字数と同じ）。
pub const MAX_TEXT_ANALYSIS_WORD_BYTES: usize = 128;

/// 構築ごとに一意な世代 ID の採番器。一意性にだけ使う（設定の値はここに持たない）。
static NEXT_ANALYSIS_ID: AtomicU64 = AtomicU64::new(1);

/// エラーの対象ファイル種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAnalysisFile {
    /// ストップワード一覧。
    Stopwords,
    /// 同義語辞書。
    Synonyms,
}

/// 検証失敗の理由（語の内容は持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextAnalysisReason {
    /// ファイルのバイト数が上限超過。
    FileTooLarge,
    /// 項目数が上限超過。
    TooManyEntries,
    /// 1 語のバイト数が上限超過。
    WordTooLong,
    /// ASCII 単語（`[A-Za-z0-9_]`）以外の文字を含む。
    InvalidWord,
    /// 1 行の語数が不正（ストップワードは 1 語、同義語は 2 語）。
    WrongFieldCount,
    /// 重複した語。
    DuplicateWord,
    /// 同義語の置き換え先がストップワードに含まれる。
    TargetIsStopword,
}

impl TextAnalysisReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::FileTooLarge => "file too large",
            Self::TooManyEntries => "too many entries",
            Self::WordTooLong => "word too long",
            Self::InvalidWord => "word must be ASCII letters, digits or underscore",
            Self::WrongFieldCount => "wrong number of words on the line",
            Self::DuplicateWord => "duplicate word",
            Self::TargetIsStopword => "synonym target is listed as a stopword",
        }
    }
}

/// 設定の検証エラー。`line` が 0 のときはファイル全体に対するエラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAnalysisError {
    /// 対象ファイル種別。
    pub file: TextAnalysisFile,
    /// 1 始まりの行番号（ファイル全体のエラーは 0）。
    pub line: usize,
    /// 理由。
    pub reason: TextAnalysisReason,
}

impl fmt::Display for TextAnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.file {
            TextAnalysisFile::Stopwords => "stopwords",
            TextAnalysisFile::Synonyms => "synonyms",
        };
        if self.line == 0 {
            write!(f, "{kind} file: {}", self.reason.as_str())
        } else {
            write!(
                f,
                "{kind} file line {}: {}",
                self.line,
                self.reason.as_str()
            )
        }
    }
}

impl std::error::Error for TextAnalysisError {}

/// 疎検索のテキスト処理設定。既定（空）は恒等で、従来のトークン化と完全に同じ結果になる。
#[derive(Debug, Clone, Default)]
pub struct TextAnalysis {
    stopwords: BTreeSet<Box<str>>,
    synonyms: BTreeMap<Box<str>, Box<str>>,
    analysis_id: u64,
}

impl TextAnalysis {
    /// 2 つのファイル内容（UTF-8 文字列）から設定を作る。`None` は「指定なし」。
    ///
    /// 検証順: バイト数 → 行ごとの語数・語長・文字種・重複 → 項目数 → 相互整合。
    /// 項目数は上限を 1 件超えた時点で打ち切る。両方とも項目 0 件なら恒等（ID 0）を返す。
    pub fn from_lists(
        stopwords: Option<&str>,
        synonyms: Option<&str>,
    ) -> Result<Self, TextAnalysisError> {
        let stop_set = match stopwords {
            Some(text) => parse_stopwords(text)?,
            None => BTreeSet::new(),
        };
        let syn_map = match synonyms {
            Some(text) => parse_synonyms(text)?,
            None => BTreeMap::new(),
        };
        // 置き換え先がストップワードなのは設定ミスの可能性が高いので、厳しい側に倒して拒否する。
        if syn_map.values().any(|t| stop_set.contains(t)) {
            return Err(TextAnalysisError {
                file: TextAnalysisFile::Synonyms,
                line: 0,
                reason: TextAnalysisReason::TargetIsStopword,
            });
        }
        let analysis_id = if stop_set.is_empty() && syn_map.is_empty() {
            0
        } else {
            NEXT_ANALYSIS_ID.fetch_add(1, Ordering::Relaxed)
        };
        Ok(Self {
            stopwords: stop_set,
            synonyms: syn_map,
            analysis_id,
        })
    }

    /// 設定が恒等（ストップワードも同義語も無い）か。
    pub fn is_empty(&self) -> bool {
        self.stopwords.is_empty() && self.synonyms.is_empty()
    }

    /// 構築ごとに一意な世代 ID（恒等は 0）。索引・フレーズパターン・キャッシュが照合する。
    pub fn analysis_id(&self) -> u64 {
        self.analysis_id
    }

    /// 小文字化済みの 1 トークンへ設定を適用する。`None` はストップワードとして除く。
    /// 同義語に当たればその置き換え先（最終形）を返し、当たらなければストップワードと照合する。
    pub(crate) fn apply<'a>(&'a self, token: &'a str) -> Option<&'a str> {
        if self.is_empty() {
            return Some(token);
        }
        if let Some(target) = self.synonyms.get(token) {
            return Some(target);
        }
        if self.stopwords.contains(token) {
            return None;
        }
        Some(token)
    }
}

/// 行頭・行末から除く空白（ASCII のみ。全角空白などは語の検証で拒否される）。
fn trim_line(line: &str) -> &str {
    line.trim_matches(|c| c == ' ' || c == '\t' || c == '\r')
}

/// 1 語を検証して ASCII 小文字にそろえる。
fn normalize_word(
    word: &str,
    file: TextAnalysisFile,
    line: usize,
) -> Result<Box<str>, TextAnalysisError> {
    let err = |reason| TextAnalysisError { file, line, reason };
    if word.len() > MAX_TEXT_ANALYSIS_WORD_BYTES {
        return Err(err(TextAnalysisReason::WordTooLong));
    }
    let lowered = word.to_ascii_lowercase();
    if lowered.is_empty() || !lowered.chars().all(is_ascii_word_char) {
        return Err(err(TextAnalysisReason::InvalidWord));
    }
    Ok(lowered.into_boxed_str())
}

fn check_file_size(text: &str, file: TextAnalysisFile) -> Result<(), TextAnalysisError> {
    if text.len() > MAX_TEXT_ANALYSIS_FILE_BYTES {
        return Err(TextAnalysisError {
            file,
            line: 0,
            reason: TextAnalysisReason::FileTooLarge,
        });
    }
    Ok(())
}

fn parse_stopwords(text: &str) -> Result<BTreeSet<Box<str>>, TextAnalysisError> {
    let file = TextAnalysisFile::Stopwords;
    check_file_size(text, file)?;
    let mut set: BTreeSet<Box<str>> = BTreeSet::new();
    for (i, raw) in text.split('\n').enumerate() {
        let line_no = i.saturating_add(1);
        let line = trim_line(raw);
        if line.is_empty() {
            continue;
        }
        let err = |reason| TextAnalysisError {
            file,
            line: line_no,
            reason,
        };
        if line.contains([' ', '\t']) {
            return Err(err(TextAnalysisReason::WrongFieldCount));
        }
        let word = normalize_word(line, file, line_no)?;
        if !set.insert(word) {
            return Err(err(TextAnalysisReason::DuplicateWord));
        }
        if set.len() > MAX_TEXT_ANALYSIS_ENTRIES {
            return Err(err(TextAnalysisReason::TooManyEntries));
        }
    }
    Ok(set)
}

fn parse_synonyms(text: &str) -> Result<BTreeMap<Box<str>, Box<str>>, TextAnalysisError> {
    let file = TextAnalysisFile::Synonyms;
    check_file_size(text, file)?;
    let mut map: BTreeMap<Box<str>, Box<str>> = BTreeMap::new();
    for (i, raw) in text.split('\n').enumerate() {
        let line_no = i.saturating_add(1);
        let line = trim_line(raw);
        if line.is_empty() {
            continue;
        }
        let err = |reason| TextAnalysisError {
            file,
            line: line_no,
            reason,
        };
        let mut fields = line.split([' ', '\t']).filter(|f| !f.is_empty());
        let (Some(from), Some(to), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(err(TextAnalysisReason::WrongFieldCount));
        };
        let from = normalize_word(from, file, line_no)?;
        let to = normalize_word(to, file, line_no)?;
        if map.insert(from, to).is_some() {
            return Err(err(TextAnalysisReason::DuplicateWord));
        }
        if map.len() > MAX_TEXT_ANALYSIS_ENTRIES {
            return Err(err(TextAnalysisReason::TooManyEntries));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(s: Option<&str>, y: Option<&str>) -> Result<TextAnalysis, TextAnalysisError> {
        TextAnalysis::from_lists(s, y)
    }

    fn reason(r: Result<TextAnalysis, TextAnalysisError>) -> (usize, TextAnalysisReason) {
        let e = r.expect_err("must be rejected");
        (e.line, e.reason)
    }

    #[test]
    fn empty_and_none_are_identity() {
        for a in [
            build(None, None),
            build(Some(""), Some("")),
            build(Some("\n\n  \n"), None),
        ] {
            let a = a.expect("ok");
            assert!(a.is_empty());
            assert_eq!(a.analysis_id(), 0);
            assert_eq!(a.apply("the"), Some("the"));
        }
    }

    #[test]
    fn accepts_crlf_blank_lines_and_uppercase() {
        let a = build(
            Some("The\r\n\r\n  AND \r\n"),
            Some("Car AUTOMOBILE\r\nx x\r\n"),
        )
        .expect("ok");
        assert_ne!(a.analysis_id(), 0);
        assert_eq!(a.apply("the"), None);
        assert_eq!(a.apply("and"), None);
        assert_eq!(a.apply("car"), Some("automobile"));
        assert_eq!(a.apply("x"), Some("x"));
        assert_eq!(a.apply("other"), Some("other"));
    }

    #[test]
    fn synonym_is_single_hop_and_final() {
        let a = build(None, Some("a b\nb c\n")).expect("ok");
        assert_eq!(a.apply("a"), Some("b"));
        assert_eq!(a.apply("b"), Some("c"));
    }

    #[test]
    fn synonym_source_wins_over_stopword() {
        let a = build(Some("the"), Some("the thee")).expect("ok");
        assert_eq!(a.apply("the"), Some("thee"));
    }

    #[test]
    fn each_construction_gets_a_distinct_id() {
        let a = build(Some("a"), None).expect("ok");
        let b = build(Some("a"), None).expect("ok");
        assert_ne!(a.analysis_id(), b.analysis_id());
    }

    #[test]
    fn rejects_oversized_file() {
        let big = "a\n".repeat(MAX_TEXT_ANALYSIS_FILE_BYTES);
        assert_eq!(
            reason(build(Some(&big), None)),
            (0, TextAnalysisReason::FileTooLarge)
        );
        assert_eq!(
            reason(build(None, Some(&big))),
            (0, TextAnalysisReason::FileTooLarge)
        );
    }

    #[test]
    fn entry_limit_boundary() {
        let ok: String = (0..MAX_TEXT_ANALYSIS_ENTRIES)
            .map(|i| format!("w{i}\n"))
            .collect();
        assert!(build(Some(&ok), None).is_ok());
        let over = format!("{ok}extra\n");
        assert_eq!(
            reason(build(Some(&over), None)),
            (
                MAX_TEXT_ANALYSIS_ENTRIES + 1,
                TextAnalysisReason::TooManyEntries
            )
        );
    }

    #[test]
    fn word_length_boundary() {
        let ok = "a".repeat(MAX_TEXT_ANALYSIS_WORD_BYTES);
        assert!(build(Some(&ok), None).is_ok());
        let over = "a".repeat(MAX_TEXT_ANALYSIS_WORD_BYTES + 1);
        assert_eq!(
            reason(build(Some(&over), None)),
            (1, TextAnalysisReason::WordTooLong)
        );
    }

    #[test]
    fn rejects_invalid_words() {
        for bad in ["café", "\u{feff}the", "# comment", "the*", "日本"] {
            let r = build(Some(&format!("ok\n{bad}")), None);
            let (line, why) = reason(r);
            assert_eq!(line, 2, "{bad}");
            assert!(
                matches!(
                    why,
                    TextAnalysisReason::InvalidWord | TextAnalysisReason::WrongFieldCount
                ),
                "{bad}"
            );
        }
        assert_eq!(
            reason(build(None, Some("car auto*"))),
            (1, TextAnalysisReason::InvalidWord)
        );
    }

    #[test]
    fn rejects_wrong_field_counts() {
        assert_eq!(
            reason(build(Some("a b"), None)),
            (1, TextAnalysisReason::WrongFieldCount)
        );
        assert_eq!(
            reason(build(None, Some("a"))),
            (1, TextAnalysisReason::WrongFieldCount)
        );
        assert_eq!(
            reason(build(None, Some("ok fine\na b c"))),
            (2, TextAnalysisReason::WrongFieldCount)
        );
    }

    #[test]
    fn rejects_duplicates_and_stopword_target() {
        assert_eq!(
            reason(build(Some("a\nA"), None)),
            (2, TextAnalysisReason::DuplicateWord)
        );
        assert_eq!(
            reason(build(None, Some("a b\na c"))),
            (2, TextAnalysisReason::DuplicateWord)
        );
        assert_eq!(
            reason(build(Some("the"), Some("a the"))),
            (0, TextAnalysisReason::TargetIsStopword)
        );
    }

    #[test]
    fn error_display_never_contains_words() {
        let e = build(Some("secret_token another"), None).expect_err("rejected");
        let s = e.to_string();
        assert!(!s.contains("secret_token"), "{s}");
        assert!(s.contains("line 1"), "{s}");
    }
}
