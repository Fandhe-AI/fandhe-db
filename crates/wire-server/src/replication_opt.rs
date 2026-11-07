//! `--replication-*` CLI 引数の受理判定と、レプリケーション専用の認証ロール・許可リストの読み込み
//! （Issue #2173・親 #2171・RECOVER-15 ポインタ。方式の記録は `docs/design/replication-streaming.md`）。
//!
//! 責務境界: untrusted な CLI 文字列・資格情報ファイルを検証済みの [`ReplicationSettings`] に変換する
//! ところまで。接続の受け付けと送信は [`crate::replication`] が担い、`main.rs::run_server` が DB を開く前に
//! [`ReplicationArgs::resolve`] を呼ぶ（不正値・組み合わせの不備は listen へ進まず起動拒否にする）。
//!
//! - `--replication-bind <addr>`: レプリケーション専用リスナーのアドレス。テナント向けの `--bind` とは
//!   別のポートで、テナントのセッションは一切受け付けない。**ループバックだけ**（TLS の配線は #2177 の
//!   担当で、それまで非ループバックの平文は fail-closed に起動拒否する）
//! - `--replication-roles <file>`: 認証ロールの資格情報ファイル（`name:phc` の行。`--users` とは別の
//!   名前空間で、テナントのロールではレプリケーションに接続できない）
//! - `--replication-allow <ip|cidr>`: 接続を受け付けるレプリカの送信元アドレス（繰り返し指定可。1 つ以上が必須）
//! - `--replication-max-connections <n>`: 同時接続数の上限
//! - `--replication-max-unacked-bytes <n>`: 送信済みで未確認の量の上限（バックプレッシャ）
//! - `--replication-ack-timeout-secs <n>`: 上限に達したまま確認応答が進まない場合に切断するまでの秒数
//!
//! 重複指定・不正値・組み合わせの不備は起動拒否で、エラー文は固定の英語（パス・値・資格情報を含まない）。
//! すべて `--change-log` が前提（変更ログが送信元）。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use crate::auth::argon2id;
use crate::bind_guard::{GuardedBindAddrs, TransportSecurity};

/// リスナーのアドレスのフラグ名。
pub const BIND_FLAG: &str = "--replication-bind";
/// 認証ロールの資格情報ファイルのフラグ名。
pub const ROLES_FLAG: &str = "--replication-roles";
/// 許可リストのフラグ名（繰り返し可）。
pub const ALLOW_FLAG: &str = "--replication-allow";
/// 同時接続数の上限のフラグ名。
pub const MAX_CONNECTIONS_FLAG: &str = "--replication-max-connections";
/// 未確認量の上限のフラグ名。
pub const MAX_UNACKED_FLAG: &str = "--replication-max-unacked-bytes";
/// 確認応答の待機上限のフラグ名。
pub const ACK_TIMEOUT_FLAG: &str = "--replication-ack-timeout-secs";

/// すべての `--replication-*` フラグ名（`main.rs` の引数ループが 1 本の分岐で受けるための一覧）。
pub const ALL_FLAGS: [&str; 6] = [
    BIND_FLAG,
    ROLES_FLAG,
    ALLOW_FLAG,
    MAX_CONNECTIONS_FLAG,
    MAX_UNACKED_FLAG,
    ACK_TIMEOUT_FLAG,
];

/// 同時接続数の既定・上限（実装既定値で spec 由来ではない）。
pub const DEFAULT_MAX_CONNECTIONS: usize = 8;
pub const MAX_CONNECTIONS_LIMIT: usize = 256;
/// 未確認量の既定・下限・上限（バイト。実装既定値）。
pub const DEFAULT_MAX_UNACKED_BYTES: u64 = 64 * 1024 * 1024;
pub const MIN_MAX_UNACKED_BYTES: u64 = 1024;
pub const MAX_MAX_UNACKED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// 確認応答の待機上限の既定・上限（秒。実装既定値）。
pub const DEFAULT_ACK_TIMEOUT_SECS: u64 = 30;
pub const MAX_ACK_TIMEOUT_SECS: u64 = 3600;

/// 資格情報ファイルの最大バイト数と、ロール名の最大長・ロール数（untrusted なファイル向けの資源上限）。
const MAX_ROLES_FILE_BYTES: u64 = 64 * 1024;
pub const MAX_ROLE_NAME_LEN: usize = 64;
const MAX_ROLES: usize = 256;
/// 許可リストの最大件数。
const MAX_ALLOW_ENTRIES: usize = 256;

