//! TCP 接続の受け付け・bind アドレスの通信路保護要件検証を担う。
//!
//! `main.rs::run_server` から呼ばれる（`tests/` からの結合テストが同じ挙動を
//! 直接検証できるよう lib.rs 経由で公開する）。responsibility はソケットレベルの
//! 防御（accept ループ・接続の受理／拒否）に限り、bind アドレスの通信路保護要件
//! 検証（TLS 未構成時は loopback 限定。TASK-70・WIRE-7）は [`crate::bind_guard`]
//! へ、同時接続数の有界化・読み取りタイムアウトの契約値は [`crate::limits`]
//! （TASK-69・WIRE-5, WIRE-6）へ、wire プロトコルそのものの解釈は
//! [`crate::handshake::handle_connection_bounded`] へそれぞれ委譲する。
//!
//! 対応: TASK-67 の review 是正（loopback bind の TOCTOU 排除。TASK-70 で
//! [`crate::bind_guard`] へ移設・拡張）。TASK-69（WIRE-5, WIRE-6。接続資源保護）。
//!
//! ソケットオプション方針: accept 直後に全接続へ `TCP_NODELAY` を設定し、
//! Nagle アルゴリズムを無効化する。簡易クエリ応答は原則
//! `crate::response_buffer::ResponseBuffer` により 1 回の `write_all` へ
//! 束ねられる（Issue #481）が、応答合計が
//! `crate::limits::MAX_RESPONSE_BUFFER_BYTES` を超える場合の分割送出・
//! `SET`/`INSERT` 等の `CommandComplete` + `ReadyForQuery`（2 write。本 Issue の
//! 対象外）では引き続き複数 `write_all` に分かれるため、delayed ACK との
//! 相互作用による遅延を避ける本設定を維持する。

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use crate::auth::UserStore;
use crate::bind_guard::{GuardedBindAddrs, TransportSecurity};
use crate::limits::{self, ConnectionLimiter, RejectWorkerLimiter};
use engine::core::EngineCore;

/// 旧 TASK-67 review 是正時点の公開 API との後方互換ラッパー。
///
/// TASK-70（WIRE-7）で本体の実装は [`crate::bind_guard::GuardedBindAddrs`]
/// （TLS 有無に応じた通信路保護要件を型で表現できる形）へ移設・拡張したが、
/// 本関数はすでに公開 API（`pub fn bind_loopback`）として利用側に届いている
/// ため、AGENTS.md の「公開 API・エラー契約の互換性（P1）」に従い削除せず
/// 残す。内部では [`GuardedBindAddrs::resolve`]（[`TransportSecurity::Cleartext`]
/// 固定 = 従来どおり loopback 限定）を呼ぶだけの薄いラッパーであり、挙動・
/// エラーメッセージ文言は旧実装と同一（[`crate::bind_guard::BindGuardError`]
/// の `Display` が旧 `validate_loopback_bind` と同じ文言を維持している）。
///
/// 新規コードは `GuardedBindAddrs::resolve` を直接呼ぶこと（TLS 導入時に
/// `TransportSecurity` へ variant が増えても、本ラッパーは cleartext 固定の
/// 意味論を変えない）。
///
/// 対応: PR #182 レビュー是正。[`crate::bind_guard::BindGuardError`] の
/// `Display` は新 API 向けに `bind_addr` を含む文脈情報を足しており
/// （`NonLoopback` に `(from {bind_addr})` 等）、旧 `validate_loopback_bind` /
/// `bind_loopback` のエラー文言とは異なる。本ラッパーは呼び出し側が診断・
/// 照合に利用しうる旧文言（`failed to bind {bind_addr} ({addrs:?}): {e}` /
/// `refusing to bind non-loopback address {addr}: ...`）を `BindGuardError`
/// の `Display` に委譲せず個別に再現し、公開 API の互換性を保つ。
#[deprecated(
    since = "0.1.0",
    note = "use crate::bind_guard::GuardedBindAddrs::resolve(..., TransportSecurity::Cleartext) instead"
)]
pub fn bind_loopback(bind_addr: &str) -> Result<TcpListener, String> {
    use crate::bind_guard::BindGuardError;

    let guarded = GuardedBindAddrs::resolve(bind_addr, TransportSecurity::Cleartext).map_err(
        |e| match e {
            BindGuardError::Resolve { bind_addr, source } => {
                format!("cannot resolve bind address {bind_addr}: {source}")
            }
            BindGuardError::NoAddress { bind_addr } => {
                format!("bind address {bind_addr} did not resolve to any socket address")
            }
            BindGuardError::NonLoopback { addr, .. } => format!(
                "refusing to bind non-loopback address {addr}: cleartext password \
                 authentication is not yet protected by TLS (TASK-72/WIRE-9); \
                 bind to a loopback address (e.g. 127.0.0.1) or place a trusted TLS \
                 terminator in front of this listener"
            ),
        },
    )?;
    let addrs = guarded.addrs().to_vec();
    guarded
        .bind()
        .map_err(|e| format!("failed to bind {bind_addr} ({addrs:?}): {e}"))
}

