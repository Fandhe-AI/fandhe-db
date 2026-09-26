//! 接続資源保護（読み取りタイムアウト・同時接続数リミッター）を担う共有モジュール。
//!
//! `server::accept_loop_with_limiter`（SQL 表層）・
//! `http::listener::accept_loop_with_limiter`（NoSQL 表層。Issue #743）の
//! 双方から呼ばれ、未認証クライアントの大量接続・Slowloris による
//! スレッド／メモリ枯渇を防ぐ。契約値（読み取りタイムアウト・同時接続数上限・
//! 上限超過時の SQLSTATE）をこのモジュールに集約し、`handshake.rs` や `server.rs`
//! に暫定値が分散しないようにする。`ConnectionLimiter` は `main.rs::run_server`
//! が `match surface` の前に 1 回だけ構築したインスタンスを両表層のループへ渡す
//! ため、プロセス内で選ばれた表層が同一の枠・同一の定数を使う（同時起動しない
//! 前提のため「共有」は構築箇所の単一化で満たされる）。
//!
//! HTTP セッション認証（TASK-174・HTTP-5）の同時有効セッション数上限も
//! [`SessionLimiter`] として本モジュールに集約する（[`MAX_CONNECTIONS`] の
//! 接続数カウンタとは独立。`http::session::store::SessionStore` から使われる）。
//!
//! 対応: TASK-69（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-5, WIRE-6）、
//! TASK-174（対象ビヘイビア HTTP-5）。
//! `SQLSTATE_TOO_MANY_CONNECTIONS` はポインタ: `docs/spec/04-behavior/error-format.md`
//! の `53300` 行を参照。

use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 接続全体（認証前後を問わず）に適用する読み取り・書き込みタイムアウト（WIRE-5）。
/// `server::accept_loop_with_limiter` が受理直後に一度だけ設定し、
/// `handshake::handle_connection_bounded` 側では変更しない（認証前後で値を
/// 切り替える経路を作らない）。
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 同時接続数の上限（WIRE-6）。
pub const MAX_CONNECTIONS: usize = 64;

/// 上限超過時に拒否応答（ErrorResponse）を書き込む際の書き込みタイムアウト。
/// 拒否応答自体が accept ループのブロッキング点にならないよう小さく設定する。
pub const REJECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// NoSQL 表層で TLS 構成済み・`--tls-mode allow` の下、同時接続数上限超過時に
/// 平文／TLS を判別するための先頭バイト `peek` に適用する読み取りタイムアウト
/// （Issue #968 codex-review P1 是正。`docs/design/tls-wire-connection.md`
/// 「HTTPS 表層」節）。`REJECT_WRITE_TIMEOUT` と同じ理由（拒否経路が accept
/// ループ・`RejectWorkerLimiter` の枠を無期限に占有しない）で小さく設定する。
pub const REJECT_TLS_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// commit 成功境界を跨いだ panic 発生時に緊急応答（TASK-153・ERR-1・ERR-5 の
/// `crate::error_response::encode_with_detail` が組み立てる `S`/`C`/`M`＋`D`
/// （`D`=`crate::error_response::MAY_BE_COMMITTED_DETAIL`）の ErrorResponse。
/// `simple_query::build_emergency_response_bytes` 参照）を
/// 書き込むためのソケット書き込みタイムアウト（TASK-97、対象ビヘイビア:
/// RECOVER-6）。この応答は
/// `engine::recovery::panic_hook::emergency_send_decision` が commit-pending
/// 世代の一致を確認できた場合にのみ送出される（`simple_query::
/// build_emergency_response_bytes` のドキュメント参照）。
///
/// `crate::simple_query::execute_and_respond` は「outcome を決定する区間」
/// （`engine.execute_sql_in_session` の呼び出しを含むブロック）だけこの値を
/// `engine::recovery::panic_hook::EmergencyResponseRegistration::register` へ
/// `Duration` として渡す（ソケットへは登録時に設定しない ―― `TcpStream::
/// try_clone` はソケット共有のため登録時に設定すると通常応答の書き込みにも
/// 波及してしまう。`panic_hook` モジュールドキュメント参照）。panic フックが
/// 実際に緊急応答を書き込む直前にのみこの値をソケットへ適用する。
/// panic フックは unwind 中の同期書き込みであり、読み出しを止めたクライアント
/// が終了処理を無期限に遅延させる経路を作らないよう、[`REJECT_WRITE_TIMEOUT`]
/// より緩めつつ有界な値とする（緊急応答は 1 フレームのみで小さいため、通常の
/// クエリ応答〔[`READ_TIMEOUT`]〕ほど長い猶予は不要）。
pub const EMERGENCY_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// 上限超過接続への拒否応答（[`reject_too_many_connections`]）を書き込むために
/// 同時に生成できるワーカースレッド数の上限（review 是正・WIRE-6）。
///
/// `server::accept_loop_with_limiter` は本体をブロックさせないため拒否応答の書き込みを
/// 使い捨てスレッドへ委譲するが、`MAX_CONNECTIONS` の枠管理外で無制限に
/// `std::thread::spawn` すると、攻撃者が上限到達後に接続を連続作成することで
/// スレッド・スタックなどの OS 資源を無制限に消費できてしまう（DoS）。
/// [`RejectWorkerLimiter`] でこのワーカー数自体を別枠の小さい上限に有界化し、
/// 上限に達した場合は応答を書かずに即座に接続をクローズする（fail-closed。
/// 応答が返らない方を、資源枯渇を許す方より安全側とする）。
pub const MAX_REJECT_WORKERS: usize = 16;

