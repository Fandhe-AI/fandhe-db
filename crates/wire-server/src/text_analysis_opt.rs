//! `--stopwords-file`／`--synonyms-file` CLI 引数の解決（Issue #2130・SEARCH-16 ポインタ。
//! タスクのポインタ: TASK-254）。
//!
//! 責務境界: 運用者が起動時に渡す 2 つのファイルを上限付きで読み、engine の
//! `TextAnalysis::from_lists` へ渡して検証済みの設定へ変換する。`main.rs::run_server` の
//! 引数ループが `PathBuf` を集め、bind・ユーザーストア読込より前にここで確定し、
//! `EngineCore::with_text_analysis` へ 1 回だけ注入する（SQL／NoSQL のセッションからは
//! 変更できない。設定の意味・上限の唯一の基準は engine 側の定数）。
//!
//! 読み込みは `take(上限 + 1)` で上限付きにする（`metadata` で大きさを確かめてから読む方式は
//! TOCTOU になり、`/dev/zero` のような無限のストリームも読み続けてしまうため使わない）。
//! エラー文はフラグ名・理由だけを含め、**ファイルの中身と語は出さない**（誤って秘密を含む
//! ファイルを指定された場合の漏えい防止）。不正な指定は黙って既定に戻さず起動を拒否する
//! （fail-closed）。

use std::io::Read;
use std::path::Path;

use engine::sparse::analysis::MAX_TEXT_ANALYSIS_FILE_BYTES;
use engine::sparse::TextAnalysis;

/// ストップワード一覧のファイルを指定する CLI フラグ。
pub const STOPWORDS_FILE_FLAG: &str = "--stopwords-file";
/// 同義語辞書のファイルを指定する CLI フラグ。
pub const SYNONYMS_FILE_FLAG: &str = "--synonyms-file";

/// 引数ループが集めた生の値（未指定は `None`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TextAnalysisRawFlags<'a> {
    /// `--stopwords-file` のパス。
    pub stopwords: Option<&'a Path>,
    /// `--synonyms-file` のパス。
    pub synonyms: Option<&'a Path>,
    /// `--english-stemmer`（Issue #3050）が指定されたか。
    pub english_stemmer: bool,
}

/// 上限付きで UTF-8 ファイルを読む。失敗時はフラグ名と理由だけの英語文を返す。
fn read_limited(path: &Path, flag: &str) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("{flag}: cannot open file ({})", e.kind()))?;
    let limit = u64::try_from(MAX_TEXT_ANALYSIS_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes: Vec<u8> = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{flag}: cannot read file ({})", e.kind()))?;
    if bytes.len() > MAX_TEXT_ANALYSIS_FILE_BYTES {
        return Err(format!(
            "{flag}: file exceeds {MAX_TEXT_ANALYSIS_FILE_BYTES} bytes"
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{flag}: file is not valid UTF-8"))
}

/// 2 つのフラグから検証済みの [`TextAnalysis`] を作る。どちらも未指定なら恒等（従来どおり）。
pub fn resolve(raw: TextAnalysisRawFlags<'_>) -> Result<TextAnalysis, String> {
    let stopwords = raw
        .stopwords
        .map(|p| read_limited(p, STOPWORDS_FILE_FLAG))
        .transpose()?;
    let synonyms = raw
        .synonyms
        .map(|p| read_limited(p, SYNONYMS_FILE_FLAG))
        .transpose()?;
    let base =
        TextAnalysis::from_lists(stopwords.as_deref(), synonyms.as_deref()).map_err(|e| {
            let flag = match e.file {
                engine::sparse::analysis::TextAnalysisFile::Stopwords => STOPWORDS_FILE_FLAG,
                engine::sparse::analysis::TextAnalysisFile::Synonyms => SYNONYMS_FILE_FLAG,
                engine::sparse::analysis::TextAnalysisFile::EnglishStemmer => {
                    crate::english_stemmer_opt::FLAG
                }
            };
            format!("{flag}: {e}")
        })?;
    if !raw.english_stemmer {
        return Ok(base);
    }
    base.with_english_stemmer()
        .map_err(|e| format!("{}: {e}", crate::english_stemmer_opt::FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_flags_resolve_to_identity() {
        let a = resolve(TextAnalysisRawFlags::default()).expect("ok");
        assert!(a.is_empty());
    }

    #[test]
    fn missing_file_is_rejected_with_flag_name_only() {
        let p = Path::new("/nonexistent-dir-2130/none.txt");
        let e = resolve(TextAnalysisRawFlags {
            stopwords: Some(p),
            synonyms: None,
            english_stemmer: false,
        })
        .expect_err("rejected");
        assert!(e.starts_with(STOPWORDS_FILE_FLAG), "{e}");
        assert!(!e.contains("nonexistent-dir-2130"), "{e}");
    }
}
