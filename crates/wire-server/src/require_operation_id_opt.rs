//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る値なしスイッチ
//! `--require-operation-id` の解決（Issue #1703・親 #1701・RECOVER-1 / TASK-92
//! ポインタ）。
//!
//! `--durability`（`durability_opt`）・`--hnsw-scope`（`hnsw_scope_opt`）と同型の
//! 「プロセス起動時にのみ明示指定する注入点」で、`engine::recovery::required_op_id::
//! LedgerMode` へ CLI から到達する唯一の入口を本モジュールに置く。`main.rs::
//! run_server` が [`resolve`] の結果を `EngineCore::with_ledger_mode` へ 1 回だけ
//! 渡す。`with_ledger_mode` は `Arc` 化前にのみ呼べる所有権消費ビルダーのため、
//! セッション・SQL・HTTP リクエストからは変更できない。
//!
//! - 指定あり: 書き込み文の `USING OPERATION_ID` 省略を `23502` で拒否する従来の
//!   必須契約（`LedgerMode::Ledgered`）
//! - 指定なし: engine の既定（[`LedgerMode::default`]）。operation_id を任意にする
//!   モードの追加と既定の移行は #1704 の担当で、本モジュールは配線のみを持つ
//!
//! 方針の経緯は `docs/design/operation-id-optional.md` を参照。

use engine::recovery::required_op_id::LedgerMode;

/// `--require-operation-id` の CLI フラグ名（値を取らないスイッチ）。
pub const FLAG: &str = "--require-operation-id";

/// フラグ指定時にだけ起動ログへ出す固定文言（`main.rs` が `log_stderr!` で出力。
/// パス・テナント・ユーザー情報を含めない）。
pub const STARTUP_NOTICE: &str = "operation_id is required for write statements (--require-operation-id); omitted USING OPERATION_ID is rejected with 23502";

/// `explicit`（フラグが指定されたか）を [`LedgerMode`] へ写像する。
///
/// - `true` は常に [`LedgerMode::Ledgered`]（現行の必須契約）。
/// - `false` は [`LedgerMode::default`]。engine の既定は現時点で `Ledgered` で、
///   #1704 が任意モードの variant を追加して既定を移す（既定を据え置く場合は
///   本関数のこの分岐を 1 行変更する）。
///
/// `CompareOnlyWithoutLedger` は台帳書き込みを無効化する本番非推奨のモードで、
/// 再送の冪等性が壊れ fail-open になるため、どの分岐でも返さない。
pub fn resolve(explicit: bool) -> LedgerMode {
    if explicit {
        LedgerMode::Ledgered
    } else {
        LedgerMode::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_maps_to_ledgered() {
        assert_eq!(resolve(true), LedgerMode::Ledgered);
    }

    #[test]
    fn unspecified_maps_to_engine_default() {
        assert_eq!(resolve(false), LedgerMode::default());
    }

    #[test]
    fn never_disables_the_ledger() {
        for explicit in [true, false] {
            assert_ne!(resolve(explicit), LedgerMode::CompareOnlyWithoutLedger);
        }
    }

    #[test]
    fn flag_is_a_bare_switch_name() {
        assert!(FLAG.starts_with("--"));
        assert!(!FLAG.contains('='));
        assert!(STARTUP_NOTICE.contains(FLAG));
    }
}
