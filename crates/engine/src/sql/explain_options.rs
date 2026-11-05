//! `EXPLAIN ( <オプション> [, ...] ) <文>` の括弧付きオプションの構文解釈と意味検証
//! （SQL-76 (1) のポインタ。Issue #2058。設計記録は `docs/design/explain-search-engine-exposure.md`）。
//!
//! 役割分担:
//!
//! - 本モジュールは [`extract`]（構文段。リストの形だけを見て本文を `EXPLAIN <文>` に組み直す）と
//!   [`RawExplainOptions::resolve`]（意味段。名前・値の検証、後の指定を勝たせる畳み込み、未実装の
//!   ゲート）を担う。実行計画の組み立て・表示は `sql::explain` 系の担当で、本モジュールは触れない。
//! - 呼び出し元は `core::EngineCore::parse_tokens_in`（`row_lock::extract_row_locking` と同じ型で、
//!   行数制限句の正規化の後に [`extract`] を 1 回だけ呼び、内側の文の検証が通った後に
//!   [`RawExplainOptions::resolve`] を呼ぶ）と `sql::params`（Prepare 経路で [`has_option_list`] により
//!   `$n` との併用を拒否する）。`fetch_clause::normalize_statement_tokens` には置かない
//!   （PL/pgSQL・ビュー本文・入れ子へ黙って効くのを避ける）。
//! - 前置形 `EXPLAIN ANALYZE [VERBOSE] <文>`・`EXPLAIN VERBOSE <文>`（Issue #2054）も構文段で括弧形の
//!   オプション列へ正規化する（PostgreSQL の旧構文。`ANALYSE` の綴りも可）。
//! - セッション無しの入口（`execute_sql` 系）は `parse_tokens_in` を通らないため、従来どおり
//!   `EXPLAIN` の直後が `(` の形・前置形は許可リストの `42601` になる。
//!
//! 受け渡し先: 受理した [`ExplainOptions`] のうち `ANALYZE`（Issue #2054）は `core::parse_tokens_in` が
//! `ParsedSql::ExplainAnalyze` に運ぶ。`COSTS` は推定値を出さないため出力を変えない。
//! `FORMAT JSON`（#2059）・`VERBOSE`（#2060）・`ANALYZE` なしの `SUMMARY` は未実装で `0A000` のゲートに
//! 掛け、文を実行しない（黙って無視する fail-open を作らない）。
//!
//! untrusted な入力を扱うため、添字アクセス・`unwrap`・`expect` を使わず `get` で走査する。確保量は
//! 入力トークン数で上限が決まり、再帰しない。エラーメッセージは固定文字列で入力をエコーしない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{Keyword, Token};

/// `FORMAT` の出力形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplainFormat {
    /// `FORMAT TEXT`（既定）。
    Text,
    /// `FORMAT JSON`（#2059 が実装するまで `0A000`）。
    Json,
}

/// 畳み込み後のオプション（後の指定が勝つ）。後続 Issue が `ParsedSql` へ運ぶ土台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `verbose`／`costs`／`format` は resolve の検証結果としてのみ使う（運搬は #2059／#2060）
pub(crate) struct ExplainOptions {
    pub(crate) analyze: bool,
    pub(crate) verbose: bool,
    pub(crate) costs: bool,
    pub(crate) timing: Option<bool>,
    pub(crate) summary: Option<bool>,
    pub(crate) format: ExplainFormat,
}

impl Default for ExplainOptions {
    fn default() -> Self {
        Self {
            analyze: false,
            verbose: false,
            costs: true,
            timing: None,
            summary: None,
            format: ExplainFormat::Text,
        }
    }
}

/// オプションの値（構文段の生の形）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RawValue {
    /// 値の省略。
    Omitted,
    /// 識別子（`QuotedBoolWord` を含む）。
    Word(String),
    /// 文字列リテラル。
    Str(String),
    /// 数値（`signed` は前置の `+`／`-` の有無）。
    Number { signed: bool, text: String },
}

/// 構文段が取り出したオプション列（名前と生の値。出現順）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawExplainOptions {
    items: Vec<(String, RawValue)>,
}

fn is_word(tok: Option<&Token>, word: &str) -> bool {
    matches!(tok, Some(Token::Ident(w)) if w.eq_ignore_ascii_case(word))
}

