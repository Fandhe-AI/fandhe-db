//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-transaction-duration-ms`・
//! `--max-transaction-statements` opt-in CLI 引数のパーサ（Issue #1553。ポインタ:
//! `docs/spec/04-behavior/` SQL-46・SQL-31）。
//!
//! `join_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲の判定は
//! engine 側の単一情報源 [`engine::sql::transaction`] の `validate_*` に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡す
//! だけにする。各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの
//! 切り出しは行わない。
//!
//! 兄弟フラグと違い、既定値（20 秒・1,000 文）は構造上のハード上限ではないため、既定の
//! 上下どちらへも設定できる（ハード上限は 60 秒・10,000 文）。解決結果は `main.rs` が
//! bind・ユーザーストア読込より前に [`resolve`] で確定し、`EngineCore::with_transaction_limits`
//! へ 1 回だけ注入する（`docs/design/transaction-limits-cli.md`）。範囲外・非数値・値欠落・
//! 重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::sql::transaction::{
    validate_max_transaction_duration_ms, validate_max_transaction_statements, TransactionLimits,
};
use std::time::Duration;

/// 明示トランザクションの最大持続時間（ミリ秒）を設定する CLI フラグ名。
pub const MAX_TRANSACTION_DURATION_MS_FLAG: &str = "--max-transaction-duration-ms";

/// 明示トランザクションの最大文数を設定する CLI フラグ名。
pub const MAX_TRANSACTION_STATEMENTS_FLAG: &str = "--max-transaction-statements";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct TransactionLimitsRawFlags<'a> {
    pub max_duration_ms: Option<&'a str>,
    pub max_statements: Option<&'a str>,
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

/// 2 フラグの未パース値から [`TransactionLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`TransactionLimits::default`]＝現行挙動）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: TransactionLimitsRawFlags<'_>) -> Result<TransactionLimits, String> {
    let mut limits = TransactionLimits::default();
    if let Some(r) = raw.max_duration_ms {
        let v = parse_flag(MAX_TRANSACTION_DURATION_MS_FLAG, r)?;
        limits.max_duration = validate_max_transaction_duration_ms(v)
            .map_err(|e| format!("{MAX_TRANSACTION_DURATION_MS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_statements {
        let v = parse_flag(MAX_TRANSACTION_STATEMENTS_FLAG, r)?;
        limits.max_statements = validate_max_transaction_statements(v)
            .map_err(|e| format!("{MAX_TRANSACTION_STATEMENTS_FLAG}: {e}"))?;
    }
    Ok(limits)
}

/// 明示指定した最大持続時間が書き込みロック待ち上限（`lock_wait`）を超える場合の警告文。
/// 超えると、長いトランザクションが単一ライタを握っている間にほかのセッションの書き込みが
/// `55P03` を受け得る。エラーにはせず、安全上の含意を見落とさせないために `main.rs` が
/// 起動ログへ出す（`dml_limits_opt::insert_rows_cap_warning` と同じ設計）。未指定の場合は
/// `None`（既定値は上限以下）。
pub fn duration_over_write_lock_wait_warning(
    limits: &TransactionLimits,
    explicit: bool,
    lock_wait: Duration,
) -> Option<String> {
    if !explicit || limits.max_duration <= lock_wait {
        return None;
    }
    Some(format!(
        "{MAX_TRANSACTION_DURATION_MS_FLAG} {} exceeds the write lock wait limit ({} ms); other sessions' writes may fail with 55P03 while a long transaction holds the writer (see docs/design/transaction-limits-cli.md)",
        limits.max_duration.as_millis(),
        lock_wait.as_millis()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(d: Option<&str>, s: Option<&str>) -> Result<TransactionLimits, String> {
        resolve(TransactionLimitsRawFlags {
            max_duration_ms: d,
            max_statements: s,
        })
    }

    #[test]
    fn unspecified_is_default() {
        let l = one(None, None).expect("default");
        let d = TransactionLimits::default();
        assert_eq!(l.max_duration, d.max_duration);
        assert_eq!(l.max_statements, d.max_statements);
        assert_eq!(l.max_duration, Duration::from_secs(20));
        assert_eq!(l.max_statements, 1_000);
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("100"), Some("1")).expect("lower");
        assert_eq!(
            (l.max_duration, l.max_statements),
            (Duration::from_millis(100), 1)
        );
        let l = one(Some("60000"), Some("10000")).expect("upper");
        assert_eq!(
            (l.max_duration, l.max_statements),
            (Duration::from_secs(60), 10_000)
        );
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        let common = [
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "abc",
            "99999999999999999999999",
            "4294967296",
        ];
        for bad in common.iter().chain(["0", "99", "60001"].iter()) {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_TRANSACTION_DURATION_MS_FLAG), "{bad}: {e}");
        }
        for bad in common.iter().chain(["0", "10001"].iter()) {
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_TRANSACTION_STATEMENTS_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("5")).expect("statements only");
        assert_eq!(l.max_statements, 5);
        assert_eq!(l.max_duration, TransactionLimits::default().max_duration);
        let l = one(Some("500"), None).expect("duration only");
        assert_eq!(l.max_duration, Duration::from_millis(500));
        assert_eq!(l.max_statements, 1_000);
    }

    #[test]
    fn warning_only_for_explicit_duration_over_lock_wait() {
        let lock_wait = Duration::from_secs(30);
        let over = one(Some("45000"), None).expect("over");
        assert!(duration_over_write_lock_wait_warning(&over, true, lock_wait).is_some());
        assert!(duration_over_write_lock_wait_warning(&over, false, lock_wait).is_none());
        let at = one(Some("30000"), None).expect("at");
        assert!(duration_over_write_lock_wait_warning(&at, true, lock_wait).is_none());
        let default = one(None, None).expect("default");
        assert!(duration_over_write_lock_wait_warning(&default, false, lock_wait).is_none());
    }
}
