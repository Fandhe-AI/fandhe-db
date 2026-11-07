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
//! | C→S | `H` Hello | 版数 u16（=1）\| ロール名長 u8 \| ロール名 \| パスワード（残り全部） |
//! | S→C | `I` Identity | DB 識別子 16B \| timeline u64 \| プライマリの最新 LSN u64（認証成功の直後。レプリカは自分のベースと照合する） |
//! | C→S | `S` Start | DB 識別子 16B \| timeline u64 \| 適用済みの最後の LSN u64（次から送る） |
//! | S→C | `O` Ok | DB 識別子 16B \| timeline u64 \| プライマリの最新 LSN u64 |
//! | S→C | `W` Record | LSN u64 \| レコードのバイト列（D3 の自己完結形式） |
//! | S→C | `P` Heartbeat | プライマリの最新 LSN u64（追いついて暇な間、約 1 秒ごと） |
//! | C→S | `K` Ack | 受け取り済みの最後の LSN u64（単調増加・送信済み以下） |
//! | S→C | `E` Error | コード u8（文言は載せない） |
//!
//! 入力は untrusted: 長さは確保前に上限検証し、フレームの型・長さ・値（ack の単調性と上限）を検証して、
//! 違反は固定コードの `E` を返して切断する。認証失敗は未知のロールと誤パスワードで同じ KDF コストと固定遅延
//! （`auth::AUTH_FAILURE_DELAY`）を課し、区別できる応答を返さない。許可リスト外の送信元には何も返さず閉じる。
//!
//! バックプレッシャ: 送信済みで未確認の量が `max_unacked_bytes` に達したら送信を止め（フロー制御）、
//! `ack_timeout` の間 ack が進まなければそのレプリカだけを切る。書き込みタイムアウトも同じ時間で、読まない
//! レプリカの書き込みで送信スレッドが固まり続けない。engine は読み取りトランザクションしか取らないため、
//! レプリカの切断・遅延・停止がプライマリの書き込みを止めることはない（非同期。同期モードは #2245）。
#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{BufWriter, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::storage::StreamError;

use crate::auth::{argon2id, AUTH_FAILURE_DELAY};
use crate::replication_opt::{ReplicationRoles, ReplicationSettings, MAX_ROLE_NAME_LEN};

/// プロトコルの版数（Hello で受け取る）。
pub const PROTOCOL_VERSION: u16 = 1;

/// フレーム型。
pub const T_HELLO: u8 = b'H';
pub const T_IDENTITY: u8 = b'I';
pub const T_START: u8 = b'S';
pub const T_OK: u8 = b'O';
pub const T_RECORD: u8 = b'W';
pub const T_HEARTBEAT: u8 = b'P';
pub const T_ACK: u8 = b'K';
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
/// 認証と開始位置の受け取りに許す時間（フレームごと。1 バイトずつ送り続けても延びない）。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
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

/// `buf` を満たすまで読む。`deadline` を過ぎる、または切断されたら `Closed`。
fn read_exact_by(
    stream: &TcpStream,
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
        match (&*stream).read(dst) {
            Ok(0) | Err(_) => return Err(FrameError::Closed),
            Ok(n) => filled = filled.checked_add(n).ok_or(FrameError::Closed)?,
        }
    }
    Ok(())
}

