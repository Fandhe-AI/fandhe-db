//! ストリーミングレプリケーションの受信側（レプリカ）の適用ループ（Issue #2174・親 #2171・RECOVER-15
//! ポインタ。方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2174）」）。
//!
//! 責務境界: `wire-server replica`（`main.rs::run_replica`）から呼ばれ、プライマリの送信側
//! （[`crate::replication`]）へ接続して、受け取った変更ログのレコードを engine の
//! `EngineCore::apply_replicated_records` で commit 番号（LSN）順に原子的に適用する。レコードの検査・適用・
//! 適用位置の永続化は engine が担い、本モジュールは接続・フレーム・バッチ化・ack・再接続・停止の分類だけを担う。
//! フレームの型・コードは送信側の定数を再利用する（プロトコルの記述は [`crate::replication`] の先頭）。
//!
//! ループの流れ: Hello → Identity（ローカルの識別子と照合。不一致なら Start を送らず報告して停止）→
//! Start（適用済み LSN = DB の世代）→ Ok → Record／Heartbeat を受信。Record は溜めてバッチで適用し、
//! **commit（`Immediate`）が済んでから** Ack を送る（前進時のみ・単調増加）。クラッシュ後は Start の
//! 適用済み LSN から再送されるので二重適用は起きない。
//!
//! 停止の分類（固定終了コードは [`crate::replica_opt`]）:
//!
//! - **適用停止**（`EXIT_APPLY_HALTED`）: 順序違反・欠番・チェックサム不正・識別子／timeline／版数の不一致・
//!   適用不能なレコード・分岐。再接続して飛ばして進めない
//! - **要報告**（`EXIT_PRIMARY_REJECTED`）: プライマリからの `FOREIGN`・`RESYNC_REQUIRED`・`AHEAD`・認証失敗・
//!   Identity 不一致。適用を止めて報告するだけで、再同期の実行は `replica-resync`（Issue #2176。
//!   `RESYNC_REQUIRED` のときだけ固有の文言を標準エラーへ出す）
//! - **一時障害**: 切断・タイムアウト・I/O エラー・ハートビート途絶・プライマリの `BUSY`／`INTERNAL`。
//!   上限つき指数バックオフ（抖動つき）で再接続し、再開位置は常に DB の世代から取り直す
//!
//! TLS（Issue #2177・RECOVER-15 (3)・WIRE-9 ポインタ）: `--primary-tls-cert`（[`crate::replica_opt`]）でピンを
//! 渡すと、接続直後に TLS 1.3 のハンドシェイクを行い（[`crate::tls::client_handshake`]）、プライマリの証明書が
//! ピンと一致して初めて Hello（ロール名・パスワード）を書く。ピン不一致・検証失敗・不正な応答は**要報告**
//! （中間者の疑いなので再試行し続けない）、接続・I/O の失敗は一時障害。平文・TLS の差は
//! [`crate::replication_conn`] の読み書きの半分が吸収する。
//!
//! 入力（プライマリから届くフレーム）も untrusted: 本体長は確保前に上限検証し、受信データの経路で
//! `unwrap`／`expect`／添字アクセスを使わない。資格情報・LSN 以外の内容をログへ出さない（進捗の LSN は
//! 運用者向けの標準エラーだけで、テナントのセッションからは参照できない）。
#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::replication_conn::{self, ConnReader, ConnWriter};
use crate::tls::client_handshake::{perform_client_handshake, PinnedServerCertificate};

use engine::core::EngineCore;
use engine::storage::{ReplicaApplyError, MAX_RECORD_BYTES};

use crate::replica_opt::{ReplicaOptions, EXIT_APPLY_HALTED, EXIT_PRIMARY_REJECTED};
use crate::replication::{
    code, PROTOCOL_VERSION_RECEIVED, T_ACK, T_ERROR, T_HEARTBEAT, T_HELLO, T_IDENTITY, T_OK,
    T_RECEIVED, T_RECORD, T_START,
};

/// 接続・ハンドシェイクの各フレームの待機上限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// フレームの途中で待つ上限（ヘッダ受信後の本体）と、ストリーム中の無音の上限（ハートビートは約 1 秒ごと
/// なので、これを超えて何も来なければ途絶とみなして再接続する）。
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// バッチを溜める間、続きのフレームが既に届いているかを見る待ち時間。
const BATCH_PEEK: Duration = Duration::from_millis(2);
/// 書き込み（Hello・Start・Ack）の期限。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 再接続バックオフの初期値。
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
/// 本体を読むときの 1 回の確保単位（宣言された長さをそのまま確保しない）。
const READ_CHUNK: usize = 64 * 1024;

