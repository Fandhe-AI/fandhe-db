//! `wire-server replica` サブコマンドの引数解釈と資格情報ファイルの読み込み（Issue #2174・親 #2171・
//! RECOVER-15 ポインタ。方式の記録は `docs/design/replication-streaming.md`）。
//!
//! 責務境界: untrusted な CLI 文字列・資格情報ファイルを検証済みの [`ReplicaOptions`] に変換する
//! ところまで。接続・受信・適用のループは [`crate::replica`] が担い、`main.rs::run_replica` が DB を開く前に
//! [`parse`] を呼ぶ（不正値は接続へ進まず起動拒否にする）。
//!
//! - `--db <path>`: レプリカの DB ファイル（`backup` で作ったベースのコピー。**既に存在すること**。
//!   存在しない場合に空の DB を新規作成しない）
//! - `--primary <ip:port>`: プライマリのレプリケーション endpoint（`--replication-bind` のアドレス）。
//!   `--primary-tls-cert` なしではループバックのみで、非ループバックの平文は fail-closed に起動拒否する
//! - `--primary-tls-cert <pem>`（Issue #2177・RECOVER-15 (3)・WIRE-9 ポインタ）: プライマリの TLS 証明書
//!   （プライマリの `--replication-tls-cert` と同じファイルを配る）。指定すると TLS 1.3 で接続し、提示された
//!   証明書がこのピンと完全一致することを確かめてから資格情報を送る（CA チェーン・ホスト名の検証はしない）。
//!   ループバックでも TLS を使う。起動時に読み込み、不正・期限切れ・Ed25519 でない証明書は起動拒否
//! - `--credentials <file>`: `role:password` の 1 行。パスワードは CLI 引数に置かない。ファイルが
//!   group／other から読める権限なら拒否する
//! - `--max-backoff-secs <n>`: 再接続のバックオフの上限（秒。既定 30）
//! - `--batch-records <n>`・`--batch-bytes <n>`: 1 回に適用するレコード数・バイト数の上限
//!
//! 重複指定・不正値・未知のフラグは起動拒否で、エラー文は固定の英語（値・パス・資格情報を含まない）。
//! `--bind`・`--replication-bind`・`--change-log` はこのサブコマンドでは受け付けない（未知のフラグとして拒否）。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::replication_opt::{ReplicationRoles, MAX_ROLE_NAME_LEN};
use crate::tls::client_handshake::PinnedServerCertificate;

/// 適用の停止（整合性違反）の終了コード。再接続して飛ばして進めない。運用者の調査が必要。
pub const EXIT_APPLY_HALTED: u8 = 10;
/// プライマリが拒否した（別系統・再同期が必要・認証失敗など）ときの終了コード。再同期は `replica-resync`（#2176）で、`RESYNC_REQUIRED` は標準エラーの文言で区別する。
pub const EXIT_PRIMARY_REJECTED: u8 = 11;

/// 再接続バックオフ上限の既定・上限（秒。実装既定値）。
pub const DEFAULT_MAX_BACKOFF_SECS: u64 = 30;
pub const MAX_MAX_BACKOFF_SECS: u64 = 3600;
/// 1 バッチのレコード数の既定・上限（上限は engine の 1 回の適用上限に合わせる。実装既定値）。
pub const DEFAULT_BATCH_RECORDS: usize = 256;
pub const MAX_BATCH_RECORDS: usize = engine::storage::MAX_APPLY_RECORDS;
/// 1 バッチのバイト数の既定・下限・上限（実装既定値）。
pub const DEFAULT_BATCH_BYTES: usize = 16 * 1024 * 1024;
pub const MIN_BATCH_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_BYTES: usize = 256 * 1024 * 1024;

/// 資格情報ファイルの最大バイト数とパスワードの最大長（untrusted なファイル向けの資源上限。
/// パスワード長は送信側の Hello の上限と同じ）。
const MAX_CREDENTIALS_FILE_BYTES: u64 = 4 * 1024;
const MAX_PASSWORD_LEN: usize = 1024;

/// 検証済みの起動設定。
pub struct ReplicaOptions {
    /// レプリカの DB ファイル。
    pub db: PathBuf,
    /// プライマリの endpoint（ピンがなければループバック）。
    pub primary: SocketAddr,
    /// プライマリの証明書のピン（Issue #2177）。`Some` なら TLS で接続し、証明書の一致を確かめてから
    /// 資格情報を送る。`None` はループバックの平文。
    pub primary_tls: Option<Arc<PinnedServerCertificate>>,
    /// 認証ロール名。
    pub role: String,
    /// パスワード（ログ・エラー文に出さない）。
    pub password: Vec<u8>,
    /// 再接続のバックオフの上限。
    pub max_backoff: Duration,
    /// 1 バッチのレコード数の上限。
    pub batch_records: usize,
    /// 1 バッチのバイト数の上限。
    pub batch_bytes: usize,
}