/// 「使用中カウンタが `max` 未満なら 1 加算して受理する」CAS 取得ループの
/// 共有実装。[`ConnectionLimiter`]・[`RejectWorkerLimiter`]・[`SessionLimiter`]
/// の 3 者が同一のループを個別に複製していたのを集約する（挙動は完全に不変
/// ―― 呼び出し元の公開 API・シグネチャ・観測可能な振る舞いは変わらない）。
/// 競合下でも「読み取り→上限比較→加算」の間に他スレッドの加算が割り込んでも
/// `max` を超えて受理しないことを `compare_exchange_weak` ループで保証し、
/// 加算は `checked_add` で行いカウンタのオーバーフローを未定義動作にしない
/// （coding-rust.md 準拠）。
fn try_acquire_slot(active: &AtomicUsize, max: usize) -> bool {
    let mut current = active.load(Ordering::Acquire);
    loop {
        if current >= max {
            return false;
        }
        let Some(next) = current.checked_add(1) else {
            return false;
        };
        match active.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// 拒否応答ワーカー 1 本ぶんの所有権。`Drop` で確実に解放する（`ConnectionPermit`
/// と同じ RAII パターン）。
pub struct RejectWorkerPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for RejectWorkerPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 拒否応答ワーカースレッド数を [`MAX_REJECT_WORKERS`] 以内に有界化する専用の
/// 小さいセマフォ。`ConnectionLimiter`（認証済み接続の枠）とは別枠で管理し、
/// 拒否経路が本来の接続枠を消費しないという既存の契約を変えない。
#[derive(Clone)]
pub struct RejectWorkerLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

impl RejectWorkerLimiter {
    /// ワーカー数上限を `max` として新しいリミッターを作る。
    pub fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。`ConnectionLimiter`
    /// と同じ CAS ループ（[`try_acquire_slot`] へ集約済み）で競合下でも `max` を
    /// 超えて確保しない。
    pub fn try_acquire(&self) -> Option<RejectWorkerPermit> {
        if try_acquire_slot(&self.active, self.max) {
            Some(RejectWorkerPermit {
                active: Arc::clone(&self.active),
            })
        } else {
            None
        }
    }
}

/// 簡易クエリ応答（`RowDescription`/`DataRow`×N/`CommandComplete`/
/// `ReadyForQuery`）を 1 回の write へ束ねる際の未送出バッファ上限
/// （フラッシュ閾値。Issue #481）。
///
/// 「拒否」の上限ではない ―― `crate::response_buffer::ResponseBuffer` は
/// この値を超えそうになったら、それまで積んだぶんを先に送出してから続ける
/// （分割送出）。接続あたりの追加常駐メモリをこの値 + フレーム 1 個ぶんに
/// 有界化し、[`MAX_CONNECTIONS`] 全体でも合計常駐メモリを有界に保つ。
pub const MAX_RESPONSE_BUFFER_BYTES: usize = 1024 * 1024;

/// COPY サブプロトコル（Issue #939・WIRE-17）が構文・上限違反等でエラー応答を
/// 返した後、クライアントが送り続ける可能性のある残り CopyData／CopyDone／
/// CopyFail を黙って読み捨てる「読み捨て状態」の総バイト数上限。PostgreSQL の
/// 実際のプロトコル契約（COPY IN 中のエラーは即座に ErrorResponse を返しつつ、
/// 相手が既に送信済みの残りデータは読み捨てて `ReadyForQuery` へ進む）を
/// 踏襲しつつ、この読み捨てが接続スロットを無期限に占有する DoS 経路になるのを
/// 防ぐ（`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。超過時は読み捨てず `08P01` で切断する（fail-closed）。
pub const COPY_DISCARD_MAX_BYTES: usize = 16 * 1024 * 1024;

/// [`COPY_DISCARD_MAX_BYTES`] へ各フレームの読み捨てごとに加算するヘッダー
/// 分の固定オーバーヘッド（型バイト 1 + 長さフィールド 4）。codex-review
/// 指摘（PRRT_kwDOUAKASM6ltyY2）: 旧実装は CopyData／Flush／Sync の *body* 長
/// のみを予算へ加算しており、body が空（宣言長 4）のフレームを連送しても
/// 予算を一切消費しなかったため、クライアントが極小フレームを送り続けて
/// 接続スロットを無期限に占有できた。フレームヘッダー自体の消費バイト量も
/// 必ず予算へ含めることで、body 長に関わらず読み捨てフレーム数に比例して
/// 予算が減っていくことを保証する。
pub const COPY_DISCARD_FRAME_OVERHEAD_BYTES: usize = 5;

/// 読み捨て状態（[`COPY_DISCARD_MAX_BYTES`] 参照）で許容するフレーム件数の
/// 上限。バイト予算とは独立に判定する（codex-review 指摘・
/// PRRT_kwDOUAKASM6ltyY2）。[`COPY_DISCARD_FRAME_OVERHEAD_BYTES`] によるバイト
/// 予算への加算だけでは、バイト予算を使い切るまでに要するフレーム数
/// （16 MiB ÷ 5 バイト ≈ 335 万件）が非現実的に大きく、実効的な上限として
/// 機能しない。本値は 1 個の COPY セッションで正当なクライアントが送る
/// Flush／Sync／空 CopyData の件数を大きく超え、かつ攻撃者が極小フレームを
/// 連送して接続スロットを無期限に占有する経路を有限時間で遮断する。
pub const COPY_DISCARD_MAX_MESSAGES: usize = 4096;

/// SQLSTATE `53300`（too_many_connections）。ポインタ:
/// `docs/spec/04-behavior/error-format.md`。値は `engine::error_format::
/// ErrorClass`（SSOT。TASK-152・ERR-2）由来（TASK-153・ERR-1 の分散定数 SSOT 化）。
pub const SQLSTATE_TOO_MANY_CONNECTIONS: &str =
    engine::error_format::ErrorClass::ConnectionLimitExceeded.wire_code();

/// 同時接続数の枠 1 つぶんの所有権。`Drop` で確実に解放する（RAII。早期 return や
/// panic があっても枠解放漏れが起きないようにする）。
pub struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 同時接続数の共有リミッター。`server::accept_loop_with_limiter` がクローンを保持し、
/// 受理のたびに [`ConnectionLimiter::try_acquire`] を呼ぶ。
#[derive(Clone)]
pub struct ConnectionLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

impl ConnectionLimiter {
    /// 同時接続数の上限を `max` として新しいリミッターを作る。
    pub fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。CAS ループ
    /// （[`try_acquire_slot`] へ集約済み）で「読み取り→上限比較→加算」の間の
    /// 競合を許さず、複数スレッドが同時に accept しても上限を超えて確保できない
    /// ようにする。加算は `checked_add` で行い、カウンタのオーバーフローを
    /// 未定義動作にしない（coding-rust.md 準拠）。
    pub fn try_acquire(&self) -> Option<ConnectionPermit> {
        if try_acquire_slot(&self.active, self.max) {
            Some(ConnectionPermit {
                active: Arc::clone(&self.active),
            })
        } else {
            None
        }
    }

    /// 現在の使用中枠数（テスト・ログ用の観測）。
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// 上限値（テスト用の観測）。
    pub fn max(&self) -> usize {
        self.max
    }
}

/// 同時有効セッション数の上限（TASK-174・HTTP-5）。[`MAX_CONNECTIONS`]（接続数の枠）
/// とは独立のカウンタで、一方の枯渇が他方へ波及しない（`http::session::store`
/// モジュール doc 参照）。
pub const MAX_SESSIONS: usize = 256;

/// セッショントークンの固定有効期間（TASK-174・HTTP-4）。発行時刻起点で
/// スライドしない（`http::session::store::SessionStore::lookup` は
/// `issued_at` を更新しない）。
pub const SESSION_TTL: Duration = Duration::from_secs(3600);

/// セッション枠 1 つぶんの所有権。`Drop` で確実に解放する（[`ConnectionPermit`]
/// と同じ RAII パターン）。`http::session::store::SessionStore` のエントリが
/// この permit を保持し、エントリの削除（`close`・期限切れ回収）がそのまま
/// 枠解放になる。
pub struct SessionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for SessionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 同時有効セッション数の共有リミッター。`http::session::store::SessionStore`
/// が保持し、発行（`issue`）のたびに [`SessionLimiter::try_acquire`] を呼ぶ。
/// `ConnectionLimiter`・`RejectWorkerLimiter` と同じ CAS ループ
/// （[`try_acquire_slot`]）を使うが、`Arc<AtomicUsize>` は独立に持つため
/// 3 者のカウンタは互いに影響しない。
#[derive(Clone)]
pub struct SessionLimiter {
    active: Arc<AtomicUsize>,
    max: usize,
}

impl SessionLimiter {
    /// 同時有効セッション数の上限を `max` として新しいリミッターを作る。
    pub fn new(max: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            max,
        }
    }

    /// `active` が `max` 未満なら枠を 1 つ確保して `Some` を返す。
    pub fn try_acquire(&self) -> Option<SessionPermit> {
        if try_acquire_slot(&self.active, self.max) {
            Some(SessionPermit {
                active: Arc::clone(&self.active),
            })
        } else {
            None
        }
    }

    /// 現在の使用中枠数（テスト・観測用）。
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// 上限値（テスト用の観測）。
    pub fn max(&self) -> usize {
        self.max
    }
}

/// `stream` に読み取り・書き込み双方のタイムアウトを設定する。設定自体の失敗
/// （OS レベルのソケットオプション設定エラー）は `Err` を返し、呼び出し元は
/// タイムアウトなしで扱い続けるより安全側（接続を破棄する）に倒す（fail-closed）。
pub fn apply_read_timeout(stream: &TcpStream, timeout: Duration) -> io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}

