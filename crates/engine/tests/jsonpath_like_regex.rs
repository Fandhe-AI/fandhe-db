//! jsonpath `like_regex` の結合テスト（Issue #2677。ポインタ: `docs/spec/04-behavior/`
//! SQL-55・TABLE-25）。
//!
//! - (AC1) `tests/data/jsonpath_like_regex_pg17.tsv`: PostgreSQL 17（`postgres:17-alpine`、
//!   `initdb --locale=C --encoding=UTF8`）で採取した「フラグ × 入力 × パターン」の結果表。
//!   採取式は plpgsql 内で
//!   `jsonb_path_exists(to_jsonb($in::text), ('$ ? (@ like_regex ' || to_jsonb($pat::text)::text
//!   || ' flag "' || $f || '")')::jsonpath)` を実行して例外を捕捉し、`t`／`f`／SQLSTATE を記録した。
//!   入力とパターンの元は `tests/data/regex_pg17.tsv`（全件を `flag ""`、8 件に 1 件を残りの 9 種の
//!   フラグ）と、改行・アンカー・埋め込みオプションに関わる手書きの組。CI では docker を使わず
//!   表を固定値として使う。表の形式: `flag TAB 入力 TAB パターン TAB 結果`（`%`・TAB・改行・
//!   制御文字だけ `%XX`）。
//! - 本 DB が PG と意図的に異なる行は [`intended_difference`] が列挙する
//!   （`docs/design/regex-matcher.md` の「jsonpath `like_regex` への配線」）。
//! - (AC2) エラーコード（`x` は `0A000`、構文誤りは `2201B`、上限超過は `54000`）と固定文言。
//! - (AC3) 病的パターンが上限内で終わること、件数・長さの境界。

use engine::error_format::ClassifiedError;
use engine::json::jsonpath::{
    jsonpath_exists, parse_jsonpath, parse_jsonpath_with_regex_limits, JsonPathEvalError,
    JsonPathEvalOptions, JsonPathParseError, MAX_JSONPATH_LIKE_REGEX,
};
use engine::json::JsonValue;
use engine::regex_limits::RegexLimits;

