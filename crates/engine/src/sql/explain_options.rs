//! `EXPLAIN (<option> [, ...]) <文>` の括弧付きオプション群の解析（Issue #2059・SQL-76 (1) ポインタ）。
//!
//! 責務境界: `core.rs` の構文入口（`parse_tokens_in`）が、文の先頭が `EXPLAIN (` で始まるときに
//! 本モジュールでオプション群をトークン列から取り除き、残り（`EXPLAIN` + 本体）を従来どおりの
//! 解析器へ渡す。本モジュールは「オプション群の構文と閉じた語彙の照合」だけを担い、`FORMAT` 以外の
//! オプションの意味（ANALYZE・VERBOSE 等。兄弟 Issue #2058・#2060 の範囲）は実装しない。
//!
//! fail-closed: 未知の名前は `42601`、既知だが未実装の名前と `FORMAT XML|YAML` は `0A000`、
//! `FORMAT` のその他の値は `22023`。エラー文は固定の英語文で入力値を含めない。括弧の探索は
//! `get()` による有界の走査で、オプション数にも上限を置く。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;

/// オプション数の上限（無制限走査の防止）。
const MAX_EXPLAIN_OPTIONS: usize = 32;

/// `EXPLAIN` の出力形式（`FORMAT` オプション）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplainFormat {
    /// 字下げした複数行テキスト（従来の出力。既定）。
    #[default]
    Text,
    /// PostgreSQL 互換の JSON（1 行 1 列・列型 `json`）。
    Json,
}

/// 名前は既知だが本 Issue では未実装のオプション（`0A000`）。#2058・#2060 が受理に切り替える。
const KNOWN_UNSUPPORTED: &[&str] = &[
    "ANALYZE",
    "ANALYSE",
    "VERBOSE",
    "COSTS",
    "TIMING",
    "SUMMARY",
    "BUFFERS",
    "WAL",
    "SETTINGS",
    "GENERIC_PLAN",
    "SERIALIZE",
    "MEMORY",
];

/// 先頭が `EXPLAIN (` + オプション名の形か（`EXPLAIN (SELECT ..) UNION ..`・`EXPLAIN (WITH ..)` の
/// 括弧は集合演算・部分クエリの括弧であり、オプション群とみなさない）。
pub(crate) fn has_option_list(tokens: &[Token]) -> bool {
    matches!(
        (tokens.first(), tokens.get(1), tokens.get(2)),
        (Some(Token::Ident(head)), Some(Token::Punct('(')), Some(Token::Ident(name)))
            if head.eq_ignore_ascii_case("EXPLAIN")
                && !["WITH", "VALUES", "TABLE"].iter().any(|k| name.eq_ignore_ascii_case(k))
    )
}