/// 同時接続数の上限超過を通知する ErrorResponse（'E', severity=FATAL,
/// code=`53300`）を書き込み、接続を閉じる。`handshake.rs` の private な
/// `write_error_response` には依存せず自己完結させる（TASK-68/71 との衝突回避）。
/// フィールドは S/C/M のみで、他テナント・存在情報・ピア識別情報は含めない。
///
/// 呼び出し元（`server::accept_loop_with_limiter`）はこの関数を呼ぶ時点でまだ
/// `std::thread::spawn` へ到達していない（スレッドを生成せずに拒否する）。
/// 書き込み失敗は無視する（拒否経路で新たなブロッキング点・panic を作らないため。
/// クライアントが応答を受け取れなくても、最終的に `shutdown` で接続は閉じる）。
pub fn reject_too_many_connections(mut stream: TcpStream, max: usize) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));

    let message = format!("too many connections (max {max})");
    let mut body = Vec::new();
    body.push(b'S');
    body.extend_from_slice(b"FATAL");
    body.push(0);
    body.push(b'C');
    body.extend_from_slice(SQLSTATE_TOO_MANY_CONNECTIONS.as_bytes());
    body.push(0);
    body.push(b'M');
    body.extend_from_slice(message.as_bytes());
    body.push(0);
    body.push(0); // フィールド終端

    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'E');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);

    use std::io::Write as _;
    let _ = stream.write_all(&msg);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// 接続単位の名前付き／無名ステートメント保持数の上限（Issue #933・TASK-71・
