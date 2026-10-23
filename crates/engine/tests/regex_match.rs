//! 正規表現マッチャ（`engine::regex_match`）の結合テスト（Issue #1786。ポインタ:
//! `docs/spec/04-behavior/` SQL-55）。
//!
//! - (AC1) `tests/data/regex_pg17.tsv`: PostgreSQL 17（`postgres:17-alpine`、
//!   `initdb --locale=C --encoding=UTF8`、各式に `COLLATE "C"`）で `psql` により採取した
//!   「入力 × パターン × 演算子」の結果表。本マッチャの結果が一致することを検査する。
//!   採取は 1 行 1 ケースで行い、結果は `t`／`f`／SQLSTATE（構文誤りは `2201B`）。CI では
//!   docker を使わず、表をそのまま固定値として使う（`tests/jsonpath_parse.rs` と同じ流儀）。
//!   表の形式: `演算子 TAB 入力 TAB パターン TAB 結果`。文字列は `%XX`（`%`・TAB・改行・
//!   制御文字）だけをエスケープする。
//! - 本 DB が PG と意図的に異なる行は [`intended_difference`] が列挙する
//!   （`docs/design/regex-matcher.md`）。
//! - (AC2) 非対応構文の `0A000` 拒否・構文誤りの `2201B`。
//! - (AC3) 病的パターンが上限内で終了すること。

use engine::error_format::ClassifiedError;
use engine::regex_limits::RegexLimits;
use engine::regex_match::{CompiledRegex, RegexCaseMode, RegexError};

const PG17_TABLE: &str = include_str!("data/regex_pg17.tsv");

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

fn run(op: &str, input: &str, pattern: &str) -> Result<bool, RegexError> {
    let case = if op == "~*" {
        RegexCaseMode::AsciiInsensitive
    } else {
        RegexCaseMode::Sensitive
    };
    CompiledRegex::compile(pattern, case, RegexLimits::default())?.is_match(input)
}

/// 意図的な差分の種別。PG が受理するが本実装が `0A000`（写像しない構文）または `54000`
/// （入れ子・複雑さの上限）で拒否するもの、PG が複雑さで `2201B` にするが本実装が受理する
/// もの。表の他の行は PG と完全一致しなければならない（`docs/design/regex-matcher.md`）。
fn intended_difference(pattern: &str) -> Option<&'static str> {
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
        let (op, input, pattern, pg) = (f[0], dec(f[1]), dec(f[2]), f[3]);
        n += 1;
        let mine = run(op, &input, &pattern);
        let ok = match (&mine, pg) {
            (Ok(true), "t") | (Ok(false), "f") => true,
            (Err(e), "2201B") if e.wire_code() == "2201B" => true,
            (Err(e), _) => {
                matches!(e.wire_code(), "0A000" | "54000")
                    && intended_difference(&pattern).is_some()
            }
            (Ok(_), "2201B") => intended_difference(&pattern) == Some("complexity"),
            _ => false,
        };
        if !ok {
            mismatches.push(format!(
                "{op}\t{input:?}\t{pattern:?}\tpg={pg}\tmine={mine:?}"
            ));
        }
    }
    assert!(n > 3000, "table too small: {n}");
    assert!(
        mismatches.is_empty(),
        "{} mismatches of {n}:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
