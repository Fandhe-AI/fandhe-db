//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る PL/pgSQL 実行上限の opt-in CLI 引数
//! （`--max-plpgsql-statements`・`--max-plpgsql-loop-iterations`・`--max-plpgsql-nesting-depth`・
//! `--max-plpgsql-body-bytes`・`--max-function-call-depth`）のパーサ（Issue #2225・SQL-68。
//! 設計は `docs/design/plpgsql-subset.md` D7）。
//!
//! `recursive_cte_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲（ハード上限
//! 以下への引き下げのみ）の判定は engine 側の単一情報源 [`engine::plpgsql_limits`] の `validate_*`
//! に委ね、本モジュールは untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を
//! 弾く）して渡すだけにする。解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`]
//! で確定し、`EngineCore::with_plpgsql_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複
//! 指定は fail-closed で起動エラー（既定へ黙って読み替えない）。全テナントで同じ値を使う。

use engine::plpgsql_limits::{
    validate_max_function_call_depth, validate_max_plpgsql_body_bytes,
    validate_max_plpgsql_loop_iterations, validate_max_plpgsql_nesting_depth,
    validate_max_plpgsql_statements, PlpgsqlLimits,
};

/// 文数上限を設定する CLI フラグ名。
pub const MAX_PLPGSQL_STATEMENTS_FLAG: &str = "--max-plpgsql-statements";
/// ループ反復数上限を設定する CLI フラグ名。
pub const MAX_PLPGSQL_LOOP_ITERATIONS_FLAG: &str = "--max-plpgsql-loop-iterations";
/// 入れ子の深さ上限を設定する CLI フラグ名。
pub const MAX_PLPGSQL_NESTING_DEPTH_FLAG: &str = "--max-plpgsql-nesting-depth";
/// 本体バイト数上限を設定する CLI フラグ名。
pub const MAX_PLPGSQL_BODY_BYTES_FLAG: &str = "--max-plpgsql-body-bytes";
/// 関数呼び出し深さ上限を設定する CLI フラグ名（#2218 と共有する）。
pub const MAX_FUNCTION_CALL_DEPTH_FLAG: &str = "--max-function-call-depth";

/// 5 フラグ（`main.rs` の引数解析が生の値を集める配列の添字順）。
pub const FLAGS: [&str; 5] = [
    MAX_PLPGSQL_STATEMENTS_FLAG,
    MAX_PLPGSQL_LOOP_ITERATIONS_FLAG,
    MAX_PLPGSQL_NESTING_DEPTH_FLAG,
    MAX_PLPGSQL_BODY_BYTES_FLAG,
    MAX_FUNCTION_CALL_DEPTH_FLAG,
];

/// `flag` が PL/pgSQL 上限フラグなら [`FLAGS`] の添字を返す。
pub fn flag_index(flag: &str) -> Option<usize> {
    FLAGS.iter().position(|f| *f == flag)
}

/// 5 つの CLI フラグの生の値（未指定は `None`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PlpgsqlLimitsRawFlags<'a> {
    pub max_statements: Option<&'a str>,
    pub max_loop_iterations: Option<&'a str>,
    pub max_nesting_depth: Option<&'a str>,
    pub max_body_bytes: Option<&'a str>,
    pub max_call_depth: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<usize, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 1 フラグ分の解決（未指定は `default`）。範囲検証は engine の `validate` に委ねる。
fn one<E: std::fmt::Display>(
    flag: &str,
    raw: Option<&str>,
    default: usize,
    validate: fn(usize) -> Result<usize, E>,
) -> Result<usize, String> {
    match raw {
        None => Ok(default),
        Some(r) => validate(parse_flag(flag, r)?).map_err(|e| format!("{flag}: {e}")),
    }
}

/// 5 フラグの未パース値から [`PlpgsqlLimits`] を解決する純関数。未指定のフラグは既定値
/// （[`PlpgsqlLimits::default`]＝ハード上限）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: PlpgsqlLimitsRawFlags<'_>) -> Result<PlpgsqlLimits, String> {
    let d = PlpgsqlLimits::default();
    Ok(PlpgsqlLimits::new(
        one(
            MAX_PLPGSQL_STATEMENTS_FLAG,
            raw.max_statements,
            d.max_statements(),
            validate_max_plpgsql_statements,
        )?,
        one(
            MAX_PLPGSQL_LOOP_ITERATIONS_FLAG,
            raw.max_loop_iterations,
            d.max_loop_iterations(),
            validate_max_plpgsql_loop_iterations,
        )?,
        one(
            MAX_PLPGSQL_NESTING_DEPTH_FLAG,
            raw.max_nesting_depth,
            d.max_nesting_depth(),
            validate_max_plpgsql_nesting_depth,
        )?,
        one(
            MAX_PLPGSQL_BODY_BYTES_FLAG,
            raw.max_body_bytes,
            d.max_body_bytes(),
            validate_max_plpgsql_body_bytes,
        )?,
        one(
            MAX_FUNCTION_CALL_DEPTH_FLAG,
            raw.max_call_depth,
            d.max_call_depth(),
            validate_max_function_call_depth,
        )?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(i: usize, v: &str) -> Result<PlpgsqlLimits, String> {
        let mut raw = PlpgsqlLimitsRawFlags::default();
        match i {
            0 => raw.max_statements = Some(v),
            1 => raw.max_loop_iterations = Some(v),
            2 => raw.max_nesting_depth = Some(v),
            3 => raw.max_body_bytes = Some(v),
            _ => raw.max_call_depth = Some(v),
        }
        resolve(raw)
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(
            resolve(PlpgsqlLimitsRawFlags::default()),
            Ok(PlpgsqlLimits::default())
        );
    }

    #[test]
    fn flag_index_matches_flags_table() {
        for (i, f) in FLAGS.iter().enumerate() {
            assert_eq!(flag_index(f), Some(i));
        }
        assert_eq!(flag_index("--max-result-rows"), None);
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        for i in 0..5 {
            let l = only(i, "1").expect("lower bound");
            assert_ne!(l, PlpgsqlLimits::default(), "flag {i}");
        }
        let l = only(0, "100000").expect("upper bound");
        assert_eq!(l, PlpgsqlLimits::default());
        assert_eq!(only(1, "1000000"), Ok(PlpgsqlLimits::default()));
        assert_eq!(only(2, "32"), Ok(PlpgsqlLimits::default()));
        assert_eq!(only(4, "32"), Ok(PlpgsqlLimits::default()));
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        let malformed = [
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
            "0",
        ];
        for (i, flag) in FLAGS.iter().enumerate() {
            for bad in malformed {
                let e = only(i, bad).expect_err(bad);
                assert!(e.contains(flag), "{bad}: {e}");
            }
        }
        assert!(only(0, "100001").is_err());
        assert!(only(1, "1000001").is_err());
        assert!(only(2, "33").is_err());
        assert!(only(4, "33").is_err());
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = only(4, "5").expect("depth only");
        assert_eq!(l.max_call_depth(), 5);
        assert_eq!(
            l.max_statements(),
            PlpgsqlLimits::default().max_statements()
        );
    }
}
