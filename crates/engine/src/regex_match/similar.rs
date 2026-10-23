//! SQL 正規表現（`SIMILAR TO`）→ PG ARE の字句変換器（Issue #1788。ポインタ:
//! `docs/spec/04-behavior/` SQL-55）。
//!
//! PostgreSQL 自身が `similar_to_escape()` で行う 2 段構成（SQL 正規表現 → ARE → 正規表現
//! エンジン）の前段にあたる。親モジュール [`super`]（`regex_match`）の公開関数
//! [`super::similar_to_are`] だけが呼ぶ非公開層で、呼び出し元は `sql::allowlist` の
//! `SIMILAR TO` 構文段。出力の ARE は `Expr::RegexMatch` のパターンとして既存の
//! `CompiledRegex::compile`（[`super::translate`] が 1 文字ずつ再検証する）へ渡るので、
//! 未検証の SQL テキストが `regex` 構文へ直接届くことはない（多層防御）。
//!
//! 出力は常に `^(?:` … `)$` で囲む。先頭が `^` で始まるため、director（`***:`）や埋め込み
//! オプションがパターン側から注入されることはなく、文字列全体との一致（両端アンカー）に
//! なる。走査は 1 文字ずつの非再帰ループで、確保量は入力長の定数倍に収まる。
//!
//! 文字ごとの写像は PG 17 の実測表（`tests/data/similar_to_pg17.tsv`）と一致させる。
//! エスケープ文字の直後の `"` は部分抽出用の区切りとして ARE の非貪欲／貪欲の境へ写す。
//! 末尾に孤立したエスケープ文字は PG と同じく出力へ何も足さない。

/// 変換の失敗種別。親モジュールが `RegexError` へ写す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SimilarError {
    /// PG でも構文誤りになる形（後方参照の記法・3 個以上の `"` 区切り。`2201B`）。
    ///
    /// 3 個以上の区切りは PG では別の SQLSTATE（`2200C`）だが、`ErrorClass` の写像表に
    /// 該当行が無いため構文誤りの `2201B` で拒否する（fail-closed。意図的な差分）。
    Invalid,
}

/// ブラケット式の走査状態。
struct Bracket {
    /// 入れ子の深さ（`[[:alpha:]]` の内側の `[` を数える）。0 ならブラケットの外。
    depth: usize,
    /// ブラケット先頭からの位置（`[` を 1 とする）。`]` が先頭直後ならリテラル扱いにする。
    pos: usize,
    /// `[` 直後の否定マーカー `^` を既に読み飛ばしたか。2 個目以降の `^` は通常のメンバー。
    negated: bool,
}