/// プライマリが `RESYNC_REQUIRED` を返したときの運用者向けの固定文言（Issue #2176）。終了コードは他の拒否と
/// 同じ 11 のまま、この文で「再同期が必要」だと区別できる。
pub(crate) const RESYNC_REQUIRED_MESSAGE: &str = "wire-server: replica: resync required: the primary no longer retains changes after the applied position; rebuild the replica with replica-resync";

/// 受信ループが終了する理由（終了コードへ写す）。
#[derive(Debug, PartialEq, Eq)]
pub enum Halt {
    /// 適用停止（整合性違反）。
    ApplyHalted,
    /// 要報告（プライマリの拒否・Identity 不一致）。
    PrimaryRejected,
}

impl Halt {
    /// 固定の終了コード。
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::ApplyHalted => EXIT_APPLY_HALTED,
            Self::PrimaryRejected => EXIT_PRIMARY_REJECTED,
        }
    }
}

/// 1 回の接続の結果。
enum SessionEnd {
    /// 一時障害（再接続する）。`progressed` は接続できて Ok を受けたか（バックオフの初期化に使う）。
    Transient { progressed: bool },
    /// 停止。
    Halt(Halt),
}

/// フレーム読み取りの失敗。
pub(crate) enum FrameError {
    /// 切断・タイムアウト・I/O エラー。
    Closed,
    /// 本体長が上限を超える。
    TooLarge,
}

/// 受信ループ本体。停止すべき理由が出るまで戻らない（再接続はここで繰り返す）。
pub fn run(core: &EngineCore, opts: &ReplicaOptions) -> Halt {
    // 開始時にベースの変更ログが commit 番号と連続していることを確かめる（Issue #2176）。記録のない区間が
    // あるベースへ `applied + 1` から続けると履歴が欠けるため、接続せずに適用停止にする。
    if let Err(e) = core.check_replica_base() {
        return apply_failure(&e);
    }
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match session(core, opts) {
            SessionEnd::Halt(h) => return h,
            SessionEnd::Transient { progressed } => {
                if progressed {
                    backoff = INITIAL_BACKOFF;
                }
                let wait = with_jitter(backoff);
                engine::log_stderr!("wire-server: replica: disconnected, will reconnect");
                std::thread::sleep(wait);
                backoff = backoff.saturating_mul(2).min(opts.max_backoff);
            }
        }
    }
}

/// バックオフに最大 1/4 の抖動を足す（再接続の同期を避ける。暗号用途ではない）。
fn with_jitter(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    let quarter = u64::try_from(base.as_millis() / 4).unwrap_or(0).max(1);
    base.saturating_add(Duration::from_millis(nanos % quarter))
}

pub(crate) fn be_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// `buf` を満たすまで読む。`deadline` を過ぎる、または切断されたら `Closed`。
fn read_exact_by(
    stream: &mut ConnReader,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<(), FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let rem = deadline.saturating_duration_since(Instant::now());
        if rem.is_zero() {
            return Err(FrameError::Closed);
        }
        stream
            .set_read_timeout(Some(rem))
            .map_err(|_| FrameError::Closed)?;
        let dst = buf.get_mut(filled..).ok_or(FrameError::Closed)?;
        match stream.read(dst) {
            Ok(0) | Err(_) => return Err(FrameError::Closed),
            Ok(n) => filled = filled.checked_add(n).ok_or(FrameError::Closed)?,
        }
    }
    Ok(())
}

/// 1 フレームを読む。本体長は `max_body` 以内でなければ確保せずに `TooLarge`。本体は宣言長を一度に
/// 確保せず、受け取った分ずつ伸ばす。
pub(crate) fn read_frame(
    stream: &mut ConnReader,
    max_body: usize,
    wait: Duration,
) -> Result<(u8, Vec<u8>), FrameError> {
    let mut header = [0u8; 5];
    read_exact_by(stream, &mut header, Instant::now() + wait)?;
    let [ty, l0, l1, l2, l3] = header;
    let len =
        usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).map_err(|_| FrameError::TooLarge)?;
    if len > max_body {
        return Err(FrameError::TooLarge);
    }
    let deadline = Instant::now() + FRAME_TIMEOUT.max(wait);
    let mut body: Vec<u8> = Vec::new();
    while body.len() < len {
        let step = READ_CHUNK.min(len.saturating_sub(body.len()));
        let start = body.len();
        body.resize(start.saturating_add(step), 0);
        let dst = body.get_mut(start..).ok_or(FrameError::Closed)?;
        read_exact_by(stream, dst, deadline)?;
    }
    Ok((ty, body))
}

