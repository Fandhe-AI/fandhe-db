//! NoSQL 表層（`--surface nosql`）の accept ループ（Issue #735・#743・
//! TASK-171／HTTP-1・HTTP-9）。
//!
//! `main.rs::run_server` が `--surface nosql` を選んだ場合に、SQL wire の
//! [`crate::server::accept_loop_with_engine`] の代わりに呼ばれる。bind 自体は
//! 両表層で [`crate::bind_guard::GuardedBindAddrs`] を共有するため（HTTP-9
//! の bind ガード適用は `main.rs` 側のディスパッチ構造で担保する）、本モジュール
//! の責務は「1 本だけ listen される」ことの受け皿にとどめる。
//!
//! [`accept_loop_with_router`]（production 入口。Issue #752）は
//! [`crate::server::accept_loop_inner`] と同構造（受理 → `read_timeout` 適用
//! → ハンドラへ委譲／拒否 → `RejectWorkerLimiter` で有界化した使い捨て
//! スレッドへ委譲）で、[`crate::limits::ConnectionLimiter`]（WIRE-6）・
//! [`crate::limits::READ_TIMEOUT`]（WIRE-5）を SQL wire と**共有**する
//! （`main.rs::run_server` が `match surface` の前に 1 回だけ構築した同一
//! インスタンスを渡す。プロセス内で 1 表層しか起動しないため「共有」は
//! 「同じ構築箇所・同じ定数・同じ型」で満たされる）。要求の読み取り・
//! パース・応答生成・panic の非伝播は接続ハンドラ本体
//! （[`crate::http::conn::handle_connection_with`]。Issue #747）が担い、本モジュール
//! はハンドラの選び方（`accept_loop_with_router` は production 用の
//! [`crate::http::router::Router`] を使う。`accept_loop_with_limiter` は
//! `PlaceholderRouter` 固定の後方互換 API）と、受理・タイムアウト適用・
//! 拒否・スレッド分岐にとどめる。テスト（`conn.rs`・#749 の層 A 網羅
//! テスト）は [`accept_loop_with_handler`] へ任意の [`crate::http::conn::
//! RequestHandler`] 実装を注入できる。
//!
//! 同時接続数上限超過時の 503 応答は
//! [`crate::http::conn::reject_too_many_connections`] が担う。TLS 構成時は
//! `--tls-mode` で分岐し、`require` は応答を書かずに閉じるが、`allow` は
//! 先頭バイトを期限付きで判定して平文なら既存の 503 応答を維持する
//! （[`crate::http::tls_transport::reject_or_close_over_limit`]。Issue
//! #968・H4・codex-review 是正）。
//!
//! [`accept_loop_with_router_tls`]（Issue #968）は `main.rs::run_server` が
//! nosql 選択時に `--tls-cert`／`--tls-key` を構成した場合の入口。接続 1 本
//! ごとの TLS／平文判定・ハンドシェイクは
//! [`crate::http::tls_transport::serve_connection`] へ委譲する。
//!
//! untrusted なバイト列は [`crate::http::conn`] 側でのみ扱う（本モジュールは
//! 受理・タイムアウト適用・スレッド分岐のみで、ストリームの中身を読み書き
//! しない）。

use std::net::{Shutdown, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use crate::http::conn::{self, RequestHandler};
use crate::http::router::Router;
use crate::http::tls_transport;
use crate::limits::{self, ConnectionLimiter, RejectWorkerLimiter};
use crate::tls::server_handshake::TlsServerConfig;
use crate::tls_opt::TlsMode;

/// 接続を受理した直後に閉じるだけの accept ループ（stub）。
///
/// Issue #743 で [`accept_loop_with_limiter`] が正式な接続処理経路になった
/// ため、`main.rs::run_server` 本体はもう本関数を呼ばない。すでに公開 API
/// として利用側に届いている可能性があるため、AGENTS.md の「公開 API・
/// エラー契約の互換性（P1）」に従い削除せず残す（`server::bind_loopback`
/// と同じ後方互換方針）。
#[deprecated(since = "0.1.0", note = "use accept_loop_with_limiter instead")]
pub fn accept_loop_stub(listener: TcpListener) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // 要求を読まず・応答も書かない（stub の契約）。`shutdown` の
                // 結果は無視してよい（すでに切断済み等のエラーは無害）。
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
            }
        }
    }
}

/// [`crate::http::conn::PlaceholderRouter`] を使う [`accept_loop_with_handler`]
/// の薄いラッパー。`main.rs::run_server` はもう本関数を呼ばない
/// （production 入口は [`accept_loop_with_router`]。Issue #752）が、
/// `PlaceholderRouter` を注入したい既存呼び出し元向けに後方互換 API として
/// 残置する（AGENTS.md P1「公開 API・エラー契約の互換性」）。
///
/// `limiter` は呼び出し元が `match surface` の前に 1 回だけ構築したインスタンス
/// を受け取る（SQL wire 側と同じ構築箇所・同じ定数・同じ型を共有する構造。
/// 本ループが独自にリミッターを作ることはない）。
pub fn accept_loop_with_limiter(
    listener: TcpListener,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) {
    accept_loop_with_handler(
        listener,
        limiter,
        read_timeout,
        Arc::new(conn::PlaceholderRouter),
        None,
    );
}

