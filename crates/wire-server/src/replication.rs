//! ストリーミングレプリケーションの送信側（プライマリ）の endpoint（Issue #2173・親 #2171・RECOVER-15
//! ポインタ。方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2173）」）。
//!
//! 責務境界: `--replication-bind` で opt-in されたときだけ、テナント向けの pg wire リスナーとは別の
//! TCP リスナーを作り、許可リストを通った接続を専用ロールで認証し、承認済みのレプリカへ engine の変更ログの
//! レコード（`change_log` 表の値そのまま）を commit 番号（LSN）順に送る。レコードの読み出しと刈り込みからの
//! 保護は engine（`EngineCore::read_change_log_from`・`pin_change_log_for_replica`）が担い、本モジュールは
//! 認証・フレーム・確認応答（ack）・バックプレッシャ・接続数だけを担う。受信側の適用ループは #2174。
//!
//! プロトコル（1 接続 1 レプリカ。フレーム = `型 u8 | 本体長 u32(BE) | 本体`、整数は BE）:
//!
//! | 方向 | 型 | 本体 |
//! | ---- | -- | ---- |
//! | C→S | `H` Hello | 版数 u16（1 または 2）\| ロール名長 u8 \| ロール名 \| パスワード（残り全部） |
//! | S→C | `I` Identity | DB 識別子 16B \| timeline u64 \| プライマリの最新 LSN u64（認証成功の直後。レプリカは自分のベースと照合する） |
//! | C→S | `S` Start | DB 識別子 16B \| timeline u64 \| 適用済みの最後の LSN u64（次から送る） |
//! | S→C | `O` Ok | DB 識別子 16B \| timeline u64 \| プライマリの最新 LSN u64 |
//! | S→C | `W` Record | LSN u64 \| レコードのバイト列（D3 の自己完結形式） |
//! | S→C | `P` Heartbeat | プライマリの最新 LSN u64（追いついて暇な間、約 1 秒ごと） |
//! | C→S | `K` Ack | 永続化（適用の commit）済みの最後の LSN u64（単調増加・送信済み以下） |
//! | C→S | `R` Received | 受け取り済み（適用前）の最後の LSN u64（版数 2 のみ。単調増加・送信済み以下。保持位置は進めない） |
//! | S→C | `E` Error | コード u8（文言は載せない） |
//!
//! TLS（Issue #2177・RECOVER-15 (3)・WIRE-9 ポインタ）: `--replication-tls-cert`／`--replication-tls-key` を構成すると
//! リスナーは**全接続を TLS 1.3 必須**にする（ループバックの送信元も含む。平文への降格経路は無い）。接続の順序は
//! 許可リスト → 失敗バックオフ → 認証前の枠 → 先頭 1 バイトが TLS の handshake レコードか確認（違えば何も返さず閉じる。
//! 平文の Hello は解釈しない）→ TLS ハンドシェイク → 上記のフレーム（Hello 以降）。資格情報は TLS 確立後にだけ読む。
//! 平文・TLS の差は [`crate::replication_conn`] の読み書きの半分が吸収する。
//!
//! 入力は untrusted: 長さは確保前に上限検証し、フレームの型・長さ・値（ack の単調性と上限）を検証して、
//! 違反は固定コードの `E` を返して切断する。認証失敗は未知のロールと誤パスワードで同じ KDF コストと固定遅延
//! （`auth::AUTH_FAILURE_DELAY`）を課し、区別できる応答を返さない。許可リスト外の送信元には何も返さず閉じる。
//!
//! バックプレッシャ: 送信済みで未確認の量が `max_unacked_bytes` に達したら送信を止め（フロー制御）、
//! `ack_timeout` の間 ack が進まなければそのレプリカだけを切る。書き込みタイムアウトも同じ時間で、読まない
//! レプリカの書き込みで送信スレッドが固まり続けない。engine は読み取りトランザクションしか取らないため、
//! レプリカの切断・遅延・停止がプライマリの書き込みを止めることはない（既定は非同期）。
//!
//! 同期レプリケーション（Issue #2245。`--replication-sync-standby` で opt-in）: 同期対象のロールの接続は
//! 1 本までで（2 本目は `BUSY`）、`K`・`R` の位置を engine の `SyncCommitCoordinator` へ報告する。確認を待つのは
//! テナント向けの応答境界（`sync_commit_wait`）で、本モジュールは位置の報告と、待機の開始を検知して
//! commit のポーリングを早めることだけを担う。
#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::storage::StreamError;

use crate::auth::{argon2id, AUTH_FAILURE_DELAY};
use crate::replication_conn::{self, ConnReader, ConnWriter, ShutdownHandle};
use crate::replication_opt::{ReplicationRoles, ReplicationSettings, MAX_ROLE_NAME_LEN};
use crate::tls::server_handshake::perform_server_handshake_with_timeout;

/// プロトコルの最小の版数（Hello で受け取る。`K` だけを送る）。
pub const PROTOCOL_VERSION: u16 = 1;
/// 版数 2: 受信済みの位置を知らせる `R` を送ってよい（同期レプリケーションの `remote_write` 用）。
pub const PROTOCOL_VERSION_RECEIVED: u16 = 2;