const PG17_TABLE: &str = include_str!("data/jsonpath_like_regex_pg17.tsv");

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(h, 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// jsonpath の文字列リテラル（JSON 文字列と同じエスケープ）にする。
fn lit(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                o.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn path_text(flag: &str, pattern: &str) -> String {
    format!("$ ? (@ like_regex {} flag \"{flag}\")", lit(pattern))
}

/// 結果を表と同じ表記（`t`／`f`／SQLSTATE）にする。
fn run(flag: &str, input: &str, pattern: &str) -> String {
    let path = match parse_jsonpath(&path_text(flag, pattern)) {
        Ok(p) => p,
        Err(e) => return e.wire_code().to_string(),
    };
    let root = JsonValue::String(input.to_string());
    match jsonpath_exists(&path, &root, JsonPathEvalOptions::new()) {
        Ok(Some(true)) => "t".into(),
        Ok(Some(false)) => "f".into(),
        Ok(None) => "null".into(),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 意図的な差分の種別（`regex_match.rs` の `intended_difference` と同じ分類）。`q` フラグでは
/// パターン全体がリテラルになるため差分は生じない。
fn intended_difference(flag: &str, pattern: &str) -> Option<&'static str> {
    if flag.contains('q') {
        return None;
    }
    const LOOKAROUND: [&str; 5] = ["(?=", "(?!", "(?<=", "(?<!", "(?#"];
    if LOOKAROUND.iter().any(|m| pattern.contains(m)) {
        return Some("lookaround/comment");
    }
    if ["(?b)", "(?e)", "(?m)", "(?n)", "(?p)", "(?w)", "(?x)"]
        .iter()
        .any(|m| pattern.starts_with(m))
    {
        return Some("embedded option");
    }
    if pattern.contains("[[.") || pattern.contains("[[=") {
        return Some("collating element");
    }
    let b = pattern.as_bytes();
    if b.windows(2).any(|w| w[0] == b'\\' && w[1].is_ascii_digit()) {
        return Some("backreference/octal");
    }
    if ["\\x00", "\\xD800", "\\u0000", "\\ud800", "\\U00110000"]
        .iter()
        .any(|m| pattern.contains(m))
    {
        return Some("unrepresentable scalar");
    }
    if pattern.len() >= 140 && pattern.starts_with("((((") {
        return Some("nesting limit");
    }
    if pattern == "(a{255}){255}" {
        return Some("complexity");
    }
    None
}

#[test]
fn pg17_table_matches() {
    let mut mismatches = Vec::new();
    let mut n = 0usize;
    for line in PG17_TABLE.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 4, "bad row: {line}");
        let (flag, input, pattern, pg) = (f[0], dec(f[1]), dec(f[2]), f[3]);
        n += 1;
        let mine = run(flag, &input, &pattern);
        let ok = if mine == pg {
            true
        } else {
            match (mine.as_str(), pg) {
                ("0A000" | "54000", "t" | "f" | "2201B") => {
                    intended_difference(flag, &pattern).is_some()
                }
                (_, "2201B") => intended_difference(flag, &pattern) == Some("complexity"),
                _ => false,
            }
        };
        if !ok {
            mismatches.push(format!(
                "flag={flag:?}\t{input:?}\t{pattern:?}\tpg={pg}\tmine={mine}"
            ));
        }
    }
    assert!(n > 9000, "table too small: {n}");
    assert!(
        mismatches.is_empty(),
        "{} mismatches of {n}:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

// --- (AC2) エラーコードと固定文言 ---

fn parse_code(path: &str) -> &'static str {
    parse_jsonpath(path).expect_err("should fail").wire_code()
}

#[test]
fn parse_time_error_codes() {
    assert_eq!(parse_code(r#"$ ? (@ like_regex "a" flag "x")"#), "0A000");
    assert_eq!(parse_code(r#"$ ? (@ like_regex "(")"#), "2201B");
    assert_eq!(parse_code(r#"$ ? (@ like_regex "(a)\\1")"#), "0A000");
    assert_eq!(parse_code(r#"$ ? (@ like_regex "a" flag "z")"#), "42601");
    assert!(parse_jsonpath(r#"$ ? (@ like_regex "a" flag "qx")"#).is_ok());
    assert!(parse_jsonpath(r#"$ ? (@ like_regex "a" flag "ismqx")"#).is_ok());
    // 長さ上限（既定のハード上限）の境界。
    let max = engine::regex_limits::MAX_REGEX_PATTERN_LEN;
    let at = format!(r#"$ ? (@ like_regex "{}")"#, "a".repeat(max));
    assert!(parse_jsonpath(&at).is_ok());
    let over = format!(r#"$ ? (@ like_regex "{}")"#, "a".repeat(max + 1));
    assert_eq!(parse_code(&over), "54000");
}

#[test]
fn error_messages_are_fixed_and_never_echo_the_pattern() {
    for p in [
        r#"$ ? (@ like_regex "secret(")"#,
        r#"$ ? (@ like_regex "secret" flag "x")"#,
        r#"$ ? (@ like_regex "(secret)\\1")"#,
    ] {
        let e = parse_jsonpath(p).unwrap_err();
        assert!(!e.client_message().contains("secret"), "{p}");
    }
    let many = format!(
        "$ ? ({})",
        vec![r#"@ like_regex "secret""#; MAX_JSONPATH_LIKE_REGEX + 1].join(" || ")
    );
    let e = parse_jsonpath(&many).unwrap_err();
    assert_eq!(e, JsonPathParseError::TooManyRegex);
    assert!(!e.client_message().contains("secret"));
    assert!(!JsonPathEvalError::RegexInputTooLong
        .client_message()
        .contains("secret"));
}

#[test]
fn like_regex_count_boundary() {
    let n = |k: usize| format!("$ ? ({})", vec![r#"@ like_regex "a""#; k].join(" || "));
    assert!(parse_jsonpath(&n(MAX_JSONPATH_LIKE_REGEX)).is_ok());
    assert_eq!(
        parse_jsonpath(&n(MAX_JSONPATH_LIKE_REGEX + 1)).unwrap_err(),
        JsonPathParseError::TooManyRegex
    );
}

#[test]
fn configured_regex_limits_are_applied_at_parse_and_eval() {
    let l = RegexLimits::new(3, 4, 1024 * 1024);
    assert!(parse_jsonpath_with_regex_limits(r#"$ ? (@ like_regex "abc")"#, l).is_ok());
    assert_eq!(
        parse_jsonpath_with_regex_limits(r#"$ ? (@ like_regex "abcd")"#, l)
            .unwrap_err()
            .wire_code(),
        "54000"
    );
    let p = parse_jsonpath_with_regex_limits(r#"$ ? (@ like_regex "a")"#, l).unwrap();
    let ok = JsonValue::String("aaaa".into());
    let long = JsonValue::String("aaaaa".into());
    assert_eq!(
        jsonpath_exists(&p, &ok, JsonPathEvalOptions::new()),
        Ok(Some(true))
    );
    // 入力長上限超過は抑止不可（silent でも返る）。
    for silent in [false, true] {
        let e =
            jsonpath_exists(&p, &long, JsonPathEvalOptions::new().with_silent(silent)).unwrap_err();
        assert_eq!(e, JsonPathEvalError::RegexInputTooLong);
        assert_eq!(e.wire_code(), "54000");
        assert!(!e.is_suppressible());
    }
}

// --- 評価の意味論（lax／strict・非文字列・述語としての位置） ---

fn eval(path: &str, json: &str) -> Result<Option<bool>, JsonPathEvalError> {
    let p = parse_jsonpath(path).unwrap();
    let v = engine::json::parse_json(json).unwrap();
    jsonpath_exists(&p, &v, JsonPathEvalOptions::new())
}

#[test]
fn lax_unwraps_arrays_and_non_strings_are_unknown() {
    // 文字列でない項目は unknown（フィルタを通らない）。
    assert_eq!(eval(r#"$ ? (@ like_regex "1")"#, "1"), Ok(Some(false)));
    // lax は配列を展開し、最初の true で true。
    assert_eq!(
        eval(r#"$ ? (@ like_regex "b")"#, r#"["a","b"]"#),
        Ok(Some(true))
    );
    // strict は配列を展開せず、配列自体は文字列でないので unknown。
    assert_eq!(
        eval(r#"strict $ ? (@ like_regex "b")"#, r#"["a","b"]"#),
        Ok(Some(false))
    );
    // `is unknown` と `!` は Kleene 論理に従う。
    assert_eq!(
        eval(r#"$ ? ((@ like_regex "b") is unknown)"#, "1"),
        Ok(Some(true))
    );
    assert_eq!(eval(r#"$ ? (!(@ like_regex "b"))"#, "1"), Ok(Some(false)));
    assert_eq!(
        eval(r#"$ ? (!(@ like_regex "b"))"#, r#""a""#),
        Ok(Some(true))
    );
}

#[test]
fn top_level_predicate_is_evaluated() {
    let p = parse_jsonpath(r#"$ like_regex "b""#).unwrap();
    let v = JsonValue::String("abc".into());
    let items = engine::json::jsonpath::jsonpath_query(&p, &v, JsonPathEvalOptions::new()).unwrap();
    assert_eq!(items, vec![JsonValue::Bool(true)]);
}

// --- (AC3) 病的パターン ---

#[test]
fn pathological_patterns_finish_within_limits() {
    let input = "a".repeat(4 * 1024 * 1024 - 1);
    let start = std::time::Instant::now();
    for pat in ["(a*)*b", "(a|a)*c", "(a{255}){255}"] {
        let text = format!(r#"$ ? (@ like_regex {})"#, lit(pat));
        let Ok(p) = parse_jsonpath(&text) else {
            continue;
        };
        let v = JsonValue::String(input.clone());
        let r = jsonpath_exists(&p, &v, JsonPathEvalOptions::new());
        // `(a*)*b` と `(a|a)*c` は一致せず、`(a{255}){255}` は a の連続が長いので一致する。
        // いずれも線形時間で終わることが目的で、結果の種別は固定しない。
        let expect_match = pat == "(a{255}){255}";
        assert!(
            matches!(&r, Ok(Some(b)) if *b == expect_match) || r.is_err(),
            "{pat}: {r:?}"
        );
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(60),
        "took {:?}",
        start.elapsed()
    );
}