/// NoSQL 表層の accept ループ本体（production 入口。Issue #752）。
/// [`crate::http::router::Router`] を使う [`accept_loop_with_handler`] の
/// 薄いラッパー。`main.rs::run_server` が nosql 選択時に構築する `router` を
/// そのまま渡す。
///
/// `limiter` は呼び出し元（`main.rs::run_server`）が `match surface` の前に
/// 1 回だけ構築したインスタンスを受け取る（SQL wire 側と同じ構築箇所・同じ
/// 定数・同じ型を共有する構造。本ループが独自にリミッターを作ることはない）。
pub fn accept_loop_with_router(
    listener: TcpListener,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
    router: Router,
) {
    accept_loop_with_handler(listener, limiter, read_timeout, Arc::new(router), None);
}

/// [`accept_loop_with_router`] の TLS opt-in 版（Issue #968・親 #941・
/// TASK-228）。`main.rs::run_server` が nosql 選択時に `--tls-cert`／
/// `--tls-key` を構成した場合の唯一の呼び出し元。接続 1 本ごとの TLS／平文
/// 判定・ハンドシェイクは [`crate::http::tls_transport::serve_connection`] が
/// 担う（詳細・fail-closed の設計判断は同モジュールの doc 参照）。
///
/// `mode` が [`TlsMode::Require`] の下では、平文と判定した接続へ一切応答を
/// 書かずに閉じる（H2）。TLS 構成を伴わない既存呼び出し元
/// （[`accept_loop_with_router`]・[`accept_loop_with_limiter`]）は本関数を
/// 経由せず、`tls: None` で [`accept_loop_with_handler`] を直接呼ぶため
/// ビット単位で従来と同一のまま。
pub fn accept_loop_with_router_tls(
    listener: TcpListener,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
    router: Router,
    tls_config: Arc<TlsServerConfig>,
    mode: TlsMode,
) {
    accept_loop_with_handler(
        listener,
        limiter,
        read_timeout,
        Arc::new(router),
        Some((tls_config, mode)),
    );
}