/// 1 フレームを読む。本体長は `max_body` 以内でなければ確保せずに `TooLarge`。
fn read_frame(
    stream: &TcpStream,
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
fn send_error(stream: &TcpStream, code: u8) {
    let _ = stream.set_write_timeout(Some(NOTICE_TIMEOUT));
    let mut w = stream;
    let _ = write_frame(&mut w, T_ERROR, &[&[code]]);
    let _ = w.flush();
}

fn be_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Hello の本体を (ロール名, パスワード) に分解する。形式違反は `None`。
fn parse_hello(body: &[u8]) -> Option<(&str, &[u8])> {
    let version = u16::from_be_bytes(body.get(..2)?.try_into().ok()?);
    if version != PROTOCOL_VERSION {
        return None;
    }
    let role_len = usize::from(*body.get(2)?);
    if role_len == 0 || role_len > MAX_ROLE_NAME_LEN {
        return None;
    }
    let role_end = 3usize.checked_add(role_len)?;
    let role = std::str::from_utf8(body.get(3..role_end)?).ok()?;
    let password = body.get(role_end..)?;
    (password.len() <= MAX_PASSWORD_LEN).then_some((role, password))
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

/// 送信済みで未確認のレコードの勘定（書き込み側と ack 読み取り側で共有する）。
struct Inflight {
    /// `(LSN, バイト数)`。送信順。
    queue: VecDeque<(u64, u64)>,
    bytes: u64,
    /// 確認応答済みの最後の LSN と、送信済みの最後の LSN。
    acked: u64,
    sent: u64,
    last_ack: Instant,
}

struct Shared {
    inflight: Mutex<Inflight>,
    /// ack の違反・切断。立ったら書き込み側は終了する。
    failed: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inflight> {
        self.inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// ack を読み続ける（レプリカ→プライマリの方向。別スレッド）。違反・切断で `failed` を立てて終わる。
fn read_acks(stream: TcpStream, shared: Arc<Shared>, pin: Arc<engine::storage::ReplicaPin>) {
    loop {
        let acked = match read_frame(&stream, ACK_BODY_LEN, None) {
            Ok((T_ACK, body)) if body.len() == ACK_BODY_LEN => be_u64(&body),
            _ => None,
        };
        let Some(lsn) = acked else { break };
        let mut g = shared.lock();
        // 単調増加で、送信済みの範囲内でなければならない（未送信の位置への ack は不正）。
        if lsn < g.acked || lsn > g.sent {
            break;
        }
        while g.queue.front().is_some_and(|(l, _)| *l <= lsn) {
            if let Some((_, len)) = g.queue.pop_front() {
                g.bytes = g.bytes.saturating_sub(len);
            }
        }
        g.acked = lsn;
        g.last_ack = Instant::now();
        drop(g);
        pin.advance(lsn);
    }
    shared.failed.store(true, Ordering::Release);
    let _ = stream.shutdown(Shutdown::Both);
}

/// `StreamError` を `E` のコードへ写す。
fn stream_error_code(e: &StreamError) -> u8 {
    match e {
        StreamError::Pruned => code::RESYNC_REQUIRED,
        StreamError::Ahead => code::AHEAD,
        _ => code::INTERNAL,
    }
}

/// 1 接続を処理する（認証 → 開始位置 → 送信ループ）。
fn handle(stream: TcpStream, core: &EngineCore, cfg: &ReplicationSettings) {
    let _ = stream.set_nodelay(true);
    if stream.set_write_timeout(Some(cfg.ack_timeout)).is_err() {
        return;
    }
    let auth_start = Instant::now();
    let deadline = Some(Instant::now() + HANDSHAKE_TIMEOUT);
    let (ty, body) = match read_frame(&stream, MAX_HELLO_BODY, deadline) {
        Ok(f) => f,
        Err(_) => return send_error(&stream, code::BAD_REQUEST),
    };
    let parsed = if ty == T_HELLO {
        parse_hello(&body)
    } else {
        None
    };
    let Some((role, password)) = parsed else {
        return send_error(&stream, code::BAD_REQUEST);
    };
    if !authenticate(&cfg.roles, role, password, auth_start) {
        return send_error(&stream, code::AUTH_FAILED);
    }
    let role = role.to_string();

    // 認証成功の直後に識別子と最新 LSN を知らせる。レプリカは自分のベース（バックアップ）の識別子と
    // 照合してから Start を送る（サーバー側も Start で再度照合する）。
    let Some(identity) = core.change_log_stream_identity() else {
        return send_error(&stream, code::INTERNAL);
    };
    let first_current = match core.read_change_log_from(0, 0, 0) {
        Ok(b) => b.current_lsn,
        Err(e) => return send_error(&stream, stream_error_code(&e)),
    };
    {
        let mut w = &stream;
        let ok = write_frame(
            &mut w,
            T_IDENTITY,
            &[
                &identity.db_id,
                &identity.timeline.to_be_bytes(),
                &first_current.to_be_bytes(),
            ],
        )
        .and_then(|()| w.flush());
        if ok.is_err() {
            return;
        }
    }

    let deadline = Some(Instant::now() + HANDSHAKE_TIMEOUT);
    let start = match read_frame(&stream, START_BODY_LEN, deadline) {
        Ok((T_START, body)) if body.len() == START_BODY_LEN => body,
        _ => return send_error(&stream, code::BAD_REQUEST),
    };
    let (Some(db_id), Some(timeline), Some(applied)) = (
        start.get(..16),
        start.get(16..24).and_then(be_u64),
        start.get(24..32).and_then(be_u64),
    ) else {
        return send_error(&stream, code::BAD_REQUEST);
    };
    if db_id != identity.db_id || timeline != identity.timeline {
        return send_error(&stream, code::FOREIGN);
    }
    // 保持位置を登録してから、開始位置の存在を確かめる（engine が刈り込みとの競合を閉じる）。
    let pin = match core.pin_change_log_for_replica(applied) {
        Ok(p) => Arc::new(p),
        Err(e) => return send_error(&stream, stream_error_code(&e)),
    };
    let current = match applied
        .checked_add(1)
        .map(|next| core.read_change_log_from(next, 0, 0))
    {
        Some(Ok(b)) => b.current_lsn,
        Some(Err(e)) => return send_error(&stream, stream_error_code(&e)),
        None => return send_error(&stream, code::AHEAD),
    };
    {
        let mut w = &stream;
        let ok = write_frame(
            &mut w,
            T_OK,
            &[
                &identity.db_id,
                &identity.timeline.to_be_bytes(),
                &current.to_be_bytes(),
            ],
        )
        .and_then(|()| w.flush());
        if ok.is_err() {
            return;
        }
    }
    engine::log_stderr!("wire-server: replication: replica '{role}' connected");
    let reason = stream_loop(&stream, core, cfg, applied, pin);
    engine::log_stderr!("wire-server: replication: replica '{role}' disconnected ({reason})");
}

/// 送信ループ。終了理由（運用者向けの固定語）を返す。
fn stream_loop(
    stream: &TcpStream,
    core: &EngineCore,
    cfg: &ReplicationSettings,
    applied: u64,
    pin: Arc<engine::storage::ReplicaPin>,
) -> &'static str {
    let shared = Arc::new(Shared {
        inflight: Mutex::new(Inflight {
            queue: VecDeque::new(),
            bytes: 0,
            acked: applied,
            sent: applied,
            last_ack: Instant::now(),
        }),
        failed: AtomicBool::new(false),
    });
    let reader = match stream.try_clone() {
        Ok(clone) => {
            let (shared, pin) = (Arc::clone(&shared), Arc::clone(&pin));
            std::thread::Builder::new()
                .name("replication-ack".into())
                .spawn(move || read_acks(clone, shared, pin))
                .ok()
        }
        Err(_) => None,
    };
    let reason = if reader.is_some() {
        send_records(stream, core, cfg, applied, &shared)
    } else {
        "internal error"
    };
    let _ = stream.shutdown(Shutdown::Both);
    if let Some(r) = reader {
        let _ = r.join();
    }
    reason
}

/// レコードを送り続ける（書き込み側）。
fn send_records(
    stream: &TcpStream,
    core: &EngineCore,
    cfg: &ReplicationSettings,
    applied: u64,
    shared: &Shared,
) -> &'static str {
    let mut out = BufWriter::with_capacity(64 * 1024, stream);
    let mut next = match applied.checked_add(1) {
        Some(n) => n,
        None => return "internal error",
    };
    let max_unacked = cfg.max_unacked_bytes;
    let max_batch_bytes = usize::try_from(max_unacked).unwrap_or(usize::MAX);
    let mut blocked_since: Option<Instant> = None;
    let mut last_write = Instant::now();
    loop {
        if shared.failed.load(Ordering::Acquire) {
            return "replica closed or violated the protocol";
        }
        let (inflight_bytes, last_ack) = {
            let g = shared.lock();
            (g.bytes, g.last_ack)
        };
        let budget =
            max_batch_bytes.saturating_sub(usize::try_from(inflight_bytes).unwrap_or(usize::MAX));
        let batch = if budget == 0 {
            None
        } else {
            match core.read_change_log_from(next, BATCH_RECORDS, budget) {
                Ok(b) => Some(b),
                Err(e) => {
                    send_error(stream, stream_error_code(&e));
                    return "log read failed";
                }
            }
        };
        let mut sent_any = false;
        let mut window_full = batch.is_none();
        let current = batch.as_ref().map_or(next, |b| b.current_lsn);
        if let Some(batch) = batch {
            let mut pending = inflight_bytes;
            for (lsn, bytes) in &batch.records {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                // 未確認が空でなければ、上限を超える送信はしない（空なら 1 件は必ず送る）。
                if pending > 0 && pending.saturating_add(len) > max_unacked {
                    window_full = true;
                    break;
                }
                let sent = write_frame(&mut out, T_RECORD, &[&lsn.to_be_bytes(), bytes]);
                if sent.is_err() {
                    return "write failed or timed out";
                }
                pending = pending.saturating_add(len);
                {
                    let mut g = shared.lock();
                    g.queue.push_back((*lsn, len));
                    g.bytes = g.bytes.saturating_add(len);
                    g.sent = *lsn;
                }
                next = match lsn.checked_add(1) {
                    Some(n) => n,
                    None => return "internal error",
                };
                sent_any = true;
            }
        }
        if sent_any {
            if out.flush().is_err() {
                return "write failed or timed out";
            }
            last_write = Instant::now();
            blocked_since = None;
            continue;
        }
        if window_full {
            // 上限に達している。ack が進まないまま待機上限を過ぎたレプリカだけを切る。
            let since = *blocked_since.get_or_insert_with(Instant::now);
            if since.max(last_ack).elapsed() >= cfg.ack_timeout {
                send_error(stream, code::BACKPRESSURE);
                return "unacknowledged data limit reached";
            }
        } else {
            blocked_since = None;
            if last_write.elapsed() >= HEARTBEAT_INTERVAL {
                let hb = write_frame(&mut out, T_HEARTBEAT, &[&current.to_be_bytes()]);
                if hb.is_err() || out.flush().is_err() {
                    return "write failed or timed out";
                }
                last_write = Instant::now();
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// accept ループを別スレッドで起動する。`main.rs::run_server` から 1 回だけ呼ばれる。スレッドを作れなければ
/// `Err`（起動拒否にする）。
pub fn spawn_server(
    listener: TcpListener,
    core: Arc<EngineCore>,
    cfg: ReplicationSettings,
) -> std::io::Result<()> {
    let cfg = Arc::new(cfg);
    std::thread::Builder::new()
        .name("replication-accept".into())
        .spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for conn in listener.incoming() {
                let Ok(conn) = conn else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                // 許可リスト外・送信元が分からない接続には何も返さず閉じる。
                let permitted = conn
                    .peer_addr()
                    .map(|a| cfg.allow.permits(a.ip()))
                    .unwrap_or(false);
                if !permitted {
                    continue;
                }
                if active.fetch_add(1, Ordering::AcqRel) >= cfg.max_connections {
                    active.fetch_sub(1, Ordering::AcqRel);
                    send_error(&conn, code::BUSY);
                    continue;
                }
                let slot = SlotGuard(Arc::clone(&active));
                let (core, cfg) = (Arc::clone(&core), Arc::clone(&cfg));
                // スレッドを作れなかった場合、クロージャごと slot が落ちて数が戻る。
                let _ = std::thread::Builder::new()
                    .name("replication-conn".into())
                    .spawn(move || {
                        let _slot = slot;
                        handle(conn, &core, &cfg);
                    });
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
        assert_eq!(parse_hello(&ok), Some(("abc", b"pw".as_slice())));
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
        write_frame(&mut c, T_ACK, &[&7u64.to_be_bytes()]).expect("write");
        let (ty, body) = read_frame(&s, 8, None).expect("read");
        assert_eq!((ty, be_u64(&body)), (T_ACK, Some(7)));
        write_frame(&mut c, T_ACK, &[&[0u8; 9]]).expect("write");
        assert_eq!(read_frame(&s, 8, None), Err(FrameError::TooLarge));
    }
}
