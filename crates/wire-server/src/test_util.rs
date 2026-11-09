//! src 配下の単体テスト専用の一時パス払い出しヘルパー（Issue #2254）。
//!
//! 背景: 単体テストの一時 DB・一時ディレクトリの名前を pid と `SystemTime::now()` だけで
//! 作ると、macOS のようにマイクロ秒分解能の環境で同一プロセス内の並列テストが同じ名前を
//! 引き、redb の `DatabaseAlreadyOpen` や `create_dir` の `AlreadyExists` で flaky に落ちる。
//! `tests/` 側の同型問題は Issue #1463・#1464 で対処済みで、本ファイルは `src/` 側
//! （`http` / `tls` / `result_encoder` / `replica_opt` の `#[cfg(test)]`）の共有版である。
//!
//! 呼び出し文脈: lib クレート内の `#[cfg(test)] mod` からのみ
//! `crate::test_util::unique_temp_path` として参照する（本番コードからは呼ばれない）。
//! bin の `main.rs` は lib の cfg(test) 項目を参照できないため独自の連番を持つ。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// プロセス内で単調増加する通番。時刻の分解能に依存せず名前を一意にする。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// `s` が `temp_dir()` 直下から逃げる経路（パス区切り・`..`）を含まないことを検査する
/// （fail-closed。呼び出し側はテスト内のリテラルだが将来の動的値混入に備える）。
fn validate_component(what: &str, s: &str) {
    assert!(
        !s.contains('/') && !s.contains('\\') && !s.contains(".."),
        "test_util {what} must not contain path separators or '..': {s:?}"
    );
}

/// `temp_dir()` 直下の一意なパスを返す（ファイル・ディレクトリは作らない）。
/// 名前は `{label}-{pid}-{nanos}-{seq}{suffix}`。作成と後片付けは呼び出し側が行う。
pub(crate) fn unique_temp_path(label: &str, suffix: &str) -> PathBuf {
    validate_component("label", label);
    validate_component("suffix", suffix);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "{label}-{}-{nanos}-{seq}{suffix}",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn unique_temp_path_is_unique_across_threads() {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..256)
                        .map(|_| unique_temp_path("uniq", ".redb"))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut seen = HashSet::new();
        for h in handles {
            for p in h.join().expect("thread") {
                assert!(seen.insert(p), "duplicate path generated");
            }
        }
        assert_eq!(seen.len(), 8 * 256);
    }

    #[test]
    #[should_panic(expected = "must not contain path separators")]
    fn rejects_separator_in_label() {
        let _ = unique_temp_path("a/b", "");
    }

    #[test]
    #[should_panic(expected = "must not contain path separators")]
    fn rejects_dotdot_in_suffix() {
        let _ = unique_temp_path("a", "..x");
    }
}