/// WIRE-11。拡張クエリプロトコルの Parse。`extended_query::PreparedStatementStore`
/// が超過を [`engine::error_format::ErrorClass::PayloadTooLarge`]（`54000`）で
/// 拒否する。本リポの実装既定値（spec は数値までは定めない）。
pub const MAX_PREPARED_STATEMENTS_PER_SESSION: usize = 64;

/// ステートメント名の最大バイト長（Issue #933）。PostgreSQL の `NAMEDATALEN`
/// （64。末尾 NUL 込み）と揃え、識別子として通用する実用上の長さに抑える。
pub const MAX_STATEMENT_NAME_LEN: usize = 63;

/// 接続単位で保持する Parse 済み SQL テキストの累計バイト数上限（Issue #933）。
/// [`MAX_PREPARED_STATEMENTS_PER_SESSION`] 件 × 1 メッセージ上限
/// （`framing::MAX_MESSAGE_LEN`）まで許すと 1 接続あたり数十 MiB を常駐しうるため、
/// 個々のメッセージ長とは独立に接続単位の総量へ別途歯止めをかける
/// （本リポの実装既定値）。
pub const MAX_PREPARED_SQL_BYTES_PER_SESSION: usize = 4 * 1024 * 1024;

/// 接続単位で保持する名前付き portal 数の上限（Issue #934・TASK-71・WIRE-11。
/// 拡張クエリプロトコルの Bind）。[`MAX_PREPARED_STATEMENTS_PER_SESSION`] と
/// 同じ設計（無名 `""` は件数にカウントせず黙って置換する）。本リポの実装
/// 既定値（spec は数値までは定めない）。
pub const MAX_PORTALS_PER_SESSION: usize = 64;

