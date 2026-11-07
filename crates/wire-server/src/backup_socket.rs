//! 稼働中のサーバーからバックアップを取得する運用者専用の Unix ドメインソケット経路
//! （Issue #2152・RECOVER-13 ポインタ。ADR は `docs/design/backup-snapshot.md` の第 6 節の経路 (ii)）。
//!
//! 責務境界: `--backup-socket <path>` で opt-in されたときだけ、テナント向けの TCP リスナーとは
//! 完全に分離したソケットを作り、接続ごとに出力先 1 行を受けて `EngineCore::export_snapshot` を
//! 呼ぶ。CLI（`wire-server backup --socket`）側の依頼 [`request`] もここに置く。
//!
//! 認可はファイルシステム権限のみで行う: 親ディレクトリが自分の所有かつ group/other の権限なし
//! （0700 相当）でなければ起動を拒否し（fail-closed）、ソケットは 0600 にする。既存のパスは
//! 「接続を拒否される古いソケット」と確かめられる場合だけ置き換え、通常ファイル・ディレクトリ・
//! シンボリックリンク・稼働中のソケットは拒否する。
//!
//! プロトコル（1 接続 1 要求）: 要求は改行終端の 1 行で、出力先の絶対パス（UTF-8。長さ
//! [`MAX_REQUEST_LINE`] バイト以内）。応答は `<結果コード> <メッセージ>` の 1 行で、結果コードは
//! CLI の終了コード（`backup_opt::EXIT_*`）と同一。成功時のメッセージは `tables=N entries=M`。
//! 入力は untrusted として扱い、長すぎる行・読み取りタイムアウト・不正な UTF-8 は固定文言で閉じる。
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;

use crate::backup_opt::{self, EXIT_USAGE};

/// 要求 1 行の最大バイト数（改行を含まない）。超過は読み続けずに閉じる。
pub const MAX_REQUEST_LINE: usize = 4096;
/// 要求を受け取るまでの読み取りタイムアウト。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 応答の書き込みタイムアウト。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// 同時に処理する接続数の上限（超過は応答せずに閉じる。書き出し自体は engine 側で 1 本まで）。
const MAX_CONCURRENT: usize = 4;

/// 起動時に作ったソケットのパスを覚え、`Drop` で片付けるガード。片付けられなくても
/// 次回起動の [`bind`] が古いソケットとして置き換える。
pub struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// ソケットを作る（親ディレクトリの検証・古いソケットの置換・0600）。失敗は固定文言の英語。
pub fn bind(path: &Path) -> Result<(UnixListener, SocketGuard), String> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if path.file_name().is_none() {
        return Err("backup socket path must name a file".into());
    }
    let pm = std::fs::metadata(parent)
        .map_err(|_| "parent directory of the backup socket does not exist".to_string())?;
    if !pm.is_dir() || pm.permissions().mode() & 0o077 != 0 {
        return Err(
            "parent directory of the backup socket must be a directory with no group/other access"
                .into(),
        );
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {
            // 接続できる＝別のサーバーが使用中。拒否された場合だけ古いソケットとみなす。
            match UnixStream::connect(path) {
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)
                        .map_err(|_| "failed to remove stale backup socket".to_string())?;
                }
                _ => return Err("backup socket path is in use".into()),
            }
        }
        Ok(_) => return Err("backup socket path exists and is not a socket".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("cannot inspect backup socket path".into()),
    }
    let listener =
        UnixListener::bind(path).map_err(|_| "failed to bind backup socket".to_string())?;
    let guard = SocketGuard {
        path: path.to_path_buf(),
    };
    // 作成者（自分）の所有でなければ、親が自分の所有ではない。fail-closed で拒否する
    // （guard の Drop がソケットを消す）。
    let owned = std::fs::symlink_metadata(path)
        .map(|m| m.uid() == pm.uid())
        .unwrap_or(false);
    if !owned {
        return Err(
            "parent directory of the backup socket must be owned by the server user".into(),
        );
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "failed to restrict backup socket permissions".to_string())?;
    Ok((listener, guard))
}