/// 固定文言（値・パスを含めない）。
pub const NEEDS_CHANGE_LOG_MESSAGE: &str = "--replication-* flags require --change-log";
pub const NEEDS_BIND_MESSAGE: &str =
    "--replication-roles, --replication-allow and the other --replication-* flags require --replication-bind";
pub const NEEDS_ROLES_MESSAGE: &str = "--replication-bind requires --replication-roles";
pub const NEEDS_ALLOW_MESSAGE: &str =
    "--replication-bind requires at least one --replication-allow";
pub const LOOPBACK_ONLY_MESSAGE: &str =
    "--replication-bind must be a loopback address (TLS for the replication endpoint is not available yet)";

/// 許可リストの 1 項目（IP アドレスまたは CIDR）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Net {
    V4(u32, u8),
    V6(u128, u8),
}

impl Net {
    /// `a.b.c.d`・`a.b.c.d/n`・IPv6 の同形式を受理する。プレフィックス 0（全許可）・範囲外は拒否する。
    fn parse(raw: &str) -> Option<Self> {
        let (addr, prefix) = match raw.split_once('/') {
            Some((a, p)) => {
                if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (a, Some(p.parse::<u8>().ok()?))
            }
            None => (raw, None),
        };
        match addr.parse::<IpAddr>().ok()?.to_canonical() {
            IpAddr::V4(v4) => {
                let p = prefix.unwrap_or(32);
                (1..=32).contains(&p).then_some(())?;
                Some(Self::V4(u32::from(v4), p))
            }
            IpAddr::V6(v6) => {
                let p = prefix.unwrap_or(128);
                (1..=128).contains(&p).then_some(())?;
                Some(Self::V6(u128::from(v6), p))
            }
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip.to_canonical()) {
            (Self::V4(net, p), IpAddr::V4(v4)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(*p)).unwrap_or(0);
                u32::from(v4) & mask == net & mask
            }
            (Self::V6(net, p), IpAddr::V6(v6)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(*p)).unwrap_or(0);
                u128::from(v6) & mask == net & mask
            }
            _ => false,
        }
    }
}

/// 接続を受け付ける送信元アドレスの許可リスト。空のリストは何も許可しない（fail-closed）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowList {
    nets: Vec<Net>,
}

impl AllowList {
    /// 送信元アドレスが許可リストのいずれかに含まれるか。
    pub fn permits(&self, ip: IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(ip))
    }
}

/// レプリケーション専用の認証ロール（`name → Argon2id の PHC`）。テナントのユーザーストアとは別物。
#[derive(Debug, Clone, Default)]
pub struct ReplicationRoles {
    roles: HashMap<String, String>,
}

impl ReplicationRoles {
    /// ロール名に対応する PHC。未知のロールは `None`（呼び出し側がダミー照合で時間差を消す）。
    pub fn phc(&self, role: &str) -> Option<&str> {
        self.roles.get(role).map(String::as_str)
    }

    /// ロール名として許される文字列か（`[A-Za-z0-9._-]`、1〜[`MAX_ROLE_NAME_LEN`] 文字）。
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= MAX_ROLE_NAME_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    }

    /// `name:phc` の行からなるファイルを読む。空行は無視し、重複・不正な行・過大なファイル・
    /// 推奨パラメータ以外の PHC を拒否する（`--users` と同じく、全ロールの KDF コストを揃えて
    /// ロールの存在をタイミングで判別させない）。
    fn load(path: &std::path::Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|_| format!("{ROLES_FLAG} cannot be read"))?;
        if !meta.is_file() || meta.len() > MAX_ROLES_FILE_BYTES {
            return Err(format!("{ROLES_FLAG} must be a small regular file"));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|_| format!("{ROLES_FLAG} cannot be read as UTF-8 text"))?;
        let mut roles = HashMap::new();
        for (idx, raw) in content.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let bad = || format!("{ROLES_FLAG} line {} is malformed", idx.saturating_add(1));
            let (name, phc) = line.split_once(':').ok_or_else(bad)?;
            if !Self::valid_name(name) {
                return Err(bad());
            }
            let (params, _, _) = argon2id::parse_phc(phc).map_err(|_| bad())?;
            if params != argon2id::RECOMMENDED_PARAMS {
                return Err(bad());
            }
            if roles.insert(name.to_string(), phc.to_string()).is_some() {
                return Err(format!(
                    "{ROLES_FLAG} line {} repeats a role",
                    idx.saturating_add(1)
                ));
            }
            if roles.len() > MAX_ROLES {
                return Err(format!("{ROLES_FLAG} has too many roles"));
            }
        }
        if roles.is_empty() {
            return Err(format!("{ROLES_FLAG} must define at least one role"));
        }
        Ok(Self { roles })
    }
}

