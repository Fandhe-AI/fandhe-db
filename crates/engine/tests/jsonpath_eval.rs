//! jsonpath 評価器（Issue #2184・TABLE-25 ポインタ）の結合テスト。
//!
//! `fixtures/jsonpath_pg17_table.inc` の `PG17_TABLE` は PostgreSQL 17（`postgres:17-alpine`）で
//! `jsonb_path_query_array`（`q`）・`jsonb_path_exists`（`e`）・`jsonb_path_match`（`m`）を
//! 実行して得た結果（出力テキストまたは SQLSTATE）を手元で採取してハードコードした表で
//! ある（CI では PostgreSQL も docker も使わない）。採取方法は、`(path, json, vars, silent)`
//! を 1 行 1 ケースで並べ、plpgsql 関数内で関数を呼んで例外を捕捉し `sqlstate` を返す SQL を
//! `psql -At` へ流した。入力は手書きの代表ケース（lax／strict の対・三値論理・項目メソッド・
//! 算術・添字・`.**`・変数）と、文法から固定シードで生成した無作為ケースである。本リポジトリ
//! の評価器は表と一致する（結果列・エラー有無・SQLSTATE）。意図的な差分だけを `DIVERGENCES`
//! に理由キーつきで列挙し、`divergence_reasons_are_documented` で理由の網羅を確かめる。
//!
//! そのほか、lax／strict の対、三値論理、`silent` で抑止されるエラーと抑止されないエラー、
//! ステップ数・深さ上限の境界（上限ちょうどは成功、1 超過は `54000`）、病的なパスの有界停止、
//! 評価器が作る値の上限、エラー文に入力を含めないこと、短い入力の全列挙と無作為入力で
//! panic しないこと、解析器が受理する最悪形が 2 MiB のスタックで溢れないことを確かめる。

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::jsonpath::{
    jsonpath_exists, jsonpath_match, jsonpath_query, parse_jsonpath, JsonPathEvalError,
    JsonPathEvalOptions, MAX_SEQUENCE_ITEMS,
};
use engine::json::{parse_json, write_canonical, JsonValue, MAX_JSON_DEPTH};
use engine::jsonpath_limits::{JsonPathLimits, MAX_JSONPATH_EVAL_DEPTH, MAX_JSONPATH_EVAL_STEPS};

/// `PG17_TABLE` の 1 行 `(path, json, vars, silent, mode, pg17)`。
type PgRow = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    char,
    &'static str,
);
/// `DIVERGENCES` の 1 行 `(path, json, vars, silent, mode, pg17, 本実装, 理由キー)`。
type DivergenceRow = (
    &'static str,
    &'static str,
    &'static str,
    bool,
    char,
    &'static str,
    &'static str,
    &'static str,
);

include!("fixtures/jsonpath_pg17_table.inc");

type Row = (&'static str, &'static str, &'static str, bool, char);

fn canon(v: &JsonValue) -> String {
    let mut s = String::new();
    write_canonical(v, &mut s);
    s
}

/// 評価結果を表と同じ形式（`ERR <sqlstate>`・`true`／`false`／`NULL`・配列テキスト）にする。
fn run_row(row: Row, limits: JsonPathLimits) -> String {
    let (path, json, vars, silent, mode) = row;
    let p = match parse_jsonpath(path) {
        Ok(p) => p,
        Err(e) => return format!("ERR {}", e.wire_code()),
    };
    let doc = parse_json(json).expect("fixture json");
    let vars_v = if vars.is_empty() {
        None
    } else {
        Some(parse_json(vars).expect("fixture vars"))
    };
    let mut o = JsonPathEvalOptions::new()
        .with_silent(silent)
        .with_limits(limits);
    if let Some(v) = vars_v.as_ref() {
        o = o.with_vars(v);
    }
    match mode {
        'e' | 'm' => {
            let r = if mode == 'e' {
                jsonpath_exists(&p, &doc, o)
            } else {
                jsonpath_match(&p, &doc, o)
            };
            match r {
                Ok(Some(b)) => b.to_string(),
                Ok(None) => "NULL".to_string(),
                Err(e) => format!("ERR {}", e.wire_code()),
            }
        }
        _ => match jsonpath_query(&p, &doc, o) {
            Ok(items) => {
                let mut t = String::from("[");
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        t.push(',');
                    }
                    t.push_str(&canon(it));
                }
                t.push(']');
                t
            }
            Err(e) => format!("ERR {}", e.wire_code()),
        },
    }
}

/// 期待値（PostgreSQL の出力テキスト）を比較用の正規形にする。配列テキストは JSON として
/// 読み直して `write_canonical` に通す（空白・区切りの違いを吸収し、数値の綴りは保つ）。
fn normalize_expected(s: &str) -> String {
    if s.starts_with('[') {
        // 数値リテラル長の上限を超える展開（PostgreSQL の巨大な平坦 10 進）は読み直せないため
        // そのまま比較する（差分表の行でのみ現れる）。
        if let Ok(v) = parse_json(s) {
            return canon(&v);
        }
    }
    s.to_string()
}

fn q(path: &str, json: &str) -> Result<Vec<String>, JsonPathEvalError> {
    q_with(path, json, None, false)
}