/// 接続受け付けループ本体。1 接続 1 スレッドで処理するが、以下の防御を課す
/// （WIRE-5, WIRE-6。契約値・実装は [`crate::limits`] に集約）:
///
/// - `limiter` の枠を確保できない接続は [`crate::handshake::handle_connection_bounded`] へ
///   進ませず、`limiter` の枠も消費しない短命な使い捨てスレッドへ
///   [`limits::reject_too_many_connections`] を委譲し、`53300`
///   （too_many_connections）の ErrorResponse を返してから即座にクローズする。
///   拒否応答の書き込み（最大 `REJECT_WRITE_TIMEOUT` の同期 `write_all`）を
///   accept ループ本体でブロックさせないことで、受信ウィンドウを閉じた相手を
///   繰り返し接続させて待受自体を止める経路（資源枯渇防御自体が新たな DoS
///   経路になる問題）を避ける。この拒否スレッド自体も
///   [`limits::RejectWorkerLimiter`]（`MAX_REJECT_WORKERS`）で別枠に有界化し、
///   上限到達後は応答を書かずに即座にクローズする（review 是正: 拒否経路の
///   無制限 `thread::spawn` によるスレッド／スタック枯渇 DoS を防ぐ）
/// - 受理した接続には読み取り・書き込み双方に `read_timeout` を一度だけ設定してから
///   ハンドシェイクへ渡す（認証前後を問わず同一値を維持する。WIRE-5）。超過時は
///   `handle_connection_bounded` が応答を書かずに `Err` を返し、スレッド終了で枠が解放される
///
/// 各スレッドの join ハンドルは無視するが、これは panic をプロセスへ波及させない
/// ための隔離を意図したものではない（`std::thread::spawn` の join ハンドル無視
/// 自体は panic の伝播経路に関与しない）。実際には `main.rs::run_server` が起動時に
/// 導入する `engine::recovery::fail_fast::install`（TASK-99・RECOVER-8）により、
/// どの接続スレッドで panic が起きてもプロセス全体を fail-fast で終了させる
/// （panic 発生時点で「共有状態が汚染された可能性がある」ことを安全側に倒すため。
/// 他接続の継続稼働より優先する）。
///
/// 対応: TASK-69（WIRE-5, WIRE-6）。旧 5 引数シグネチャ（`max_connections` を
/// 直接受け取る形）は [`accept_loop`]（deprecated 互換ラッパー）として維持する
/// （codex-review / Cursor Bugbot 再指摘: 別名ラッパーでは後方互換にならず、
/// 旧名・旧シグネチャをそのまま残す必要がある）。
pub fn accept_loop_with_limiter(
    listener: TcpListener,
    store: Arc<UserStore>,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    accept_loop_inner(
        listener,
        store,
        None,
        None,
        crate::tls_opt::TlsMode::Allow,
        limiter,
        read_timeout,
    )
}