/// 1 フレームを書いて flush する。
pub(crate) fn write_frame(w: &mut ConnWriter, ty: u8, parts: &[&[u8]]) -> std::io::Result<()> {
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
    w.flush()
}

/// プライマリの `E` コードを停止の分類へ写す。`None` は一時障害（再接続する）。
pub(crate) fn classify_error(c: u8) -> Option<Halt> {
    match c {
        code::BUSY | code::INTERNAL | code::BACKPRESSURE => None,
        // FOREIGN・RESYNC_REQUIRED・AHEAD・AUTH_FAILED・BAD_REQUEST・未知のコードは、再接続しても
        // 直らない（運用者の対応が要る）ので報告して停止する。
        _ => Some(Halt::PrimaryRejected),
    }
}

/// 適用エラーを停止の分類へ写す（すべて適用停止。ストレージ層のエラーも状態を確信できないので停止する）。
fn apply_failure(e: &ReplicaApplyError) -> Halt {
    engine::log_stderr!("wire-server: replica: apply halted ({e})");
    Halt::ApplyHalted
}

/// 1 回の接続（ハンドシェイクから切断・停止まで）。
fn session(core: &EngineCore, opts: &ReplicaOptions) -> SessionEnd {
    let transient = |progressed: bool| SessionEnd::Transient { progressed };
    let local = match core.replica_identity() {
        Ok(i) => i,
        Err(e) => return SessionEnd::Halt(apply_failure(&e)),
    };
    let stream = match TcpStream::connect_timeout(&opts.primary, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return transient(false),
    };
    let _ = stream.set_nodelay(true);
    // ピンがあれば TLS を確立し、証明書の一致を確かめる。資格情報（Hello）は**この後**にだけ書く。
    let (mut rd, mut wr) = match open_conn(stream, opts.primary_tls.as_deref()) {
        Ok(halves) => halves,
        Err(end) => return end,
    };
    if wr.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
        return transient(false);
    }

    // Hello（版数 | ロール名長 | ロール名 | パスワード）。
    let Ok(role_len) = u8::try_from(opts.role.len()) else {
        return SessionEnd::Halt(Halt::PrimaryRejected);
    };
    if write_frame(
        &mut wr,
        T_HELLO,
        &[
            &PROTOCOL_VERSION_RECEIVED.to_be_bytes(),
            &[role_len],
            opts.role.as_bytes(),
            &opts.password,
        ],
    )
    .is_err()
    {
        return transient(false);
    }

    // Identity（DB 識別子 16B | timeline | プライマリの最新 LSN）。ローカルと照合してから Start を送る。
    let (primary_identity_ok, mut primary_latest) = match read_handshake(&mut rd, T_IDENTITY) {
        Handshake::Frame(body) => {
            let ok = body.get(..16) == Some(&local.db_id[..])
                && body.get(16..24).and_then(be_u64) == Some(local.timeline);
            (ok, body.get(24..32).and_then(be_u64).unwrap_or(0))
        }
        Handshake::Rejected(h) => return SessionEnd::Halt(h),
        Handshake::Transient => return transient(false),
    };
    if !primary_identity_ok {
        engine::log_stderr!("wire-server: replica: the primary belongs to another database");
        return SessionEnd::Halt(Halt::PrimaryRejected);
    }

    // Start（適用済み LSN は常に DB の世代から取り直す）。
    let applied = match core.replica_applied_lsn() {
        Ok(n) => n,
        Err(e) => return SessionEnd::Halt(apply_failure(&e)),
    };
    if write_frame(
        &mut wr,
        T_START,
        &[
            &local.db_id,
            &local.timeline.to_be_bytes(),
            &applied.to_be_bytes(),
        ],
    )
    .is_err()
    {
        return transient(false);
    }
    match read_handshake(&mut rd, T_OK) {
        Handshake::Frame(body) => {
            if let Some(latest) = body.get(24..32).and_then(be_u64) {
                primary_latest = latest;
            }
        }
        Handshake::Rejected(h) => return SessionEnd::Halt(h),
        Handshake::Transient => return transient(false),
    }
    engine::log_stderr!("wire-server: replica: connected (applied lsn={applied})");
    stream_loop(core, opts, &mut rd, &mut wr, applied, primary_latest)
}