/// 検証済みのレプリケーション設定。
#[derive(Debug, Clone)]
pub struct ReplicationSettings {
    /// 検証済みの bind 先（ループバックだけ）。
    pub bind: SocketAddr,
    /// 認証ロール。
    pub roles: ReplicationRoles,
    /// 送信元の許可リスト。
    pub allow: AllowList,
    /// 同時接続数の上限。
    pub max_connections: usize,
    /// 送信済みで未確認の量の上限（バイト）。
    pub max_unacked_bytes: u64,
    /// 上限に達したまま確認応答が進まない場合に切断するまでの時間。
    pub ack_timeout: Duration,
}

/// `--replication-*` の受け取り中の値。`main.rs` の引数ループが [`Self::accept`] を呼び、全引数の後に
/// [`Self::resolve`] で検証済みの設定へ変換する。
#[derive(Debug, Default)]
pub struct ReplicationArgs {
    bind: Option<String>,
    roles: Option<PathBuf>,
    allow: Vec<String>,
    max_connections: Option<String>,
    max_unacked: Option<String>,
    ack_timeout: Option<String>,
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

impl ReplicationArgs {
    /// 1 つでも `--replication-*` が指定されたか。
    pub fn is_set(&self) -> bool {
        self.bind.is_some()
            || self.roles.is_some()
            || !self.allow.is_empty()
            || self.max_connections.is_some()
            || self.max_unacked.is_some()
            || self.ack_timeout.is_some()
    }

    /// フラグ 1 つ分を記録する。重複指定（`--replication-allow` を除く）・空の値は拒否する。
    /// 値の範囲検証は [`Self::resolve`] で行う。
    pub fn accept(&mut self, flag: &str, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err(format!("{flag} must not be empty"));
        }
        let slot_taken = |taken: bool| -> Result<(), String> {
            if taken {
                Err(duplicate(flag))
            } else {
                Ok(())
            }
        };
        match flag {
            BIND_FLAG => {
                slot_taken(self.bind.is_some())?;
                self.bind = Some(value.to_string());
            }
            ROLES_FLAG => {
                slot_taken(self.roles.is_some())?;
                self.roles = Some(PathBuf::from(value));
            }
            ALLOW_FLAG => {
                if self.allow.len() >= MAX_ALLOW_ENTRIES {
                    return Err(format!("{ALLOW_FLAG} has too many entries"));
                }
                self.allow.push(value.to_string());
            }
            MAX_CONNECTIONS_FLAG => {
                slot_taken(self.max_connections.is_some())?;
                self.max_connections = Some(value.to_string());
            }
            MAX_UNACKED_FLAG => {
                slot_taken(self.max_unacked.is_some())?;
                self.max_unacked = Some(value.to_string());
            }
            ACK_TIMEOUT_FLAG => {
                slot_taken(self.ack_timeout.is_some())?;
                self.ack_timeout = Some(value.to_string());
            }
            _ => return Err("unknown replication flag".to_string()),
        }
        Ok(())
    }