/// 接続（セッション）単位で、中断（`PortalSuspended`）中の全 portal が
/// 合算で保持できる、エンコード済み `DataRow` の合計バイト数上限
/// （Issue #934。PR #1013 レビュー指摘・P0 を受け、名称どおりセッション
/// 全体の上限として運用する）。`max_rows` による分割送出で残り行を
/// メモリ上に保持し続ける間の上限であり、`extended_query::Portal` が
/// `Suspended` 状態へ遷移する行を 1 行エンコードするたびに、他の全 portal
/// の中断保持分と合算して判定する（超過が判明した時点で残りの行の
/// エンコードを打ち切り、結果セット全体を先に確保しない）。超過は
/// `54000`（`PayloadTooLarge`）。本リポの実装既定値であり、`MAX_PORTALS_
/// PER_SESSION` 個の portal が同時に存在しうる前提でこの合計値へ上限を
/// 適用する（`docs/design/wire-extended-query-bind-execute-sync.md` 参照）。
pub const MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION: usize = 16 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::net::TcpListener;

    /// 枠の取得・解放（RAII）がネットワークを介さず単体で検証できること。
    #[test]
    fn try_acquire_enforces_max_and_releases_on_drop() {
        let limiter = ConnectionLimiter::new(2);
        let permit_a = limiter.try_acquire().expect("first permit");
        let permit_b = limiter.try_acquire().expect("second permit");
        assert!(
            limiter.try_acquire().is_none(),
            "third permit must be rejected at max=2"
        );
        assert_eq!(limiter.active(), 2);

        drop(permit_a);
        assert_eq!(limiter.active(), 1);
        let permit_c = limiter
            .try_acquire()
            .expect("permit must be released on drop");

        drop(permit_b);
        drop(permit_c);
        assert_eq!(limiter.active(), 0);
    }

    /// 並行 `try_acquire` を回しても、観測した使用中枠数が `max` を超えないこと
    /// （CAS ループの競合耐性の確認）。
    #[test]
    fn try_acquire_never_exceeds_max_under_concurrency() {
        const MAX: usize = 8;
        const THREADS: usize = 16;
        const ITERS: usize = 200;

        let limiter = ConnectionLimiter::new(MAX);
        let max_observed = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let limiter = limiter.clone();
                let max_observed = Arc::clone(&max_observed);
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        if let Some(permit) = limiter.try_acquire() {
                            let observed = limiter.active();
                            max_observed.fetch_max(observed, Ordering::AcqRel);
                            drop(permit);
                        }
                    }
                });
            }
        });

        assert!(
            max_observed.load(Ordering::Acquire) <= MAX,
            "observed active count must never exceed max={MAX}"
        );
        assert_eq!(limiter.active(), 0, "all permits must be released");
    }

    /// `apply_read_timeout` がソケットへ実際に反映されること。
    #[test]
    fn apply_read_timeout_sets_read_and_write_timeouts() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        let timeout = Duration::from_millis(250);
        apply_read_timeout(&server_stream, timeout).expect("apply timeout");

        assert_eq!(
            server_stream.read_timeout().expect("read timeout"),
            Some(timeout)
        );
        assert_eq!(
            server_stream.write_timeout().expect("write timeout"),
            Some(timeout)
        );
        drop(client);
    }

    /// `reject_too_many_connections` が `'E'` / SQLSTATE `53300` を含む
    /// ErrorResponse を書き込み、その後 EOF になること。
    #[test]
    fn reject_too_many_connections_writes_53300_error_and_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server_stream, _) = listener.accept().expect("accept");

        std::thread::spawn(move || {
            reject_too_many_connections(server_stream, MAX_CONNECTIONS);
        });

        let mut header = [0u8; 1];
        client.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "expected ErrorResponse");

        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).expect("read length");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        client.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains(SQLSTATE_TOO_MANY_CONNECTIONS),
            "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
        );

        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        assert_eq!(n, 0, "connection must be closed after the rejection");
    }

    /// [`SessionLimiter`] も [`ConnectionLimiter`] と同じ RAII 契約（上限到達で
    /// 拒否・drop で解放）を持つこと。`ConnectionLimiter` とはカウンタを
    /// 共有しないため、片方の枠を使い切ってももう片方には影響しない。
    #[test]
    fn session_limiter_enforces_max_independently_of_connection_limiter() {
        let sessions = SessionLimiter::new(2);
        let connections = ConnectionLimiter::new(2);

        let s1 = sessions.try_acquire().expect("first session permit");
        let s2 = sessions.try_acquire().expect("second session permit");
        assert!(
            sessions.try_acquire().is_none(),
            "third session permit must be rejected at max=2"
        );
        assert_eq!(sessions.active(), 2);

        // 接続枠は未使用のままであること（独立カウンタの確認）。
        assert_eq!(connections.active(), 0);
        let c1 = connections.try_acquire().expect("connection permit");
        assert_eq!(sessions.active(), 2, "session count must not be affected");

        drop(s1);
        assert_eq!(sessions.active(), 1);
        let s3 = sessions
            .try_acquire()
            .expect("permit must be released on drop");

        drop(s2);
        drop(s3);
        drop(c1);
        assert_eq!(sessions.active(), 0);
        assert_eq!(connections.active(), 0);
    }

    /// 並行 `try_acquire` を回しても [`SessionLimiter`] の使用中枠数が `max`
    /// を超えないこと（CAS ループの競合耐性）。
    #[test]
    fn session_limiter_never_exceeds_max_under_concurrency() {
        const MAX: usize = 8;
        const THREADS: usize = 16;
        const ITERS: usize = 200;

        let limiter = SessionLimiter::new(MAX);
        let max_observed = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                let limiter = limiter.clone();
                let max_observed = Arc::clone(&max_observed);
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        if let Some(permit) = limiter.try_acquire() {
                            let observed = limiter.active();
                            max_observed.fetch_max(observed, Ordering::AcqRel);
                            drop(permit);
                        }
                    }
                });
            }
        });

        assert!(
            max_observed.load(Ordering::Acquire) <= MAX,
            "observed active count must never exceed max={MAX}"
        );
        assert_eq!(limiter.active(), 0, "all permits must be released");
    }
}