/// プライマリとの通信路を作る（Issue #2177）。ピンなしは平文。ピンありは TLS 1.3 のハンドシェイクを行い、
/// 提示された証明書がピンと一致しなければ接続を閉じて返す。ピン不一致・検証失敗・不正な応答は**要報告**
/// （`Halt::PrimaryRejected`。中間者の疑いなので再接続し続けない）、接続・I/O の失敗は一時障害。
fn open_conn(
    mut stream: TcpStream,
    pin: Option<&PinnedServerCertificate>,
) -> Result<(ConnReader, ConnWriter), SessionEnd> {
    let transient = || SessionEnd::Transient { progressed: false };
    let Some(pin) = pin else {
        return replication_conn::plain(stream).map_err(|_| transient());
    };
    match perform_client_handshake(&mut stream, pin) {
        Ok(session) => replication_conn::tls(stream, session).map_err(|_| transient()),
        Err(e) if e.is_rejection() => {
            engine::log_stderr!("{TLS_REJECTED_MESSAGE}");
            Err(SessionEnd::Halt(Halt::PrimaryRejected))
        }
        Err(_) => Err(transient()),
    }
}

/// TLS の検証に失敗した（ピン不一致・不正なハンドシェイク）ときの運用者向けの固定文言（Issue #2177）。
/// 原因（どの検査か・相手の証明書の中身）は出さない。
pub(crate) const TLS_REJECTED_MESSAGE: &str =
    "wire-server: replica: the primary's TLS handshake was rejected (check --primary-tls-cert)";

pub(crate) enum Handshake {
    /// 期待した型のフレームの本体。
    Frame(Vec<u8>),
    /// プライマリの拒否（停止）。
    Rejected(Halt),
    /// 一時障害。
    Transient,
}

/// ハンドシェイクのフレームを 1 つ読む。期待した型、または `E`。それ以外は停止（プロトコル違反）。
pub(crate) fn read_handshake(stream: &mut ConnReader, expect: u8) -> Handshake {
    match read_frame(stream, 64, HANDSHAKE_TIMEOUT) {
        Ok((ty, body)) if ty == expect && body.len() == 32 => Handshake::Frame(body),
        Ok((T_ERROR, body)) => match body.first().copied().and_then(classify_error) {
            Some(h) => {
                if body.first().copied() == Some(code::RESYNC_REQUIRED) {
                    engine::log_stderr!("{RESYNC_REQUIRED_MESSAGE}");
                } else {
                    engine::log_stderr!(
                        "wire-server: replica: the primary rejected the connection"
                    );
                }
                Handshake::Rejected(h)
            }
            None => Handshake::Transient,
        },
        Ok(_) | Err(FrameError::TooLarge) => {
            engine::log_stderr!("wire-server: replica: protocol violation by the primary");
            Handshake::Rejected(Halt::PrimaryRejected)
        }
        Err(FrameError::Closed) => Handshake::Transient,
    }
}