impl std::fmt::Debug for ReplicaOptions {
    /// パスワードを含めない（誤って `{:?}` で出しても漏れない）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaOptions")
            .field("primary", &self.primary)
            .field("tls", &self.primary_tls.is_some())
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

fn duplicate(flag: &str) -> String {
    format!("{flag} specified more than once")
}

fn parse_digits(flag: &str, raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{flag} must be a non-negative integer"));
    }
    raw.parse::<u64>()
        .map_err(|_| format!("{flag} is out of range"))
}

fn bounded(flag: &str, raw: &str, min: u64, max: u64) -> Result<u64, String> {
    let n = parse_digits(flag, raw)?;
    if n < min || n > max {
        return Err(format!("{flag} is out of range"));
    }
    Ok(n)
}

/// `replica` サブコマンドの引数（サブコマンド名の後ろ）を検証する。
pub fn parse(args: &[String]) -> Result<ReplicaOptions, String> {
    let mut db: Option<String> = None;
    let mut primary: Option<String> = None;
    let mut credentials: Option<String> = None;
    let mut primary_tls: Option<String> = None;
    let mut backoff: Option<String> = None;
    let mut records: Option<String> = None;
    let mut bytes: Option<String> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let slot = match flag.as_str() {
            "--db" => &mut db,
            "--primary" => &mut primary,
            "--primary-tls-cert" => &mut primary_tls,
            "--credentials" => &mut credentials,
            "--max-backoff-secs" => &mut backoff,
            "--batch-records" => &mut records,
            "--batch-bytes" => &mut bytes,
            _ => return Err("unknown argument for replica".to_string()),
        };
        let value = it
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if value.is_empty() {
            return Err(format!("{flag} must not be empty"));
        }
        if slot.is_some() {
            return Err(duplicate(flag));
        }
        *slot = Some(value.clone());
    }
    let db = db.ok_or("--db is required")?;
    let primary = primary.ok_or("--primary is required")?;
    let credentials = credentials.ok_or("--credentials is required")?;

    let db = PathBuf::from(db);
    // 既存の DB ファイルだけを受け付ける（`Storage::open` は無ければ新規作成するため、ここで拒否する）。
    match std::fs::metadata(&db) {
        Ok(m) if m.is_file() => {}
        _ => return Err("--db must be an existing database file".to_string()),
    }
    let primary: SocketAddr = primary
        .parse()
        .map_err(|_| "--primary must be an ip:port address".to_string())?;
    let primary_tls = load_primary_pin(primary_tls.as_deref().map(Path::new))?;
    check_primary_transport(&primary, primary_tls.is_some())?;
    let (role, password) = load_credentials(Path::new(&credentials))?;
    let max_backoff = match &backoff {
        Some(v) => bounded("--max-backoff-secs", v, 1, MAX_MAX_BACKOFF_SECS)?,
        None => DEFAULT_MAX_BACKOFF_SECS,
    };
    let batch_records = match &records {
        Some(v) => {
            let n = bounded("--batch-records", v, 1, MAX_BATCH_RECORDS as u64)?;
            usize::try_from(n).map_err(|_| "--batch-records is out of range".to_string())?
        }
        None => DEFAULT_BATCH_RECORDS,
    };
    let batch_bytes = match &bytes {
        Some(v) => {
            let n = bounded(
                "--batch-bytes",
                v,
                MIN_BATCH_BYTES as u64,
                MAX_BATCH_BYTES as u64,
            )?;
            usize::try_from(n).map_err(|_| "--batch-bytes is out of range".to_string())?
        }
        None => DEFAULT_BATCH_BYTES,
    };
    Ok(ReplicaOptions {
        db,
        primary,
        primary_tls,
        role,
        password,
        max_backoff: Duration::from_secs(max_backoff),
        batch_records,
        batch_bytes,
    })
}

/// `--primary` の通信路の保護を確かめる（Issue #2177）。ピン（`--primary-tls-cert`）が無ければ平文なので
/// ループバックに限り、非ループバックは TLS 必須として起動拒否する（fail-closed）。`replica` と
/// `replica-resync` で共有する。
pub(crate) fn check_primary_transport(primary: &SocketAddr, pinned: bool) -> Result<(), String> {
    if !pinned && !primary.ip().is_loopback() {
        return Err(
            "--primary must be a loopback address unless --primary-tls-cert is given".to_string(),
        );
    }
    Ok(())
}

/// `--primary-tls-cert` のピンを読み込む（`replica`・`replica-resync` で共有）。未指定は `None`。
/// エラー文は固定で、パス・証明書の中身を含まない。
pub(crate) fn load_primary_pin(
    path: Option<&Path>,
) -> Result<Option<Arc<PinnedServerCertificate>>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    // 掃除（Issue #3094）が消す予約名の形式のファイルは拒否する（fail-closed）。
    crate::backup_opt::reject_reserved_name("--primary-tls-cert", path)?;
    PinnedServerCertificate::load_file(path)
        .map(|pin| Some(Arc::new(pin)))
        .map_err(|e| format!("--primary-tls-cert: {e}"))
}