/// オプション群を取り除く。`has_option_list` が真のときだけ呼ぶ。戻り値は出力形式と、
/// オプション群を除いたトークン列（`EXPLAIN` + 本体）。
pub(crate) fn split_explain_options(
    tokens: &[Token],
) -> Result<(ExplainFormat, Vec<Token>), SqlSurfaceError> {
    let syntax = |d: &'static str| SqlSurfaceError::unsupported(d);
    let bad_value = || SqlSurfaceError::InvalidParameterValue {
        detail: "unrecognized value for EXPLAIN option FORMAT",
    };
    let mut format: ExplainFormat;
    let mut i = 2usize;
    let mut count = 0usize;
    loop {
        count += 1;
        if count > MAX_EXPLAIN_OPTIONS {
            return Err(syntax("too many EXPLAIN options"));
        }
        let Some(Token::Ident(name)) = tokens.get(i) else {
            return Err(syntax("malformed EXPLAIN option list"));
        };
        i += 1;
        let upper = name.to_ascii_uppercase();
        if upper == "FORMAT" {
            let value = match tokens.get(i) {
                Some(Token::Ident(v)) => v.to_ascii_uppercase(),
                Some(Token::Punct(',' | ')')) | None => {
                    return Err(syntax("EXPLAIN option FORMAT requires a value"));
                }
                Some(_) => return Err(bad_value()),
            };
            i += 1;
            format = match value.as_str() {
                "TEXT" => ExplainFormat::Text,
                "JSON" => ExplainFormat::Json,
                "XML" | "YAML" => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "EXPLAIN FORMAT is not supported except TEXT and JSON",
                    ));
                }
                _ => return Err(bad_value()),
            };
        } else if KNOWN_UNSUPPORTED.contains(&upper.as_str()) {
            return Err(SqlSurfaceError::feature_not_supported(
                "this EXPLAIN option is not supported",
            ));
        } else {
            return Err(syntax("unrecognized EXPLAIN option"));
        }
        match tokens.get(i) {
            Some(Token::Punct(',')) => i += 1,
            Some(Token::Punct(')')) => {
                i += 1;
                break;
            }
            _ => return Err(syntax("malformed EXPLAIN option list")),
        }
    }
    let mut rest = Vec::with_capacity(tokens.len().saturating_sub(i).saturating_add(1));
    rest.extend(tokens.first().cloned());
    rest.extend(tokens.get(i..).unwrap_or(&[]).iter().cloned());
    Ok((format, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn run(sql: &str) -> Result<ExplainFormat, String> {
        let toks = tokenize(sql).unwrap();
        assert!(has_option_list(&toks), "{sql}");
        split_explain_options(&toks)
            .map(|(f, _)| f)
            .map_err(|e| e.wire_code().to_string())
    }

    #[test]
    fn format_values() {
        // SQL-76 (1)
        assert_eq!(
            run("EXPLAIN (FORMAT JSON) SELECT 1"),
            Ok(ExplainFormat::Json)
        );
        assert_eq!(
            run("explain (format text) SELECT 1"),
            Ok(ExplainFormat::Text)
        );
        assert_eq!(
            run("EXPLAIN (FORMAT JSON, FORMAT TEXT) SELECT 1"),
            Ok(ExplainFormat::Text)
        );
        assert_eq!(
            run("EXPLAIN (FORMAT TEXT, FORMAT JSON) SELECT 1"),
            Ok(ExplainFormat::Json)
        );
    }

    #[test]
    fn rest_keeps_explain_and_body() {
        let toks = tokenize("EXPLAIN (FORMAT JSON) SELECT 1").unwrap();
        let (_, rest) = split_explain_options(&toks).unwrap();
        assert_eq!(rest, tokenize("EXPLAIN SELECT 1").unwrap());
    }

    #[test]
    fn rejections() {
        let code = |s: &str| run(s).unwrap_err();
        assert_eq!(code("EXPLAIN (FORMAT XML) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (FORMAT YAML) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (FORMAT CSV) SELECT 1"), "22023");
        assert_eq!(code("EXPLAIN (FORMAT 1) SELECT 1"), "22023");
        assert_eq!(code("EXPLAIN (FORMAT) SELECT 1"), "42601");
        assert_eq!(code("EXPLAIN (FORMAT, FORMAT JSON) SELECT 1"), "42601");
        assert_eq!(code("EXPLAIN (ANALYZE) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (VERBOSE true) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (COSTS off) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (BUFFERS) SELECT 1"), "0A000");
        assert_eq!(code("EXPLAIN (BOGUS) SELECT 1"), "42601");
        assert_eq!(code("EXPLAIN (FORMAT JSON SELECT 1"), "42601");
        assert_eq!(code("EXPLAIN (FORMAT JSON,) SELECT 1"), "42601");
        assert_eq!(code("EXPLAIN (FORMAT JSON"), "42601");
    }

    #[test]
    fn option_count_is_bounded() {
        let many = vec!["FORMAT TEXT"; MAX_EXPLAIN_OPTIONS + 1].join(", ");
        assert_eq!(
            run(&format!("EXPLAIN ({many}) SELECT 1")).unwrap_err(),
            "42601"
        );
        let ok = vec!["FORMAT TEXT"; MAX_EXPLAIN_OPTIONS].join(", ");
        assert!(run(&format!("EXPLAIN ({ok}) SELECT 1")).is_ok());
    }

    #[test]
    fn set_operation_parentheses_are_not_options() {
        for sql in [
            "EXPLAIN (SELECT 1) UNION SELECT 2",
            "EXPLAIN (WITH a AS (SELECT 1) SELECT 1)",
            "EXPLAIN SELECT 1",
        ] {
            assert!(!has_option_list(&tokenize(sql).unwrap()), "{sql}");
        }
    }
}