/// 接続数を数え、`Drop` で戻すガード。
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// accept ループを別スレッドで起動する。サーバー本体（`main.rs::run_server`）から 1 回だけ呼ばれる。
pub fn spawn_server(listener: UnixListener, core: Arc<EngineCore>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("backup-socket".into())
        .spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for conn in listener.incoming() {
                let conn = match conn {
                    Ok(c) => c,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT {
                    active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let slot = SlotGuard(Arc::clone(&active));
                let core = Arc::clone(&core);
                let spawned = std::thread::Builder::new()
                    .name("backup-socket-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle(conn, &core);
                    });
                // スレッドを作れなかった場合、クロージャごと slot が落ちて数が戻る。
                let _ = spawned;
            }
        })
        .map(|_| ())
}

/// 1 接続を処理する。要求を読み、検証し、書き出し、応答 1 行を返す。
fn handle(conn: UnixStream, core: &EngineCore) {
    if conn.set_read_timeout(Some(REQUEST_TIMEOUT)).is_err()
        || conn.set_write_timeout(Some(RESPONSE_TIMEOUT)).is_err()
    {
        return;
    }
    let (code, msg) = match read_request(&conn) {
        Ok(out) => backup_opt::classify(&core.export_snapshot(&out)),
        Err(m) => (EXIT_USAGE, m.to_string()),
    };
    let mut w = &conn;
    let _ = w.write_all(format!("{code} {msg}\n").as_bytes());
}

/// 要求 1 行を長さ上限つきで読み、出力先パスとして検証する。
fn read_request(conn: &UnixStream) -> Result<PathBuf, &'static str> {
    let mut reader = BufReader::new(conn.take(MAX_REQUEST_LINE as u64 + 1));
    let mut buf = Vec::new();
    if reader.read_until(b'\n', &mut buf).is_err() {
        return Err("failed to read request");
    }
    if buf.last() != Some(&b'\n') || buf.len() > MAX_REQUEST_LINE {
        return Err("invalid request");
    }
    buf.pop();
    let raw = String::from_utf8(buf).map_err(|_| "invalid request")?;
    if !Path::new(&raw).is_absolute() {
        return Err("--out must be an absolute path");
    }
    backup_opt::validate_out(&raw).map_err(|_| "invalid output path")
}

/// CLI 側: ソケットへ出力先を依頼し、(結果コード, メッセージ) を返す。応答の形が不正なら
/// 実行時の失敗（コード 1）として扱う。
pub fn request(socket: &Path, out: &Path) -> Result<(u8, String), String> {
    let mut conn = UnixStream::connect(socket).map_err(|_| {
        "cannot connect to the backup socket (is the server running with --backup-socket?)"
            .to_string()
    })?;
    conn.set_write_timeout(Some(RESPONSE_TIMEOUT))
        .map_err(|_| "failed to configure the backup socket".to_string())?;
    let out = out
        .to_str()
        .ok_or_else(|| "--out must be valid UTF-8".to_string())?;
    conn.write_all(format!("{out}\n").as_bytes())
        .map_err(|_| "failed to send the backup request".to_string())?;
    // 書き出しは長くかかりうるため、応答の待ち時間に上限は設けない。
    let mut line = String::new();
    BufReader::new(&conn)
        .take(MAX_REQUEST_LINE as u64)
        .read_line(&mut line)
        .map_err(|_| "failed to read the backup response".to_string())?;
    let line = line.trim_end_matches('\n');
    let (code, msg) = line.split_once(' ').unwrap_or((line, ""));
    match code.parse::<u8>() {
        Ok(c) if c <= backup_opt::EXIT_PUBLISHED_UNVERIFIED => Ok((c, msg.to_string())),
        _ => Err("unexpected response from the backup socket".to_string()),
    }
}