/// `role:password` の 1 行を読む。通常ファイルで小さく、group／other に読める権限が無いこと。
/// パスワードは最初の `:` より後ろ全部（`:` を含んでよい）。末尾の改行は取り除く。
pub(crate) fn load_credentials(path: &Path) -> Result<(String, Vec<u8>), String> {
    // 掃除（Issue #3094）が消す予約名の形式のファイルは拒否する（fail-closed）。
    crate::backup_opt::reject_reserved_name("--credentials", path)?;
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).map_err(|_| "--credentials cannot be read".to_string())?;
    if !meta.is_file() || meta.len() > MAX_CREDENTIALS_FILE_BYTES {
        return Err("--credentials must be a small regular file".to_string());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("--credentials must not be accessible by group or others".to_string());
    }
    let raw = std::fs::read(path).map_err(|_| "--credentials cannot be read".to_string())?;
    let text = std::str::from_utf8(&raw)
        .map_err(|_| "--credentials must be UTF-8 text".to_string())?
        .trim_end_matches(['\n', '\r']);
    let bad = || "--credentials must hold a single role:password line".to_string();
    if text.contains(['\n', '\r']) {
        return Err(bad());
    }
    let (role, password) = text.split_once(':').ok_or_else(bad)?;
    if !ReplicationRoles::valid_name(role) || role.len() > MAX_ROLE_NAME_LEN {
        return Err(bad());
    }
    if password.is_empty() || password.len() > MAX_PASSWORD_LEN {
        return Err(bad());
    }
    Ok((role.to_string(), password.as_bytes().to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn dir(label: &str) -> PathBuf {
        let d = crate::test_util::unique_temp_path(&format!("replica-opt-{label}"), "");
        std::fs::create_dir(&d).expect("create unique fixture dir");
        d
    }

    fn args(db: &Path, cred: &Path, extra: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = [
            "--db",
            db.to_str().expect("utf-8"),
            "--primary",
            "127.0.0.1:5999",
            "--credentials",
            cred.to_str().expect("utf-8"),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    fn fixture(label: &str, mode: u32) -> (PathBuf, PathBuf, PathBuf) {
        let d = dir(label);
        let db = d.join("db.redb");
        std::fs::write(&db, b"x").expect("db");
        let cred = d.join("cred");
        std::fs::write(&cred, "replica-1:pw:with:colons\n").expect("cred");
        std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(mode)).expect("chmod");
        (d, db, cred)
    }

    #[test]
    fn accepts_a_valid_configuration() {
        let (d, db, cred) = fixture("ok", 0o600);
        let o = parse(&args(&db, &cred, &["--max-backoff-secs", "5"])).expect("parse");
        assert_eq!(o.role, "replica-1");
        assert_eq!(o.password, b"pw:with:colons");
        assert_eq!(o.max_backoff, Duration::from_secs(5));
        assert!(!format!("{o:?}").contains("colons"));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn primary_transport_requires_tls_off_loopback() {
        let lo: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let wan: SocketAddr = "10.0.0.5:1".parse().expect("addr");
        assert!(check_primary_transport(&lo, false).is_ok());
        assert!(check_primary_transport(&lo, true).is_ok());
        assert!(check_primary_transport(&wan, true).is_ok());
        assert!(check_primary_transport(&wan, false).is_err());
    }

    #[test]
    fn an_unreadable_pin_is_refused_without_leaking_the_path() {
        let (d, db, cred) = fixture("pin", 0o600);
        let mut a = args(&db, &cred, &["--primary-tls-cert", "/secret-dir/pin.pem"]);
        a[3] = "10.0.0.5:5999".to_string();
        let e = parse(&a).expect_err("unreadable pin");
        assert!(!e.contains("secret-dir"), "{e}");
        // 重複指定も拒否される。
        assert!(parse(&args(
            &db,
            &cred,
            &["--primary-tls-cert", "a.pem", "--primary-tls-cert", "b.pem"]
        ))
        .is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn rejects_loose_permissions_and_non_loopback_and_duplicates() {
        let (d, db, cred) = fixture("bad", 0o644);
        assert!(parse(&args(&db, &cred, &[])).is_err());
        std::fs::set_permissions(&cred, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let mut a = args(&db, &cred, &[]);
        a[3] = "10.0.0.5:5999".to_string();
        assert!(parse(&a).is_err());
        assert!(parse(&args(&db, &cred, &["--db", "/x"])).is_err());
        assert!(parse(&args(&db, &cred, &["--bind", "127.0.0.1:0"])).is_err());
        assert!(parse(&args(&db, &cred, &["--batch-records", "0"])).is_err());
        let missing = d.join("none.redb");
        assert!(parse(&args(&missing, &cred, &[])).is_err());
        std::fs::remove_dir_all(d).ok();
    }
}