/// engine（SQL 表層）を接続した接続受け付けループ（TASK-73・WIRE-1）。
/// `main.rs::run_server` が `--db` 指定時にこちらを呼ぶ。受理・拒否・タイムアウト・
/// 有界化の契約は [`accept_loop_with_limiter`] と同一で、各接続ハンドラに
/// `engine::core::EngineCore`（`Arc` で複数接続スレッド間共有。`EngineCore` は
/// `Send + Sync`）を渡す点のみが異なる。
pub fn accept_loop_with_engine(
    listener: TcpListener,
    store: Arc<UserStore>,
    engine: Arc<EngineCore>,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    accept_loop_inner(
        listener,
        store,
        Some(engine),
        None,
        crate::tls_opt::TlsMode::Allow,
        limiter,
        read_timeout,
    )
}

/// TLS opt-in を含む接続受け付けループ（Issue #966）。`tls` が `Some` の
/// 場合に限り、各接続で `SSLRequest` へ `'S'` を返し TLS ハンドシェイクへ
/// 進む（[`crate::handshake::handle_connection_with_options`]）。`None` の
/// 場合は [`accept_loop_with_engine`] とビット単位で同一の平文経路になる。
///
/// **`--tls-mode` opt-in（Issue #967）**: 本関数は常に
/// [`crate::tls_opt::TlsMode::Allow`]（`tls` が `Some` のとき `SSLRequest`
/// を経ない平文 StartupMessage も受理する。#966 時点の既存挙動）で
/// [`accept_loop_with_tls_mode`] へ委譲する後方互換ラッパーとして維持する
/// （AGENTS.md 公開 API 互換方針）。`--tls-mode require` の平文拒否は新規
/// エントリポイント [`accept_loop_with_tls_mode`] が担う。
pub fn accept_loop_with_tls(
    listener: TcpListener,
    store: Arc<UserStore>,
    engine: Option<Arc<EngineCore>>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    accept_loop_with_tls_mode(
        listener,
        store,
        engine,
        tls,
        crate::tls_opt::TlsMode::Allow,
        limiter,
        read_timeout,
    )
}

/// TLS opt-in と平文接続ポリシー（[`crate::tls_opt::TlsMode`]）の双方を
/// 含む接続受け付けループ（Issue #967。`main.rs::run_server` の `--tls-cert`／
/// `--tls-key`／`--tls-mode` opt-in から呼ばれる想定）。`tls` が `None` の
/// 場合、`mode` は無視され [`accept_loop_with_engine`] とビット単位で同一の
/// 平文経路になる。各接続の実処理は
/// [`crate::handshake::handle_connection_with_tls_mode`] が担う。
pub fn accept_loop_with_tls_mode(
    listener: TcpListener,
    store: Arc<UserStore>,
    engine: Option<Arc<EngineCore>>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
    mode: crate::tls_opt::TlsMode,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    accept_loop_inner(listener, store, engine, tls, mode, limiter, read_timeout)
}

