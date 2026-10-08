//! テスト専用の一時 DB パス／一時ディレクトリ払い出しヘルパー（Issue #173）。
//!
//! 背景: 従来は `unique_db_path` / `CleanupGuard` / `TempDir` / `tempdir()` が
//! `storage.rs`・`catalog.rs`・`arena.rs`・`tenant.rs`・`core.rs`・`rls.rs`（unit test）と
//! 16 本の結合テスト（`tests/*.rs`）にほぼ同一のまま複製されていた。`rls.rs` の
//! `DatabaseAlreadyOpen` フレーク対策（`SEQ` 通番の追加）が他コピーへ波及しなかった
//! ことが実際の障害として観測されたため、本ファイルへ一本化する。
//!
//! 呼び出し文脈:
//! - engine クレート内の unit test（`storage.rs` 等）からは
//!   `crate::test_util::temp_db::{...}` として `mod`（`lib.rs` の `#[cfg(test)] mod test_util`）
//!   経由で参照する
//! - 結合テスト（`tests/*.rs`）からは `#[path = "../src/test_util/temp_db.rs"] mod temp_db;`
//!   で同一ソースを取り込む（`tests/power_loss.rs`・`tests/bench_accept.rs` と同じ方式）。
//!   この取り込み方式では `crate::` はテストバイナリ自身を指してしまうため、本ファイルは
//!   意図的に `std` のみに依存し `crate::` を一切参照しない（dependency-policy 準拠、
//!   外部依存なし）
//!
//! 契約: 呼び出し側 API（`unique_db_path(label)` / `CleanupGuard(path)` /
//! `TempDir::new(label)` / `TempDir::path()` / `TempDir::db_path()` / `tempdir()`）は
//! 従来の複製版とシグネチャ互換を維持し、既存の 100 箇所超の呼び出しサイトを
//! 変更せずに済むようにする。
//!
//! 再現・診断手順: 衝突やクリーンアップ失敗が疑われる場合は
//! `TMPDIR=$(mktemp -d) cargo test -p fandhe-db-engine -- --test-threads=1 --nocapture` で
//! 一時ディレクトリを固定・並列度を落として再現を試みる。生成失敗時は panic
//! メッセージに、削除失敗時は `eprintln!` に [`describe_temp_dir_state`] の出力
//! （実体パス・書込可否・残骸件数）が含まれる。
//!
//! Windows 向け注意: 開いたままの redb ファイル・ディレクトリは削除できないため、
//! 呼び出し側では `CleanupGuard` / `TempDir` を `Storage`（またはそれを保持する値）より
//! *先に* 宣言する（`Drop` は宣言の逆順に実行されるため、先に宣言したガードは
//! `Storage` の後に drop され、ファイルハンドルが閉じてから削除が走る。既存呼び出し
//! サイトの慣行と一致させる）。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// プロセス内で一意な通番。同一 tick で並行実行された複数スレッドが
/// `SystemTime::now()` の分解能不足で名前衝突する経路（旧 `rls.rs` の記録どおり）を
/// 塞ぐ。`unique_db_path` と `TempDir::new` の両方で共有する（分離する理由がない）。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// プロセスごとに一度だけ計算する識別子（pid・起動時刻・ASLR に依存する静的アドレス
/// を混ぜる）。pid 再利用（短命な CI コンテナ等）と旧プロセスの残骸ファイルが
/// 組み合わさっても、`SEQ` だけでは区別できない別プロセス由来の名前衝突を避ける。
fn process_salt() -> u64 {
    static SALT: OnceLock<u64> = OnceLock::new();
    *SALT.get_or_init(|| {
        let pid = u64::from(std::process::id());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // `SALT` 自身のアドレス。ASLR が有効な環境では実行のたびに変わるため、
        // 起動が同一ナノ秒に重なった別プロセス同士の salt が一致する確率をさらに下げる。
        let addr = std::ptr::addr_of!(SALT) as u64;
        pid ^ nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ addr
    })
}

/// `label` が `temp_dir()` 直下から逃げる経路（パス区切り・`..`）を含まないことを
/// 検査する（fail-closed。呼び出し側はすべてテストコード内のリテラル文字列だが、
/// 将来 `format!` 経由で動的な値が混入しても安全側に倒す）。
fn validate_label(label: &str) {
    assert!(
        !label.contains('/') && !label.contains('\\') && !label.contains(".."),
        "temp_db label must not contain path separators or '..': {label:?}"
    );
}

/// `temp_dir()` 配下に残った `fandhe-db-` 接頭辞のエントリを数え、生成・削除の失敗時に
/// panic メッセージ／`eprintln!` へ埋め込む診断情報を組み立てる（発生時診断。実行中の
/// 正常経路では呼ばれない＝ログ過多にならない）。
pub(crate) fn describe_temp_dir_state() -> String {
    let dir = std::env::temp_dir();
    let writable = std::fs::metadata(&dir)
        .map(|m| !m.permissions().readonly())
        .unwrap_or(false);
    let mut count = 0usize;
    let mut sample = String::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("fandhe-db-") {
                continue;
            }
            count += 1;
            if count <= 5 {
                if !sample.is_empty() {
                    sample.push_str(", ");
                }
                sample.push_str(name);
            }
        }
    }
    format!(
        "temp_dir={} writable={writable} leftover_fandhe_db_entries={count} sample=[{sample}]",
        dir.display()
    )
}

