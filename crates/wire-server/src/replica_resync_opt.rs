//! `wire-server replica-resync` サブコマンドの引数解釈と終了コード（Issue #2176・親 #2171・RECOVER-15・
//! RECOVER-13 ポインタ。方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2176）」）。
//!
//! 責務境界: untrusted な CLI 文字列を検証済みの [`ReplicaResyncOptions`] に変換するところまで。
//! ベースの取得・検査・プライマリへの確認・公開は [`crate::replica_resync`]（unix のみ）が担い、
//! `main.rs::run_replica_resync` が DB にもソケットにも触れる前に [`parse`] を呼ぶ。
//!
//! - `--socket <path>`: 稼働中のプライマリの運用者専用バックアップソケット（`--backup-socket`）。ここから
//!   フルバックアップを取る
//! - `--out <path>`: 新しいレプリカ DB のパス。**まだ存在しない新規パスに限る**（上書きのフラグは設けない。
//!   古いレプリカ DB は運用者が退避または削除する）
//! - `--primary <ip:port>`・`--credentials <file>`: プライマリのレプリケーション endpoint（ピンなしならループバック
//!   のみ）と資格情報ファイル（`replica` と同じ検証。[`crate::replica_opt`]）
//! - `--primary-tls-cert <pem>`（Issue #2177）: プライマリの証明書のピン（`replica` と同じ。指定すると確認接続を
//!   TLS で行い、証明書が一致しなければ何も公開しない）
//!
//! 重複指定・不正値・未知のフラグは拒否で、エラー文は固定の英語（値・パス・資格情報を含まない）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backup_opt;
use crate::replica_opt;
use crate::tls::client_handshake::PinnedServerCertificate;

/// 終了コード: 成功（`replica-resync written: ...`）。
pub const EXIT_OK: u8 = 0;
/// 終了コード: 実行時の失敗（ソケットへの依頼の失敗・プライマリに届かない・I/O）。
pub const EXIT_FAILURE: u8 = backup_opt::EXIT_FAILURE;
/// 終了コード: 引数・パスの拒否（何も開いておらず、どこにも接続していない）。`--out` の親ディレクトリが共有
/// （グループ・その他が書き込める）か他人所有の場合（`ResyncError::InsecureParent`。Issue #3135）も、何も作らず
/// ソケットにもプライマリにも触れずにこのコードで拒否する。
pub const EXIT_USAGE: u8 = backup_opt::EXIT_USAGE;
/// 終了コード: 公開済みで中身は完全だが、後始末（一時名の削除・親ディレクトリの fsync）を確かめられない。
pub const EXIT_NOT_FINALIZED: u8 = backup_opt::EXIT_PUBLISHED_UNVERIFIED;
/// 終了コード: ベースの整合検査で矛盾が見つかり、公開しなかった。
pub const EXIT_INCONSISTENT: u8 = crate::verify_opt::EXIT_INCONSISTENT;
/// 終了コード: ベースの整合検査で検査できない項目だけが残り、公開しなかった。
pub const EXIT_INCOMPLETE: u8 = crate::verify_opt::EXIT_INCOMPLETE;
/// 終了コード: ベースが変更ログの連続性を満たさない・レプリカのベースでない（`replica` の適用停止と同じ値）。
pub const EXIT_BASE_REJECTED: u8 = replica_opt::EXIT_APPLY_HALTED;
/// 終了コード: プライマリが拒否した（`RESYNC_REQUIRED`・別系統・認証失敗など。`replica` と同じ値）。
pub const EXIT_PRIMARY_REJECTED: u8 = replica_opt::EXIT_PRIMARY_REJECTED;

/// 検証済みの引数。
pub struct ReplicaResyncOptions {
    /// プライマリのバックアップソケット。
    pub socket: PathBuf,
    /// 新しいレプリカ DB の絶対パス（まだ存在しない）。
    pub out: PathBuf,
    /// プライマリのレプリケーション endpoint（ピンがなければループバック）。
    pub primary: SocketAddr,
    /// プライマリの証明書のピン（Issue #2177。`replica` と同じ）。`Some` なら確認接続を TLS で行う。
    pub primary_tls: Option<Arc<PinnedServerCertificate>>,
    /// 認証ロール名。
    pub role: String,
    /// パスワード（ログ・エラー文・`Debug` に出さない）。
    pub password: Vec<u8>,
}

