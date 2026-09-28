//! 認証後に届くフロントエンドメッセージの型バイト分類と、未対応の拡張クエリ
//! プロトコル系メッセージに対する fail-closed な拒否応答＋切断を担う。
//!
//! `handshake::post_auth_loop` から呼ばれる。フレーミング（長さ検証）は
//! `handshake` モジュール（TASK-68 の管轄）、接続数・タイムアウトは `server`
//! モジュール（TASK-69 の管轄）のままで、本モジュールはどちらにも触れない。
//!
//! 対応: TASK-71（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-8）で
//! 拡張クエリプロトコル全系列（Parse/Bind/Describe/Execute/Sync/Close/Flush）を
//! 一律拒否する契約を確立した。WIRE-11（Issue #933）で Parse（'P'）・
//! Describe（'D' 種別 S）を [`crate::extended_query`] が受理する経路へ切り替え、
//! 本モジュールの `ExtendedQuery` 分類は Bind/Execute/Sync/Close/Flush
//! （'B'/'E'/'S'/'C'/'H'）に縮小した——Parse/Describe は
//! [`FrontendMessageKind::Parse`]／[`FrontendMessageKind::Describe`] として
//! 別分類になり、`handshake::post_auth_loop` から本モジュールの
//! `reject_and_close` へは到達しない（`extended_query` モジュールドキュメント
//! 参照）。SQLSTATE `0A000` の応答契約は `docs/spec/04-behavior/error-format.md`
//! を参照（spec 本文は転記しない）。

use std::io;
use std::time::{Duration, Instant};

use crate::wire_stream::WireStream;

/// 型バイト 1 つから決まる分類。ネットワーク非依存の純関数（[`classify`]）に
/// 切り出し、全 256 値を単体テストで走査できるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontendMessageKind {
    /// 簡易クエリ（'Q'）。分類のみ担い、実処理は `handshake::post_auth_loop` の
    /// 既存分岐（変更なし）に委ねる。
    SimpleQuery,
    /// Terminate（'X'）。分類のみ担い、実処理は `handshake::post_auth_loop` の
    /// 既存分岐（変更なし）に委ねる。
    Terminate,
    /// Parse（'P'）。WIRE-11（Issue #933）で `crate::extended_query::handle_parse`
    /// が受理する経路へ切り替わったため、本モジュールの `reject_and_close` には
    /// 到達しない（`classify` の全域性を保つために分類のみ残す）。
    Parse,
    /// Describe（'D'）。種別バイト（'S'/'P'）は body 側にあるため本分類では
    /// 判別しない。`crate::extended_query::handle_describe` が本文を読んで
    /// 種別ごとに振り分ける（statement は受理、portal は WIRE-8 のまま
    /// `0A000` + 切断）。
    Describe,
    /// 拡張クエリプロトコル系（Bind/Execute/Sync/Close/Flush）。WIRE-8 の
    /// 主対象のまま残る（Parse/Describe は上記へ切り出し済み）。中に元の
    /// 型バイトを保持する（ログ用途のみ）。
    ExtendedQuery(u8),
    /// COPY・関数呼び出し系（FunctionCall/CopyData/CopyDone/CopyFail）。
    /// 拡張クエリと同様に未対応のため同じ応答契約（0A000 + 切断）に倒す。
    UnsupportedFeature(u8),
    /// 上記のいずれにも該当しない型バイト（認証後に来るべきでない 'p' を含む）。
    Unknown(u8),
}

/// 型バイト 1 つを [`FrontendMessageKind`] へ写像する。対象フレームの body・
/// 後続のパイプラインは一切読まない（untrusted 入力の parse 面を増やさないため、
/// 分類は型バイトのみで完結させる）。
pub(crate) fn classify(type_byte: u8) -> FrontendMessageKind {
    match type_byte {
        b'Q' => FrontendMessageKind::SimpleQuery,
        b'X' => FrontendMessageKind::Terminate,
        b'P' => FrontendMessageKind::Parse,
        b'D' => FrontendMessageKind::Describe,
        b'B' | b'E' | b'S' | b'C' | b'H' => FrontendMessageKind::ExtendedQuery(type_byte),
        b'F' | b'd' | b'c' | b'f' => FrontendMessageKind::UnsupportedFeature(type_byte),
        other => FrontendMessageKind::Unknown(other),
    }
}

