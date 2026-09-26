//! NoSQL 表層（HTTP/1.1 最小サブセット）の TLS 終端（Issue #968・親 #941・
//! TASK-228。対象ビヘイビア WIRE-9・HTTP-9・HTTP-10）。
//!
//! `crate::http::listener::accept_loop_with_handler` が TLS 構成を持つ場合
//! （`main.rs::run_server` の nosql 分岐が `--tls-cert`／`--tls-key` を渡した
//! とき）に、接続 1 本ごとに [`serve_connection`] を呼ぶ。HTTP には pg wire の
//! `SSLRequest` のような明示ネゴシエーションが無いため、接続受理直後の
//! 先頭 1 バイトを `peek` して TLS レコード（`0x16`）か平文 HTTP かを判定する
//! （[`classify_first_byte`]）。TLS と判定した接続は
//! [`crate::tls::server_handshake::perform_server_handshake`] でハンドシェイク
//! してから [`crate::tls::stream::TlsStream`] へ包み、それ以外は平文のまま、
//! いずれも同じ [`crate::http::conn::handle_connection_with`]（`S:
//! crate::wire_stream::WireStream` に一般化済み）へ委譲する。要求の解析・
//! 応答の組み立てはここでは一切行わない（本モジュールの責務はトランスポート
//! 層の終端のみ）。
//!
//! fail-closed の設計判断（設計記録: `docs/design/tls-wire-connection.md`
//! 「HTTPS 表層（#968）」節。H1〜H7 として番号を振っている）:
//! - H1: 先頭バイト判定は `0x16`（TLS ハンドシェイクレコード）のみを TLS と
//!   みなす。HTTP 要求行の先頭にこのバイトは現れないため曖昧さは無い。
//! - H2: `--tls-mode require` は平文と判定した接続を要求を一切解釈せず
//!   即座に閉じる。`allow` は平文ハンドラへそのまま進む。
//! - H3: ハンドシェイクの絶対期限は `perform_server_handshake` が使う
//!   `HANDSHAKE_READ_TIMEOUT`（SQL wire の TLS 経路と同じ定数。Issue #966）
//!   に委ねる。前後で接続のタイムアウト設定を退避・復元する
//!   （`crate::handshake::handle_tls_upgrade` と同型）。
//! - H4: TLS 構成時の同時接続数上限超過は `--tls-mode` で分岐する
//!   （codex-review 指摘・Issue #968 是正。旧実装は `tls.is_some()` のみで
//!   無応答クローズしており、`allow` 下の平文クライアントへも既存の
//!   503／`53300` 応答が返らなくなる回帰があった）。`require` は平文と
//!   TLS レコードのどちらであっても要求を解釈せず即座に閉じる（TLS
//!   ハンドシェイクをしていないクライアントに平文 503 を送ると `require`
//!   下での平文送出になり `--tls-mode require` の意図に反するため）。
//!   `allow` は [`reject_or_close_over_limit`] が `RejectWorkerLimiter` の
//!   枠の中で先頭バイトを期限付きで `peek` し、平文なら既存の 503／
//!   `53300` 応答（[`conn::reject_too_many_connections`]）を返す。TLS
//!   レコードと判定した場合も（codex-review 再指摘・同 Issue 是正）
//!   ハンドシェイクを完了したうえで同じ応答を TLS 上で返す（拒否応答が
//!   `write_all` 直後の `shutdown(Both)` で未読データを残したまま閉じて
//!   TCP RST になり応答を読めなくなる回帰は、[`conn::
//!   reject_too_many_connections_on`] 側の有界 lingering close で対処
//!   済み）。呼び出し元（`listener::accept_loop_with_handler`）が `tls`
//!   の有無と `--tls-mode` で分岐する。
//! - H5: `--tls-scram-channel-binding enable` × nosql は起動を拒否せず
//!   no-op として受理する（`main.rs` 側の判断。NoSQL 表層は SASL 往復を
//!   持たないため実際には提示されない）。本モジュールに直接の関与は無い。
//! - H6: `close_notify` を経ない TLS 下層 EOF は `TlsStream` が
//!   `UnexpectedEof`／`InvalidData` を返すため、平文なら「宣言長より短い
//!   本文」として `08P01` 応答になる経路が TLS では無応答クローズになる
//!   （失敗した TLS ストリームへは応答を書けないため許容する）。
//! - H7: 終端は `graceful_close`（TLS では `close_notify` 送出。平文では
//!   no-op）してから `shutdown_both` する。

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use crate::http::conn::{self, RequestHandler};
use crate::http::deadline_stream::DeadlineStream;
use crate::tls::server_handshake::TlsServerConfig;
use crate::tls::stream::TlsStream;
use crate::tls_opt::TlsMode;