fn accept_loop_inner(
    listener: TcpListener,
    store: Arc<UserStore>,
    engine: Option<Arc<EngineCore>>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
    mode: crate::tls_opt::TlsMode,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    // 拒否応答ワーカースレッドの有界化専用リミッター（`limiter` とは別枠。
    // review 是正: 拒否経路の無制限 `thread::spawn` による DoS 対策）。
    let reject_limiter = RejectWorkerLimiter::new(limits::MAX_REJECT_WORKERS);

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                engine::log_stderr!("wire-server: accept error: {e}");
                continue;
            }
        };

        let Some(permit) = limiter.try_acquire() else {
            // 上限超過: ハンドシェイクへ進ませず、スレッドを生成せずに `53300` を
            // 返してから即座にクローズする（WIRE-6）。ピアアドレス等の識別情報は
            // ログに出さない。
            engine::log_stderr!(
                "wire-server: rejecting connection: too many connections (active={}, max={})",
                limiter.active(),
                limiter.max()
            );
            // `reject_too_many_connections` は同期 `write_all`（最大
            // `REJECT_WRITE_TIMEOUT`）を伴うため、accept ループ本体では呼ばない。
            // 受信ウィンドウを閉じた相手を繰り返し接続させることで待受自体を
            // 止められる経路（今回追加した資源枯渇防御が新たな DoS 経路になる問題）
            // を避けるため、拒否応答の書き込みは短命な使い捨てスレッドへ委譲し、
            // accept ループは即座に次の `accept` へ戻る。このスレッドは
            // `limiter` の枠を消費しない（枠管理対象は認証済み接続処理のみ）。
            //
            // ただしこの拒否スレッド自体を無制限に生成すると、攻撃者が上限到達後に
            // 接続を連続作成することでスレッド／スタックなどの OS 資源を無制限に
            // 消費できてしまう（review 是正）。`reject_limiter` で別枠に有界化し、
            // 上限に達した場合は応答を書かずに即座にクローズする（fail-closed。
            // 応答が返らない方を、資源枯渇を許す方より安全側とする）。
            let max = limiter.max();
            match reject_limiter.try_acquire() {
                Some(reject_permit) => {
                    // `std::thread::spawn` はスレッド生成失敗時に panic し、
                    // accept ループ自体を停止させうる（review 指摘）ため、
                    // panic しない `Builder::spawn` を使い、失敗時はログのみで
                    // 継続する。生成に失敗した場合、`stream`・`reject_permit` は
                    // クロージャごと `Err` の一部としてこの場でドロップされ、
                    // 接続は応答なしにクローズされる。
                    if let Err(e) = std::thread::Builder::new().spawn(move || {
                        // 拒否応答の書き込み中だけ `reject_permit` を保持し、
                        // スレッド終了時（正常終了・panic いずれも）に Drop で
                        // 確実に枠を解放する。
                        let _reject_permit = reject_permit;
                        limits::reject_too_many_connections(stream, max);
                    }) {
                        engine::log_stderr!(
                            "wire-server: failed to spawn reject worker thread: {e}"
                        );
                    }
                }
                None => {
                    // 拒否ワーカーも枯渇: 新たにスレッドを生成せず、応答を書かずに
                    // 即座にクローズする（有界化を優先し fail-closed に倒す）。
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
            continue;
        };

        if let Err(e) = limits::apply_read_timeout(&stream, read_timeout) {
            engine::log_stderr!("wire-server: failed to configure connection timeouts: {e}");
            // `permit` はここでスコープを抜けて解放される。
            continue;
        }

        // TCP_NODELAY を有効化する（Nagle アルゴリズムの無効化）。
        // PostgreSQL 本体・libpq も同様に接続へ TCP_NODELAY を設定する慣行があり、
        // 簡易クエリ応答は複数の小さな `write_all`（RowDescription・DataRow・
        // CommandComplete・ReadyForQuery 等）に分かれて送出されるため、Nagle
        // アルゴリズムと受信側の delayed ACK が相互作用し 40ms 級の余計な遅延が
        // 発生しうる。失敗時は `apply_read_timeout` と同様に当該接続をスキップする
        // （fail-closed。ソケットオプション設定に失敗した接続を放置しない）。
        if let Err(e) = stream.set_nodelay(true) {
            engine::log_stderr!("wire-server: failed to set TCP_NODELAY: {e}");
            // `permit` はここでスコープを抜けて解放される。
            continue;
        }

        let store = Arc::clone(&store);
        let engine = engine.clone();
        let tls = tls.clone();
        std::thread::spawn(move || {
            // 接続処理中は `permit` を保持し続け、スレッド終了時（正常終了・panic
            // いずれも）に Drop で確実に枠を解放する。
            let _permit = permit;
            // `tls` が `None` の間は既存の 2 エントリポイントをそのまま呼び、
            // ビット単位で従来と同一の平文経路を維持する（受入基準 2。`mode` は
            // `tls` が `None` の間は意味を持たない）。
            let result = match (&engine, &tls) {
                (_, Some(_)) => crate::handshake::handle_connection_with_tls_mode(
                    stream,
                    &store,
                    engine.as_deref(),
                    tls,
                    mode,
                ),
                (Some(engine), None) => {
                    crate::handshake::handle_connection_with_engine(stream, &store, engine)
                }
                (None, None) => crate::handshake::handle_connection_bounded(stream, &store),
            };
            if let Err(e) = result {
                engine::log_stderr!("wire-server: connection error: {e}");
            }
        });
    }
}