/// lingering close の上限（時間）。ErrorResponse 送出後、クライアントが
/// パイプライン済みの後続メッセージを読み捨てる猶予。
pub(crate) const LINGER_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// lingering close の上限（バイト数）。悪意ある／誤動作クライアントが送り続けても
/// 認証済み接続スロットを長時間占有させない。
pub(crate) const LINGER_DRAIN_MAX_BYTES: usize = 64 * 1024;

/// 拒否応答の書き込みを担う関数への依存を注入で切るための trait。
/// `handshake::write_error_response_io` を実体として渡す。`sqlstate: &str` ではなく
/// `ErrorClass` を受け取ることで、送出経路が `crate::error_response::encode` の
/// severity/SQLSTATE 一元化・NUL 拒否を必ず経由する（codex-review P1 指摘対応・
/// PR #258）。
type WriteErrorResponseFn<S> = fn(&mut S, engine::error_format::ErrorClass, &str) -> io::Result<()>;

/// 未対応メッセージへの応答（SQLSTATE 0A000 + 分類ごとの固定英語メッセージ）と、
/// 有界な lingering close を行う（WIRE-8 の本体）。ReadyForQuery は送らない。
/// 呼び出し元（`handshake::post_auth_loop`）はこの関数が返った後にループを抜けて
/// `Ok(())` を返し、`stream` の drop によって接続を閉じる契約とする。
///
/// SQLSTATE は `ExtendedQuery` / `UnsupportedFeature` / `Unknown` のいずれも
/// `0A000` で統一する（ポインタ: TASK-71・WIRE-8、`docs/spec/04-behavior/error-format.md`）。
/// 一方でメッセージ文言は分類ごとに事実に即した表現へ分ける（[`response_message`]）。
pub(crate) fn reject_and_close<S: WireStream>(
    stream: &mut S,
    kind: FrontendMessageKind,
    write_error_response: WriteErrorResponseFn<S>,
) -> io::Result<()> {
    // 受信ペイロード・ユーザー名は出さず、分類名と型バイト（固定集合の1文字）のみ
    // ログに残す（P0: テナント情報・存在情報を漏らさない）。
    engine::log_stderr!(
        "wire-server: post-auth rejecting unsupported message ({})",
        describe_kind(kind)
    );

    write_error_response(
        stream,
        engine::error_format::ErrorClass::FeatureNotSupported,
        response_message(kind),
    )?;
    stream.flush()?;

    drain_and_close(stream, LINGER_DRAIN_TIMEOUT, LINGER_DRAIN_MAX_BYTES);
    Ok(())
}

/// 分類ごとの応答メッセージ文言。`ExtendedQuery` 以外（COPY・関数呼び出し系・
/// 未知の型バイト）に対して「拡張クエリプロトコル」と述べるのは事実と異なるため、
/// 分類名を分けて表現する。
fn response_message(kind: FrontendMessageKind) -> &'static str {
    match kind {
        FrontendMessageKind::ExtendedQuery(_) => {
            "extended query protocol is not supported on this connection"
        }
        FrontendMessageKind::UnsupportedFeature(_) => {
            "COPY and function call protocol messages are not supported on this connection"
        }
        // 認証後に来るべきでない 'p'（PasswordMessage）や、既知の型バイト集合に
        // 該当しない値をまとめて扱う（ポインタ: TASK-71・WIRE-8、
        // `docs/spec/04-behavior/error-format.md`）。文言のみ「未対応の型バイト」
        // であることを明示する。
        FrontendMessageKind::Unknown(_) => {
            "this frontend message type is not supported on this connection"
        }
        // SimpleQuery / Terminate / Parse / Describe は `handshake::post_auth_loop`
        // の既存分岐（Parse/Describe は `crate::extended_query` 経由。WIRE-11・
        // Issue #933）で処理され本関数には到達しない契約だが、`classify` の
        // 全域性を保つために網羅しておく。
        FrontendMessageKind::SimpleQuery
        | FrontendMessageKind::Terminate
        | FrontendMessageKind::Parse
        | FrontendMessageKind::Describe => {
            "this frontend message type is not supported on this connection"
        }
    }
}

