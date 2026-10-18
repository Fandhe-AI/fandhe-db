//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-temp-tables`・
//! `--max-temp-bytes` opt-in CLI 引数のパーサ（TABLE-44・Issue #1683。SQL-46 の
//! `group_by_limits_opt` と同型）。
//!
//! 範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::temp_table_limits`] の `validate_*` に委ね、本モジュールは untrusted な CLI
//! 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。各モジュールが
//! 独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_temp_table_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::temp_table_limits::{
    validate_max_temp_bytes, validate_max_temp_tables, TempTableLimits,
};

/// 1 セッションが持てる一時表の数の上限を設定する CLI フラグ名。
pub const MAX_TEMP_TABLES_FLAG: &str = "--max-temp-tables";

/// 1 セッションの一時表の論理総バイト数の上限を設定する CLI フラグ名。
pub const MAX_TEMP_BYTES_FLAG: &str = "--max-temp-bytes";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct TempTableLimitsRawFlags<'a> {
    pub max_temp_tables: Option<&'a str>,
    pub max_temp_bytes: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`group_by_limits_opt` と同じ方針。
/// `str::parse::<u64>` が受理する先頭 `+` 等を弾く）。
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

/// 2 フラグの未パース値から [`TempTableLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`TempTableLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: TempTableLimitsRawFlags<'_>) -> Result<TempTableLimits, String> {
    let base = TempTableLimits::default();
    let mut max_tables = base.max_tables_per_session();
    let mut max_bytes = base.max_bytes_per_session();
    if let Some(r) = raw.max_temp_tables {
        let v = parse_flag(MAX_TEMP_TABLES_FLAG, r)?;
        max_tables =
            validate_max_temp_tables(v).map_err(|e| format!("{MAX_TEMP_TABLES_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_temp_bytes {
        let v = parse_flag(MAX_TEMP_BYTES_FLAG, r)?;
        max_bytes =
            validate_max_temp_bytes(v).map_err(|e| format!("{MAX_TEMP_BYTES_FLAG}: {e}"))?;
    }
    Ok(TempTableLimits::new(max_tables, max_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(tables: Option<&str>, bytes: Option<&str>) -> Result<TempTableLimits, String> {
        resolve(TempTableLimitsRawFlags {
            max_temp_tables: tables,
            max_temp_bytes: bytes,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(TempTableLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1")).expect("lower bound");
        assert_eq!(
            (l.max_tables_per_session(), l.max_bytes_per_session()),
            (1, 1)
        );
        let l = one(Some("64"), Some("67108864")).expect("upper bound");
        assert_eq!(l, TempTableLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "65",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_TEMP_TABLES_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "67108865", "+5", "", "x"] {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_TEMP_BYTES_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("4096")).expect("bytes only");
        assert_eq!(l.max_bytes_per_session(), 4096);
        assert_eq!(
            l.max_tables_per_session(),
            TempTableLimits::default().max_tables_per_session()
        );
    }
}
