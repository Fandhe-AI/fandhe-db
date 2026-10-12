//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-scanned-rows`
//! opt-in CLI 引数のパーサ（Issue #1549。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `schema_limits_opt`・`dml_limits_opt` と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。範囲（ハード上限 1,000,000 以下への引き下げのみ）の判定は engine 側の単一
//! 情報源 [`engine::scan_limits::validate_max_scanned_rows`] に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して
//! 渡すだけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、
//! 共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_scan_limits` へ 1 回だけ注入する。範囲外・非数値・値欠落・重複
//! 指定は fail-closed で起動エラー（既定へ黙って読み替えない）。分割実行 DML の
//! `--partitioned-dml-scan-budget` とは連動せず、組合せ検査も行わない
//! （`docs/design/scan-limits-cli.md`）。

use engine::scan_limits::{validate_max_scanned_rows, ScanLimits};

/// 1 文あたりの総走査行数上限を設定する CLI フラグ名。
pub const MAX_SCANNED_ROWS_FLAG: &str = "--max-scanned-rows";

/// ASCII 数字だけを受理する厳密な 10 進パース（`schema_limits_opt::parse_strict_decimal`
/// と同じ方針。`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// フラグの未パース値から [`ScanLimits`] を解決する純関数。未指定は既定
/// （[`ScanLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は `Err`
/// （fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: Option<&str>) -> Result<ScanLimits, String> {
    let Some(r) = raw else {
        return Ok(ScanLimits::default());
    };
    let v = parse_strict_decimal(r).ok_or_else(|| {
        format!("{MAX_SCANNED_ROWS_FLAG} expects a non-negative integer, got {r:?}")
    })?;
    let v = validate_max_scanned_rows(v).map_err(|e| format!("{MAX_SCANNED_ROWS_FLAG}: {e}"))?;
    Ok(ScanLimits::new(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_is_default() {
        assert_eq!(resolve(None).expect("ok"), ScanLimits::default());
    }

    #[test]
    fn boundaries_are_accepted() {
        assert_eq!(resolve(Some("1")).expect("ok").max_scanned_rows(), 1);
        assert_eq!(
            resolve(Some("1000000")).expect("ok").max_scanned_rows(),
            1_000_000
        );
        assert_eq!(resolve(Some("0050")).expect("ok").max_scanned_rows(), 50);
    }

    #[test]
    fn invalid_values_are_rejected_with_flag_name() {
        for bad in [
            "0",
            "1000001",
            "99999999999999999999999",
            "+5",
            " 5",
            "5 ",
            "-1",
            "abc",
            "",
            "５",
        ] {
            let e = resolve(Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_SCANNED_ROWS_FLAG), "{bad}: {e}");
        }
    }
}