/// フレーム型。
pub const T_HELLO: u8 = b'H';
pub const T_IDENTITY: u8 = b'I';
pub const T_START: u8 = b'S';
pub const T_OK: u8 = b'O';
pub const T_RECORD: u8 = b'W';
pub const T_HEARTBEAT: u8 = b'P';
pub const T_ACK: u8 = b'K';
pub const T_RECEIVED: u8 = b'R';
pub const T_ERROR: u8 = b'E';

/// `E` フレームのコード（文言は載せない）。
pub mod code {
    /// 不正なフレーム・値（型・長さ・版数・ack の違反）。
    pub const BAD_REQUEST: u8 = 1;
    /// 認証失敗（未知のロールと誤パスワードを区別しない）。
    pub const AUTH_FAILED: u8 = 2;
    /// 接続数の上限。
    pub const BUSY: u8 = 3;
    /// DB 識別子または timeline が一致しない。
    pub const FOREIGN: u8 = 4;
    /// 開始位置が刈り込み済み（再同期が必要。#2176）。
    pub const RESYNC_REQUIRED: u8 = 5;
    /// 開始位置がプライマリの最新より先。
    pub const AHEAD: u8 = 6;
    /// プライマリ側の内部エラー（変更ログが無効・読み出し失敗）。
    pub const INTERNAL: u8 = 7;
    /// 未確認量の上限に達したまま ack が進まない（切断）。
    pub const BACKPRESSURE: u8 = 8;
}

/// Hello の本体の最大長（版数 2 + ロール名長 1 + ロール名 + パスワード）。
const MAX_PASSWORD_LEN: usize = 1024;
const MAX_HELLO_BODY: usize = 2 + 1 + MAX_ROLE_NAME_LEN + MAX_PASSWORD_LEN;
const START_BODY_LEN: usize = 16 + 8 + 8;
const ACK_BODY_LEN: usize = 8;
/// 認証前の Hello と、認証後の Start を受け取るまでに許す時間（フレームごと。1 バイトずつ送り続けても
/// 延びない）。認証前の接続が枠を占有し続けないよう短くする。
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// TLS ハンドシェイク全体の絶対期限（認証前の枠を占有できる時間の上限）。
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS の handshake レコードの先頭バイト（content type 22）。
const TLS_HANDSHAKE_RECORD: u8 = 0x16;
/// 認証前の同時接続数の上限（認証済みの枠 `max_connections` とは別枠）。
const MAX_PRE_AUTH: usize = 16;
/// 1 バッチで読むバイト数の上限（`max_unacked` とは別の固定上限。1 件でこれを超えるレコードは 1 件だけ返す）。
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// 送信元ごとの認証失敗の表の最大件数と、無料で許す失敗回数、バックオフの上限・失効時間。
const MAX_FAILURE_ENTRIES: usize = 1024;
const FREE_FAILURES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const FAILURE_TTL: Duration = Duration::from_secs(600);
/// 監視スレッドの確認間隔。
const WATCHDOG_STEP: Duration = Duration::from_millis(20);
/// 追いついた状態での新規 commit のポーリング間隔と、ハートビートの間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
/// 1 回の読み出しで取るレコード数の上限。
const BATCH_RECORDS: usize = 256;
/// 上限超過の通知（接続数・バックプレッシャ）を書く際の書き込み期限。
const NOTICE_TIMEOUT: Duration = Duration::from_secs(1);

/// フレームの読み取り失敗（理由は呼び出し側で固定コードへ写す）。
#[derive(Debug, PartialEq, Eq)]
enum FrameError {
    /// 切断・タイムアウト・I/O エラー。
    Closed,
    /// 本体長が上限を超える。
    TooLarge,
}

/// 接続数を数え、`Drop` で戻すガード。
struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 送信元アドレスごとの認証失敗の累積（総当たり攻撃の抑止）。失敗が [`FREE_FAILURES`] 回を超えると、
/// 最後の失敗から指数的に伸びるバックオフの間（上限 [`MAX_BACKOFF`]）、その送信元の接続を読まずに閉じる。
/// 成功で消える。表は [`MAX_FAILURE_ENTRIES`] 件で有界（満杯なら失効分、無ければ最も古い失敗を捨てる）。
#[derive(Default)]
struct FailureTable {
    entries: std::collections::HashMap<std::net::IpAddr, (u32, Instant)>,
}

impl FailureTable {
    fn backoff(failures: u32) -> Duration {
        let over = failures.saturating_sub(FREE_FAILURES);
        let secs = 1u64.checked_shl(over).unwrap_or(u64::MAX);
        Duration::from_secs(secs).min(MAX_BACKOFF)
    }

    /// 送信元が現在バックオフ中か。
    fn blocked(&self, ip: std::net::IpAddr, now: Instant) -> bool {
        self.entries.get(&ip.to_canonical()).is_some_and(|(n, at)| {
            *n >= FREE_FAILURES && now.saturating_duration_since(*at) < Self::backoff(*n)
        })
    }