impl std::fmt::Debug for ReplicaResyncOptions {
    /// パスワードとパスを含めない（誤って `{:?}` で出しても漏れない）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaResyncOptions")
            .field("primary", &self.primary)
            .field("tls", &self.primary_tls.is_some())
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// `replica-resync` の引数（サブコマンド名の後ろ）を検証する。
pub fn parse(args: &[String]) -> Result<ReplicaResyncOptions, String> {
    let mut socket: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut primary: Option<&str> = None;
    let mut credentials: Option<&str> = None;
    let mut primary_tls: Option<&str> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let slot = match flag.as_str() {
            "--socket" => &mut socket,
            "--out" => &mut out,
            "--primary" => &mut primary,
            "--primary-tls-cert" => &mut primary_tls,
            "--credentials" => &mut credentials,
            _ => return Err("unknown argument for replica-resync".to_string()),
        };
        let value = it
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if value.is_empty() {
            return Err(format!("{flag} must not be empty"));
        }
        if slot.is_some() {
            return Err(format!("{flag} specified more than once"));
        }
        *slot = Some(value.as_str());
    }
    let socket = socket.ok_or("--socket is required")?;
    let out = out.ok_or("--out is required")?;
    let primary = primary.ok_or("--primary is required")?;
    let credentials = credentials.ok_or("--credentials is required")?;

    let primary: SocketAddr = primary
        .parse()
        .map_err(|_| "--primary must be an ip:port address".to_string())?;
    let primary_tls = replica_opt::load_primary_pin(primary_tls.map(Path::new))?;
    replica_opt::check_primary_transport(&primary, primary_tls.is_some())?;
    let (role, password) = replica_opt::load_credentials(Path::new(credentials))?;

    let out = backup_opt::validate_new_path("--out", out)?;
    // サーバー（バックアップソケットの相手）の作業ディレクトリは CLI と異なるため、絶対パスで渡す。
    // 行プロトコルの区切りと衝突する改行を含むパスは拒否する。
    let out = std::path::absolute(&out)
        .map_err(|_| "cannot resolve --out to an absolute path".to_string())?;
    if out.to_str().is_none_or(|s| s.contains('\n')) {
        return Err("--out must be valid UTF-8 without line breaks".to_string());
    }
    check_out_parent(&out)?;
    crate::backup_socket::check_socket_owner(Path::new(socket))?;
    Ok(ReplicaResyncOptions {
        socket: PathBuf::from(socket),
        out,
        primary,
        primary_tls,
        role,
        password,
    })
}

/// `--out` の親ディレクトリが自分の所有で、group/other から書き込めないことを取得前に確かめる
/// （fail-closed）。他者が書ける場所では、検査前の一時ベースを差し替えられ、同一長の改変が
/// `TempBase` の同一性確認（dev・ino・長さ）をすり抜けるため、競合の前提そのものを断つ。
fn check_out_parent(out: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    const MSG: &str = "the parent directory of --out must be owned by the current user and not writable by group/other";
    let parent = out.parent().ok_or_else(|| MSG.to_string())?;
    let m = std::fs::metadata(parent).map_err(|_| MSG.to_string())?;
    if !m.is_dir() || m.mode() & 0o022 != 0 {
        return Err(MSG.to_string());
    }
    let own = crate::backup_socket::probe_uid(parent).map_err(|_| MSG.to_string())?;
    if m.uid() != own {
        return Err(MSG.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["--socket"])).is_err());
        assert!(parse(&v(&["--bogus", "x"])).is_err());
        assert!(parse(&v(&["--socket", "a", "--socket", "b"])).is_err());
        assert!(parse(&v(&[
            "--socket",
            "a",
            "--out",
            "b",
            "--primary",
            "127.0.0.1:1"
        ]))
        .is_err());
        assert!(parse(&v(&["--socket", "", "--out", "b"])).is_err());
    }

    #[test]
    fn rejects_a_non_loopback_primary_before_touching_anything() {
        let e = parse(&v(&[
            "--socket",
            "/nonexistent/s",
            "--out",
            "/nonexistent/o",
            "--primary",
            "10.0.0.5:5999",
            "--credentials",
            "/nonexistent/c",
        ]))
        .expect_err("rejected");
        assert!(e.contains("loopback"));
    }

    #[test]
    fn rejects_a_group_writable_out_parent() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("resync-opt-parent-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(check_out_parent(&d.join("o.redb")).is_err());
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(check_out_parent(&d.join("o.redb")).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}
