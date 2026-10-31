//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る WASM UDF サンドボックス上限の opt-in
//! CLI 引数（`--wasm-max-memory-bytes`・`--wasm-max-call-duration-ms`・`--wasm-max-fuel`）の
//! パーサ（Issue #1927・EXT-6／EXT-9 ポインタ。設計は `docs/design/wasmtime-adoption.md`）。
//!
//! `plpgsql_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲（ハード上限
//! 以下への引き下げのみ）の判定は engine 側の単一情報源 [`engine::wasm_udf`] の `validate_wasm_*`
//! に委ね、本モジュールは untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を
//! 弾く）して渡すだけにする。解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`]
//! で確定し、`EngineCore::with_wasm_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複
//! 指定は fail-closed で起動エラー（既定へ黙って読み替えない）。全テナントで同じ値を使う。
//! 実際に効くのは engine の `wasm-udf` feature を有効にしたビルドだけ（値は締める方向のみ）。

use engine::wasm_udf::{
    validate_wasm_max_call_duration_ms, validate_wasm_max_fuel, validate_wasm_max_memory_bytes,
    SandboxLimits,
};

/// 線形メモリ上限（バイト）を設定する CLI フラグ名。
pub const WASM_MAX_MEMORY_BYTES_FLAG: &str = "--wasm-max-memory-bytes";
/// 呼び出し 1 回の実行時間上限（ミリ秒）を設定する CLI フラグ名。
pub const WASM_MAX_CALL_DURATION_MS_FLAG: &str = "--wasm-max-call-duration-ms";
/// 呼び出し 1 回の燃料（命令数予算）上限を設定する CLI フラグ名。
pub const WASM_MAX_FUEL_FLAG: &str = "--wasm-max-fuel";

/// 3 フラグ（`main.rs` の引数解析が生の値を集める配列の添字順）。
pub const FLAGS: [&str; 3] = [
    WASM_MAX_MEMORY_BYTES_FLAG,
    WASM_MAX_CALL_DURATION_MS_FLAG,
    WASM_MAX_FUEL_FLAG,
];

/// `flag` が WASM サンドボックス上限フラグなら [`FLAGS`] の添字を返す。
pub fn flag_index(flag: &str) -> Option<usize> {
    FLAGS.iter().position(|f| *f == flag)
}

/// 3 つの CLI フラグの生の値（未指定は `None`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct WasmLimitsRawFlags<'a> {
    pub max_memory_bytes: Option<&'a str>,
    pub max_call_duration_ms: Option<&'a str>,
    pub max_fuel: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<u64, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 1 フラグ分の解決（未指定は `default`）。範囲検証は engine の `validate` に委ねる。
fn one<T, E: std::fmt::Display>(
    flag: &str,
    raw: Option<&str>,
    default: T,
    validate: fn(u64) -> Result<T, E>,
) -> Result<T, String> {
    match raw {
        None => Ok(default),
        Some(r) => validate(parse_flag(flag, r)?).map_err(|e| format!("{flag}: {e}")),
    }
}

/// 3 フラグの未パース値から [`SandboxLimits`] を解決する純関数。未指定のフラグは既定値
/// （[`SandboxLimits::default`]＝ハード上限）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: WasmLimitsRawFlags<'_>) -> Result<SandboxLimits, String> {
    let d = SandboxLimits::default();
    Ok(SandboxLimits {
        memory_limit_bytes: one(
            WASM_MAX_MEMORY_BYTES_FLAG,
            raw.max_memory_bytes,
            d.memory_limit_bytes,
            validate_wasm_max_memory_bytes,
        )?,
        deadline_ticks: one(
            WASM_MAX_CALL_DURATION_MS_FLAG,
            raw.max_call_duration_ms,
            d.deadline_ticks,
            validate_wasm_max_call_duration_ms,
        )?,
        fuel: one(
            WASM_MAX_FUEL_FLAG,
            raw.max_fuel,
            d.fuel,
            validate_wasm_max_fuel,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(
            resolve(WasmLimitsRawFlags::default()),
            Ok(SandboxLimits::default())
        );
    }

    #[test]
    fn flag_index_matches_flags_table() {
        for (i, f) in FLAGS.iter().enumerate() {
            assert_eq!(flag_index(f), Some(i));
        }
        assert_eq!(flag_index("--max-functions"), None);
    }

    #[test]
    fn values_are_lowered_and_ms_floors_to_ticks() {
        let l = resolve(WasmLimitsRawFlags {
            max_memory_bytes: Some("65536"),
            max_call_duration_ms: Some("199"),
            max_fuel: Some("1000"),
        })
        .expect("in range");
        assert_eq!(l.memory_limit_bytes, 65536);
        assert_eq!(l.deadline_ticks, 1);
        assert_eq!(l.fuel, 1000);
    }

    #[test]
    fn out_of_range_and_malformed_values_are_rejected_with_flag_name() {
        let bad: [(&str, &str); 8] = [
            (WASM_MAX_MEMORY_BYTES_FLAG, "65535"),
            (WASM_MAX_MEMORY_BYTES_FLAG, "8388609"),
            (WASM_MAX_CALL_DURATION_MS_FLAG, "99"),
            (WASM_MAX_CALL_DURATION_MS_FLAG, "5001"),
            (WASM_MAX_FUEL_FLAG, "0"),
            (WASM_MAX_FUEL_FLAG, "100000001"),
            (WASM_MAX_FUEL_FLAG, "+5"),
            (WASM_MAX_FUEL_FLAG, "１２"),
        ];
        for (flag, v) in bad {
            let mut raw = WasmLimitsRawFlags::default();
            match flag_index(flag) {
                Some(0) => raw.max_memory_bytes = Some(v),
                Some(1) => raw.max_call_duration_ms = Some(v),
                _ => raw.max_fuel = Some(v),
            }
            let e = resolve(raw).expect_err("must be rejected");
            assert!(e.contains(flag), "{e}");
        }
        for v in ["", " 5", "5a", "-1"] {
            let raw = WasmLimitsRawFlags {
                max_fuel: Some(v),
                ..Default::default()
            };
            assert!(resolve(raw).is_err(), "{v:?}");
        }
    }
}
