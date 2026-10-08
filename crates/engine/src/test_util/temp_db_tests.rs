//! `temp_db.rs` の自己テスト（Issue #201 レビュー対応）。
//!
//! 背景: 以前は `temp_db.rs` 内に `mod tests` を直接持っていたが、`tests/*.rs`
//! 側は `#[path = "../src/test_util/temp_db.rs"] mod temp_db;` でファイル全体を
//! 取り込むため、結合テストバイナリを増やすたびに自己テストまで重複コンパイル・
//! 重複実行されていた。本ファイルは `lib.rs` の `#[cfg(test)] mod test_util` からのみ
//! （`temp_db` 本体とは別モジュールとして）取り込み、一度しかコンパイル・実行されない
//! ようにする。結合テスト側の `#[path=...]` 取り込みはこのファイルを対象にしないため
//! 影響を受けない。
use super::temp_db::*;

#[test]
fn unique_db_path_returns_distinct_paths_for_consecutive_calls() {
    let a = unique_db_path("dup-check");
    let b = unique_db_path("dup-check");
    assert_ne!(a, b, "consecutive calls must not collide");
}

#[test]
fn unique_db_path_rejects_path_traversal_labels() {
    let result = std::panic::catch_unwind(|| unique_db_path(".."));
    assert!(result.is_err(), "label \"..\" must be rejected");

    let result = std::panic::catch_unwind(|| unique_db_path("a/b"));
    assert!(result.is_err(), "label containing '/' must be rejected");
}

#[test]
fn cleanup_guard_removes_the_file_and_tolerates_missing_file() {
    let path = unique_db_path("cleanup-guard");
    std::fs::write(&path, b"placeholder").expect("seed placeholder file");
    {
        let _guard = CleanupGuard(path.clone());
    }
    assert!(!path.exists(), "CleanupGuard must remove the file on drop");

    // 既に無い場合も panic しない（二重 drop 相当の状況を模擬）。
    drop(CleanupGuard(path));
}

#[test]
fn tempdir_creates_and_removes_a_directory() {
    let dir = TempDir::new("tempdir-basic");
    let path = dir.path().to_path_buf();
    assert!(path.is_dir(), "TempDir::new must create the directory");
    drop(dir);
    assert!(!path.exists(), "TempDir drop must remove the directory");
}

#[test]
fn tempdir_db_path_is_inside_the_directory() {
    let dir = TempDir::new("tempdir-dbpath");
    assert_eq!(dir.db_path(), dir.path().join("db.redb"));
}

#[test]
fn describe_temp_dir_state_reports_the_temp_dir_path_without_panicking() {
    // 発生時診断（panic メッセージ・`eprintln!` に埋め込む文字列）が、実際に
    // `temp_dir()` の実体パスを含み、呼び出し自体が panic しないことを確認する
    // （このヘルパは通常経路では呼ばれないため、生成・削除の失敗経路以外に
    // 検証手段がない）。
    let description = describe_temp_dir_state();
    let expected_path = std::env::temp_dir();
    assert!(
        description.contains(&expected_path.display().to_string()),
        "description must mention the temp_dir() path: {description}"
    );
    assert!(description.contains("writable="));
    assert!(description.contains("leftover_fandhe_db_entries="));
}