    fn record_failure(&mut self, ip: std::net::IpAddr, now: Instant) {
        let ip = ip.to_canonical();
        if !self.entries.contains_key(&ip) && self.entries.len() >= MAX_FAILURE_ENTRIES {
            self.entries
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < FAILURE_TTL);
            if self.entries.len() >= MAX_FAILURE_ENTRIES {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(k, _)| *k);
                if let Some(k) = oldest {
                    self.entries.remove(&k);
                }
            }
        }
        let e = self.entries.entry(ip).or_insert((0, now));
        // 長く失敗が無かった送信元は数え直す。
        if now.saturating_duration_since(e.1) >= FAILURE_TTL {
            e.0 = 0;
        }
        e.0 = e.0.saturating_add(1);
        e.1 = now;
    }

    fn clear(&mut self, ip: std::net::IpAddr) {
        self.entries.remove(&ip.to_canonical());
    }
}

/// リスナーが全接続で共有する状態。
struct Server {
    core: Arc<EngineCore>,
    cfg: ReplicationSettings,
    /// 認証済みの接続数と、認証前の接続数（別枠）。
    active: Arc<AtomicUsize>,
    pre_auth: Arc<AtomicUsize>,
    failures: Mutex<FailureTable>,
}

impl Server {
    fn failures(&self) -> MutexGuard<'_, FailureTable> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// `buf` を満たすまで読む。`deadline` を過ぎる、または切断されたら `Closed`。
fn read_exact_by(
    stream: &mut ConnReader,
    buf: &mut [u8],
    deadline: Option<Instant>,
) -> Result<(), FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let timeout = match deadline {
            Some(d) => {
                let rem = d.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    return Err(FrameError::Closed);
                }
                Some(rem)
            }
            None => None,
        };
        stream
            .set_read_timeout(timeout)
            .map_err(|_| FrameError::Closed)?;
        let dst = buf.get_mut(filled..).ok_or(FrameError::Closed)?;
        match stream.read(dst) {
            Ok(0) | Err(_) => return Err(FrameError::Closed),
            Ok(n) => filled = filled.checked_add(n).ok_or(FrameError::Closed)?,
        }
    }
    Ok(())
}

/// 1 フレームを読む。本体長は `max_body` 以内でなければ確保せずに `TooLarge`。
fn read_frame(
    stream: &mut ConnReader,
    max_body: usize,
    deadline: Option<Instant>,
) -> Result<(u8, Vec<u8>), FrameError> {
    let mut header = [0u8; 5];
    read_exact_by(stream, &mut header, deadline)?;
    let [ty, l0, l1, l2, l3] = header;
    let len =
        usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).map_err(|_| FrameError::TooLarge)?;
    if len > max_body {
        return Err(FrameError::TooLarge);
    }
    let mut body = vec![0u8; len];
    read_exact_by(stream, &mut body, deadline)?;
    Ok((ty, body))
}

/// 1 フレームを書く（本体は複数片の連結）。本体長が `u32` に収まらなければ `InvalidInput`。
fn write_frame(w: &mut impl Write, ty: u8, parts: &[&[u8]]) -> std::io::Result<()> {
    let total = parts
        .iter()
        .try_fold(0usize, |acc, p| acc.checked_add(p.len()))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    w.write_all(&[ty])?;
    w.write_all(&total.to_be_bytes())?;
    for p in parts {
        w.write_all(p)?;
    }
    Ok(())
}