fn describe_kind(kind: FrontendMessageKind) -> String {
    match kind {
        FrontendMessageKind::SimpleQuery => "SimpleQuery".to_string(),
        FrontendMessageKind::Terminate => "Terminate".to_string(),
        FrontendMessageKind::Parse => "Parse".to_string(),
        FrontendMessageKind::Describe => "Describe".to_string(),
        FrontendMessageKind::ExtendedQuery(b) => format!("ExtendedQuery({})", b as char),
        FrontendMessageKind::UnsupportedFeature(b) => format!("UnsupportedFeature({})", b as char),
        FrontendMessageKind::Unknown(b) => format!("Unknown(0x{b:02x})"),
    }
}

/// 応答送出後の有界 lingering close 本体。書き込み方向を先に閉じて
/// （FIN 送出）クライアントへ「これ以上送るな」を伝えたうえ、読み取りタイムアウトを
/// 設定してパイプライン済みの残データを固定長バッファへ読み捨てる。
///
/// 終了条件（いずれか）: EOF（`Ok(0)`）／読み取りタイムアウト／総経過時間が
/// `timeout` 到達／読み捨てバイト数が `max_bytes` 到達。`shutdown` /
/// `set_read_timeout` の失敗は無視してそのまま戻る（クローズ方向に倒す。
/// 失敗を理由に drain を続行しない）。
///
/// 受信データを解釈しないため未検証の長さフィールドを信用する経路がなく、
/// バッファは固定長のスタック配列（`Vec::with_capacity` を使わない）。
///
/// `pub(crate)`: SQL wire（[`reject_and_close`]。`LINGER_DRAIN_TIMEOUT`・
/// `LINGER_DRAIN_MAX_BYTES` を渡す）に加え、NoSQL 表層の接続ハンドラ
/// （[`crate::http::conn`]。Issue #747）が同じ有界読み捨て契約（PoC-15 の実装
/// ガイドライン）を共有するために `pub(crate)` へ昇格した（旧名
/// `reject_and_close_with`。private のため互換義務を負わない改名）。
pub(crate) fn drain_and_close<S: WireStream>(stream: &mut S, timeout: Duration, max_bytes: usize) {
    // 書き込み方向を閉じてクライアントへ FIN を送る。失敗しても drain は続行する
    // （読み取り自体は shutdown 非依存で機能するため）。
    let _ = stream.shutdown_write();

    let deadline = Instant::now() + timeout;
    let mut drained: usize = 0;
    let mut buf = [0u8; 4096];

    loop {
        if drained >= max_bytes {
            break;
        }
        let remaining = match deadline.checked_duration_since(Instant::now()) {
            Some(d) if d > Duration::from_millis(1) => d,
            _ => break,
        };
        if stream.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        let want = buf.len().min(max_bytes.saturating_sub(drained));
        match stream.read(&mut buf[..want.max(1)]) {
            Ok(0) => break,
            Ok(n) => {
                drained = drained.saturating_add(n);
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};

    #[test]
    fn response_message_differs_by_classification() {
        // ExtendedQuery / UnsupportedFeature / Unknown で文言を分け、
        // 「拡張クエリプロトコル」という事実と異なる文言を UnsupportedFeature・
        // Unknown に流用しないことを保証する（レビュー指摘の回帰防止）。
        let extended = response_message(FrontendMessageKind::ExtendedQuery(b'B'));
        let unsupported = response_message(FrontendMessageKind::UnsupportedFeature(b'F'));
        let unknown = response_message(FrontendMessageKind::Unknown(b'?'));

        assert!(extended.contains("extended query protocol"));
        assert!(!unsupported.contains("extended query protocol"));
        assert!(!unknown.contains("extended query protocol"));
        assert_ne!(extended, unsupported);
        assert_ne!(extended, unknown);
    }

    #[test]
    fn classify_maps_all_256_type_bytes_without_panicking() {
        for b in 0u16..=255 {
            let byte = b as u8;
            let kind = classify(byte);
            match byte {
                b'Q' => assert_eq!(kind, FrontendMessageKind::SimpleQuery),
                b'X' => assert_eq!(kind, FrontendMessageKind::Terminate),
                b'P' => assert_eq!(kind, FrontendMessageKind::Parse),
                b'D' => assert_eq!(kind, FrontendMessageKind::Describe),
                b'B' | b'E' | b'S' | b'C' | b'H' => {
                    assert_eq!(kind, FrontendMessageKind::ExtendedQuery(byte))
                }
                b'F' | b'd' | b'c' | b'f' => {
                    assert_eq!(kind, FrontendMessageKind::UnsupportedFeature(byte))
                }
                other => assert_eq!(kind, FrontendMessageKind::Unknown(other)),
            }
        }
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    fn fake_write_error_response(
        stream: &mut TcpStream,
        class: engine::error_format::ErrorClass,
        message: &str,
    ) -> io::Result<()> {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"ERROR");
        body.push(0);
        body.push(b'C');
        body.extend_from_slice(class.wire_code().as_bytes());
        body.push(0);
        body.push(b'M');
        body.extend_from_slice(message.as_bytes());
        body.push(0);
        body.push(0);
        let total_len = (4 + body.len()) as i32;
        let mut msg = Vec::new();
        msg.push(b'E');
        msg.extend_from_slice(&total_len.to_be_bytes());
        msg.extend_from_slice(&body);
        stream.write_all(&msg)
    }

    #[test]
    fn reject_and_close_writes_error_then_eof() {
        let (mut server, mut client) = loopback_pair();

        reject_and_close(
            &mut server,
            FrontendMessageKind::ExtendedQuery(b'B'),
            fake_write_error_response,
        )
        .expect("reject_and_close succeeds");

        let mut header = [0u8; 1];
        client.read_exact(&mut header).expect("read type");
        assert_eq!(header[0], b'E');
        let mut len_buf = [0u8; 4];
        client.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        client.read_exact(&mut body).expect("read body");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("0A000"), "got: {body_str:?}");

        client.shutdown(Shutdown::Write).ok();
        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        assert_eq!(n, 0, "connection must be closed after rejection");
    }

    #[test]
    fn drain_and_close_bounds_drain_by_timeout() {
        let (mut server, mut client) = loopback_pair();

        let start = Instant::now();
        drain_and_close(&mut server, Duration::from_millis(150), 64 * 1024);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "drain must return within a bounded time, took {elapsed:?}"
        );

        // クライアントは閉じずに送り続けるが、サーバーはタイムアウトで抜けている。
        let _ = client.write_all(b"still sending");
    }

    #[test]
    fn drain_and_close_bounds_drain_by_max_bytes() {
        let (mut server, mut client) = loopback_pair();

        let sender = std::thread::spawn(move || {
            let chunk = [0u8; 4096];
            // max_bytes を超える量を送り続ける（サーバー側が打ち切ることを期待）。
            for _ in 0..64 {
                if client.write_all(&chunk).is_err() {
                    break;
                }
            }
        });

        let start = Instant::now();
        drain_and_close(&mut server, Duration::from_secs(10), 8 * 1024);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "drain must stop at max_bytes well before the time budget, took {elapsed:?}"
        );

        let _ = sender.join();
    }
}
