//! 正規表現マッチャ（`engine::regex_match`）の拒否・上限・堅牢性の結合テスト
//! （Issue #1786。ポインタ: `docs/spec/04-behavior/` SQL-55）。
//!
//! PG との結果一致（AC1）は `tests/regex_match.rs`。本ファイルは
//! (AC2) 非対応構文の `0A000`・構文誤りの `2201B`、(AC3) 病的パターンの有界終了、
//! 上限（パターン長・入力長・コンパイル後サイズ・入れ子）の境界、panic しないことを検査する。

use engine::error_format::ClassifiedError;
use engine::regex_limits::{RegexLimits, MAX_REGEX_NEST_DEPTH};
use engine::regex_match::{CompiledRegex, RegexCaseMode, RegexError};

fn compile(p: &str) -> Result<CompiledRegex, RegexError> {
    CompiledRegex::compile(p, RegexCaseMode::Sensitive, RegexLimits::default())
}

/// (AC2) SQL-55: 非対応構文は明示エラー `0A000`。
#[test]
fn unsupported_syntax_is_rejected_with_0a000() {
    for p in [
        r"(a)\1",
        r"\1(a)",
        r"(a)(b)\2",
        r"(?=a)",
        r"(?!a)",
        r"(?<=a)b",
        r"(?<!a)b",
        r"[[.a.]]",
        r"[[=a=]]",
        r"(?x)a",
        r"(?n)a",
        r"(?b)a",
        r"(?#c)a",
    ] {
        let e = compile(p).expect_err(p);
        assert_eq!(e.wire_code(), "0A000", "{p}");
        // 文言は固定で、パターンの断片を含まない。
        assert_eq!(
            e.client_message(),
            "regular expression feature is not supported"
        );
    }
}

/// (AC2) SQL-55: PG でも構文誤りになるものは `2201B`。
#[test]
fn syntax_errors_are_2201b() {
    for p in [
        "(", "a)", "*a", "a**", "a{1", "a{256}", "[a", "[z-a]", "\\", "(?i", "***a",
    ] {
        let e = compile(p).expect_err(p);
        assert_eq!(e.wire_code(), "2201B", "{p}");
        assert_eq!(e.client_message(), "invalid regular expression");
    }
}

/// (AC3) SQL-55: 入れ子の量指定など病的パターンが長い入力でも上限内で終了する
/// （線形時間の保証。指数時間ならここで 10 秒を超える）。
#[test]
fn pathological_patterns_terminate() {
    let mut input = "a".repeat(100_000);
    input.push('!');
    for p in [
        "(a*)*b",
        "(a|a)*b",
        "((a+)+)+$",
        "(x+x+)+y",
        "(a|aa)*c",
        "^(a+)+$",
        "(a?){255}a{255}",
    ] {
        let re = compile(p).expect(p);
        let t = std::time::Instant::now();
        let _ = re.is_match(&input).expect(p);
        assert!(t.elapsed().as_secs() < 10, "{p} took {:?}", t.elapsed());
    }
}

/// コンパイル後サイズの上限を下げると、膨れるパターンは `54000` になる。
#[test]
fn compiled_size_limit_rejects_blowup() {
    let l = RegexLimits::new(4096, 1024, 10_000);
    let e = CompiledRegex::compile("(?:(?:a{255}){255}){255}", RegexCaseMode::Sensitive, l)
        .expect_err("blowup");
    assert_eq!(e.wire_code(), "54000");
}

/// パターン長・入力長は境界ちょうどが受理され、+1 が `54000`。
#[test]
fn pattern_and_input_length_boundaries() {
    let l = RegexLimits::new(8, 16, 1_000_000);
    assert!(CompiledRegex::compile("abcdefgh", RegexCaseMode::Sensitive, l).is_ok());
    let e = CompiledRegex::compile("abcdefghi", RegexCaseMode::Sensitive, l).unwrap_err();
    assert_eq!(e.wire_code(), "54000");
    let re = CompiledRegex::compile("x", RegexCaseMode::Sensitive, l).unwrap();
    assert_eq!(re.is_match(&"y".repeat(16)), Ok(false));
    assert_eq!(
        re.is_match(&"y".repeat(17)).unwrap_err().wire_code(),
        "54000"
    );
}

/// 括弧の入れ子は上限ちょうどが受理され、+1 は `54000`（再帰せずに数える）。
#[test]
fn nesting_boundary() {
    let mk = |k: usize| format!("{}a{}", "(".repeat(k), ")".repeat(k));
    assert!(compile(&mk(MAX_REGEX_NEST_DEPTH)).is_ok());
    assert_eq!(
        compile(&mk(MAX_REGEX_NEST_DEPTH + 1))
            .unwrap_err()
            .wire_code(),
        "54000"
    );
    // 閉じていない巨大な入れ子でもスタックを使い切らず拒否される。
    assert!(compile(&"(".repeat(4000)).is_err());
}

/// 無作為・境界入力でも panic しない（トークンの擬似乱数連結を網羅的に試す）。
#[test]
fn no_panic_on_token_soup() {
    const TOK: [&str; 30] = [
        "a",
        "(",
        ")",
        "[",
        "]",
        "[^",
        "-",
        "\\",
        "\\x",
        "\\u",
        "\\U",
        "{",
        "}",
        "{2,3}",
        "*",
        "+",
        "?",
        "|",
        "^",
        "$",
        ".",
        "(?",
        "(?i)",
        "[[:",
        ":]]",
        "***",
        "=",
        "é",
        "\\d",
        "\u{1F600}",
    ];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let len = (next() % 8) as usize;
        let mut p = String::new();
        for _ in 0..len {
            p.push_str(TOK[(next() % TOK.len() as u64) as usize]);
        }
        for case in [RegexCaseMode::Sensitive, RegexCaseMode::AsciiInsensitive] {
            if let Ok(re) = CompiledRegex::compile(&p, case, RegexLimits::default()) {
                let _ = re.is_match("aAé\n😀 ]-\\");
            }
        }
    }
}