/// 固定コードの `E` を書く（失敗は無視。切断直前の通知）。
fn send_error(w: &mut ConnWriter, code: u8) {
    let _ = w.set_write_timeout(Some(NOTICE_TIMEOUT));
    let _ = write_frame(w, T_ERROR, &[&[code]]);
    let _ = w.flush();
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Hello の本体を (版数, ロール名, パスワード) に分解する。形式違反・未知の版数は `None`。
fn parse_hello(body: &[u8]) -> Option<(u16, &str, &[u8])> {
    let version = u16::from_be_bytes(body.get(..2)?.try_into().ok()?);
    if version != PROTOCOL_VERSION && version != PROTOCOL_VERSION_RECEIVED {
        return None;
    }
    let role_len = usize::from(*body.get(2)?);
    if role_len == 0 || role_len > MAX_ROLE_NAME_LEN {
        return None;
    }
    let role_end = 3usize.checked_add(role_len)?;
    let role = std::str::from_utf8(body.get(3..role_end)?).ok()?;
    let password = body.get(role_end..)?;
    (password.len() <= MAX_PASSWORD_LEN).then_some((version, role, password))
}

/// ロールとパスワードを照合する。未知のロール・不正なロール名は既知ロールと同じコストのダミー照合を
/// 行い、失敗は経路によらず `start` から [`AUTH_FAILURE_DELAY`] が経つまで待ってから返す。
fn authenticate(roles: &ReplicationRoles, role: &str, password: &[u8], start: Instant) -> bool {
    let known = ReplicationRoles::valid_name(role)
        .then(|| roles.phc(role))
        .flatten();
    let phc = known.unwrap_or_else(|| crate::auth::dummy_phc());
    let matched = argon2id::verify_phc(phc, password).unwrap_or(false);
    let ok = known.is_some() && matched;
    if !ok {
        let elapsed = start.elapsed();
        if elapsed < AUTH_FAILURE_DELAY {
            std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
        }
    }
    ok
}

/// 送信済みで未確認のレコードの勘定（書き込み側・ack 読み取り側・監視で共有する）。
struct Inflight {
    /// `(LSN, バイト数, 送信を始めた時刻)`。送信順。先頭が最も古い未確認。
    queue: VecDeque<(u64, u64, Instant)>,
    bytes: u64,
    /// 確認応答済みの最後の LSN と、送信済みの最後の LSN。
    acked: u64,
    /// 受け取り済み（`R`）の最後の LSN（`acked` 以上。保持位置とは無関係）。
    received: u64,
    sent: u64,
}

/// 切断の理由（監視が立てる。0 は無し）。
const CUT_NONE: u8 = 0;
const CUT_ACK_TIMEOUT: u8 = 1;
const CUT_LAG: u8 = 2;
const CUT_INTERNAL: u8 = 3;
/// ack 読み取りが見つけたプロトコル違反（型・長さ・値）。
const CUT_PROTOCOL: u8 = 4;
/// 監視がプライマリの最新 LSN を読み直す間隔。送信ループの状態（窓が満杯・書き込みが塞がる）に依らず遅れを判定する。
const LAG_POLL: Duration = Duration::from_millis(100);

struct Shared {
    inflight: Mutex<Inflight>,
    /// ack の違反・切断。立ったら書き込み側は終了する。
    failed: AtomicBool,
    /// 送信ループが終わった。監視が抜ける合図。
    done: AtomicBool,
    /// 監視が決めた切断の理由。
    cut: std::sync::atomic::AtomicU8,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inflight> {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// ack を読み続ける（レプリカ→プライマリの方向。別スレッド）。違反・切断で `failed` を立てて終わる。
fn read_acks(
    mut stream: ConnReader,
    closer: ShutdownHandle,
    shared: &Shared,
    pin: &engine::storage::ReplicaPin,
    version: u16,
    standby: Option<&engine::storage::StandbyHandle>,
) {
    // 通常の切断（`Closed`）とプロトコル違反（型・長さ・値）を区別する。違反は理由を立てて抜け、送信ループが
    // `BAD_REQUEST` を書いてから切る（書き込みは送信ループだけが行うので、フレームは交錯しない）。
    let violation = loop {
        let (is_received, lsn) = match read_frame(&mut stream, ACK_BODY_LEN, None) {
            Ok((T_ACK, body)) if body.len() == ACK_BODY_LEN => match be_u64(&body) {
                Some(l) => (false, l),
                None => break true,
            },
            // `R` は版数 2 の接続だけが送ってよい。
            Ok((T_RECEIVED, body))
                if version >= PROTOCOL_VERSION_RECEIVED && body.len() == ACK_BODY_LEN =>
            {
                match be_u64(&body) {
                    Some(l) => (true, l),
                    None => break true,
                }
            }
            Err(FrameError::Closed) => break false,
            _ => break true,
        };
        let mut g = shared.lock();
        if is_received {
            // 受信済みの位置: 単調増加で、送信済みの範囲内（保持位置・未確認の勘定は動かさない）。
            if lsn < g.received.max(g.acked) || lsn > g.sent {
                break true;
            }
            g.received = lsn;
            drop(g);
            if let Some(s) = standby {
                s.report_received(lsn);
            }
            continue;
        }
        // 単調増加で、送信済みの範囲内でなければならない（未送信の位置への ack は不正）。
        if lsn < g.acked || lsn > g.sent {
            break true;
        }
        while g.queue.front().is_some_and(|(l, _, _)| *l <= lsn) {
            if let Some((_, len, _)) = g.queue.pop_front() {
                g.bytes = g.bytes.saturating_sub(len);
            }
        }
        g.acked = lsn;
        g.received = g.received.max(lsn);
        drop(g);
        pin.advance(lsn);
        if let Some(s) = standby {
            s.report_durable(lsn);
        }
    };
    if violation {
        // 他の理由（監視の切断）が先に立っていればそちらを優先する。
        let _ = shared.cut.compare_exchange(
            CUT_NONE,
            CUT_PROTOCOL,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        // 送信ループが通知を書くまで接続は閉じない（書き込みが塞がる場合は監視が有界の待機で閉じる）。
        return;
    }
    shared.failed.store(true, Ordering::Release);
    closer.shutdown();
}

/// 監視（別スレッド）。書き込みが `write`／`flush` でブロックしても判定できるよう、送信ループとは独立に動く。
/// 最も古い未確認レコードが送信を始めてから `ack_timeout` を超えた、またはプライマリの最新との差（確認応答済みの
/// 位置からの遅れ）が `max_lag_records` を超えたら切断の理由を立てる。送信ループが [`NOTICE_TIMEOUT`] の
/// うちに通知を書いて抜けなければ、`shutdown` でブロックを解く。
fn watchdog(closer: ShutdownHandle, core: &EngineCore, shared: &Shared, cfg: &ReplicationSettings) {
    // プライマリの最新 LSN は送信ループのキャッシュに頼らず、この監視が read トランザクションで自分で読む
    // （窓が満杯・書き込みが塞がって送信ループが読まない間も遅れを判定するため）。取得に失敗したら
    // 保持位置を無期限に残さないよう、fail-closed に切断する。
    let mut current = applied_floor(shared);
    let mut last_poll: Option<Instant> = None;
    while !shared.done.load(Ordering::Acquire) && !shared.failed.load(Ordering::Acquire) {
        std::thread::sleep(WATCHDOG_STEP);
        let mut poll_failed = false;
        if last_poll.is_none_or(|t| t.elapsed() >= LAG_POLL) {
            last_poll = Some(Instant::now());
            match core.read_change_log_from(0, 0, 0) {
                Ok(b) => current = b.current_lsn,
                Err(_) => poll_failed = true,
            }
        }
        let reason = if poll_failed {
            CUT_INTERNAL
        } else {
            let g = shared.lock();
            if g.queue
                .front()
                .is_some_and(|(_, _, at)| at.elapsed() >= cfg.ack_timeout)
            {
                CUT_ACK_TIMEOUT
            } else if current.saturating_sub(g.acked) > cfg.max_lag_records {
                CUT_LAG
            } else {
                CUT_NONE
            }
        };
        // ack 読み取りが先にプロトコル違反を立てていればそれを使う（通知を書く有界の待機は同じ）。
        let reason = match shared.cut.load(Ordering::Acquire) {
            CUT_NONE => reason,
            already => already,
        };
        if reason == CUT_NONE {
            continue;
        }
        let _ = shared
            .cut
            .compare_exchange(CUT_NONE, reason, Ordering::AcqRel, Ordering::Acquire);
        let grace = Instant::now() + NOTICE_TIMEOUT;
        while Instant::now() < grace && !shared.done.load(Ordering::Acquire) {
            std::thread::sleep(WATCHDOG_STEP);
        }
        closer.shutdown();
        return;
    }
}

/// 監視の最初の「最新 LSN」の仮置き（確認応答済みの位置。最初の取得までは遅れ 0 とみなす）。
fn applied_floor(shared: &Shared) -> u64 {
    shared.lock().acked
}

/// `StreamError` を `E` のコードへ写す。
fn stream_error_code(e: &StreamError) -> u8 {
    match e {
        StreamError::Pruned => code::RESYNC_REQUIRED,
        StreamError::Ahead => code::AHEAD,
        _ => code::INTERNAL,
    }
}

/// TLS リスナーで、先頭 1 バイトが TLS の handshake レコードか（読み出しは消費しない）。平文の Hello を
/// TLS ハンドシェイクへ渡して平文の alert を返したりしないよう、違えば呼び出し側が何も返さずに閉じる。
fn starts_like_tls(stream: &TcpStream) -> bool {
    if stream.set_read_timeout(Some(HELLO_TIMEOUT)).is_err() {
        return false;
    }
    let mut probe = [0u8; 1];
    matches!(stream.peek(&mut probe), Ok(1)) && probe == [TLS_HANDSHAKE_RECORD]
}

/// 受け付けた接続を読み書きの半分にする。TLS 構成なら TLS ハンドシェイクまでをここで終える（失敗は `None`。
/// 何も返さずに閉じる。ハンドシェイクの失敗は認証失敗の表へ数えない: 資格情報の総当たりではなく、
/// 認証前の枠と絶対期限で資源は有界）。資格情報を読むのは TLS 確立後だけ。
fn open_conn(stream: TcpStream, cfg: &ReplicationSettings) -> Option<(ConnReader, ConnWriter)> {
    let Some(tls) = cfg.tls.as_ref() else {
        return replication_conn::plain(stream).ok();
    };
    let mut stream = stream;
    if !starts_like_tls(&stream) {
        engine::log_stderr!(
            "wire-server: replication: refused a connection that did not start TLS"
        );
        return None;
    }
    let session = match perform_server_handshake_with_timeout(
        &mut stream,
        Arc::clone(tls),
        TLS_HANDSHAKE_TIMEOUT,
    ) {
        Ok(s) => s,
        Err(_) => {
            engine::log_stderr!("wire-server: replication: TLS handshake failed");
            return None;
        }
    };
    replication_conn::tls(stream, session).ok()
}

/// 1 接続を処理する（TLS 確立 → 認証 → 開始位置 → 送信ループ）。
fn handle(stream: TcpStream, peer: std::net::IpAddr, srv: &Server, pre_auth: SlotGuard) {
    let (core, cfg) = (&*srv.core, &srv.cfg);
    let _ = stream.set_nodelay(true);
    let Some((mut rd, mut wr)) = open_conn(stream, cfg) else {
        return;
    };
    if wr.set_write_timeout(Some(cfg.ack_timeout)).is_err() {
        return;
    }
    let auth_start = Instant::now();
    let deadline = Some(Instant::now() + HELLO_TIMEOUT);
    let (ty, body) = match read_frame(&mut rd, MAX_HELLO_BODY, deadline) {
        Ok(f) => f,
        Err(_) => return send_error(&mut wr, code::BAD_REQUEST),
    };
    let parsed = if ty == T_HELLO {
        parse_hello(&body)
    } else {
        None
    };
    let Some((version, role, password)) = parsed else {
        return send_error(&mut wr, code::BAD_REQUEST);
    };
    if !authenticate(&cfg.roles, role, password, auth_start) {
        srv.failures().record_failure(peer, Instant::now());
        return send_error(&mut wr, code::AUTH_FAILED);
    }
    srv.failures().clear(peer);
    let role = role.to_string();
    // 認証前の枠を返し、認証済みの枠を取る（満杯なら断る）。
    drop(pre_auth);
    if srv.active.fetch_add(1, Ordering::AcqRel) >= cfg.max_connections {
        srv.active.fetch_sub(1, Ordering::AcqRel);
        return send_error(&mut wr, code::BUSY);
    }
    let _slot = SlotGuard(Arc::clone(&srv.active));

    // 認証成功の直後に識別子と最新 LSN を知らせる。レプリカは自分のベース（バックアップ）の識別子と
    // 照合してから Start を送る（サーバー側も Start で再度照合する）。
    let Some(identity) = core.change_log_stream_identity() else {
        return send_error(&mut wr, code::INTERNAL);
    };
    let first_current = match core.read_change_log_from(0, 0, 0) {
        Ok(b) => b.current_lsn,
        Err(e) => return send_error(&mut wr, stream_error_code(&e)),
    };
    {
        let ok = write_frame(
            &mut wr,
            T_IDENTITY,
            &[
                &identity.db_id,
                &identity.timeline.to_be_bytes(),
                &first_current.to_be_bytes(),
            ],
        )
        .and_then(|()| wr.flush());
        if ok.is_err() {
            return;
        }
    }

    let deadline = Some(Instant::now() + START_TIMEOUT);
    let start = match read_frame(&mut rd, START_BODY_LEN, deadline) {
        Ok((T_START, body)) if body.len() == START_BODY_LEN => body,
        _ => return send_error(&mut wr, code::BAD_REQUEST),
    };
    let (Some(db_id), Some(timeline), Some(applied)) = (
        start.get(..16),
        start.get(16..24).and_then(be_u64),
        start.get(24..32).and_then(be_u64),
    ) else {
        return send_error(&mut wr, code::BAD_REQUEST);
    };
    if db_id != identity.db_id || timeline != identity.timeline {
        return send_error(&mut wr, code::FOREIGN);
    }
    // 最初から遅れの上限を超えている位置からは始めさせない（保持を引き延ばさない。再同期が必要）。
    if first_current.saturating_sub(applied) > cfg.max_lag_records {
        return send_error(&mut wr, code::RESYNC_REQUIRED);
    }
    // 保持位置を登録してから、開始位置の存在を確かめる（engine が刈り込みとの競合を閉じる）。
    let pin = match core.pin_change_log_for_replica(applied) {
        Ok(p) => Arc::new(p),
        Err(e) => return send_error(&mut wr, stream_error_code(&e)),
    };
    let current = match applied
        .checked_add(1)
        .map(|next| core.read_change_log_from(next, 0, 0))
    {
        Some(Ok(b)) => b.current_lsn,
        Some(Err(e)) => return send_error(&mut wr, stream_error_code(&e)),
        None => return send_error(&mut wr, code::AHEAD),
    };
    // Identity から Start までの間（最大 START_TIMEOUT）に commit が進みうる。保持位置を登録した直後に取り直した
    // 最新 LSN で遅れを再判定し、超えていれば登録（`pin`）を外して再同期を促す。
    if current.saturating_sub(applied) > cfg.max_lag_records {
        return send_error(&mut wr, code::RESYNC_REQUIRED);
    }
    // 同期レプリケーションの対象ロールなら、位置の追跡を始める（同じロールの 2 本目は断る）。ハンドルは
    // 接続の終わりまで持ち、`Drop` で位置を外して待機側を起こす。非対象のロール・同期無効では何もしない。
    let standby = match core.sync_commit().map(|c| c.attach_standby(&role, applied)) {
        Some(Err(_)) => return send_error(&mut wr, code::BUSY),
        Some(Ok(h)) => h,
        None => None,
    };
    // Ok の書き込みは短い期限にする（読まないレプリカで、監視が動き出す前に保持位置が残り続けない）。
    if wr.set_write_timeout(Some(START_TIMEOUT)).is_err() {
        return;
    }
    {
        let ok = write_frame(
            &mut wr,
            T_OK,
            &[
                &identity.db_id,
                &identity.timeline.to_be_bytes(),
                &current.to_be_bytes(),
            ],
        )
        .and_then(|()| wr.flush());
        if ok.is_err() {
            return;
        }
    }
    if wr.set_write_timeout(Some(cfg.ack_timeout)).is_err() {
        return;
    }
    engine::log_stderr!("wire-server: replication: replica '{role}' connected");
    let reason = stream_loop(rd, wr, core, cfg, applied, &pin, version, standby.as_ref());
    engine::log_stderr!("wire-server: replication: replica '{role}' disconnected ({reason})");
}

/// 送信ループ。終了理由（運用者向けの固定語）を返す。ack の読み取りと監視を同じ接続の別スレッドで動かし、
/// 抜けるときは必ず `shutdown` でそれらを止めてから返す。
#[allow(clippy::too_many_arguments)]
fn stream_loop(
    reader: ConnReader,
    mut writer: ConnWriter,
    core: &EngineCore,
    cfg: &ReplicationSettings,
    applied: u64,
    pin: &engine::storage::ReplicaPin,
    version: u16,
    standby: Option<&engine::storage::StandbyHandle>,
) -> &'static str {
    let shared = Shared {
        inflight: Mutex::new(Inflight {
            queue: VecDeque::new(),
            bytes: 0,
            acked: applied,
            received: applied,
            sent: applied,
        }),
        failed: AtomicBool::new(false),
        done: AtomicBool::new(false),
        cut: std::sync::atomic::AtomicU8::new(CUT_NONE),
    };
    // 監視・ack の読み取りは、同じソケットを閉じるハンドルで送信ループのブロックを解く。
    let (Ok(ack_closer), Ok(watch_closer)) = (reader.shutdown_handle(), reader.shutdown_handle())
    else {
        return "internal error";
    };
    std::thread::scope(|scope| {
        let shared = &shared;
        scope.spawn(move || read_acks(reader, ack_closer, shared, pin, version, standby));
        scope.spawn(move || watchdog(watch_closer, core, shared, cfg));
        let reason = send_records(&mut writer, core, cfg, applied, shared);
        shared.done.store(true, Ordering::Release);
        writer.shutdown();
        reason
    })
}

/// レコードを送り続ける（書き込み側）。
fn send_records(
    stream: &mut ConnWriter,
    core: &EngineCore,
    cfg: &ReplicationSettings,
    applied: u64,
    shared: &Shared,
) -> &'static str {
    let mut out = BufWriter::with_capacity(64 * 1024, &mut *stream);
    let mut next = match applied.checked_add(1) {
        Some(n) => n,
        None => return "internal error",
    };
    let max_unacked = cfg.max_unacked_bytes;
    let max_batch_bytes = usize::try_from(max_unacked).unwrap_or(usize::MAX);
    let mut last_write = Instant::now();
    let mut held: Option<(u64, Vec<u8>)> = None;
    loop {
        // 同期レプリケーションが有効なら、読み出しの前に待機開始の連番を取る（取った後に始まる待機を
        // 取りこぼさないため。commit は待機の開始より前に可視になっている）。
        let wake_seen = core.sync_commit().map(|c| c.wake_seq());
        // 監視が切断を決めたら、通知を書いて抜ける（ack の待機上限・遅れの上限）。
        match shared.cut.load(Ordering::Acquire) {
            CUT_ACK_TIMEOUT => {
                send_error(out.get_mut(), code::BACKPRESSURE);
                return "unacknowledged data timed out";
            }
            CUT_LAG => {
                send_error(out.get_mut(), code::RESYNC_REQUIRED);
                return "replica lag limit exceeded";
            }
            CUT_INTERNAL => {
                send_error(out.get_mut(), code::INTERNAL);
                return "log position unavailable";
            }
            CUT_PROTOCOL => {
                send_error(out.get_mut(), code::BAD_REQUEST);
                return "replica violated the protocol";
            }
            _ => {}
        }
        if shared.failed.load(Ordering::Acquire) {
            return "replica closed or violated the protocol";
        }
        let inflight_bytes = shared.lock().bytes;
        let budget =
            max_batch_bytes.saturating_sub(usize::try_from(inflight_bytes).unwrap_or(usize::MAX));
        let mut sent_any = false;
        let mut window_full = false;
        let mut current = next;
        // 窓に入らず持ち越した 1 件があれば、読み直さずにそれを使う（毎ループの再読み出しと検証を避ける）。
        // 持ち越しは常に 1 件までで、その大きさは 1 件のレコードの上限（`MAX_RECORD_BYTES`）以内。通常は
        // 窓（`max_unacked`）に収まらなかったレコードなので、読み出し 1 回分のメモリと同じ範囲に収まる。
        let records: Vec<(u64, Vec<u8>)> = if let Some(rec) = held.take() {
            vec![rec]
        } else if budget == 0 {
            window_full = true;
            Vec::new()
        } else {
            match core.read_change_log_from(next, BATCH_RECORDS, budget.min(MAX_BATCH_BYTES)) {
                Ok(b) => {
                    current = b.current_lsn;
                    b.records
                }
                Err(e) => {
                    send_error(out.get_mut(), stream_error_code(&e));
                    return "log read failed";
                }
            }
        };
        let mut pending = inflight_bytes;
        for (lsn, bytes) in records {
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            // 未確認が空でなければ、上限を超える送信はしない（空なら 1 件は必ず送る）。
            if pending > 0 && pending.saturating_add(len) > max_unacked {
                window_full = true;
                held = Some((lsn, bytes));
                break;
            }
            // 書き込みでブロックしても監視が経過時間を見られるよう、書く前に勘定へ入れる。
            {
                let mut g = shared.lock();
                g.queue.push_back((lsn, len, Instant::now()));
                g.bytes = g.bytes.saturating_add(len);
                g.sent = lsn;
            }
            if write_frame(&mut out, T_RECORD, &[&lsn.to_be_bytes(), &bytes]).is_err() {
                return "write failed or timed out";
            }
            pending = pending.saturating_add(len);
            next = match lsn.checked_add(1) {
                Some(n) => n,
                None => return "internal error",
            };
            sent_any = true;
        }
        if sent_any {
            if out.flush().is_err() {
                return "write failed or timed out";
            }
            last_write = Instant::now();
            continue;
        }
        // 上限に達している間は送らない。ack が来ないままの切断は監視が判定する。
        if !window_full && last_write.elapsed() >= HEARTBEAT_INTERVAL {
            let hb = write_frame(&mut out, T_HEARTBEAT, &[&current.to_be_bytes()]);
            if hb.is_err() || out.flush().is_err() {
                return "write failed or timed out";
            }
            last_write = Instant::now();
        }
        // 追いついている間の待機。同期有効なら commit の待機開始で起きて、同期 commit の遅延を下げる。
        match (core.sync_commit(), wake_seen) {
            (Some(c), Some(seen)) => c.park_until_commit_waiting(seen, POLL_INTERVAL),
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// accept ループを別スレッドで起動する。`main.rs::run_server` から 1 回だけ呼ばれる。スレッドを作れなければ
/// `Err`（起動拒否にする）。
pub fn spawn_server(
    listener: TcpListener,
    core: Arc<EngineCore>,
    cfg: ReplicationSettings,
) -> std::io::Result<()> {
    let srv = Arc::new(Server {
        core,
        cfg,
        active: Arc::new(AtomicUsize::new(0)),
        pre_auth: Arc::new(AtomicUsize::new(0)),
        failures: Mutex::new(FailureTable::default()),
    });
    std::thread::Builder::new()
        .name("replication-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                // 許可リスト外・送信元が分からない接続には何も返さず閉じる。
                let Some(peer) = conn
                    .peer_addr()
                    .ok()
                    .map(|a| a.ip())
                    .filter(|ip| srv.cfg.allow.permits(*ip))
                else {
                    continue;
                };
                // 認証に失敗し続けている送信元は、バックオフの間は読まずに閉じる。
                if srv.failures().blocked(peer, Instant::now()) {
                    continue;
                }
                // 認証前の枠（認証済みの枠とは別）。満杯なら断る。
                if srv.pre_auth.fetch_add(1, Ordering::AcqRel) >= MAX_PRE_AUTH {
                    srv.pre_auth.fetch_sub(1, Ordering::AcqRel);
                    // TLS リスナーは平文のフレームを書かない（何も返さずに閉じる）。
                    if srv.cfg.tls.is_none() {
                        if let Ok((_, mut w)) = replication_conn::plain(conn) {
                            send_error(&mut w, code::BUSY);
                        }
                    }
                    continue;
                }
                let pre = SlotGuard(Arc::clone(&srv.pre_auth));
                let srv = Arc::clone(&srv);
                // スレッドを作れなかった場合、クロージャごと枠が落ちて数が戻る。
                let _ = std::thread::Builder::new()
                    .name("replication-conn".into())
                    .spawn(move || handle(conn, peer, &srv, pre));
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_parsing_enforces_shape() {
        let mut ok = PROTOCOL_VERSION.to_be_bytes().to_vec();
        ok.push(3);
        ok.extend_from_slice(b"abcpw");
        assert_eq!(
            parse_hello(&ok),
            Some((PROTOCOL_VERSION, "abc", b"pw".as_slice()))
        );
        let mut v2 = ok.clone();
        v2[1] = 2;
        assert_eq!(
            parse_hello(&v2),
            Some((PROTOCOL_VERSION_RECEIVED, "abc", b"pw".as_slice()))
        );
        let mut v3 = ok.clone();
        v3[1] = 3;
        assert!(parse_hello(&v3).is_none());
        // 版数違い・ロール名長 0 / 過大 / 本体を超える・UTF-8 でない。
        let mut bad_version = ok.clone();
        bad_version[1] = 9;
        assert!(parse_hello(&bad_version).is_none());
        assert!(parse_hello(&[0, 1, 0]).is_none());
        assert!(parse_hello(&[0, 1, 200, b'a']).is_none());
        assert!(parse_hello(&[0, 1, 5, b'a']).is_none());
        assert!(parse_hello(&[0, 1, 1, 0xff]).is_none());
        assert!(parse_hello(&[]).is_none());
    }

    #[test]
    fn frames_round_trip_and_reject_oversize() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = l.local_addr().expect("addr");
        let mut c = TcpStream::connect(addr).expect("connect");
        let (s, _) = l.accept().expect("accept");
        let (mut sr, _sw) = replication_conn::plain(s).expect("split");
        write_frame(&mut c, T_ACK, &[&7u64.to_be_bytes()]).expect("write");
        let (ty, body) = read_frame(&mut sr, 8, None).expect("read");
        assert_eq!((ty, be_u64(&body)), (T_ACK, Some(7)));
        write_frame(&mut c, T_ACK, &[&[0u8; 9]]).expect("write");
        assert_eq!(read_frame(&mut sr, 8, None), Err(FrameError::TooLarge));
    }

    #[test]
    fn failure_table_backs_off_clears_and_stays_bounded() {
        let ip = |n: u8| std::net::IpAddr::from([10, 0, 0, n]);
        let t0 = Instant::now();
        let mut t = FailureTable::default();
        for _ in 0..FREE_FAILURES.saturating_sub(1) {
            t.record_failure(ip(1), t0);
        }
        assert!(!t.blocked(ip(1), t0), "within the free failures");
        t.record_failure(ip(1), t0);
        assert!(t.blocked(ip(1), t0));
        assert!(!t.blocked(ip(1), t0 + Duration::from_secs(2)));
        assert!(!t.blocked(ip(2), t0), "per source");
        t.record_failure(ip(1), t0);
        assert!(
            t.blocked(ip(1), t0 + Duration::from_secs(1)),
            "backoff grows"
        );
        t.clear(ip(1));
        assert!(!t.blocked(ip(1), t0));
        for n in 0..=u16::MAX {
            let [a, b] = n.to_be_bytes();
            t.record_failure(std::net::IpAddr::from([172, 16, a, b]), t0);
        }
        assert!(t.entries.len() <= MAX_FAILURE_ENTRIES);
        assert_eq!(FailureTable::backoff(u32::MAX), MAX_BACKOFF);
    }
}