/// Ok の後の受信ループ。Record をバッチにして適用し、commit 後に Ack を送る。
fn stream_loop(
    core: &EngineCore,
    opts: &ReplicaOptions,
    rd: &mut ConnReader,
    wr: &mut ConnWriter,
    mut acked: u64,
    mut primary_latest: u64,
) -> SessionEnd {
    let progressed = true;
    let transient = SessionEnd::Transient { progressed };
    let max_body = 8usize.saturating_add(MAX_RECORD_BYTES);
    let mut batch: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut batch_bytes = 0usize;
    loop {
        // バッチが空なら無音の上限まで待つ。溜めている間は、続きが既に届いているときだけ読む。
        let wait = if batch.is_empty() {
            IDLE_TIMEOUT
        } else {
            BATCH_PEEK
        };
        let ready = if batch.is_empty() {
            true
        } else {
            match rd.data_ready(wait) {
                Some(r) => r,
                None => {
                    // 切断を検出した。溜めた分を適用してから再接続する。
                    if let Err(h) = flush(
                        core,
                        wr,
                        &mut batch,
                        &mut batch_bytes,
                        &mut acked,
                        primary_latest,
                    ) {
                        return SessionEnd::Halt(h);
                    }
                    return transient;
                }
            }
        };
        if !ready || batch.len() >= opts.batch_records || batch_bytes >= opts.batch_bytes {
            if let Err(h) = flush(
                core,
                wr,
                &mut batch,
                &mut batch_bytes,
                &mut acked,
                primary_latest,
            ) {
                return SessionEnd::Halt(h);
            }
            if !ready {
                continue;
            }
        }
        let (ty, body) = match read_frame(rd, max_body, wait.max(IDLE_TIMEOUT)) {
            Ok(f) => f,
            Err(FrameError::Closed) => {
                if let Err(h) = flush(
                    core,
                    wr,
                    &mut batch,
                    &mut batch_bytes,
                    &mut acked,
                    primary_latest,
                ) {
                    return SessionEnd::Halt(h);
                }
                return transient;
            }
            Err(FrameError::TooLarge) => {
                engine::log_stderr!("wire-server: replica: oversized frame from the primary");
                return SessionEnd::Halt(Halt::ApplyHalted);
            }
        };
        match ty {
            T_RECORD => {
                let (Some(lsn), Some(raw)) = (body.get(..8).and_then(be_u64), body.get(8..)) else {
                    return SessionEnd::Halt(Halt::ApplyHalted);
                };
                batch_bytes = batch_bytes.saturating_add(raw.len());
                batch.push((lsn, raw.to_vec()));
            }
            T_HEARTBEAT => {
                if let Some(latest) = body.get(..8).and_then(be_u64).filter(|_| body.len() == 8) {
                    primary_latest = latest;
                }
                // 溜めた分を先に確定する（ハートビートは暇な印）。
                if let Err(h) = flush(
                    core,
                    wr,
                    &mut batch,
                    &mut batch_bytes,
                    &mut acked,
                    primary_latest,
                ) {
                    return SessionEnd::Halt(h);
                }
            }
            T_ERROR => {
                if let Err(h) = flush(
                    core,
                    wr,
                    &mut batch,
                    &mut batch_bytes,
                    &mut acked,
                    primary_latest,
                ) {
                    return SessionEnd::Halt(h);
                }
                return match body.first().copied().and_then(classify_error) {
                    Some(h) => {
                        if body.first().copied() == Some(code::RESYNC_REQUIRED) {
                            engine::log_stderr!("{RESYNC_REQUIRED_MESSAGE}");
                        } else {
                            engine::log_stderr!(
                                "wire-server: replica: the primary reported an error"
                            );
                        }
                        SessionEnd::Halt(h)
                    }
                    None => transient,
                };
            }
            _ => {
                engine::log_stderr!("wire-server: replica: unexpected frame from the primary");
                return SessionEnd::Halt(Halt::ApplyHalted);
            }
        }
    }
}

/// 溜めたレコードを 1 トランザクションで適用し、commit 後に Ack（前進時のみ）を送る。
/// 適用の失敗は停止。Ack の書き込み失敗は接続の障害だが、適用は確定済みなので次の接続で再開位置が進む。
fn flush(
    core: &EngineCore,
    stream: &mut ConnWriter,
    batch: &mut Vec<(u64, Vec<u8>)>,
    batch_bytes: &mut usize,
    acked: &mut u64,
    primary_latest: u64,
) -> Result<(), Halt> {
    if batch.is_empty() {
        return Ok(());
    }
    // 同期レプリケーション（`remote_write`。Issue #2245）のため、適用の前に受け取り済みの位置（`R`）を知らせる。
    // 失敗しても適用は続ける（切断は次のループの読み取りが検出する）。
    if let Some((last, _)) = batch.last() {
        let _ = write_frame(stream, T_RECEIVED, &[&last.to_be_bytes()]);
    }
    let report = core
        .apply_replicated_records(batch)
        .map_err(|e| apply_failure(&e))?;
    batch.clear();
    *batch_bytes = 0;
    log_progress(report.last_lsn, primary_latest);
    if report.last_lsn > *acked {
        *acked = report.last_lsn;
        // 失敗しても適用済み。次のループの読み取りが切断を検出して再接続する。
        let _ = write_frame(stream, T_ACK, &[&report.last_lsn.to_be_bytes()]);
    }
    Ok(())
}

/// 進捗を運用者向けの標準エラーへ出す（適用済み LSN とプライマリの最新 LSN）。
fn log_progress(applied: u64, primary_latest: u64) {
    engine::log_stderr!(
        "wire-server: replica: applied lsn={applied} primary_latest={primary_latest} lag={}",
        primary_latest.saturating_sub(applied)
    );
}