/// `EXPLAIN` の直後（`rest`）がオプションのリストかを判定する。`(` の次が `SELECT`・`(`・`WITH`
/// なら集合演算・CTE の括弧でありオプションではない。
fn rest_has_option_list(rest: &[Token]) -> bool {
    if !matches!(rest.first(), Some(Token::Punct('('))) {
        return false;
    }
    !(matches!(rest.get(1), Some(Token::Keyword(Keyword::Select)))
        || matches!(rest.get(1), Some(Token::Punct('(')))
        || is_word(rest.get(1), "WITH"))
}

/// 前置形の修飾語（`ANALYZE`／`ANALYSE`／`VERBOSE`）か。
fn is_prefix_modifier(tok: Option<&Token>) -> bool {
    is_word(tok, "ANALYZE") || is_word(tok, "ANALYSE") || is_word(tok, "VERBOSE")
}

/// 先頭が `EXPLAIN ( <オプション>` の形か。`sql::params` が Prepare 経路で `$n` との併用を拒否するのに使う。
pub(crate) fn has_option_list(tokens: &[Token]) -> bool {
    is_word(tokens.first(), "EXPLAIN") && rest_has_option_list(tokens.get(1..).unwrap_or(&[]))
}

/// 先頭が `EXPLAIN` で、括弧のオプション列か前置の修飾語（`ANALYZE`／`VERBOSE`）を持つ形か。
/// `sql::params` が Prepare 経路で `$n` との併用を拒否するのに使う（Issue #2054）。
pub(crate) fn has_explain_modifiers(tokens: &[Token]) -> bool {
    has_option_list(tokens)
        || (is_word(tokens.first(), "EXPLAIN") && is_prefix_modifier(tokens.get(1)))
}

