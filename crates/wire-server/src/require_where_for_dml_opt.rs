//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る値なしスイッチ
//! `--require-where-for-dml` の解決（Issue #2210・SQL-36 ポインタ）。
//!
//! `--require-operation-id`（`require_operation_id_opt`）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」で、`WHERE` 省略の SQL `UPDATE`／`DELETE` を変更開始前に
//! `42601` で拒否する engine の opt-in 構成へ CLI から到達する唯一の入口を本モジュールに
//! 置く。`main.rs::run_server` が [`resolve`] の結果を
//! `EngineCore::with_require_where_for_dml` へ 1 回だけ渡す。同メソッドは `Arc` 化前にのみ
//! 呼べる所有権消費ビルダーのため、セッション・SQL・HTTP リクエストからは変更できない。
//!
//! - 指定あり: `WHERE` 省略の `UPDATE`／`DELETE` を `42601` で拒否する（明示した述語は
//!   `WHERE TRUE` を含め本スイッチの影響を受けない）
//! - 指定なし: PostgreSQL 互換の既定（`WHERE` 省略を受理）
//!
//! NoSQL（HTTP）表層の `update`／`delete` は filter 必須のままで、本スイッチの対象外。
//! 設計記録は `docs/design/require-where-for-dml-cli.md` を参照。

/// `--require-where-for-dml` の CLI フラグ名（値を取らないスイッチ）。
pub const FLAG: &str = "--require-where-for-dml";

/// フラグ指定時にだけ起動ログへ出す固定文言（`main.rs` が `log_stderr!` で出力。
/// パス・テナント・ユーザー情報を含めない）。
pub const STARTUP_NOTICE: &str =
    "UPDATE/DELETE without WHERE is rejected with 42601 (--require-where-for-dml)";

/// `explicit`（フラグが指定されたか）を engine へ渡す値へ写像する。配線点を本モジュールに
/// 一本化するための恒等写像で、未指定は既定の `false`（受理）になる。
pub fn resolve(explicit: bool) -> bool {
    explicit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_maps_to_enabled() {
        assert!(resolve(true));
    }

    #[test]
    fn unspecified_maps_to_disabled() {
        assert!(!resolve(false));
    }

    #[test]
    fn flag_is_a_bare_switch_name() {
        assert!(FLAG.starts_with("--"));
        assert!(!FLAG.contains('='));
        assert!(STARTUP_NOTICE.contains(FLAG));
    }
}