fn q_with(
    path: &str,
    json: &str,
    vars: Option<&str>,
    silent: bool,
) -> Result<Vec<String>, JsonPathEvalError> {
    let p = parse_jsonpath(path).expect("path parses");
    let doc = parse_json(json).expect("json parses");
    let vars_v = vars.map(|v| parse_json(v).expect("vars parse"));
    let mut o = JsonPathEvalOptions::new().with_silent(silent);
    if let Some(v) = vars_v.as_ref() {
        o = o.with_vars(v);
    }
    jsonpath_query(&p, &doc, o).map(|v| v.iter().map(canon).collect())
}

fn code(path: &str, json: &str) -> &'static str {
    match q(path, json) {
        Ok(_) => "OK",
        Err(e) => e.wire_code(),
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL 17 対照
// ---------------------------------------------------------------------------

#[test]
fn matches_pg17_table() {
    let mut bad = Vec::new();
    for (path, json, vars, silent, mode, pg) in PG17_TABLE {
        let got = run_row(
            (path, json, vars, *silent, *mode),
            JsonPathLimits::default(),
        );
        if normalize_expected(pg) != got {
            bad.push(format!(
                "{path} | {json} | {vars} | silent={silent} | {mode}\n  pg17: {pg}\n  ours: {got}"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} mismatches:\n{}",
        bad.len(),
        bad.join("\n")
    );
    assert!(PG17_TABLE.len() > 1000, "table must stay broad");
}

/// 差分の理由キー → 説明。表の `DIVERGENCES` の理由が全てここにあること。
const DIVERGENCE_REASONS: &[(&str, &str)] = &[
    (
        "keyvalue_id",
        ".keyvalue() の id は PostgreSQL のバイトオフセット由来で再現しない（ルートは 0 で一致）",
    ),
    (
        "unsupported_datetime",
        "datetime 系メソッドは 0A000 で拒否する（対象外。fail-closed）",
    ),
    (
        "numeric_precision_limit",
        "numeric 演算はオペランド 38 桁・結果は i128 かつ scale 38 以下に限る（超過は 22003／22036）",
    ),
    (
        "exponent_spelling",
        "指数表記の数値は綴りのまま保持する（PostgreSQL は平坦な 10 進へ展開する）",
    ),
    ("hex_float", "16 進浮動小数の文字列は未対応（22036）"),
];

#[test]
fn divergences_are_pinned_and_documented() {
    assert!(!DIVERGENCES.is_empty());
    for (path, json, vars, silent, mode, pg, ours, reason) in DIVERGENCES {
        assert!(
            DIVERGENCE_REASONS.iter().any(|(k, _)| k == reason),
            "undocumented reason {reason} for {path}"
        );
        let got = run_row(
            (path, json, vars, *silent, *mode),
            JsonPathLimits::default(),
        );
        assert_eq!(
            normalize_expected(ours),
            got,
            "divergence changed: {path} | {json} (pg17: {pg})"
        );
        assert_ne!(normalize_expected(pg), got, "no longer diverges: {path}");
    }
}

#[test]
fn divergence_reasons_are_documented() {
    for (k, text) in DIVERGENCE_REASONS {
        assert!(!k.is_empty() && !text.is_empty());
    }
}

// ---------------------------------------------------------------------------
// lax／strict の対（結果列・エラー有無・SQLSTATE）
// ---------------------------------------------------------------------------

#[test]
fn lax_vs_strict_pairs() {
    // キー欠落: lax は空列、strict は 2203A。
    assert_eq!(q("lax $.b", r#"{"a":1}"#).unwrap(), Vec::<String>::new());
    assert_eq!(code("strict $.b", r#"{"a":1}"#), "2203A");
    // 非オブジェクトへのメンバーアクセサ。
    assert_eq!(q("lax $.a", "5").unwrap(), Vec::<String>::new());
    assert_eq!(code("strict $.a", "5"), "2203A");
    // 配列への自動展開（lax）と strict の拒否。
    assert_eq!(
        q("lax $.a", r#"[{"a":1},{"a":2}]"#).unwrap(),
        vec!["1", "2"]
    );
    assert_eq!(code("strict $.a", r#"[{"a":1},{"a":2}]"#), "2203A");
    // 非配列への配列アクセサ: lax は単一要素配列として扱い、strict は 22039。
    assert_eq!(q("lax $[0]", "5").unwrap(), vec!["5"]);
    assert_eq!(q("lax $[*]", "5").unwrap(), vec!["5"]);
    assert_eq!(code("strict $[0]", "5"), "22039");
    assert_eq!(code("strict $[*]", "5"), "22039");
    // 添字範囲外: lax は空列、strict は 22033。
    assert_eq!(q("lax $[5]", "[1,2]").unwrap(), Vec::<String>::new());
    assert_eq!(code("strict $[5]", "[1,2]"), "22033");
    assert_eq!(code("strict $[-1]", "[1,2]"), "22033");
    // `.*` は非オブジェクトで strict のみ 2203C。
    assert_eq!(q("lax $.*", "5").unwrap(), Vec::<String>::new());
    assert_eq!(code("strict $.*", "5"), "2203C");
    // `.size()`: lax は非配列を 1、strict は 22039。
    assert_eq!(q("lax $.size()", "5").unwrap(), vec!["1"]);
    assert_eq!(code("strict $.size()", "5"), "22039");
    assert_eq!(q("strict $.size()", "[1,2,3]").unwrap(), vec!["3"]);
    // `.keyvalue()` は非オブジェクトで strict のみ 2203C。lax でも非オブジェクトは 2203C。
    assert_eq!(code("strict $.keyvalue()", "5"), "2203C");
    // 比較の両辺の自動展開（lax）。strict は配列同士を比較できず unknown。
    assert_eq!(q("lax $.a ? (@ == 2)", r#"{"a":[1,2]}"#).unwrap().len(), 1);
    assert_eq!(
        q("strict $.a ? (@ == 2)", r#"{"a":[1,2]}"#).unwrap(),
        Vec::<String>::new()
    );
    // 算術の非数値は lax でも送出される（構造エラーではない）。
    assert_eq!(code("lax $.a + 1", r#"{"a":"x"}"#), "22038");
    assert_eq!(code("lax -$.a", r#"{"a":"x"}"#), "2203B");
    assert_eq!(code("lax $.a / 0", r#"{"a":1}"#), "22012");
}

// ---------------------------------------------------------------------------
// 三値論理
// ---------------------------------------------------------------------------

#[test]
fn three_valued_logic() {
    // 型違いの比較は unknown（フィルタを通過しない）。null との比較は false／true。
    assert_eq!(
        q(r#"$[*] ? (@ == 1)"#, r#"[1,"1",true,null]"#).unwrap(),
        vec!["1"]
    );
    assert_eq!(
        q("$[*] ? (@ != null)", r#"[null,1,"a"]"#).unwrap(),
        vec!["1", r#""a""#]
    );
    assert_eq!(q("$[*] ? (@ == null)", "[null,1]").unwrap(), vec!["null"]);
    // `is unknown`: 述語内のエラー（キー欠落は strict でなくても比較側が空→false）。
    assert_eq!(
        q(
            r#"$[*] ? ((@.b == "x") is unknown)"#,
            r#"[{"b":1},{"b":"x"},{"c":3}]"#
        )
        .unwrap(),
        vec![r#"{"b":1}"#]
    );
    // `!`: unknown は unknown のまま。
    assert_eq!(
        q("$[*] ? (!(@.b == 1))", r#"[{"b":1},{"b":2},{"b":"x"}]"#).unwrap(),
        vec![r#"{"b":2}"#]
    );
    // `&&`: unknown && false は false、unknown && true は unknown。
    assert_eq!(
        q(r#"$ ? (!((@.a == "x") && (@.b == 2)))"#, r#"{"a":1,"b":1}"#).unwrap(),
        vec![r#"{"a":1,"b":1}"#]
    );
    assert_eq!(
        q(r#"$ ? (!((@.a == "x") && (@.b == 1)))"#, r#"{"a":1,"b":1}"#).unwrap(),
        Vec::<String>::new()
    );
    // `||`: unknown || true は true、unknown || false は unknown。
    assert_eq!(
        q(r#"$ ? ((@.a == "x") || (@.b == 1))"#, r#"{"a":1,"b":1}"#)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        q(r#"$ ? (!((@.a == "x") || (@.b == 2)))"#, r#"{"a":1,"b":1}"#).unwrap(),
        Vec::<String>::new()
    );
    // 述語内の算術エラーは両モードで unknown（エラーにならない）。
    assert_eq!(
        q(r#"strict $[*] ? (@.a + 1 > 0)"#, r#"[{"a":"x"},{"a":1}]"#).unwrap(),
        vec![r#"{"a":1}"#]
    );
    assert_eq!(
        q(r#"$ ? (@.b / 0 == 1)"#, r#"{"b":1}"#).unwrap(),
        Vec::<String>::new()
    );
    // 述語を式として使うと true／false／null の値になる。
    assert_eq!(q("$ > 1", "5").unwrap(), vec!["true"]);
    assert_eq!(q("$ > 1", r#""a""#).unwrap(), vec!["null"]);
}

#[test]
fn exists_and_match_apis() {
    let run = |path: &str, json: &str, silent: bool| {
        let p = parse_jsonpath(path).unwrap();
        let d = parse_json(json).unwrap();
        let o = JsonPathEvalOptions::new().with_silent(silent);
        (jsonpath_exists(&p, &d, o), jsonpath_match(&p, &d, o))
    };
    let (e, m) = run("$.a", r#"{"a":1}"#, false);
    assert_eq!(e, Ok(Some(true)));
    assert_eq!(m, Err(JsonPathEvalError::BooleanResultRequired));
    let (e, m) = run("$.a", r#"{"a":1}"#, true);
    assert_eq!(e, Ok(Some(true)));
    assert_eq!(m, Ok(None));
    let (e, _) = run("strict $.x", r#"{"a":1}"#, false);
    assert_eq!(e, Err(JsonPathEvalError::MemberNotFound));
    let (e, _) = run("strict $.x", r#"{"a":1}"#, true);
    assert_eq!(e, Ok(None));
    let (e, m) = run("$.a == 1", r#"{"a":1}"#, false);
    assert_eq!(e, Ok(Some(true)));
    assert_eq!(m, Ok(Some(true)));
    let (_, m) = run("$.a == 1", r#"{"a":"x"}"#, false);
    assert_eq!(m, Ok(None));
}

// ---------------------------------------------------------------------------
// silent と抑止できないエラー
// ---------------------------------------------------------------------------

#[test]
fn silent_suppresses_only_suppressible_errors() {
    // 抑止される: 構造エラー・算術エラー・項目メソッドの型違い。
    for (path, json) in [
        ("strict $.b", r#"{"a":1}"#),
        ("strict $[5]", "[1]"),
        (r#"$.a + 1"#, r#"{"a":"x"}"#),
        ("$.a / 0", r#"{"a":1}"#),
        ("$.a.double()", r#"{"a":"x"}"#),
        ("$.a.decimal(0)", r#"{"a":1}"#),
    ] {
        assert!(q(path, json).is_err(), "{path} must error when not silent");
        assert_eq!(
            q_with(path, json, None, true).unwrap(),
            Vec::<String>::new(),
            "{path} must be suppressed"
        );
    }
    // 抑止されない: 未定義変数 42704・vars 非オブジェクト 22023・未対応 0A000。
    let e = q_with("$x", "null", None, true).unwrap_err();
    assert_eq!(e, JsonPathEvalError::UndefinedVariable);
    assert_eq!(e.wire_code(), "42704");
    assert!(!e.is_suppressible());
    let e = q_with("$x", "null", Some("[1]"), true).unwrap_err();
    assert_eq!(e, JsonPathEvalError::VarsNotObject);
    assert_eq!(e.wire_code(), "22023");
    let e = q_with("$.a.date()", r#"{"a":"2020-01-01"}"#, None, true).unwrap_err();
    assert_eq!(e, JsonPathEvalError::Unsupported);
    assert_eq!(e.wire_code(), "0A000");
    // 述語内の未定義変数も unknown にならず送出される。
    assert_eq!(
        q_with("$ ? ($x == 1)", r#"{"b":1}"#, None, false).unwrap_err(),
        JsonPathEvalError::UndefinedVariable
    );
    // 上限超過は silent でも送出される。
    let p = parse_jsonpath("$[*] ? (@ == $[*])").unwrap();
    let doc = parse_json(&format!(
        "[{}]",
        (0..3000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ))
    .unwrap();
    let o = JsonPathEvalOptions::new()
        .with_silent(true)
        .with_limits(JsonPathLimits::new(1000, 1024));
    assert_eq!(
        jsonpath_query(&p, &doc, o).unwrap_err(),
        JsonPathEvalError::StepLimitExceeded
    );
    // silent で抑止されたエラーまでに得た項目は返る（PostgreSQL と同じ）。
    assert_eq!(
        q_with("strict $[*].a", r#"[{"a":1},{"b":2},{"a":3}]"#, None, true).unwrap(),
        vec!["1"]
    );
}

#[test]
fn any_path_keeps_descendant_results_when_level_zero_is_suppressed() {
    // PostgreSQL は 0 階層目で抑止されたエラー（strict の配列への `.double()`）を、子孫の走査
    // 結果で上書きする。非 silent では 0 階層目のエラーがそのまま送出される。
    assert_eq!(
        q_with("strict $.**.double()", "[3]", None, true).unwrap(),
        vec!["3"]
    );
    assert_eq!(code("strict $.**.double()", "[3]"), "22036");
    // 走査できない値（スカラー）ではエラーが残り、silent では空列になる。
    assert_eq!(
        q_with("strict $.**.double()", r#""x""#, None, true).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn error_classes_and_suppressibility() {
    use JsonPathEvalError as E;
    let table: &[(E, &str, ErrorClass, bool)] = &[
        (
            E::MemberNotFound,
            "2203A",
            ErrorClass::JsonMemberNotFound,
            true,
        ),
        (
            E::ObjectNotFound,
            "2203C",
            ErrorClass::JsonObjectNotFound,
            true,
        ),
        (
            E::ArrayNotFound,
            "22039",
            ErrorClass::JsonArrayNotFound,
            true,
        ),
        (
            E::SubscriptOutOfBounds,
            "22033",
            ErrorClass::JsonSubscriptInvalid,
            true,
        ),
        (
            E::SubscriptNotNumeric,
            "22033",
            ErrorClass::JsonSubscriptInvalid,
            true,
        ),
        (
            E::SubscriptOutOfRange,
            "22033",
            ErrorClass::JsonSubscriptInvalid,
            true,
        ),
        (
            E::NumberRequired,
            "2203B",
            ErrorClass::JsonNumberNotFound,
            true,
        ),
        (
            E::SingletonRequired,
            "22038",
            ErrorClass::SingletonJsonItemRequired,
            true,
        ),
        (
            E::BooleanResultRequired,
            "22038",
            ErrorClass::SingletonJsonItemRequired,
            false,
        ),
        (
            E::NonNumericItem,
            "22036",
            ErrorClass::NonNumericJsonItem,
            true,
        ),
        (E::DivisionByZero, "22012", ErrorClass::DivisionByZero, true),
        (
            E::NumericOutOfRange,
            "22003",
            ErrorClass::NumericOutOfRange,
            true,
        ),
        (
            E::UndefinedVariable,
            "42704",
            ErrorClass::UndefinedObject,
            false,
        ),
        (
            E::VarsNotObject,
            "22023",
            ErrorClass::InvalidParameterValue,
            false,
        ),
        (
            E::InvalidMethodArgument,
            "22023",
            ErrorClass::InvalidParameterValue,
            true,
        ),
        (
            E::StepLimitExceeded,
            "54000",
            ErrorClass::PayloadTooLarge,
            false,
        ),
        (
            E::DepthLimitExceeded,
            "54000",
            ErrorClass::PayloadTooLarge,
            false,
        ),
        (
            E::ResultTooLarge,
            "54000",
            ErrorClass::PayloadTooLarge,
            false,
        ),
        (
            E::Unsupported,
            "0A000",
            ErrorClass::FeatureNotSupported,
            false,
        ),
        (E::Internal, "XX000", ErrorClass::InternalError, false),
    ];
    for (e, wire, class, suppressible) in table {
        assert_eq!(e.wire_code(), *wire, "{e:?}");
        assert_eq!(e.error_class(), *class, "{e:?}");
        assert_eq!(e.is_suppressible(), *suppressible, "{e:?}");
        assert!(ErrorClass::from_wire_code(wire).is_some());
    }
}

// ---------------------------------------------------------------------------
// 上限（ステップ数・再帰深さ）
// ---------------------------------------------------------------------------

fn eval_with(path: &str, json: &str, limits: JsonPathLimits) -> Result<usize, JsonPathEvalError> {
    let p = parse_jsonpath(path).expect("path parses");
    let d = parse_json(json).expect("json parses");
    jsonpath_query(&p, &d, JsonPathEvalOptions::new().with_limits(limits)).map(|v| v.len())
}

/// 成功する最小のステップ上限を二分探索で求める。
fn min_steps(path: &str, json: &str) -> u64 {
    let (mut lo, mut hi) = (1u64, MAX_JSONPATH_EVAL_STEPS);
    assert!(eval_with(path, json, JsonPathLimits::new(hi, 1024)).is_ok());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if eval_with(path, json, JsonPathLimits::new(mid, 1024)).is_ok() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

fn min_depth(path: &str, json: &str) -> usize {
    let (mut lo, mut hi) = (1usize, MAX_JSONPATH_EVAL_DEPTH);
    assert!(eval_with(path, json, JsonPathLimits::new(MAX_JSONPATH_EVAL_STEPS, hi)).is_ok());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if eval_with(
            path,
            json,
            JsonPathLimits::new(MAX_JSONPATH_EVAL_STEPS, mid),
        )
        .is_ok()
        {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[test]
fn step_limit_boundary_is_exact() {
    for (path, json) in [
        ("$.a.b", r#"{"a":{"b":1}}"#),
        ("$[*] ? (@ > 1)", "[1,2,3]"),
        ("$.**.a", r#"{"a":{"a":1},"b":[{"a":2}]}"#),
        ("$.a + $.b * 2", r#"{"a":1,"b":2}"#),
    ] {
        let n = min_steps(path, json);
        assert!(n >= 2, "{path}: trivial step count {n}");
        assert!(eval_with(path, json, JsonPathLimits::new(n, 1024)).is_ok());
        let err = eval_with(path, json, JsonPathLimits::new(n - 1, 1024)).unwrap_err();
        assert_eq!(err, JsonPathEvalError::StepLimitExceeded, "{path}");
        assert_eq!(err.wire_code(), "54000");
    }
}

#[test]
fn depth_limit_boundary_is_exact() {
    for (path, json) in [
        ("$.a.b.c", r#"{"a":{"b":{"c":1}}}"#),
        ("$ ? (@.a > 0 || @.b > 0)", r#"{"a":1}"#),
        ("1 + 2 + 3 + 4", "null"),
    ] {
        let n = min_depth(path, json);
        assert!(n >= 2, "{path}: trivial depth {n}");
        assert!(eval_with(path, json, JsonPathLimits::new(MAX_JSONPATH_EVAL_STEPS, n)).is_ok());
        let err = eval_with(
            path,
            json,
            JsonPathLimits::new(MAX_JSONPATH_EVAL_STEPS, n - 1),
        )
        .unwrap_err();
        assert_eq!(err, JsonPathEvalError::DepthLimitExceeded, "{path}");
        assert_eq!(err.wire_code(), "54000");
    }
}

#[test]
fn default_limits_accept_every_parsed_shape_within_hard_limits() {
    // 解析器の高さ上限ちょうどの左結合連鎖は、既定の深さ上限で評価できる。
    let path = format!("1{}", " + 1".repeat(1022));
    assert_eq!(eval_with(&path, "null", JsonPathLimits::default()), Ok(1));
}

fn wide_array(n: usize) -> String {
    format!(
        "[{}]",
        (0..n).map(|i| i.to_string()).collect::<Vec<_>>().join(",")
    )
}

#[test]
fn pathological_paths_stop_with_54000_in_bounded_time() {
    let start = std::time::Instant::now();
    // 組合せ爆発: `$.**` の連鎖を深く広い文書に。
    let doc = r#"{"a":{"a":{"a":{"a":{"a":{"a":[1,2,3,{"a":[1,2,3,4,5,6,7,8,9]}]}}}}}}"#;
    let wide = format!(
        "{{\"a\":{},\"b\":{},\"c\":{}}}",
        wide_array(20_000),
        wide_array(20_000),
        wide_array(20_000)
    );
    for (path, json) in [
        ("$.**.**.**.**.**.**", wide.as_str()),
        ("$.**.**.**.**.**.**.**.**", doc),
        ("$[*] ? (@ == $[*])", wide_array(60_000).leak()),
        ("$[*] ? (exists($[*]))", wide_array(60_000).leak()),
        ("$.a[*] ? (@ == $.b[*] || @ == $.c[*])", wide.as_str()),
        ("$[*] ? (@ < $[*] + $[*])", wide_array(5_000).leak()),
    ] {
        let r = eval_with(path, json, JsonPathLimits::default());
        // 組合せ爆発は `54000`、爆発しない形は成功のどちらか（panic・無限時間は不可）。
        if let Err(e) = r {
            assert_eq!(e.wire_code(), "54000", "{path}");
        }
    }
    // 少なくとも次の 2 つは必ず上限で止まる。
    assert_eq!(
        eval_with(
            "$[*] ? (@ == $[*])",
            &wide_array(60_000),
            JsonPathLimits::default()
        ),
        Err(JsonPathEvalError::StepLimitExceeded)
    );
    assert_eq!(
        eval_with(
            "$.**.**.**.**.**.**",
            doc,
            JsonPathLimits::new(10_000, 1024)
        ),
        Err(JsonPathEvalError::StepLimitExceeded)
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(120),
        "pathological evaluation took {:?}",
        start.elapsed()
    );
}

#[test]
fn sequence_length_is_bounded() {
    const { assert!(MAX_SEQUENCE_ITEMS >= 65_536) };
}

// ---------------------------------------------------------------------------
// 評価器が作る値の上限
// ---------------------------------------------------------------------------

fn nested_object(depth: usize) -> JsonValue {
    let mut v = JsonValue::Number(engine::json::JsonNumber::PosInt(1));
    for _ in 0..depth {
        let mut m = std::collections::BTreeMap::new();
        m.insert("a".to_string(), v);
        v = JsonValue::Object(m);
    }
    v
}

#[test]
fn keyvalue_respects_json_depth_limit() {
    let run = |inner_depth: usize| {
        let mut m = std::collections::BTreeMap::new();
        m.insert("k".to_string(), nested_object(inner_depth));
        let doc = JsonValue::Object(m);
        let p = parse_jsonpath("$.keyvalue()").unwrap();
        jsonpath_query(&p, &doc, JsonPathEvalOptions::new())
    };
    // 値の深さ + 1 が上限ちょうどなら成功し、1 超えたら 54000。
    assert!(run(MAX_JSON_DEPTH - 1).is_ok());
    let e = run(MAX_JSON_DEPTH).unwrap_err();
    assert_eq!(e, JsonPathEvalError::ResultTooLarge);
    assert_eq!(e.wire_code(), "54000");
}

// ---------------------------------------------------------------------------
// エラー文に入力の断片を含めない
// ---------------------------------------------------------------------------

#[test]
fn error_messages_do_not_echo_input() {
    let doc = r#"{"SECRETKEY":"SECRETVALUE","n":"SECRETSTR"}"#;
    let cases: &[(&str, Option<&str>)] = &[
        ("strict $.SECRETMISSING", None),
        ("strict $.SECRETKEY.SECRETMISSING", None),
        ("$.n + 1", None),
        ("-$.n", None),
        ("$.n.double()", None),
        ("$.n.bigint()", None),
        ("$.SECRETKEY.abs()", None),
        ("$ ? ($SECRETVAR == 1)", None),
        ("$ ? ($SECRETVAR == 1)", Some(r#"{"other":1}"#)),
        ("$x", Some(r#"[1]"#)),
        ("strict $[SECRETIDX]", None),
        ("strict $.SECRETKEY[0]", None),
        ("$.SECRETKEY.date()", None),
        ("strict $.n.size()", None),
    ];
    for (path, vars) in cases {
        let Ok(p) = parse_jsonpath(path) else {
            continue;
        };
        let d = parse_json(doc).unwrap();
        let vars_v = vars.map(|v| parse_json(v).unwrap());
        let mut o = JsonPathEvalOptions::new();
        if let Some(v) = vars_v.as_ref() {
            o = o.with_vars(v);
        }
        if let Err(e) = jsonpath_query(&p, &d, o) {
            let msg = e.client_message();
            for frag in ["SECRET", "MISSING", "VAR", "IDX"] {
                assert!(
                    !msg.contains(frag) && !format!("{e:?}").contains(frag),
                    "{path}: message leaks input: {msg}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 順序・個別の意味論
// ---------------------------------------------------------------------------

#[test]
fn object_members_follow_jsonb_order() {
    // jsonb は短いキーから順に保持する（`b`〔1 バイト〕→ `aa`〔2 バイト〕）。
    assert_eq!(
        q("$.*", r#"{"aa":2,"b":1,"c":3}"#).unwrap(),
        vec!["1", "3", "2"]
    );
    assert_eq!(
        q("$.keyvalue().key", r#"{"aa":2,"b":1}"#).unwrap(),
        vec![r#""b""#, r#""aa""#]
    );
}

#[test]
fn keyvalue_ids_are_zero_for_root_object() {
    assert_eq!(
        q("$.keyvalue().id", r#"{"a":1,"b":2}"#).unwrap(),
        vec!["0", "0"]
    );
    // 非ルートのオブジェクトは文書内で一意（0 以外）。
    let ids = q("$.a.keyvalue().id", r#"{"a":{"x":1}}"#).unwrap();
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], "0");
    // 再適用は未対応（黙って空にしない）。
    assert_eq!(
        q("$.keyvalue().keyvalue()", r#"{"a":1}"#).unwrap_err(),
        JsonPathEvalError::Unsupported
    );
}

#[test]
fn variables_and_current_item() {
    assert_eq!(
        q_with("$x", "null", Some(r#"{"x":[1,2]}"#), false).unwrap(),
        vec!["[1,2]"]
    );
    assert_eq!(
        q_with("$[*] ? (@ == $x)", "[1,2,3]", Some(r#"{"x":2}"#), false).unwrap(),
        vec!["2"]
    );
    assert_eq!(
        q_with(r#"$"a b""#, "null", Some(r#"{"a b":1}"#), false).unwrap(),
        vec!["1"]
    );
    assert_eq!(
        q_with("$x", "null", None, false).unwrap_err(),
        JsonPathEvalError::UndefinedVariable
    );
}

#[test]
fn numeric_arithmetic_is_exact_and_bounded() {
    assert_eq!(q("$.a + $.b", r#"{"a":0.1,"b":0.2}"#).unwrap(), vec!["0.3"]);
    assert_eq!(
        q("$.a / $.b", r#"{"a":1,"b":3}"#).unwrap(),
        vec!["0.33333333333333333333"]
    );
    assert_eq!(q("$.a % $.b", r#"{"a":-7,"b":3}"#).unwrap(), vec!["-1"]);
    assert_eq!(
        q("$.a / $.b", r#"{"a":1,"b":0}"#).unwrap_err().wire_code(),
        "22012"
    );
    // 38 桁ちょうどは成功、結果が表現範囲を超えれば 22003。
    let max38 = "99999999999999999999999999999999999999";
    assert_eq!(
        q("$.a + 1", &format!(r#"{{"a":{max38}}}"#)).unwrap(),
        vec!["100000000000000000000000000000000000000"]
    );
    assert_eq!(
        q("$.a * $.a", &format!(r#"{{"a":{max38}}}"#))
            .unwrap_err()
            .wire_code(),
        "22003"
    );
    // 比較は f64 を経由しない（2^53 超の整数・桁違いの小数）。
    assert_eq!(
        q("$ ? (@ == 9007199254740993)", "9007199254740993")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        q("$ ? (@ > 9007199254740992)", "9007199254740993")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(q("$ ? (@ == 1.0)", "1").unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// panic しないこと
// ---------------------------------------------------------------------------

#[test]
fn short_inputs_never_panic() {
    let alphabet = [
        "$",
        "@",
        ".",
        "a",
        "[",
        "]",
        "*",
        "0",
        "1",
        "last",
        "?",
        "(",
        ")",
        "==",
        "<",
        "&&",
        "!",
        "+",
        "\"s\"",
        "$x",
        "**",
        "{",
        "}",
        " to ",
        ",",
        "-",
        "exists",
        "starts with",
    ];
    let docs = [
        "null",
        "1",
        r#""s""#,
        "[1,[2],{}]",
        r#"{"a":{"a":[1,2]},"s":"s"}"#,
    ];
    let parsed: Vec<_> = docs.iter().map(|d| parse_json(d).unwrap()).collect();
    let vars = parse_json(r#"{"x":[1,"s"]}"#).unwrap();
    let mut evaluated = 0usize;
    let mut buf = String::new();
    // 長さ 1〜3 の全列挙。
    let mut idx = [0usize; 3];
    for len in 1..=3usize {
        idx.iter_mut().for_each(|i| *i = 0);
        loop {
            buf.clear();
            for &i in idx.iter().take(len) {
                buf.push_str(alphabet[i]);
                buf.push(' ');
            }
            for prefix in ["", "strict "] {
                let text = format!("{prefix}{buf}");
                if let Ok(p) = parse_jsonpath(&text) {
                    for d in &parsed {
                        for silent in [false, true] {
                            let o = JsonPathEvalOptions::new()
                                .with_vars(&vars)
                                .with_silent(silent);
                            let _ = jsonpath_query(&p, d, o);
                            let _ = jsonpath_exists(&p, d, o);
                            let _ = jsonpath_match(&p, d, o);
                            evaluated += 1;
                        }
                    }
                }
            }
            // 次の組合せ。
            let mut k = 0;
            while k < len {
                idx[k] += 1;
                if idx[k] < alphabet.len() {
                    break;
                }
                idx[k] = 0;
                k += 1;
            }
            if k == len {
                break;
            }
        }
    }
    assert!(evaluated > 1000, "only {evaluated} evaluated");
}

#[test]
fn seeded_random_inputs_never_panic() {
    let tokens = [
        "$",
        "$.a",
        "$.b",
        "[*]",
        "[0]",
        "[last]",
        ".*",
        ".**",
        ".**{1 to 2}",
        ".type()",
        ".size()",
        ".abs()",
        ".double()",
        ".bigint()",
        ".decimal(4,2)",
        ".keyvalue()",
        " ? (@ > 1)",
        " ? (@.a == \"s\")",
        " ? (exists(@.a))",
        " ? (@ starts with \"s\")",
        " + 1",
        " - 2",
        " * 3",
        " / 0",
        " % 2",
        "-",
        "1",
        "\"s\"",
        "null",
        "true",
        "$x",
    ];
    let docs = [
        r#"{"a":1,"b":[1,2,{"a":"s"}]}"#,
        r#"[{"a":1},{"a":"s"},[1,[2,[3]]],null,true]"#,
        r#"{"a":{"a":{"a":{"a":{"a":1}}}}}"#,
        r#""s""#,
    ];
    let parsed: Vec<_> = docs.iter().map(|d| parse_json(d).unwrap()).collect();
    let mut state: u64 = 0x2184_2184_2184_2184;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let mut evaluated = 0usize;
    for _ in 0..4000 {
        let n = 1 + next() % 6;
        let mut s = String::new();
        if next() % 3 == 0 {
            s.push_str("strict ");
        }
        for _ in 0..n {
            s.push_str(tokens[next() % tokens.len()]);
        }
        if let Ok(p) = parse_jsonpath(&s) {
            let d = &parsed[next() % parsed.len()];
            let o = JsonPathEvalOptions::new().with_silent(next() % 2 == 0);
            let _ = jsonpath_query(&p, d, o);
            let _ = jsonpath_exists(&p, d, o);
            evaluated += 1;
        }
    }
    assert!(evaluated > 200, "only {evaluated} evaluated");
}

// ---------------------------------------------------------------------------
// スタック
// ---------------------------------------------------------------------------

fn in_small_stack(path: String, json: &'static str) -> Result<usize, JsonPathEvalError> {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || eval_with(&path, json, JsonPathLimits::default()))
        .expect("spawn")
        .join()
        .expect("evaluation must not overflow the stack")
}

#[test]
fn worst_case_shapes_fit_in_two_mib_stack() {
    let long_ops = |op: &str| format!("${}", op.repeat(1000));
    let results = [
        in_small_stack(long_ops("[*]"), "1"),
        in_small_stack(long_ops(".type()"), "1"),
        in_small_stack(format!("1{}", " + 1".repeat(1022)), "1"),
        in_small_stack(format!("$ ? (@ == 5{})", " || @ == 5".repeat(1000)), "1"),
        in_small_stack(format!("$ ? (@ == 1{})", " && @ == 1".repeat(1000)), "1"),
        in_small_stack(format!("$ ? (1{} == 1)", " + 1".repeat(1000)), "1"),
        in_small_stack(format!("{}1", "-".repeat(60)), "1"),
        in_small_stack(format!("{}1{}", "(".repeat(60), ")".repeat(60)), "1"),
        in_small_stack(
            format!("$ ? ({}@ == 1{})", "!(".repeat(30), ")".repeat(30)),
            "1",
        ),
    ];
    for r in results {
        // 上限内で成功するか、上限超過の 54000 になるか（どちらでもスタックは溢れない）。
        if let Err(e) = r {
            assert_eq!(e.wire_code(), "54000");
        }
    }
    // 深さ 1,000 の連鎖は既定の上限内で成功する。
    assert_eq!(in_small_stack(long_ops("[*]"), "1"), Ok(1));
}

fn eval_query(path: &str, json: &str) -> Result<Vec<JsonValue>, JsonPathEvalError> {
    let p = parse_jsonpath(path).expect("parse");
    let doc = parse_json(json).expect("json");
    jsonpath_query(&p, &doc, JsonPathEvalOptions::new())
}

#[test]
fn cloned_bytes_are_budgeted_cumulatively() {
    // 1 MiB の文字列を同じ添字で多数回複製する。ノード数課金は 1 件 1 ノードで通るが、
    // 累計バイト予算（256 MiB）で 54000 になる。
    let doc = format!("[\"{}\"]", "a".repeat(1 << 20));
    let many = format!("$[{}]", vec!["0"; 300].join(","));
    let err = eval_query(&many, &doc).expect_err("over budget");
    assert_eq!(err.wire_code(), "54000");
    // 予算内（8 回 = 8 MiB）は成功する。
    let few = format!("$[{}]", ["0"; 8].join(","));
    assert_eq!(eval_query(&few, &doc).map(|v| v.len()), Ok(8));
}

#[test]
fn numeric_string_conversion_emits_valid_json_text() {
    // 整数部の先頭ゼロは Decimal に収まらない指数でも正規化される（RFC 8259）。
    let out = eval_query("$.number()", "\"01e400\"").expect("number");
    let text = out.first().map(canon).unwrap_or_default();
    assert!(parse_json(&text).is_ok(), "invalid json: {text}");
    assert!(!text.starts_with("01"), "leading zero kept: {text}");
}

#[test]
fn division_does_not_overflow_on_intermediate_values() {
    let out = eval_query("$ / $", "100000000000000000000").expect("divide");
    assert_eq!(
        out.first().map(canon).as_deref(),
        Some("1.00000000000000000000")
    );
}

// ---------------------------------------------------------------------------
// PR #2466 レビュー対応の回帰テスト
// ---------------------------------------------------------------------------

/// `.boolean()` は PG 17 実測表に合わせ、scale 付き数値（2.0・0.0・1.5）を 22036 で拒否する。
#[test]
fn boolean_method_rejects_scaled_numbers() {
    for json in ["2.0", "0.0", "1.5", "1.0"] {
        let err = eval_query("$.boolean()", json).unwrap_err();
        assert_eq!(err.wire_code(), "22036", "{json}");
    }
    assert_eq!(
        eval_query("$.boolean()", "2").unwrap(),
        vec![JsonValue::Bool(true)]
    );
    assert_eq!(
        eval_query("$.boolean()", "0").unwrap(),
        vec![JsonValue::Bool(false)]
    );
}

/// 巨大文字列の比較は走査バイト数に比例して課金され、上限超過は fail-closed（54000）。
#[test]
fn string_comparison_is_charged_by_scanned_bytes() {
    let big = |tail: char| format!("{}{}", "a".repeat(1 << 16), tail);
    let doc = format!(r#"{{"a":"{}","b":"{}"}}"#, big('x'), big('y'));
    let small = r#"{"a":"x","b":"y"}"#;
    let limits = JsonPathLimits::new(500, 1024);
    let path = "$ ? ($.a < $.b)";
    assert_eq!(eval_with(path, small, limits), Ok(1));
    assert_eq!(
        eval_with(path, &doc, limits),
        Err(JsonPathEvalError::StepLimitExceeded)
    );
    // 十分な予算があれば同じ入力でも成功する。
    assert_eq!(
        eval_with(path, &doc, JsonPathLimits::new(1_000_000, 1024)),
        Ok(1)
    );
}