fn syntax(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// 構文段。オプションのリストを持たない文は `(tokens, None)` のまま返す。持つ場合は
/// `EXPLAIN` + 本文のトークン列と生のオプション列を返す。誤りは `42601`。
pub(crate) fn extract(
    tokens: Vec<Token>,
) -> Result<(Vec<Token>, Option<RawExplainOptions>), SqlSurfaceError> {
    if !has_option_list(&tokens) {
        if is_word(tokens.first(), "EXPLAIN") && is_prefix_modifier(tokens.get(1)) {
            return extract_prefix_form(tokens);
        }
        return Ok((tokens, None));
    }
    // tokens[0] = EXPLAIN, tokens[1] = '('。
    let mut pos = 2usize;
    let mut items: Vec<(String, RawValue)> = Vec::new();
    loop {
        let name = match tokens.get(pos) {
            Some(Token::Ident(n)) => n.clone(),
            _ => return Err(syntax("invalid EXPLAIN option list")),
        };
        pos += 1;
        let value = match tokens.get(pos) {
            Some(Token::Punct(',')) | Some(Token::Punct(')')) => RawValue::Omitted,
            Some(Token::Ident(w)) | Some(Token::QuotedBoolWord(w)) => {
                pos += 1;
                RawValue::Word(w.clone())
            }
            Some(Token::StringLiteral(s)) => {
                pos += 1;
                RawValue::Str(s.clone())
            }
            Some(Token::Number(n)) => {
                pos += 1;
                RawValue::Number {
                    signed: false,
                    text: n.clone(),
                }
            }
            Some(Token::Punct(c)) if *c == '+' || *c == '-' => match tokens.get(pos + 1) {
                Some(Token::Number(n)) => {
                    pos += 2;
                    RawValue::Number {
                        signed: true,
                        text: n.clone(),
                    }
                }
                _ => return Err(syntax("invalid EXPLAIN option list")),
            },
            _ => return Err(syntax("invalid EXPLAIN option list")),
        };
        items.push((name, value));
        match tokens.get(pos) {
            Some(Token::Punct(',')) => pos += 1,
            Some(Token::Punct(')')) => {
                pos += 1;
                break;
            }
            _ => return Err(syntax("invalid EXPLAIN option list")),
        }
    }
    let body = tokens.get(pos..).unwrap_or(&[]);
    if body.is_empty() || matches!(body.first(), Some(Token::Punct(';'))) {
        return Err(syntax("EXPLAIN requires a statement"));
    }
    // 二重のリスト（`EXPLAIN (a) (b) SELECT`）は受理しない。
    if rest_has_option_list(body) {
        return Err(syntax("invalid EXPLAIN option list"));
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(1));
    if let Some(head) = tokens.first() {
        out.push(head.clone());
    }
    out.extend_from_slice(body);
    Ok((out, Some(RawExplainOptions { items })))
}

/// 前置形 `EXPLAIN [ANALYZE|ANALYSE] [VERBOSE] <文>` を括弧形と同じ生のオプション列へ正規化する
/// （Issue #2054）。前置の直後に括弧のリストが続く形・本文が空の形は `42601`。
fn extract_prefix_form(
    tokens: Vec<Token>,
) -> Result<(Vec<Token>, Option<RawExplainOptions>), SqlSurfaceError> {
    let mut pos = 1usize;
    let mut items: Vec<(String, RawValue)> = Vec::new();
    if is_word(tokens.get(pos), "ANALYZE") || is_word(tokens.get(pos), "ANALYSE") {
        items.push(("ANALYZE".to_string(), RawValue::Omitted));
        pos += 1;
    }
    if is_word(tokens.get(pos), "VERBOSE") {
        items.push(("VERBOSE".to_string(), RawValue::Omitted));
        pos += 1;
    }
    let body = tokens.get(pos..).unwrap_or(&[]);
    if body.is_empty() || matches!(body.first(), Some(Token::Punct(';'))) {
        return Err(syntax("EXPLAIN requires a statement"));
    }
    if rest_has_option_list(body) {
        return Err(syntax("invalid EXPLAIN option list"));
    }
    let mut out = Vec::with_capacity(body.len().saturating_add(1));
    if let Some(head) = tokens.first() {
        out.push(head.clone());
    }
    out.extend_from_slice(body);
    Ok((out, Some(RawExplainOptions { items })))
}

fn parse_bool(value: &RawValue) -> Result<bool, SqlSurfaceError> {
    let invalid = || SqlSurfaceError::InvalidParameterValue {
        detail: "EXPLAIN option requires a Boolean value",
    };
    match value {
        RawValue::Omitted => Ok(true),
        RawValue::Word(w) | RawValue::Str(w) => match w.to_ascii_lowercase().as_str() {
            "true" | "on" => Ok(true),
            "false" | "off" => Ok(false),
            _ => Err(invalid()),
        },
        RawValue::Number {
            signed: false,
            text,
        } => match text.as_str() {
            "1" => Ok(true),
            "0" => Ok(false),
            _ => Err(invalid()),
        },
        RawValue::Number { .. } => Err(invalid()),
    }
}

impl RawExplainOptions {
    /// 意味段。名前・値を出現順に検証し、後の指定を勝たせて畳み込み、未実装のゲートを掛ける。
    pub(crate) fn resolve(&self) -> Result<ExplainOptions, SqlSurfaceError> {
        let mut opts = ExplainOptions::default();
        for (name, value) in &self.items {
            match name.to_ascii_uppercase().as_str() {
                "ANALYZE" | "ANALYSE" => opts.analyze = parse_bool(value)?,
                "VERBOSE" => opts.verbose = parse_bool(value)?,
                "COSTS" => opts.costs = parse_bool(value)?,
                "TIMING" => opts.timing = Some(parse_bool(value)?),
                "SUMMARY" => opts.summary = Some(parse_bool(value)?),
                "FORMAT" => {
                    let word = match value {
                        RawValue::Omitted => {
                            return Err(syntax("EXPLAIN option FORMAT requires a value"));
                        }
                        RawValue::Word(w) | RawValue::Str(w) => w.to_ascii_uppercase(),
                        RawValue::Number { .. } => String::new(),
                    };
                    opts.format = match word.as_str() {
                        "TEXT" => ExplainFormat::Text,
                        "JSON" => ExplainFormat::Json,
                        "XML" | "YAML" => {
                            return Err(SqlSurfaceError::feature_not_supported(
                                "EXPLAIN format is not supported",
                            ));
                        }
                        _ => {
                            return Err(SqlSurfaceError::InvalidParameterValue {
                                detail: "unrecognized value for EXPLAIN option FORMAT",
                            });
                        }
                    };
                }
                "BUFFERS" | "WAL" | "SETTINGS" | "GENERIC_PLAN" | "SERIALIZE" | "MEMORY" => {
                    return Err(SqlSurfaceError::feature_not_supported(
                        "EXPLAIN option is not supported",
                    ));
                }
                _ => return Err(syntax("unrecognized EXPLAIN option")),
            }
        }
        if opts.timing == Some(true) && !opts.analyze {
            return Err(SqlSurfaceError::InvalidParameterValue {
                detail: "EXPLAIN option TIMING requires ANALYZE",
            });
        }
        // 未実装のゲート（後続 Issue が外す）。文は実行しない。
        // `ANALYZE`（Issue #2054）は呼び出し元（`parse_tokens_in`）が対象の種別を見て受理・拒否する。
        // `SUMMARY` の真は実測を伴う `ANALYZE` と併せたときだけ意味を持つ。
        if opts.summary == Some(true) && !opts.analyze {
            return Err(SqlSurfaceError::feature_not_supported(
                "EXPLAIN SUMMARY is not implemented yet",
            ));
        }
        if opts.verbose {
            return Err(SqlSurfaceError::feature_not_supported(
                "EXPLAIN VERBOSE is not implemented yet",
            ));
        }
        if opts.format == ExplainFormat::Json {
            return Err(SqlSurfaceError::feature_not_supported(
                "EXPLAIN FORMAT JSON is not implemented yet",
            ));
        }
        Ok(opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize_with_params(sql).expect("tokenize")
    }

    fn run(sql: &str) -> Result<Option<ExplainOptions>, SqlSurfaceError> {
        let (_, raw) = extract(toks(sql))?;
        raw.map(|r| r.resolve()).transpose()
    }

    fn code(sql: &str) -> &'static str {
        run(sql).expect_err("error").wire_code()
    }

    /// SQL-76 (1): 非対象の括弧（集合演算・CTE）はトークンに触れない。
    #[test]
    fn non_option_parens_untouched() {
        for sql in [
            "EXPLAIN (SELECT k FROM a) UNION SELECT k FROM a",
            "EXPLAIN ((SELECT k FROM a)) UNION SELECT k FROM a",
            "EXPLAIN (WITH x AS (SELECT 1) SELECT 1)",
            "EXPLAIN SELECT 1",
        ] {
            let t = toks(sql);
            let (out, raw) = extract(t.clone()).expect("ok");
            assert!(raw.is_none());
            assert_eq!(out, t);
        }
    }

    /// SQL-76 (1): 受理する形と本文の組み直し。
    #[test]
    fn accepted_forms() {
        for sql in [
            "EXPLAIN (COSTS) SELECT 1",
            "EXPLAIN (costs off) SELECT 1",
            "EXPLAIN (COSTS 0, FORMAT TEXT) SELECT 1",
            "EXPLAIN (FORMAT 'text', SUMMARY false, TIMING off, ANALYZE FALSE) SELECT 1",
            "EXPLAIN (ANALYZE, ANALYSE OFF) SELECT 1",
            "EXPLAIN (FORMAT JSON, FORMAT TEXT) SELECT 1",
            "EXPLAIN (TIMING, TIMING OFF) SELECT 1",
            "EXPLAIN (ANALYZE) SELECT 1",
            "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) SELECT 1",
            "EXPLAIN (ANALYZE, SUMMARY) SELECT 1",
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN ANALYSE SELECT 1",
        ] {
            assert!(run(sql).expect(sql).is_some(), "{sql}");
        }
        let (out, _) = extract(toks("EXPLAIN (COSTS OFF) SELECT 1")).expect("ok");
        assert_eq!(out, toks("EXPLAIN SELECT 1"));
    }

    /// SQL-76 (1): 構文の誤りは 42601。
    #[test]
    fn syntax_errors() {
        for sql in [
            "EXPLAIN () SELECT 1",
            "EXPLAIN (COSTS,) SELECT 1",
            "EXPLAIN (COSTS SELECT 1",
            "EXPLAIN (COSTS)",
            "EXPLAIN (COSTS);",
            "EXPLAIN (COSTS) (COSTS) SELECT 1",
            "EXPLAIN (COSTS $1) SELECT 1",
            "EXPLAIN (COSTS -) SELECT 1",
            "EXPLAIN (1) SELECT 1",
            "EXPLAIN (COSTS ON OFF) SELECT 1",
            "EXPLAIN (FOO) SELECT 1",
            "EXPLAIN (FORMAT) SELECT 1",
        ] {
            let c = match run(sql) {
                Err(e) => e.wire_code(),
                Ok(_) => "ok",
            };
            assert_eq!(c, "42601", "{sql}");
        }
    }

    /// SQL-76 (1): 値の不正は 22023、TIMING は ANALYZE を要する。
    #[test]
    fn invalid_values() {
        for sql in [
            "EXPLAIN (COSTS 2) SELECT 1",
            "EXPLAIN (COSTS 1.0) SELECT 1",
            "EXPLAIN (COSTS -1) SELECT 1",
            "EXPLAIN (COSTS 'maybe') SELECT 1",
            "EXPLAIN (COSTS foo) SELECT 1",
            "EXPLAIN (FORMAT foo) SELECT 1",
            "EXPLAIN (TIMING) SELECT 1",
        ] {
            assert_eq!(code(sql), "22023", "{sql}");
        }
    }

    /// SQL-76 (1): 対象外・未実装は 0A000。エラーは並び順の先頭で決まる。
    #[test]
    fn unsupported_and_gates() {
        for sql in [
            "EXPLAIN (BUFFERS) SELECT 1",
            "EXPLAIN (WAL) SELECT 1",
            "EXPLAIN (SETTINGS) SELECT 1",
            "EXPLAIN (GENERIC_PLAN) SELECT 1",
            "EXPLAIN (SERIALIZE) SELECT 1",
            "EXPLAIN (MEMORY) SELECT 1",
            "EXPLAIN (FORMAT XML) SELECT 1",
            "EXPLAIN (FORMAT YAML) SELECT 1",
            "EXPLAIN (VERBOSE) SELECT 1",
            "EXPLAIN (ANALYZE, VERBOSE) SELECT 1",
            "EXPLAIN ANALYZE VERBOSE SELECT 1",
            "EXPLAIN VERBOSE SELECT 1",
            "EXPLAIN (SUMMARY) SELECT 1",
            "EXPLAIN (FORMAT JSON) SELECT 1",
            "EXPLAIN (ANALYZE OFF, SUMMARY) SELECT 1",
            "EXPLAIN (FORMAT TEXT, FORMAT JSON) SELECT 1",
            "EXPLAIN (BUFFERS, FOO) SELECT 1",
        ] {
            assert_eq!(code(sql), "0A000", "{sql}");
        }
        assert_eq!(code("EXPLAIN (FOO, BUFFERS) SELECT 1"), "42601");
    }

    /// SQL-76 (1)・Issue #2054: 前置形は括弧形と同じ生のオプション列へ正規化される。
    #[test]
    fn prefix_form_normalized() {
        let (out, raw) = extract(toks("EXPLAIN ANALYZE SELECT 1")).expect("ok");
        assert_eq!(out, toks("EXPLAIN SELECT 1"));
        let opts = raw.expect("raw").resolve().expect("resolve");
        assert!(opts.analyze && !opts.verbose);
        let (out, raw) = extract(toks("EXPLAIN ANALYSE VERBOSE SELECT 1")).expect("ok");
        assert_eq!(out, toks("EXPLAIN SELECT 1"));
        assert_eq!(raw.expect("raw").items.len(), 2);
        for sql in [
            "EXPLAIN ANALYZE (COSTS) SELECT 1",
            "EXPLAIN ANALYZE",
            "EXPLAIN VERBOSE;",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
        assert!(has_explain_modifiers(&toks("EXPLAIN ANALYZE SELECT 1")));
        assert!(has_explain_modifiers(&toks("EXPLAIN (COSTS) SELECT 1")));
        assert!(!has_explain_modifiers(&toks("EXPLAIN SELECT 1")));
    }

    /// SQL-76 (1): Prepare 経路の判別。
    #[test]
    fn has_option_list_detection() {
        assert!(has_option_list(&toks("EXPLAIN (COSTS OFF) SELECT 1")));
        assert!(!has_option_list(&toks("EXPLAIN (SELECT 1) UNION SELECT 1")));
        assert!(!has_option_list(&toks("EXPLAIN SELECT 1")));
        assert!(!has_option_list(&toks("SELECT (COSTS)")));
    }
}