/// 同時接続数の暫定上限だった旧定数。TASK-69（WIRE-6）で
/// [`crate::limits::MAX_CONNECTIONS`] が正式な契約値として上限を管理するように
/// なったため、`accept_loop_with_limiter` 本体は本定数を参照しない。すでに公開
/// API（`pub const MAX_CONCURRENT_CONNECTIONS`）として利用側に届いている可能性
/// があるため、AGENTS.md の「公開 API・エラー契約の互換性（P1）」に従い削除せず
/// 残す（[`bind_loopback`] と同じ後方互換方針）。
#[deprecated(since = "0.1.0", note = "use crate::limits::MAX_CONNECTIONS instead")]
pub const MAX_CONCURRENT_CONNECTIONS: usize = limits::MAX_CONNECTIONS;

/// 認証前フェーズにのみ課していた I/O 期限の旧定数。TASK-69（WIRE-5）で
/// [`crate::limits::READ_TIMEOUT`] が接続全体（認証前後を問わず）へ適用する
/// 単一の契約値になったため、`accept_loop_with_limiter` 本体は本定数を参照
/// しない。
#[deprecated(since = "0.1.0", note = "use crate::limits::READ_TIMEOUT instead")]
pub const CONNECTION_IO_TIMEOUT: Duration = limits::READ_TIMEOUT;

/// 認証後のセッションに課していた緩いアイドル期限の旧定数。WIRE-5 の契約が
/// 「認証前後で同一の読み取りタイムアウトを適用する」単一値方式へ統一された
/// ため、`accept_loop_with_limiter`／`handle_connection_bounded` のどちらも
/// この値で挙動を切り替えることはない。[`accept_loop`]（deprecated 互換
/// ラッパー）が受け取る同名引数もこの理由で無視する（doc 参照）。
#[deprecated(
    since = "0.1.0",
    note = "post-auth idle timeout switching was removed by WIRE-5 (single read_timeout contract); this value is unused"
)]
pub const POST_AUTH_IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// 旧 `(listener, store, max_connections, io_timeout, post_auth_idle_timeout)`
/// 5 引数シグネチャとの後方互換ラッパー（**旧名・旧シグネチャをそのまま維持**。
/// codex-review / Cursor Bugbot 再指摘: 別名のラッパーを追加するだけでは
/// 呼び出し元のコンパイルが通らず後方互換にならないため、新実装は
/// [`accept_loop_with_limiter`] という別名へ移し、この名前・シグネチャの方を
/// 互換層として残す）。
///
/// TASK-69（WIRE-5, WIRE-6）で本体の実装は
/// `(listener, store, limiter: ConnectionLimiter, read_timeout)` の 4 引数へ
/// 移行し、認証前後で読み取りタイムアウトを切り替える経路も廃止した（WIRE-5:
/// 接続全体に同一の期限を適用する単一値方式）。本関数はすでに公開 API として
/// 利用側に届いている可能性のある旧シグネチャを維持しつつ、内部では新実装
/// （[`accept_loop_with_limiter`] と [`ConnectionLimiter::new`]）へ委譲する
/// （[`bind_loopback`] と同じ後方互換方針）。
///
/// `post_auth_idle_timeout` は WIRE-5 の単一タイムアウト契約により**無視**する
/// （認証前後で値を切り替える経路自体が存在しないため、渡された値を使う先が
/// ない）。新規コードは `accept_loop_with_limiter` を `ConnectionLimiter` と
/// ともに直接呼ぶこと。
#[deprecated(
    since = "0.1.0",
    note = "use accept_loop_with_limiter(listener, store, ConnectionLimiter::new(max_connections), read_timeout) instead; post_auth_idle_timeout is ignored (WIRE-5 uses a single read_timeout for the whole connection)"
)]
pub fn accept_loop(
    listener: TcpListener,
    store: Arc<UserStore>,
    max_connections: usize,
    io_timeout: Duration,
    _post_auth_idle_timeout: Duration,
) {
    accept_loop_with_limiter(
        listener,
        store,
        ConnectionLimiter::new(max_connections),
        io_timeout,
    );
}