    /// 組み合わせと値を検証して設定にする。何も指定されていなければ `Ok(None)`（レプリケーション無効）。
    /// DB を開く前に呼ぶ（fail-closed）。
    pub fn resolve(self, change_log: bool) -> Result<Option<ReplicationSettings>, String> {
        if !self.is_set() {
            return Ok(None);
        }
        let Some(bind_raw) = self.bind else {
            return Err(NEEDS_BIND_MESSAGE.to_string());
        };
        if !change_log {
            return Err(NEEDS_CHANGE_LOG_MESSAGE.to_string());
        }
        let roles_path = self.roles.ok_or_else(|| NEEDS_ROLES_MESSAGE.to_string())?;
        if self.allow.is_empty() {
            return Err(NEEDS_ALLOW_MESSAGE.to_string());
        }
        let mut nets = Vec::with_capacity(self.allow.len());
        for raw in &self.allow {
            nets.push(
                Net::parse(raw)
                    .ok_or_else(|| format!("{ALLOW_FLAG} must be an IP address or CIDR"))?,
            );
        }
        let max_connections = match self.max_connections.as_deref() {
            None => DEFAULT_MAX_CONNECTIONS,
            Some(raw) => {
                let n = parse_digits(MAX_CONNECTIONS_FLAG, raw)?;
                usize::try_from(n)
                    .ok()
                    .filter(|n| (1..=MAX_CONNECTIONS_LIMIT).contains(n))
                    .ok_or_else(|| format!("{MAX_CONNECTIONS_FLAG} is out of range"))?
            }
        };
        let max_unacked_bytes = match self.max_unacked.as_deref() {
            None => DEFAULT_MAX_UNACKED_BYTES,
            Some(raw) => {
                let n = parse_digits(MAX_UNACKED_FLAG, raw)?;
                if !(MIN_MAX_UNACKED_BYTES..=MAX_MAX_UNACKED_BYTES).contains(&n) {
                    return Err(format!("{MAX_UNACKED_FLAG} is out of range"));
                }
                n
            }
        };
        let ack_secs = match self.ack_timeout.as_deref() {
            None => DEFAULT_ACK_TIMEOUT_SECS,
            Some(raw) => {
                let n = parse_digits(ACK_TIMEOUT_FLAG, raw)?;
                if !(1..=MAX_ACK_TIMEOUT_SECS).contains(&n) {
                    return Err(format!("{ACK_TIMEOUT_FLAG} is out of range"));
                }
                n
            }
        };
        // ループバックだけを受け付ける（TLS の配線は #2177。それまで平文を外へ晒す経路を作らない）。
        let guarded = GuardedBindAddrs::resolve(&bind_raw, TransportSecurity::Cleartext)
            .map_err(|_| LOOPBACK_ONLY_MESSAGE.to_string())?;
        let bind = match guarded.addrs() {
            [only] => *only,
            _ => return Err(format!("{BIND_FLAG} must resolve to exactly one address")),
        };
        let roles = ReplicationRoles::load(&roles_path)?;
        Ok(Some(ReplicationSettings {
            bind,
            roles,
            allow: AllowList { nets },
            max_connections,
            max_unacked_bytes,
            ack_timeout: Duration::from_secs(ack_secs),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn allow_list_matches_exact_and_cidr() {
        let list = AllowList {
            nets: vec![
                Net::parse("127.0.0.1").expect("exact"),
                Net::parse("10.1.0.0/16").expect("cidr"),
                Net::parse("::1").expect("v6"),
            ],
        };
        assert!(list.permits(ip("127.0.0.1")));
        assert!(!list.permits(ip("127.0.0.2")));
        assert!(list.permits(ip("10.1.200.3")));
        assert!(!list.permits(ip("10.2.0.1")));
        assert!(list.permits(ip("::1")));
        // IPv4-mapped IPv6 は IPv4 として照合する。
        assert!(list.permits(ip("::ffff:127.0.0.1")));
        assert!(!AllowList::default().permits(ip("127.0.0.1")));
    }

    #[test]
    fn allow_entries_reject_malformed_and_match_all() {
        for bad in [
            "",
            "0.0.0.0/0",
            "::/0",
            "1.2.3.4/33",
            "1.2.3.4/",
            "1.2.3.4/-1",
            "host",
            "1.2.3/24",
            "1.2.3.4/0x8",
            "1.2.3.4/1000",
        ] {
            assert!(Net::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn role_names_are_restricted() {
        assert!(ReplicationRoles::valid_name("replica-1.a_b"));
        assert!(!ReplicationRoles::valid_name(""));
        assert!(!ReplicationRoles::valid_name("a:b"));
        assert!(!ReplicationRoles::valid_name(
            &"a".repeat(MAX_ROLE_NAME_LEN + 1)
        ));
    }

    #[test]
    fn duplicates_and_missing_prerequisites_are_rejected() {
        let mut a = ReplicationArgs::default();
        a.accept(BIND_FLAG, "127.0.0.1:0").expect("first");
        assert!(a.accept(BIND_FLAG, "127.0.0.1:1").is_err());
        assert!(a.accept(ALLOW_FLAG, "").is_err());
        assert!(a.accept("--replication-nope", "1").is_err());
        // --change-log なし。
        assert_eq!(
            a.resolve(false).expect_err("needs change log"),
            NEEDS_CHANGE_LOG_MESSAGE
        );
        // フラグなしは無効のまま。
        assert!(ReplicationArgs::default()
            .resolve(false)
            .expect("off")
            .is_none());
        // bind なしの従属フラグ。
        let mut b = ReplicationArgs::default();
        b.accept(ALLOW_FLAG, "127.0.0.1").expect("allow");
        assert_eq!(b.resolve(true).expect_err("needs bind"), NEEDS_BIND_MESSAGE);
    }

    #[test]
    fn non_loopback_bind_is_refused() {
        let mut a = ReplicationArgs::default();
        a.accept(BIND_FLAG, "0.0.0.0:0").expect("bind");
        a.accept(ROLES_FLAG, "/nonexistent").expect("roles");
        a.accept(ALLOW_FLAG, "127.0.0.1").expect("allow");
        assert_eq!(
            a.resolve(true).expect_err("loopback"),
            LOOPBACK_ONLY_MESSAGE
        );
    }
}