/// SQL 正規表現 `pattern` を ARE へ変換する。
///
/// `escape` が `None` ならエスケープ文字なし（`ESCAPE ''`）。呼び出し元が既定の `\` を渡す。
/// パターン長の上限検査は呼び出し元（親モジュール）が確保の前に済ませている前提。
pub(super) fn convert(pattern: &str, escape: Option<char>) -> Result<String, SimilarError> {
    let chars: Vec<char> = pattern.chars().collect();
    // 各文字は最大 `){1,1}?(`（8 文字）に広がる。囲み 6 文字を加えた上限で先に確保する。
    let cap = chars.len().saturating_mul(8).saturating_add(8);
    let mut out = String::with_capacity(cap.min(64 * 1024));
    out.push_str("^(?:");
    let mut bracket = Bracket {
        depth: 0,
        pos: 0,
        negated: false,
    };
    let mut after_escape = false;
    let mut quotes: u8 = 0;
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i.saturating_add(1)).copied();
        i = i.saturating_add(1);
        if after_escape {
            after_escape = false;
            if c == '"' && bracket.depth == 0 {
                match quotes {
                    0 => out.push_str("){1,1}?("),
                    1 => out.push_str("){1,1}(?:"),
                    _ => return Err(SimilarError::Invalid),
                }
                quotes = quotes.saturating_add(1);
            } else {
                // 後方参照 `\1`〜`\9`: SIMILAR の `(` はすべて非捕捉なので参照先が存在せず、PG も
                // 構文誤りにする。本実装の ARE 変換器が `0A000` にしてしまわないよう、ここで確定する。
                if matches!(c, '1'..='9') {
                    return Err(SimilarError::Invalid);
                }
                out.push('\\');
                out.push(c);
            }
            // ブラケット内のエスケープ対も 1 文字として数える（先頭位置の判定を進める）。
            if bracket.depth > 0 {
                bracket.pos = bracket.pos.saturating_add(1);
            }
            continue;
        }
        if escape == Some(c) {
            after_escape = true;
            continue;
        }
        if bracket.depth > 0 {
            if c == '\\' {
                out.push('\\');
            }
            out.push(c);
            if c == ']' && bracket.pos > 1 {
                bracket.depth = bracket.depth.saturating_sub(1);
            } else if c == '[' && matches!(next, Some(':' | '.' | '=')) {
                bracket.depth = bracket.depth.saturating_add(1);
            }
            // `[` 直後の否定マーカー `^`（1 回だけ）は位置を進めない（`[^]a]` の `]` はリテラル）。
            // 2 個目の `^`（`[^^]`）は通常のメンバーとして位置を進める。
            if c == '^' && bracket.pos == 1 && !bracket.negated {
                bracket.negated = true;
            } else {
                bracket.pos = bracket.pos.saturating_add(1);
            }
            continue;
        }
        match c {
            '[' => {
                out.push('[');
                bracket = Bracket {
                    depth: 1,
                    pos: 1,
                    negated: false,
                };
            }
            '%' => out.push_str(".*"),
            '_' => out.push('.'),
            '(' => out.push_str("(?:"),
            '\\' | '.' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push_str(")$");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(p: &str) -> Result<String, SimilarError> {
        convert(p, Some('\\'))
    }

    #[test]
    fn double_caret_is_negation_plus_member() {
        // `[^^]` は否定マーカー 1 個＋メンバー `^`。直後の `]` でクラスが閉じ、後続の
        // SQL メタ文字は通常どおり変換される。
        assert_eq!(c("[^^]%").unwrap(), "^(?:[^^].*)$");
        assert_eq!(c("[^^]_(a)").unwrap(), "^(?:[^^].(?:a))$");
        // 否定マーカー直後の `]` は従来どおりリテラル。
        assert_eq!(c("[^]a]%").unwrap(), "^(?:[^]a].*)$");
    }

    #[test]
    fn wildcards_and_groups_map_to_are() {
        assert_eq!(c("a%b_c").unwrap(), "^(?:a.*b.c)$");
        assert_eq!(c("(a|b)+").unwrap(), "^(?:(?:a|b)+)$");
        assert_eq!(c("a{2,3}").unwrap(), "^(?:a{2,3})$");
    }

    #[test]
    fn regex_metacharacters_that_are_literal_in_sql_are_escaped() {
        assert_eq!(c("a.b^c$d").unwrap(), "^(?:a\\.b\\^c\\$d)$");
        assert_eq!(c("a\\\\b").unwrap(), "^(?:a\\\\b)$");
    }

    #[test]
    fn escape_char_passes_next_char_through() {
        assert_eq!(c("a\\%b").unwrap(), "^(?:a\\%b)$");
        assert_eq!(convert("a#%b#_", Some('#')).unwrap(), "^(?:a\\%b\\_)$");
        // エスケープ文字以外の `\` は ARE のメタ文字なので二重化される。
        assert_eq!(convert("a\\b", Some('#')).unwrap(), "^(?:a\\\\b)$");
    }

    #[test]
    fn no_escape_when_escape_is_none() {
        assert_eq!(convert("a\\%", None).unwrap(), "^(?:a\\\\.*)$");
    }

    #[test]
    fn trailing_escape_is_dropped() {
        assert_eq!(c("a\\").unwrap(), "^(?:a)$");
    }

    #[test]
    fn backreference_notation_is_invalid() {
        assert_eq!(c("(a)\\1"), Err(SimilarError::Invalid));
        // 数字以外は ARE の定義済みエスケープとして通す（`\d` 等）。
        assert_eq!(c("\\d").unwrap(), "^(?:\\d)$");
    }

    #[test]
    fn escape_double_quote_separators() {
        assert_eq!(
            convert("a#\"b#\"c", Some('#')).unwrap(),
            "^(?:a){1,1}?(b){1,1}(?:c)$"
        );
        assert_eq!(
            convert("a#\"b#\"c#\"d", Some('#')),
            Err(SimilarError::Invalid)
        );
    }

    #[test]
    fn brackets_pass_through_with_backslash_doubled() {
        assert_eq!(c("[a%_(]").unwrap(), "^(?:[a%_(])$");
        assert_eq!(c("[a\\\\b]").unwrap(), "^(?:[a\\\\b])$");
        // 先頭の `]` と `[^]` の `]` はリテラル。ブラケットの後ろの `%` は通常どおり写る。
        assert_eq!(c("[]%]%").unwrap(), "^(?:[]%].*)$");
        assert_eq!(c("[^]%]%").unwrap(), "^(?:[^]%].*)$");
        assert_eq!(c("[]]%").unwrap(), "^(?:[]].*)$");
        // 入れ子の `[:alpha:]` はブラケットを閉じない。
        assert_eq!(c("[[:alpha:]%]%").unwrap(), "^(?:[[:alpha:]%].*)$");
    }

    #[test]
    fn multibyte_escape_char() {
        assert_eq!(
            convert("\u{e9}%\u{e9}_%", Some('\u{e9}')).unwrap(),
            "^(?:\\%\\_.*)$"
        );
    }
}
