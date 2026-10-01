//! user store フィクスチャの所有ガード（Issue #1303）。
//!
//! `tests/common/mod.rs`（`#[path]` 経由で各結合テストへ include される共有ヘルパー）と、
//! `common` を include せず個別に user store 生成ヘルパーを持つテスト
//! （`wire_auth.rs`・`wire_framing.rs`・`wire_limits.rs`・`wire_scram_auth.rs`・
//! `wire_scram_plus_tls.rs` 等）の双方から `#[path = "common/fixture_guard.rs"]` で
//! 取り込まれる。`std` のみに依存し `crate::` を参照しない（include 元ごとに独立に
//! コンパイルされるため）。
#![allow(dead_code)]

/// `write_user_store_file` が作る user store フィクスチャ（一時ディレクトリ +
/// `users.txt`）の所有ガード（Issue #1303）。`Drop` で一時ディレクトリごと削除する
/// ため、テスト関数のローカル変数へ必ず束縛すること（一時値のまま使うと文末で
/// 削除され、サーバー起動時の読み込みに間に合わない）。子プロセスを起動する
/// テストでは、子の kill + wait ガードより先に宣言する（drop は宣言の逆順＝子の
/// 終了後に削除される）。`Deref<Target = Path>` なので `&Path` 引数へそのまま渡せる。
pub struct UserStoreFile {
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
}

impl UserStoreFile {
    /// `dir`（削除対象）と `path`（`dir` 配下の user store ファイル）を所有する。
    /// 呼び出し側は `dir` を自分が排他的に作成した一意ディレクトリとすること。
    pub fn new(dir: std::path::PathBuf, path: std::path::PathBuf) -> Self {
        Self { dir, path }
    }
}

impl std::ops::Deref for UserStoreFile {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for UserStoreFile {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for UserStoreFile {
    fn drop(&mut self) {
        remove_dir_all_logged(&self.dir);
    }
}

/// 自分が作成した一時ディレクトリを削除する。`NotFound` は成功扱い、それ以外の
/// 失敗はテストを失敗させずパスとエラーだけを stderr へ出す（Drop 内 panic は
/// 二重 panic による abort を招くため禁止。Issue #1303）。
pub fn remove_dir_all_logged(dir: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("warning: failed to remove temp dir {}: {e}", dir.display());
        }
    }
}
