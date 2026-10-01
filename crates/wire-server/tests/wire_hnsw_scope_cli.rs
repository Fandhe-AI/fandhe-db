//! `--hnsw-scope`（Issue #1065・オーナー判断 2026-09-28）をバイナリ子プロセス
//! として起動し、CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合
//! テスト。`tests/wire_durability_cli.rs`（Issue #850）と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを確認する）。
//!
//! - R1: `all`／`declared` のいずれも、HNSW opt-in の有無によらず
//!   `listening on` に到達すること（opt-in 無効時は scope は無関係で、
//!   組合せエラーにもしない）
//! - R2: 不正値・値欠落・重複指定・`=` 連結形は非 0 終了・stderr に
//!   `--hnsw-scope` を含む説明が出ること
//!
//! scope ごとの経路選択（宣言テーブルのみ HNSW 等）そのものは engine 側の
//! `crates/engine/tests/index_declaration_targets.rs` が固定する。

#[path = "common/mod.rs"]
mod common;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）を確保し、`Drop` で確実に削除するガード
/// （`wire_durability_cli.rs::TempFixtureDir` と同型）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-hnsw-scope-cli-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        // 認証まで到達する必要がないため空のユーザーストアで足りる。
        std::fs::write(dir.join("users.txt"), "").expect("write empty user store");
        Self { dir }
    }

    fn path_str(&self, name: &str) -> String {
        self.dir
            .join(name)
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    /// `--users`／`--db`／`--bind` の基本引数。
    fn base_args(&self) -> Vec<String> {
        vec![
            "--users".to_string(),
            self.path_str("users.txt"),
            "--db".to_string(),
            self.path_str("db.redb"),
            "--bind".to_string(),
            "127.0.0.1:0".to_string(),
        ]
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
        // パスとエラーだけを stderr へ出す（Issue #1303）。
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.dir.display()
                );
            }
        }
    }
}

/// R1: `all`／`declared` のいずれも、`--search-engine hnsw` の有無によらず
/// `listening on` に到達すること。
#[test]
fn both_tokens_start_listening_with_and_without_hnsw_opt_in() {
    for token in ["all", "declared"] {
        for with_hnsw in [false, true] {
            let fixture = TempFixtureDir::new(&format!("r1-{token}-{with_hnsw}"));
            let mut args = fixture.base_args();
            args.push("--hnsw-scope".to_string());
            args.push(token.to_string());
            if with_hnsw {
                args.push("--search-engine".to_string());
                args.push("hnsw".to_string());
            }
            let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn wire-server");
            let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
            let _ = child.kill();
            let _ = child.wait();
            assert!(
                listening,
                "--hnsw-scope {token} (hnsw opt-in: {with_hnsw}) must reach listening state"
            );
        }
    }
}

/// R2: 不正値・値欠落・重複指定・`=` 連結形はいずれも fail-closed（非 0 終了・
/// `--hnsw-scope` を含む stderr）。
#[test]
fn invalid_or_missing_or_duplicate_hnsw_scope_arg_is_rejected() {
    let fixture = TempFixtureDir::new("r2");
    let cases: [&[&str]; 8] = [
        &["--hnsw-scope", "tables"],
        &["--hnsw-scope", "All"],
        &["--hnsw-scope", "DECLARED"],
        &["--hnsw-scope", ""],
        // 直後の既知フラグ名をそのまま値として食い、閉じた語彙のいずれとも
        // 一致しないため拒否される（他フラグと同じ「次トークンを無条件で値と
        // みなす」仕様）。
        &["--hnsw-scope", "--bind"],
        // 重複指定（last-wins にしない）。
        &["--hnsw-scope", "all", "--hnsw-scope", "declared"],
        // 値欠落（末尾にフラグだけを置く）。
        &["--hnsw-scope"],
        // `=` 連結形は受理しない（他フラグと同じく空白区切りのみ。未知引数として拒否）。
        &["--hnsw-scope=declared"],
    ];

    for extra_args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(fixture.base_args())
            .args(extra_args)
            .output()
            .expect("spawn wire-server");
        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--hnsw-scope"),
            "args={extra_args:?}: expected stderr to mention --hnsw-scope, got: {stderr}"
        );
    }
}