/// [`accept_loop_with_limiter`] の本体。SQL wire の
/// [`crate::server::accept_loop_inner`] と同じ資源保護契約
/// （WIRE-5, WIRE-6。契約値は [`crate::limits`] に集約）を適用したうえで、
/// 接続 1 本ごとに `handler`（[`crate::http::conn::handle_connection_with`]
/// への注入 seam）を使う:
///
/// - `limiter` の枠を確保できない接続は `handler` へ進ませず、`limiter` の
///   枠を消費しない短命な使い捨てスレッドへ [`crate::http::conn::
///   reject_too_many_connections`] を委譲し、HTTP 503／`wire_code` 53300 の
///   JSON 応答を返してから即座にクローズする。この拒否スレッド自体も
///   [`RejectWorkerLimiter`]（`MAX_REJECT_WORKERS`）で別枠に有界化し、上限
///   到達後は応答を書かずに即座にクローズする（`accept_loop_inner` と同じ
///   review 是正: 拒否経路の無制限 `thread::spawn` による DoS 対策）
/// - 受理した接続には読み取り・書き込み双方に `read_timeout` を一度だけ
///   設定してから [`crate::http::conn::handle_connection_with`] へ `handler`
///   （`Arc` で各接続スレッドへ複製）を渡す
///
/// `H: 'static` は `thread::Builder::spawn`（`join` しない）の要件。テスト
/// （`conn.rs`・#749 の層 A 網羅テスト）は任意の [`RequestHandler`] 実装
/// （panic 注入を含む）を渡せる。
pub(crate) fn accept_loop_with_handler<H: RequestHandler + Send + Sync + 'static>(
    listener: TcpListener,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
    handler: Arc<H>,
    tls: Option<(Arc<TlsServerConfig>, TlsMode)>,
) {
    // 拒否応答ワーカースレッドの有界化専用リミッター（`limiter` とは別枠。
    // `crate::server::accept_loop_inner` と同じ review 是正方針）。
    let reject_limiter = RejectWorkerLimiter::new(limits::MAX_REJECT_WORKERS);

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wire-server: accept error: {e}");
                continue;
            }
        };

        let Some(permit) = limiter.try_acquire() else {
            // 上限超過: ハンドラへ進ませず、スレッドを生成せずにクローズする
            // （WIRE-6）。ピアアドレス等の識別情報はログに出さない。
            eprintln!(
                "wire-server: rejecting connection: too many connections (active={}, max={})",
                limiter.active(),
                limiter.max()
            );
            // H4（Issue #968・`docs/design/tls-wire-connection.md`
            // 「HTTPS 表層」節。codex-review 指摘・同 Issue で是正）:
            // `--tls-mode require` は平文と TLS レコードのどちらでも要求を
            // 解釈せず即座に閉じる（TLS ハンドシェイクをしていない接続へ
            // 平文 503 を送ると `require` の意図（平文を一切送出しない）に
            // 反するため）。`--tls-mode allow` は平文接続を受理するモード
            // のため、上限超過時も平文なら既存の 503／`53300` 応答
            // （[`conn::reject_too_many_connections`]）を維持する必要があり、
            // これを `tls.is_some()` だけで無応答クローズすると `allow` 下の
            // 平文クライアントに対するエラー契約の退行になる（AGENTS.md
            // 「公開 API・エラー契約の互換性（P1）」）。`allow` では
            // [`tls_transport::reject_or_close_over_limit`] が拒否ワーカー
            // （`RejectWorkerLimiter` で有界化済み）の中で先頭バイトを
            // 期限付きで判定し、平文なら既存の拒否応答をそのまま返す。
            // TLS レコードと判定した場合も（codex-review 再指摘・同 Issue
            // 是正）ハンドシェイクを完了したうえで同じ 503 応答を TLS 上で
            // 返す（`RejectWorkerLimiter` の 1 枠＝1 スレッドの中で完結する
            // ため、遅いクライアントでも `HANDSHAKE_READ_TIMEOUT` の絶対
            // 期限がそのままこのワーカーの専有時間の上限になる）。TLS
            // 未構成時は既存の 503／`53300` 経路とバイト単位で同一。
            // `TlsMode` は `#[non_exhaustive]`（下流クレート向け）だが、本
            // クレート内では通常どおり網羅性検査が効く。将来 variant を
            // 追加する場合はここが確実にコンパイルエラーになり、`allow`
            // 相当の受理側（平文へ応答を返す側）へ黙ってフォールバック
            // しない（`tls_transport::serve_connection` の `FirstByte`
            // 網羅と同じ fail-closed 方針）。
            match &tls {
                Some((_, TlsMode::Require)) => {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                Some((_, TlsMode::Allow)) | None => {}
            }
            // ここに到達する時点で `tls` は `None` か `Some((_, TlsMode::
            // Allow))` のいずれかのみ（`Require` は直前の分岐で `continue`
            // 済み）。TLS 構成があれば `TlsServerConfig` を拒否ワーカー
            // スレッドへ複製する（`reject_or_close_over_limit` が TLS
            // レコード判定時にハンドシェイクを完了して 503 を返すために
            // 必要。Issue #968 codex-review 再指摘の是正）。
            let tls_config_for_reject = tls.as_ref().map(|(config, _)| Arc::clone(config));
            match reject_limiter.try_acquire() {
                Some(reject_permit) => {
                    // `std::thread::spawn` はスレッド生成失敗時に panic し、
                    // accept ループ自体を停止させうるため、panic しない
                    // `Builder::spawn` を使い、失敗時はログのみで継続する。
                    if let Err(e) = std::thread::Builder::new().spawn(move || {
                        let _reject_permit = reject_permit;
                        if let Some(config) = tls_config_for_reject {
                            tls_transport::reject_or_close_over_limit(stream, config);
                        } else {
                            conn::reject_too_many_connections(stream);
                        }
                    }) {
                        eprintln!("wire-server: failed to spawn reject worker thread: {e}");
                    }
                }
                None => {
                    // 拒否ワーカーも枯渇: 新たにスレッドを生成せず、応答を
                    // 書かずに即座にクローズする（有界化を優先し fail-closed
                    // に倒す）。
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
            continue;
        };

        if let Err(e) = limits::apply_read_timeout(&stream, read_timeout) {
            eprintln!("wire-server: failed to configure connection timeouts: {e}");
            // `permit` はここでスコープを抜けて解放される。
            continue;
        }

        // `std::thread::spawn` はスレッド生成失敗時に panic し、accept
        // ループ自体を停止させうる（OS のスレッド数制限・メモリ不足は
        // 同時接続数上限（`MAX_CONNECTIONS`）を満たしていても発生しうる）。
        // 拒否ワーカー経路と同じく panic しない `Builder::spawn` を使い、
        // 失敗時は当該接続の `permit`／ストリームを解放して accept ループを
        // 継続する（fail-closed。プロセス全体を落とさない）。
        let handler_for_thread = Arc::clone(&handler);
        let tls_for_thread = tls.clone();
        if let Err(e) = std::thread::Builder::new().spawn(move || {
            // 接続処理中は `permit` を保持し続け、スレッド終了時（正常終了・
            // panic いずれも）に Drop で確実に枠を解放する。
            let _permit = permit;
            // `read_timeout` を要求全体（頭＋本文）の絶対読み取り期限としても
            // 渡す（`conn::handle_connection_with` の doc・Slowloris 対策
            // 参照）。接続受理直後に `apply_read_timeout` へ渡した値と同じ
            // 1 つの値を「受理直後のソケットタイムアウト」と「要求読み取り
            // 全体の期限」の双方に使う契約。
            match &tls_for_thread {
                Some(tls) => {
                    tls_transport::serve_connection(
                        stream,
                        handler_for_thread.as_ref(),
                        read_timeout,
                        tls,
                    );
                }
                None => {
                    conn::handle_connection_with(stream, handler_for_thread.as_ref(), read_timeout);
                }
            }
        }) {
            eprintln!("wire-server: failed to spawn connection handler thread: {e}");
            // クロージャへ move された `permit` はスレッド生成失敗時に
            // 即座に Drop され枠が解放される。ストリームは outgoing の
            // `spawn` 失敗で誰も所有しなくなるため、OS の接続クローズに
            // 任せる（追加の `shutdown` 呼び出しは不要）。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    /// `stream.read` が実際に EOF（`Ok(0)`）で終わったことを確認する。
    ///
    /// `read(...).unwrap_or(0)` は、クライアント側の read が `WouldBlock`／
    /// `TimedOut` で終わった場合も `Ok(0)`（EOF）と同一視してしまい、
    /// 「サーバーの `read_timeout` 超過後に接続が閉じる」という検証対象の
    /// 契約が破れていてもテストを通してしまう（codex-review 指摘）。
    /// ここでは `Ok(0)` のみを合格とし、それ以外（`WouldBlock`／`TimedOut`
    /// を含む）はテスト失敗として明示する。
    fn assert_eof(stream: &mut TcpStream) {
        let mut buf = [0u8; 8];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => panic!("expected EOF without any response bytes, got {n} bytes"),
            Err(e) => panic!("expected EOF (Ok(0)), got read error: {e:?}"),
        }
    }

    /// stub リスナーが接続を受理した直後に閉じ、要求を読まないこと・
    /// 応答を書かないこと・ループが次の接続を受理し続けること（1 回の
    /// 接続で終了しない）を確認する。
    #[test]
    #[allow(deprecated)]
    fn accept_loop_stub_closes_connections_without_reading_or_writing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback");
        let addr = listener.local_addr().expect("local addr");

        std::thread::spawn(move || {
            accept_loop_stub(listener);
        });

        for _ in 0..2 {
            let mut stream =
                TcpStream::connect(addr).expect("connect to stub listener should succeed");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut buf = [0u8; 8];
            let read_result = stream.read(&mut buf);
            match read_result {
                Ok(n) => assert_eq!(n, 0, "expected EOF (0 bytes), got {n} bytes"),
                Err(e) => {
                    let kind = e.kind();
                    assert!(
                        kind == std::io::ErrorKind::ConnectionReset
                            || kind == std::io::ErrorKind::BrokenPipe,
                        "unexpected read error: {e:?}"
                    );
                }
            }
        }
    }

    /// 上限超過接続が HTTP 503／`53300` を受けて `active()` が枠を消費
    /// しないこと（`server.rs` の同名テストの HTTP 版）。
    #[test]
    fn accept_loop_with_limiter_rejects_connection_over_capacity_with_503() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let limiter = ConnectionLimiter::new(1);
        let limiter_for_loop = limiter.clone();

        std::thread::spawn(move || {
            accept_loop_with_limiter(listener, limiter_for_loop, Duration::from_secs(5));
        });

        // 1 本目: 枠を保持し続ける（何も送らない）。
        let holder = TcpStream::connect(addr).expect("connect holder");

        // limiter に反映されるまで待つ。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while limiter.active() < 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for permit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // 2 本目: 拒否されるはず。
        let mut rejected = TcpStream::connect(addr).expect("connect rejected");
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut received = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match rejected.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&received);
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text:?}");
        assert!(text.contains("53300"), "got: {text:?}");

        assert_eq!(limiter.active(), 1, "reject path must not consume a permit");
        drop(holder);
    }

    /// 短縮タイムアウトで受理した接続が、タイムアウト後に応答なしで
    /// EOF になり、枠が解放されること。
    #[test]
    fn accept_loop_with_limiter_releases_permit_after_read_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let limiter = ConnectionLimiter::new(1);
        let limiter_for_loop = limiter.clone();
        let short_timeout = Duration::from_millis(150);

        std::thread::spawn(move || {
            accept_loop_with_limiter(listener, limiter_for_loop, short_timeout);
        });

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        assert_eof(&mut stream);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while limiter.active() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for permit release"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