/// 接続受理直後の先頭 1 バイトから、TLS レコードか平文 HTTP かを判定する
/// （H1）。純関数として切り出し、判定表そのものを単体テストできるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirstByte {
    /// TLS レコード層のハンドシェイクレコード（`ContentType::handshake` =
    /// `0x16`）。SSLv2 互換 `ClientHello`（先頭バイトの最上位ビットが立つ
    /// 0x80 系）は本サーバーが実装しない旧方式のため対象外で `Plain` へ
    /// 落ちる。`Plain` 判定後の扱いは `--tls-mode` 次第（H2）: `allow` なら
    /// 通常の HTTP パーサへ進み最終的に `08P01` で拒否されるが、`require`
    /// なら要求を一切解釈せず応答なしで切断する。
    Tls,
    /// 平文 HTTP の要求行（`POST` 等）の先頭バイトを含む、`0x16` 以外
    /// すべて。
    Plain,
}

pub(crate) fn classify_first_byte(byte: u8) -> FirstByte {
    const TLS_HANDSHAKE_CONTENT_TYPE: u8 = 0x16;
    if byte == TLS_HANDSHAKE_CONTENT_TYPE {
        FirstByte::Tls
    } else {
        FirstByte::Plain
    }
}

/// TLS 構成（証明書・鍵と `--tls-mode`）を伴う接続 1 本の処理（H1〜H7）。
/// `crate::http::listener::accept_loop_with_handler` の接続スレッドから
/// 呼ばれる。`read_timeout` は呼び出し元が受理直後に一度だけ適用した値
/// （[`crate::limits::apply_read_timeout`]）と同じものを渡す契約
/// （`conn::handle_connection_with` の `request_read_deadline` と同じ意味）。
///
/// `peek` に使うバッファは固定長 1 バイト（`untrusted` な長さフィールドに
/// 依存しない）。`peek` が `Ok(0)`（EOF）・`Err`（多くは `read_timeout` 超過の
/// `WouldBlock`／`TimedOut`）のいずれでも、応答を書かずに閉じる（HTTP-11 と
/// 同じ「無応答クローズ」方針）。
pub(crate) fn serve_connection<H: RequestHandler>(
    stream: TcpStream,
    handler: &H,
    read_timeout: Duration,
    tls: &(Arc<TlsServerConfig>, TlsMode),
) {
    let (config, mode) = tls;

    let mut probe = [0u8; 1];
    let first_byte = match stream.peek(&mut probe) {
        Ok(0) | Err(_) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        Ok(_) => match probe.first() {
            Some(b) => classify_first_byte(*b),
            None => {
                // `peek` が `Ok(1)` 以上を返した以上 `probe` は必ず 1 要素
                // 埋まっているため構造的に到達しない。fail-closed な安全弁
                // として応答を書かずに閉じる。
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
        },
    };

    match first_byte {
        FirstByte::Tls => {
            serve_tls_connection(stream, handler, read_timeout, Arc::clone(config));
        }
        // `TlsMode` は `#[non_exhaustive]`（下流クレート向け）だが、本クレート
        // 内では通常どおり網羅性検査が効く。将来 variant を追加する場合は
        // ここが確実にコンパイルエラーになり、拒否側／受理側のどちらへ倒すかを
        // 明示的に判断させる（黙って `Allow` 相当の受理側へフォールバック
        // しない。fail-closed）。
        FirstByte::Plain => match mode {
            TlsMode::Require => {
                // H2/H4 と同じ理由: `require` の下では平文接続へ一切応答
                // せず閉じる（要求を解釈しない。ピアアドレス・受信バイトは
                // ログに出さない）。
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            TlsMode::Allow => {
                conn::handle_connection_with(stream, handler, read_timeout);
            }
        },
    }
}

/// [`crate::http::listener::accept_loop_with_handler`] の同時接続数上限超過
/// 経路（`--tls-mode allow` の場合）から呼ばれる、拒否応答ワーカースレッド内
/// 専用の分岐（Issue #968 codex-review P1 是正）。
///
/// `tls.is_some()` だけで無応答クローズすると、`--tls-mode allow` で本来
/// 受理されるはずの平文 HTTP クライアントに対しても、TLS 未構成時と同じ
/// 既存の 503／`wire_code` 53300 応答（[`conn::reject_too_many_connections`]）
/// が返らなくなり、AGENTS.md の「公開 API・エラー契約の互換性（P1）」に反する
/// 退行になる。`allow` は平文接続を受理するモードのため、上限超過時も
/// 平文なら既存の拒否応答を維持する（`require` は `listener::
/// accept_loop_with_handler` 側で本関数を経由せず常に無応答クローズする。
/// TLS のみ受理するモードで平文へ応答を返す理由が無いため）。
///
/// 先頭バイトが TLS レコードの場合も、[`crate::tls::server_handshake::
/// perform_server_handshake`] でハンドシェイクを完了したうえで同じ 503／
/// `53300` 応答を TLS 上で返す（codex-review 再指摘・Issue #968 是正: 旧
/// 実装はハンドシェイクをせず無応答クローズしており、`allow` の下でも
/// HTTPS クライアントだけがこのエラー契約から取り残されていた）。
/// `RejectWorkerLimiter` は本関数の呼び出し 1 回＝1 スレッドで既に有界化
/// 済みのため、ハンドシェイク自体の絶対期限（`HANDSHAKE_READ_TIMEOUT`。
/// `serve_tls_connection` と同じ）がそのままこの拒否ワーカーの専有時間の
/// 上限になる。
///
/// 先頭 1 バイトの `peek` には [`crate::limits::REJECT_TLS_PROBE_TIMEOUT`]
/// を読み取りタイムアウトとして適用する（`RejectWorkerLimiter` の枠を
/// 無期限に占有しない。`peek` が `Ok(0)`／`Err`（タイムアウト超過含む）の
/// 場合は TLS ハンドシェイクをしていないクライアントへ応答を書く根拠が
/// 無いため、`serve_connection` と同じく応答を書かずに閉じる）。
pub(crate) fn reject_or_close_over_limit(stream: TcpStream, config: Arc<TlsServerConfig>) {
    if stream
        .set_read_timeout(Some(crate::limits::REJECT_TLS_PROBE_TIMEOUT))
        .is_err()
    {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }

    let mut probe = [0u8; 1];
    match stream.peek(&mut probe) {
        Ok(0) | Err(_) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        Ok(_) => match probe.first() {
            // `peek` が `Ok(1)` 以上を返した以上 `probe` は必ず 1 要素
            // 埋まっているため構造的に到達しないが、untrusted なソケット
            // 入力経路では添字アクセスを使わない（`serve_connection` と
            // 同じ方針。coding-rust.md）。
            None => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Some(byte) => match classify_first_byte(*byte) {
                FirstByte::Plain => conn::reject_too_many_connections(stream),
                FirstByte::Tls => reject_over_limit_after_tls_handshake(stream, config),
            },
        },
    }
}

/// [`reject_or_close_over_limit`] の TLS レコード判定後の分岐。ハンドシェイク
/// を完了できた場合のみ [`TlsStream`] 上へ既存の 503／`53300` 応答
/// （[`conn::reject_too_many_connections_on`]）を書く。ハンドシェイク失敗時は
/// 応答を書かない（alert の送出・切断は driver 側が既に行っている契約。
/// `serve_tls_connection` と同じ）。
///
/// `serve_tls_connection` と同じく `DeadlineStream` で包んでから
/// `TlsStream` へ渡す（H8）。`TlsStream::fill_from_inner` は 1 レコード分
/// 揃うまで下位ソケットの `read` を複数回呼び直すため、`DeadlineStream` を
/// 挟まないと `drain_and_close`（[`conn::reject_too_many_connections_on`]
/// が内部で呼ぶ）が `set_read_timeout` を都度呼び直しても各 `read` 呼び出し
/// 単位のタイムアウトにしかならず、1 バイトずつ間隔を空けて送り続ける
/// クライアントに対して `RejectWorkerLimiter` の 1 枠が
/// `LINGER_DRAIN_TIMEOUT` を大幅に超えて専有され続けうる（トリクル攻撃で
/// 拒否ワーカーの有界性が崩れる。TLS-7 が検証する `DeadlineStream` の
/// 契約と同じ理由）。
fn reject_over_limit_after_tls_handshake(mut stream: TcpStream, config: Arc<TlsServerConfig>) {
    let session = match crate::tls::server_handshake::perform_server_handshake(&mut stream, config)
    {
        Ok(session) => session,
        Err(_e) => return,
    };
    let mut tls_stream = TlsStream::new(DeadlineStream::new(stream), session);
    conn::reject_too_many_connections_on(&mut tls_stream);
}

/// TLS ハンドシェイクを実行し、成功したら [`TlsStream`] 上で
/// [`conn::handle_connection_with`] を走らせる（H3・H7）。
fn serve_tls_connection<H: RequestHandler>(
    mut stream: TcpStream,
    handler: &H,
    read_timeout: Duration,
    config: Arc<TlsServerConfig>,
) {
    // `perform_server_handshake` の driver（`DeadlineReader`/`DeadlineWriter`）は
    // ソケットの読み書きタイムアウトを都度上書きし、成功後も
    // `HANDSHAKE_READ_TIMEOUT` 定数のまま残す（`crate::handshake::
    // handle_tls_upgrade` と同じ既存設計）。呼び出し元が接続受理直後に
    // 設定した値（`read_timeout`）へ明示的に戻す。
    let saved_read_timeout = match stream.read_timeout() {
        Ok(v) => v,
        Err(_) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    };
    let saved_write_timeout = match stream.write_timeout() {
        Ok(v) => v,
        Err(_) => {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    };

    let session = match crate::tls::server_handshake::perform_server_handshake(&mut stream, config)
    {
        Ok(session) => session,
        Err(_e) => {
            // alert の送出・切断は driver 側が既に行っている
            // （`perform_server_handshake` の doc・`handle_tls_upgrade` と
            // 同じ契約）。HTTP 応答は送らない（TLS ハンドシェイク失敗は
            // ERR-1/2/4 の `wire_code` 写像の対象外）。
            return;
        }
    };

    if stream.set_read_timeout(saved_read_timeout).is_err()
        || stream.set_write_timeout(saved_write_timeout).is_err()
    {
        // タイムアウトを復元できない接続をそのまま処理へ進めると、
        // Slowloris 対策の絶対期限が想定外の値になりうるため fail-closed に
        // 閉じる。
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }

    // `DeadlineStream` で包んでから `TlsStream` に渡す（H8）。TlsStream の
    // 内部レコード読み取りループ（`fill_from_inner`）が `inner.read` を
    // 複数回呼んでも、各呼び出しの直前に絶対期限までの残り時間を下位
    // ソケットへ再設定するため、`conn::handle_connection_with` 側が 1 回の
    // `read` 呼び出し前にしか `set_read_timeout` を呼ばなくても Slowloris
    // 対策の絶対期限が正しく効く（`crate::http::deadline_stream` モジュール
    // doc 参照）。
    let tls_stream = TlsStream::new(DeadlineStream::new(stream), session);
    conn::handle_connection_with(tls_stream, handler, read_timeout);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_first_byte_recognizes_tls_handshake_record() {
        assert_eq!(classify_first_byte(0x16), FirstByte::Tls);
    }

    #[test]
    fn classify_first_byte_treats_http_methods_as_plain() {
        assert_eq!(classify_first_byte(b'P'), FirstByte::Plain);
        assert_eq!(classify_first_byte(b'G'), FirstByte::Plain);
    }

    #[test]
    fn classify_first_byte_treats_other_bytes_as_plain() {
        // SSLv2 互換 ClientHello（最上位ビットが立つ）・NUL・その他任意の
        // バイト値はいずれも「本サーバーが解釈する TLS レコードではない」の
        // 意味で `Plain` へ畳み込む（本サーバーは TLS 1.3 レコード層のみ
        // 実装し、SSLv2 互換 ClientHello を解釈しない）。`Plain` 判定後の
        // 扱いは `--tls-mode` 次第（`serve_connection`・H2 参照）。
        assert_eq!(classify_first_byte(0x00), FirstByte::Plain);
        assert_eq!(classify_first_byte(0x80), FirstByte::Plain);
        assert_eq!(classify_first_byte(0xFF), FirstByte::Plain);
    }
}