/// テストごとに一意な DB ファイルパスを払い出す（`Storage::open` 等に渡す前提。
/// ファイル自体は作成しない）。名前は `fandhe-db-{crate}-{label}-{pid}-{salt}-{seq}.redb`
/// の形式で、`crate` は呼び出し元クレート（`env!("CARGO_CRATE_NAME")`。unit test では
/// `engine`、結合テストでは各テストバイナリ名に展開される）。
pub fn unique_db_path(label: &str) -> PathBuf {
    validate_label(label);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "fandhe-db-{}-{label}-{}-{:x}-{seq}.redb",
        env!("CARGO_CRATE_NAME"),
        std::process::id(),
        process_salt(),
    ));
    path
}

/// プロセス寿命のバックグラウンドスレッド（accept ループ等）へ `EngineCore` を move
/// するテスト用に、DB ファイルを open 直後に unlink する（Issue #1303）。
///
/// そうしたスレッドはプロセス終了まで生き続けるため `CleanupGuard` を結び付けられず、
/// 実行のたびに一時ファイルが残置していた。unix ではオープン済みファイルを unlink しても
/// ハンドル経由の読み書きは継続でき、実体は最後の close（プロセス終了）で解放される
/// （`Storage` は open 後にパスを再利用しないこと前提）。非 unix（Windows）では開いたままの
/// ファイルを削除できないため従来通り残置する（文書化された例外）。削除失敗はテストを
/// 失敗させず `eprintln!` のみ（他テナント情報・秘密情報を含まない）。
pub fn unlink_open_db_file(path: &Path) {
    #[cfg(unix)]
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "warning: failed to unlink open db file {}: {e}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `unique_db_path` で払い出したパスのファイルを、値が drop されるタイミングで
/// 削除する RAII ガード。既存呼び出しサイトの `CleanupGuard(path.clone())` 記法を
/// 維持するため、フィールドは公開タプルとする。
pub struct CleanupGuard(pub PathBuf);

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        // OS 側の遅延削除（Windows の delete-pending 等）への耐性として有限回リトライする。
        // Drop 内で panic すると（テスト失敗時の巻き戻し中は）二重 panic → abort に
        // なるため、最終手段は `eprintln!` による診断出力に留める。
        const ATTEMPTS: u32 = 5;
        for attempt in 0..ATTEMPTS {
            match std::fs::remove_file(&self.0) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) if attempt + 1 < ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let _ = e;
                }
                Err(e) => {
                    eprintln!(
                        "CleanupGuard: failed to remove {}: {e} ({})",
                        self.0.display(),
                        describe_temp_dir_state()
                    );
                }
            }
        }
    }
}

/// 一時ディレクトリ（外部クレート非依存。dependency-policy 準拠）。`std::fs::create_dir`
/// （`create_dir_all` ではなく排他生成）を使い、同名候補が既に存在する場合は通番を
/// 進めて次候補へ進む。`create_dir_all` の黙った共有（片方の `Drop` が他方の DB を
/// 途中で消す経路）を fail-closed に閉じる。
pub struct TempDir(PathBuf);

impl TempDir {
    /// `label` を含む一意なディレクトリを排他生成する。衝突時は上限付きで再試行し、
    /// 上限超過・その他の I/O エラーは診断情報付きで panic する
    /// （テスト専用コードのため panic による fail-closed を許容する）。
    pub fn new(label: &str) -> Self {
        validate_label(label);
        const ATTEMPTS: u32 = 16;
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..ATTEMPTS {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "fandhe-db-{}-{label}-{}-{:x}-{seq}-dir",
                env!("CARGO_CRATE_NAME"),
                std::process::id(),
                process_salt(),
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return TempDir(dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(e);
                }
                Err(e) => {
                    panic!(
                        "failed to create temp dir {}: {e} ({})",
                        dir.display(),
                        describe_temp_dir_state()
                    );
                }
            }
        }
        panic!(
            "failed to reserve a unique temp dir after {ATTEMPTS} attempts: {} ({})",
            last_err.map(|e| e.to_string()).unwrap_or_default(),
            describe_temp_dir_state()
        );
    }

    /// このディレクトリのパス（`core.rs`/`rls.rs` の unit test が
    /// `Storage::open(dir.path().join("db.redb"))` の形で使う）。
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// このディレクトリ直下の固定ファイル名 `db.redb` へのパス
    /// （`tests/vector_core.rs`・`tests/search_engine.rs` が使う簡便 API）。
    pub fn db_path(&self) -> PathBuf {
        self.0.join("db.redb")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        const ATTEMPTS: u32 = 5;
        for attempt in 0..ATTEMPTS {
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(e) if attempt + 1 < ATTEMPTS => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let _ = e;
                }
                Err(e) => {
                    eprintln!(
                        "TempDir: failed to remove {}: {e} ({})",
                        self.0.display(),
                        describe_temp_dir_state()
                    );
                }
            }
        }
    }
}

/// `core.rs`・`rls.rs` の unit test 向けの薄いラッパ（label 固定）。
/// 呼び出し側の既存記法 `let dir = tempdir();` を維持する。
pub fn tempdir() -> TempDir {
    TempDir::new("tempdir")
}