#[cfg(test)]
mod tests {
    /// フィクスチャ用一時ディレクトリを `Drop` で削除するガード（Issue #1303）。
    /// 削除失敗はテストを失敗させず stderr へ出すのみ。
    struct DirCleanup(std::path::PathBuf);

    impl Drop for DirCleanup {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "warning: failed to remove temp dir {}: {e}",
                        self.0.display()
                    );
                }
            }
        }
    }

    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
    /// プロセス内単調カウンタ（`tests/wire_auth.rs` と同一クラスの競合対策。
    /// Issue #172: lib テストバイナリ内でも同一 pid で並列実行されるため）。
    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    /// P1 review 是正の再現ケース: 削除された公開 API との後方互換ラッパー
    /// `bind_loopback` が、新実装（`GuardedBindAddrs`）と同じ fail-closed 挙動
    /// （loopback は許可・非 loopback は拒否）を維持していること。
    #[test]
    #[allow(deprecated)]
    fn bind_loopback_compat_wrapper_accepts_loopback_and_rejects_non_loopback() {
        let listener = bind_loopback("127.0.0.1:0").expect("loopback bind must succeed");
        drop(listener);

        let err = bind_loopback("0.0.0.0:0").expect_err("non-loopback bind must be rejected");
        assert!(
            err.contains("refusing to bind non-loopback address"),
            "unexpected error message: {err}"
        );
    }

    /// レビュー指摘の再現ケース: 同時接続数の上限を超える接続は、ハンドシェイクへ
    /// 進ませずに `53300` の ErrorResponse を受け取った後クローズされること
    /// （Slowloris 対策・WIRE-6）。
    #[test]
    fn accept_loop_rejects_connection_over_capacity_with_53300() {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-server-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
        // （Issue #172）。
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let _dir_cleanup = DirCleanup(dir.clone());
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = Arc::new(UserStore::load_from_file(&path).expect("valid empty store"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let limiter = ConnectionLimiter::new(1);

        std::thread::spawn(move || {
            accept_loop_with_limiter(listener, store, limiter, Duration::from_secs(5));
        });

        // 1 本目: 上限(1)に達する接続。ハンドシェイクを進めず接続だけ保持する。
        let _held = TcpStream::connect(addr).expect("connect first");
        std::thread::sleep(Duration::from_millis(100));

        // 2 本目: 上限超過のため `'E'` / `53300` を受け取った後クローズされること。
        let mut second = TcpStream::connect(addr).expect("connect second");
        let mut header = [0u8; 1];
        second.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "expected ErrorResponse");
        let mut len_buf = [0u8; 4];
        second.read_exact(&mut len_buf).expect("read length");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        second.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains(limits::SQLSTATE_TOO_MANY_CONNECTIONS),
            "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
        );
        let mut extra = [0u8; 1];
        let n = second.read(&mut extra).unwrap_or(0);
        assert_eq!(
            n, 0,
            "connection over the concurrency limit must be closed after the rejection"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// レビュー指摘の再現ケース: StartupMessage を送らない接続は読み取りタイムアウト
    /// で応答なしに切断され、接続枠が解放されて後続接続を受け付けられること（WIRE-5）。
    #[test]
    fn accept_loop_releases_permit_after_read_timeout() {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-server-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
        // （Issue #172）。
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let _dir_cleanup = DirCleanup(dir.clone());
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = Arc::new(UserStore::load_from_file(&path).expect("valid empty store"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let short_timeout = Duration::from_millis(150);
        let client_probe_timeout = Duration::from_millis(50);
        assert!(
            client_probe_timeout < short_timeout,
            "probe window must be shorter than the server timeout to distinguish before/after"
        );
        let limiter = ConnectionLimiter::new(1);

        std::thread::spawn(move || {
            accept_loop_with_limiter(listener, store, limiter, short_timeout);
        });

        // 1 本目: 何も送らず保持する（クライアント側では明示的に close しない。
        // サーバー側の read タイムアウトだけで枠が解放されることを確認するため）。
        let stalled = TcpStream::connect(addr).expect("connect stalled");

        // タイムアウト前: 枠(1)を stalled が占有しているため、2 本目は
        // `53300` を受けた後クローズされること。
        std::thread::sleep(Duration::from_millis(50));
        let mut before = TcpStream::connect(addr).expect("connect before timeout");
        let mut header = [0u8; 1];
        before.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "slot must still be held before timeout");

        // サーバー側の read タイムアウトが経過するのを待つ（stalled はクライアント側で
        // close していないため、枠が解放されるのはサーバーのタイムアウトのみが理由）。
        std::thread::sleep(short_timeout * 3);

        // タイムアウト後: 枠が解放されているため、3 本目は即座には閉じられない
        // （読み取りが `client_probe_timeout` 以内に WouldBlock することで、
        // 「即座に EOF」ではなく「接続を受理して待機している」ことを確認する）。
        let mut after = TcpStream::connect(addr).expect("connect after timeout");
        after
            .set_read_timeout(Some(client_probe_timeout))
            .expect("set client read timeout");
        let mut buf = [0u8; 1];
        let err = after.read(&mut buf).expect_err("no data sent yet");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "connection after the server-side read timeout must not be closed immediately"
        );

        drop(stalled);
        let _ = std::fs::remove_file(&path);
    }

    /// P1 review 再指摘（codex-review / Cursor Bugbot: 別名ラッパーでは
    /// 後方互換にならない）の再現ケース: 旧名・旧 5 引数シグネチャの
    /// [`accept_loop`]（deprecated）を経由しても、新実装（[`accept_loop_with_limiter`]）
    /// と同じ fail-closed 挙動（上限超過接続への `53300` 応答）が得られること。
    /// `post_auth_idle_timeout` 引数は無視される契約のため、呼び出し時の値
    /// そのものは検証しない。
    #[test]
    #[allow(deprecated)]
    fn accept_loop_compat_wrapper_enforces_connection_limit() {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-server-test-legacy-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
        // （Issue #172）。
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let _dir_cleanup = DirCleanup(dir.clone());
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = Arc::new(UserStore::load_from_file(&path).expect("valid empty store"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        std::thread::spawn(move || {
            // 旧名・旧シグネチャをそのまま呼ぶ（互換性の実体はここで検証する）。
            accept_loop(
                listener,
                store,
                1,
                Duration::from_secs(5),
                POST_AUTH_IDLE_TIMEOUT,
            );
        });

        let _held = TcpStream::connect(addr).expect("connect first");
        std::thread::sleep(Duration::from_millis(100));

        let mut second = TcpStream::connect(addr).expect("connect second");
        let mut header = [0u8; 1];
        second.read_exact(&mut header).expect("read message type");
        assert_eq!(header[0], b'E', "expected ErrorResponse via legacy wrapper");
        let mut len_buf = [0u8; 4];
        second.read_exact(&mut len_buf).expect("read length");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        second.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains(limits::SQLSTATE_TOO_MANY_CONNECTIONS),
            "ErrorResponse must carry SQLSTATE 53300, got: {body_str:?}"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// 旧定数 `MAX_CONCURRENT_CONNECTIONS` / `CONNECTION_IO_TIMEOUT` が
    /// [`crate::limits`] の現行契約値と同値のまま参照できること（互換層が
    /// 値のドリフトを起こしていないことの回帰確認）。
    #[test]
    #[allow(deprecated)]
    fn legacy_constants_stay_in_sync_with_limits_module() {
        assert_eq!(MAX_CONCURRENT_CONNECTIONS, limits::MAX_CONNECTIONS);
        assert_eq!(CONNECTION_IO_TIMEOUT, limits::READ_TIMEOUT);
    }
}
