//! `SIMILAR TO` の変換＋照合（`engine::regex_match::similar_to_are` → `CompiledRegex`）の結合
//! テスト（Issue #1788。ポインタ: `docs/spec/04-behavior/` SQL-55）。
//!
//! `tests/data/similar_to_pg17.tsv` は PostgreSQL 17.11（`postgres:17`、
//! `initdb --locale=C --encoding=UTF8`、各式に `COLLATE "C"`）で採取した結果表。
//! 1 行 1 ケースで `入力 TAB パターン TAB ESCAPE TAB 結果` を持つ。ESCAPE 列は `-`（句なし。
//! 既定 `\`）または `+` に続けて ESCAPE の値（`+` のみは `ESCAPE ''`）。結果は `t`／`f`／
//! SQLSTATE。文字列は `%XX`（`%`・TAB・改行・CR）だけをエスケープする。CI では docker を
//! 使わず、表をそのまま固定値として使う（`tests/regex_match.rs` と同じ流儀）。
//!
//! 本 DB が PG と意図的に異なる行は [`intended_difference`] が列挙する
//! （`docs/design/regex-matcher.md`）。
//!
//! 2 文字以上の ESCAPE（`22025`）の検査は構文段の責務なので、本テストでは表の行を対象外とし、
//! `tests/sql55_similar_to.rs` が SQL 表層で検査する。

use engine::error_format::ClassifiedError;
use engine::regex_limits::RegexLimits;
use engine::regex_match::{similar_to_are, CompiledRegex, RegexCaseMode, RegexError};

const PG17_TABLE: &str = include_str!("data/similar_to_pg17.tsv");

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

fn run(input: &str, pattern: &str, escape: Option<char>) -> Result<bool, RegexError> {
    let are = similar_to_are(pattern, escape)?;
    CompiledRegex::compile(&are, RegexCaseMode::Sensitive, RegexLimits::default())?.is_match(input)
}

/// PG が受理するが本実装が `0A000`（写像しない ARE 構文）で拒否する行の種別。
fn intended_difference(pattern: &str) -> Option<&'static str> {
    if pattern.contains("[[.") || pattern.contains("[[=") {
        return Some("collating element");
    }
    if pattern.contains("[[:<:]]") || pattern.contains("[[:>:]]") {
        return Some("bracket word boundary");
    }
    if pattern.contains("\\u0061") || pattern.contains("\\x61") {
        return Some("unrepresentable or unsupported escape");
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
        let (input, pattern, esc_col, pg) = (dec(f[0]), dec(f[1]), f[2], f[3]);
        let escape = if esc_col == "-" {
            Some('\\')
        } else {
            let v = dec(esc_col.strip_prefix('+').expect("escape column"));
            let mut it = v.chars();
            let first = it.next();
            if it.next().is_some() {
                assert_eq!(pg, "22025", "multi-char escape row: {line}");
                continue;
            }
            first
        };
        n += 1;
        let mine = run(&input, &pattern, escape);
        let ok = match (&mine, pg) {
            (Ok(true), "t") | (Ok(false), "f") => true,
            (Err(e), "2201B") if e.wire_code() == "2201B" => true,
            // `"` 区切りが 3 個以上: PG は `2200C`、本実装は写像表に行が無いため `2201B`。
            (Err(e), "2200C") => e.wire_code() == "2201B",
            (Err(e), _) => {
                matches!(e.wire_code(), "0A000" | "54000")
                    && intended_difference(&pattern).is_some()
            }
            _ => false,
        };
        if !ok {
            mismatches.push(format!(
                "{input:?}\t{pattern:?}\tesc={esc_col}\tpg={pg}\tmine={mine:?}"
            ));
        }
    }
    assert!(n > 20_000, "table too small: {n}");
    assert!(
        mismatches.is_empty(),
        "{} mismatches of {n}:\n{}",
        mismatches.len(),
        mismatches
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn over_long_pattern_is_rejected_before_conversion() {
    let long = "a".repeat(engine::regex_limits::MAX_REGEX_PATTERN_LEN + 1);
    let e = similar_to_are(&long, Some('\\')).unwrap_err();
    assert_eq!(e, RegexError::PatternTooLong);
    assert_eq!(e.wire_code(), "54000");
}

#[test]
fn lowered_limit_applies_to_converted_pattern() {
    let l = RegexLimits::new(8, 1024, 1_000_000);
    let ok = similar_to_are("a%", Some('\\')).unwrap();
    assert_eq!(ok, "^(?:a.*)$");
    // 変換後の ARE（`^(?:` … `)$` の 6 文字を含む）が上限 8 を超えれば `54000`。
    assert_eq!(
        CompiledRegex::compile(&ok, RegexCaseMode::Sensitive, l).unwrap_err(),
        RegexError::PatternTooLong
    );
}

#[test]
fn pathological_patterns_terminate() {
    // 入れ子の括弧・大きな量指定でも上限内で完了する（線形時間の `regex`）。
    let p = format!("{}a{}", "(".repeat(100), ")".repeat(100));
    let r = run("a", &p, Some('\\'));
    assert!(matches!(r, Ok(_) | Err(RegexError::TooComplex)));
    let r = run(&"a".repeat(1000), "(a*)*b", Some('\\'));
    assert_eq!(r, Ok(false));
}

#[test]
fn double_caret_bracket_closes_and_following_meta_is_converted() {
    assert_eq!(run("a", "[^^]%", Some('\\')), Ok(true));
    assert_eq!(run("^", "[^^]%", Some('\\')), Ok(false));
    assert_eq!(run("axyz", "[^^]%", Some('\\')), Ok(true));
    assert_eq!(run("a%", "[^^]%", Some('\\')), Ok(true));
}

#[test]
fn converted_pattern_is_checked_against_raw_length_limit() {
    // 変換前 90 バイト・変換後 66 バイト。上限 70 では生パターン長で拒否する。
    let raw = "é%".repeat(30);
    let are = similar_to_are(&raw, Some('é')).unwrap();
    assert!(are.len() < 70 && raw.len() > 70);
    let l = RegexLimits::new(70, 1024, 1_000_000);
    assert_eq!(
        CompiledRegex::compile_converted(&are, raw.len(), RegexCaseMode::Sensitive, l).unwrap_err(),
        RegexError::PatternTooLong
    );
    // 上限 1 でも 1 文字のパターンはラッパー分で弾かれない。
    let one = similar_to_are("a", Some('\\')).unwrap();
    let l1 = RegexLimits::new(1, 1024, 1_000_000);
    assert!(CompiledRegex::compile_converted(&one, 1, RegexCaseMode::Sensitive, l1).is_ok());
}
