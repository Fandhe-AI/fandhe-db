//! HTTP 接続 1 本ぶんの受理後処理（Issue #747・TASK-173／HTTP-12。関連
//! ビヘイビア HTTP-2・HTTP-3・HTTP-11。対象ポインタ: `docs/spec/05-tasks.md`
//! TASK-173・`docs/spec/04-behavior/http-transport.md`）。
//!
//! `http::listener::accept_loop_with_router`（production では
//! [`crate::http::router::Router`] を `Arc` で包んで渡す。`accept_loop_with_limiter`
//! は後方互換 API で [`PlaceholderRouter`] を使う）から呼ばれる、接続単位の
//! 2 経路:
//! - [`handle_connection_with`][]: 接続ハンドラ本体。要求行
//!   （[`crate::http::request`]）→ ヘッダ（[`crate::http::headers`]）→
//!   `Expect` ヘッダの拒否（[`reject_if_expect`]。下記「`Expect` の扱い」節）
//!   → 本文長・`Content-Type` の読み取り前検証（[`crate::http::body`]）→
//!   本文読み取り → `handler: &impl RequestHandler` へのルーティング
//!   （production は [`crate::http::router::Router`]。`/v1/session` のみ発行
//!   パイプラインへ委譲し、他パスは `08P01`。Issue #752）の順に 1 往復だけ
//!   処理し、応答を 1 回書き込んでからクローズする（keep-alive・パイプライン
//!   非対応。応答は常に `Connection: close`）。
//!   `handler` はテスト（本ファイル・#749 の層 A 網羅テスト）が任意の
//!   [`RequestHandler`] 実装（panic 注入を含む）を差し込むための注入 seam
//! - [`reject_too_many_connections`][]: 同時接続数の枠を確保できなかった
//!   接続へ HTTP 503 ＋ JSON 本文（`wire_code`＝`53300`）を返してからクローズ
//!   する拒否経路（Issue #743 で実装済み・本 Issue では無変更）
//!
//! ## `Expect` ヘッダの扱い（codex-review 指摘・PR #810）
//!
//! 本ハンドラは 1 要求につき応答を 1 回だけ書く非パイプライン設計であり、
//! `100 Continue` の暫定応答を送る経路を持たない。`Expect: 100-continue` を
//! 送るクライアントは、この暫定応答を受け取るまで本文の送信を待つ実装が
//! ある（[`crate::http::body`] のモジュール doc も「実際の... `Expect:
//! 100-continue`... 処理は接続ハンドラの責務」と明記している）。これを
//! 無視して本文読み取りへ進むと、サーバーは届かない本文を待ち、クライアント
//! は届かない暫定応答を待つ形で双方が待機し、最終的に無応答のまま
//! タイムアウト切断になる。[`reject_if_expect`] が本文読み取りより前
//! （[`body::plan_body`] の前）で `Expect` ヘッダの有無を検査し、1 件でも
//! 付いていれば（値を問わず）暫定応答の代わりに最終エラー応答
//! （`ErrorClass::FeatureNotSupported`）を返すことで、この待機を構造的に
//! 回避する。
//!
//! ## 不正フレームでも応答を失わない（PoC-15 実装ガイドライン）
//!
//! 要求行・ヘッダ・本文のいずれかが不正で早期拒否する場合も、応答バイト列を
//! 書き込んだ**後**に、SQL wire の [`crate::protocol_dispatch::reject_and_close`]
//! と同じ有界 lingering close（[`crate::protocol_dispatch::drain_and_close`]）で
//! 未読データを読み捨ててからクローズする。書き込み直後に `shutdown(Both)` で
//! 即座に閉じると、クライアントが送信済み・送信中のバイト列と応答の競合で
//! TCP RST を受け取り応答を読めなくなりうるため（PoC-15）。
//!
//! `LINGER_DRAIN_TIMEOUT`（1 秒）は SQL wire と共有するが、読み捨て上限
//! バイト数（`drain_budget`）は固定定数ではなく [`Outcome::Respond`] ごとに
//! 呼び出し元（[`build_outcome`]）が動的に決める（codex-review 指摘・PR
//! #810 再指摘。詳細は [`drain_budget_after_headers`] の doc を参照）。
//! SQL wire の `LINGER_DRAIN_MAX_BYTES`＝64 KiB を流用しない理由・固定上限を
//! 採らない理由も同 doc に記す。
//!
//! ## panic 非伝播と RECOVER-8（fail-fast）との関係
//!
//! [`handle_connection_with`] は要求の解析・ルーティングを
//! `std::panic::catch_unwind` で包み、書き込みは **catch_unwind の外**（`Result`
//! を見てから）でのみ行う。これにより「途中まで書いた通常応答に 500 を
//! 追記する」経路が構造的に存在しない。
//!
//! production バイナリは `main.rs::run_server` が起動時に
//! `engine::recovery::fail_fast::install`（TASK-99・RECOVER-8）を導入して
//! おり、その panic hook は unwind **前** に `std::process::abort()` する。
//! したがって production では本モジュールの `catch_unwind` へ実際には
//! 到達せずプロセスが終了する（RECOVER-8 の意図した契約であり本 Issue は
//! それを変更しない）。ここでの `catch_unwind` は (a) `wire-server` を lib
//! として使う経路・テストでの防御、(b) panic 時でも応答・クローズ・
//! `ConnectionPermit` 解放（呼び出し元 `listener` が Drop で解放）を
//! 接続単位で決定的に行うための多層防御であり、HTTP-12 の「他接続へ
//! panic が波及しない」という受け入れ条件の本体は、各パーサ群が不正
//! フレームで panic しない（fail-closed な `Result` 型）ことで満たす。
//! 「panic 注入時に他接続が継続する」ことを検証するテストは、
//! `fail_fast::install` を**呼ばずに**（Rust 既定の panic hook のまま）
//! in-process で行う（呼ぶとテストバイナリ内の全 panic が abort になる）。
//!
//! 受信データ経路（要求行・ヘッダ・本文の読み取り）のため
//! `unwrap`／`expect`／添字アクセス（`[]`）を用いない。
//!
//! ## ストリーム抽象（Issue #968）
//!
//! 接続ハンドラ本体・その内部ヘルパ（[`build_outcome`]・[`read_head`]・
//! [`read_body`]・[`fill_remaining_body`]・[`respond_and_close`]）は
//! `S: `[`crate::wire_stream::WireStream`] に一般化されており、平文
//! `TcpStream`・TLS 上の `TlsStream`（[`crate::http::tls_transport`]。
//! Issue #968）のどちらを渡しても分岐・応答内容・順序は一切変わらない
//! （型を広げるだけ）。呼び出し元は [`crate::http::listener::
//! accept_loop_with_handler`]（平文専用の後方互換経路）と
//! [`crate::http::tls_transport::serve_connection`]（先頭バイトで TLS／平文を
//! 判定した後、平文はそのまま・TLS はハンドシェイク後の `TlsStream` を渡す）
//! の 2 系統。
//!
//! ## RECOVER-5（応答境界）と `catch_unwind` の相互作用
//!
//! 上記の `catch_unwind` は production では `fail_fast` の panic hook が
//! 先に発火するため実際には到達しないが、`fail_fast::install` を呼ばない
//! ライブラリ利用では commit 成功後の panic を「通常の `500` へ縮退させて
//! 処理を継続する」経路になってしまう（`insert` op が engine の commit
//! 境界へ到達する唯一の HTTP 書き込み op。RECOVER-5 違反）。
//! [`build_outcome`] は `handler.handle` の呼び出しを
//! `engine::recovery::commit_boundary::ResponseBoundaryGuard` で覆い、
//! `catch_unwind` が panic を捕捉するより前（unwind 中）にこのガードの
//! `Drop` が作用して `std::process::abort()` することで、`catch_unwind` の
//! 有無に関わらず commit 成功後の panic を常にプロセス終了へ倒す
//! （`crate::simple_query::execute_and_respond` が SQL wire 側で使うのと
//! 同じ機構。詳細は [`build_outcome`] 内のコメント参照）。

use std::net::TcpStream;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant, SystemTime};

use engine::error_format::ErrorClass;

use crate::framing::FrameError;
use crate::http::headers::{parse_headers, HeaderParse, Headers};
use crate::http::request::{parse_request_line, Method, RequestLine, RequestLineParse};
use crate::http::response;
use crate::http::{body, error_body, status};
use crate::limits::REJECT_WRITE_TIMEOUT;
use crate::protocol_dispatch::{drain_and_close, LINGER_DRAIN_TIMEOUT};
use crate::wire_stream::WireStream;

/// 要求の「頭」（要求行＋ヘッダ部）を読み取る固定長スタックバッファの長さ。
///
/// [`crate::http::request::MAX_REQUEST_LINE_LEN`]（要求行の上限）＋
/// [`crate::http::headers::MAX_HEADER_SECTION_LEN`]（ヘッダ部の上限）に等しい。
/// この長さを取ることで、要求行がどれだけ短くても（最短で数バイト）ヘッダ部が
/// 自身の内部上限（`MAX_HEADER_SECTION_LEN`）に到達するまでの余地が常に
/// バッファ内に残る（要求行の消費 ≤ `MAX_REQUEST_LINE_LEN` なので、残り
/// ≥ `MAX_HEADER_SECTION_LEN`）。したがって「バッファが満杯なのにどちらの
/// パーサも Incomplete のまま」という状態は構造的に起こらない契約になる
/// （[`read_head`] の `filled >= HEAD_BUF_LEN` 分岐は、この契約が破れた場合の
/// 安全弁として fail-closed に `08P01` を返すのみで、通常経路では到達しない）。
const HEAD_BUF_LEN: usize =
    crate::http::request::MAX_REQUEST_LINE_LEN + crate::http::headers::MAX_HEADER_SECTION_LEN;

/// 宣言済みヘッダを持たない（＝`Content-Length` を一切知り得ない）状態で
/// 拒否する経路（[`read_head`] が要求行・ヘッダ自体を不正と判定した場合、
/// および panic 捕捉時）向けの読み捨て予算のフォールバック値。
///
/// この経路ではクライアントが実際に何バイト送るつもりかを知る手がかりが
/// 無い（宣言長どころかヘッダ自体が届いていない／壊れている）ため、
/// 「予算が尽きたら即座に打ち切る」設計では原理的に安全な値を選びようが
/// ない。[`LINGER_DRAIN_TIMEOUT`]（1 秒）が実質的な唯一の資源上限になる
/// ことを踏まえ、`usize::MAX` を渡してバイト数側の早期終了を無効化し、
/// 時間切れ（[`drain_and_close`] の `WouldBlock`／`TimedOut` 分岐）にのみ
/// 委ねる（nginx の `lingering_close` と同様、時間のみを資源境界とする
/// 設計）。読み取りバッファ自体は [`drain_and_close`] 内部で 4096 バイトの
/// 固定長スタック配列のまま変わらないため、単位時間あたりの作業量は
/// 変化しない。
const HTTP_LINGER_DRAIN_FALLBACK_BUDGET: usize = usize::MAX;

/// 要求の頭（要求行＋ヘッダ）＋本文の読み取り全体に適用する絶対期限
/// （Slowloris 対策。codex-review 再指摘・PR #810）。
///
/// 呼び出し元（`http::listener::accept_loop_with_handler`）が受理直後に
/// [`crate::limits::apply_read_timeout`] で設定する `read_timeout`
/// （[`crate::limits::READ_TIMEOUT`]）は**個々の `read` 呼び出し**の待機上限
/// にすぎない。[`read_head`]・[`read_body`] のように複数回の `read` を
/// ループする経路でこの値をそのまま使い続けると、攻撃者が各回のタイムアウトが
/// 切れる直前に 1 バイトずつ送り続けることで「読み取りは常に進んでいる」
/// 状態を保ち続け、`WouldBlock`／`TimedOut` に到達せずループ全体の所要時間を
/// 無期限に引き延ばせる。結果として [`crate::limits::MAX_CONNECTIONS`] の
/// 接続枠を長時間占有し続けられる（Slowloris 型 DoS。読み取りタイムアウトが
/// 個々の `read` 単位にしか効かず要求全体の期限にならない構造的欠陥）。
///
/// 対策として [`build_outcome`] が要求の読み取り開始時刻を基準にこの定数を
/// 1 度だけ絶対期限（[`Instant`]）へ変換し、[`read_head`]・[`read_body`] の
/// 各 `read` 呼び出し**前**に「期限までの残り時間」を都度 `set_read_timeout`
/// へ設定するループへ変更する（`protocol_dispatch::drain_and_close` と同じ
/// パターン）。個々の `read` がどれだけ速く応答しても、ループ全体は必ずこの
/// 期限内に完了するか、期限到達でタイムアウト同様に打ち切られる。値は接続
/// 受理時に適用済みの [`crate::limits::READ_TIMEOUT`] と同じ 30 秒とし、
/// 新たなチューニング可能値を増やさない。
///
/// production 経路（`http::listener::accept_loop_with_handler`）は
/// [`crate::limits::READ_TIMEOUT`] そのものを [`handle_connection_with`] の
/// `request_read_deadline` 引数へ渡すため（`main.rs` が `apply_read_timeout`
/// と同じ値を両方へ渡す契約。[`handle_connection_with`] の doc 参照）、
/// この定数自体は production 経路からは直接参照されない。本ファイルの
/// 単体テスト（既定期限での検証）向けの命名済み定数として残す
/// （`#[cfg(test)]` 外の doc コメントから意味づけを共有するため、
/// テスト専用の `#[cfg(test)]` 定数へは分離しない）。
#[cfg_attr(not(test), allow(dead_code))]
const REQUEST_READ_DEADLINE: Duration = crate::limits::READ_TIMEOUT;

/// 期限までの残り時間を求め、`stream` の読み取りタイムアウトへ反映する。
/// 残り時間が実質的に無い（1ms 以下）場合・`set_read_timeout` 自体が失敗した
/// 場合は `None` を返し、呼び出し元は無応答クローズ（HTTP-11）へ倒す。
///
/// [`read_head`]・[`read_body`] の読み取りループが共有する（`protocol_dispatch::
/// drain_and_close` と同型のパターンをこの 1 関数へ集約し重複させない）。
/// `S: WireStream` は平文 `TcpStream`・TLS 上の `TlsStream`（Issue #968）の
/// 双方で同じロジックを走らせるための一般化（Issue #966 の `wire_stream`
/// 抽象を HTTP 接続ハンドラへも適用する）。
fn arm_read_timeout_for_deadline<S: WireStream>(stream: &mut S, deadline: Instant) -> Option<()> {
    let remaining = match deadline.checked_duration_since(Instant::now()) {
        Some(d) if d > Duration::from_millis(1) => d,
        _ => return None,
    };
    stream.set_read_timeout(Some(remaining)).ok()
}

/// ヘッダ解析済み（＝`Content-Length` が既知）の状態で本文読み取り前に
/// 拒否する経路（[`reject_if_expect`]・[`body::plan_body`] のいずれかが
/// `Err` を返した場合）向けの読み捨て予算を求める（codex-review 再指摘・
/// PR #810）。
///
/// # 背景（先行修正 8d3ec05 が不十分だった理由）
///
/// 先行修正は本予算を [`body::MAX_BODY_LEN`]（1 MiB）に固定していたが、
/// `Content-Length` の宣言値そのもの（[`Headers::content_length`]）は
/// untrusted な入力であり `MAX_BODY_LEN` を上回りうる（`plan_body` は宣言長が
/// `MAX_BODY_LEN` を超えていることを理由に拒否するのであって、宣言長を
/// `MAX_BODY_LEN` へ切り詰めるわけではない）。したがって「本文を受理できる
/// 上限」（`MAX_BODY_LEN`）と「拒否後に読み捨てるべき未読データ量」は別の
/// 量であり、前者を後者に流用すると、宣言長が `MAX_BODY_LEN` を超える
/// 拒否応答（413／`54000` 等）でクライアントが実際にそれだけの量を送信中の
/// 場合に読み捨てが打ち切られ、未読データを残した `close` による TCP RST
/// で応答自体が失われうる（モジュール doc の PoC-15 節と同じ懸念）。
///
/// # 予算の求め方
///
/// 宣言済み本文長（`content_length`）から、[`read_head`] が要求の頭を
/// 読み取る際に**既にバッファへ読み込み済み**の本文先頭部分（`residual`。
/// ソケットからは読み取り済みのため drain 不要）を差し引いた残りを予算と
/// する。`content_length` は untrusted なため上限を持たず（`MAX_BODY_LEN`
/// を超えていてもそのまま使う）ため通常は `residual.len()` を上回り、
/// その差分が予算になる。逆に `residual.len()` が `content_length` を
/// 上回ること（例: `Content-Length: 1` かつ不正な `Content-Type` の
/// 要求で本文 `abc` が頭の読み取り時点で既にバッファへ入っていた場合）
/// もあり得る——超過分はいずれにせよ既にソケットから読み取り済みで
/// drain の必要が無いため、`saturating_sub` が 0 を返すだけで正しい
/// （本関数は「本文が宣言長を超える」ことを理由に拒否する経路ではなく、
/// `reject_if_expect`／`plan_body` の拒否から呼ばれる。両者は
/// `residual.len()` と `content_length` の大小関係を検査しない）。
/// 宣言長が巨大な場合は事実上 `usize::MAX` に近い予算になり、
/// [`HTTP_LINGER_DRAIN_FALLBACK_BUDGET`] と同じく
/// [`LINGER_DRAIN_TIMEOUT`] のみが実質的な資源上限として働く
/// （固定バッファ・時間制限は維持したまま、小さい固定上限への到達だけで
/// 即座に打ち切らない設計。codex-review 指摘の修正方針どおり）。
fn drain_budget_after_headers(content_length: usize, residual: &[u8]) -> usize {
    content_length.saturating_sub(residual.len())
}

/// 解析済みの 1 要求（要求行・ヘッダ・本文）。フィールドはいずれも接続ハンドラが
/// 保持するバッファからの借用であり、`RequestHandler` 実装へ読み取り専用で渡す。
///
/// production ルータ（[`crate::http::router::Router`]。Issue #752）は
/// `line.target`／`body` を読む。`headers` は `/v1/session/close` の
/// `Authorization: Bearer` 検証（`crate::http::session::bearer`。Issue #753）
/// が消費し、`/v1/query` 前段ミドルウェア（Issue #754）も同モジュールを
/// 再利用して消費する想定。
pub(crate) struct Request<'a> {
    pub(crate) line: RequestLine<'a>,
    pub(crate) headers: Headers<'a>,
    pub(crate) body: &'a [u8],
}

/// 1 要求を受け取り応答バイト列を返す trait。`handle_connection_with` の注入
/// seam であり、production では [`crate::http::router::Router`]（Issue #752）
/// を、`accept_loop_with_limiter`（後方互換 API）は [`PlaceholderRouter`] を、
/// テストでは任意のスタブ実装（panic 注入を含む）を渡す。
pub(crate) trait RequestHandler {
    fn handle(&self, req: &Request<'_>) -> Vec<u8>;
}

/// 未知の要求ターゲットに対する固定応答バイト列（`08P01`）を組み立てる
/// 共通ヘルパ（Issue #758）。[`PlaceholderRouter`]（フレーミング層テスト用）
/// と production [`crate::http::router::Router`] の双方がこれを呼ぶことで、
/// 「未知パスの応答は両者でバイト同一」という契約を構造的に保証する
/// （文言 `"unknown request target"` は `tests/http_common` 等が固定値として
/// 依存しているため変更しない）。
pub(crate) fn unknown_target_response(now: SystemTime) -> Vec<u8> {
    response::encode_error(ErrorClass::ProtocolViolation, "unknown request target", now)
}

/// パス・メソッドを問わず常に `08P01`（`ErrorClass::ProtocolViolation`）で
/// 拒否する placeholder。`http::listener::accept_loop_with_limiter`
/// （後方互換 API）が使う。production 入口は
/// [`crate::http::router::Router`]（Issue #752・#758。`/v1/session`・
/// `/v1/session/close`・`/v1/query` の 3 エンドポイントへディスパッチし、
/// 他パスは [`unknown_target_response`] で本型と同じバイト列で拒否する）。
pub(crate) struct PlaceholderRouter;

impl RequestHandler for PlaceholderRouter {
    fn handle(&self, _req: &Request<'_>) -> Vec<u8> {
        unknown_target_response(SystemTime::now())
    }
}

/// [`build_outcome`] の結果。書き込みは呼び出し元（`handle_connection_with`）が
/// `catch_unwind` の外で行うため、本 enum は「何を書くか／書かずに閉じるか」の
/// 判断結果のみを保持する。
pub(crate) enum Outcome {
    /// 応答バイト列を 1 回 `write_all` してから [`drain_and_close`] する。
    /// `drain_budget` は読み捨てる上限バイト数（呼び出し元が
    /// [`drain_budget_after_headers`]／[`HTTP_LINGER_DRAIN_FALLBACK_BUDGET`]
    /// のいずれかで決める。doc 参照）。
    Respond { bytes: Vec<u8>, drain_budget: usize },
    /// 応答を書かずに `shutdown` する（相手が何も送らず切断した場合等）。
    CloseSilently,
}

/// 接続ハンドラ本体。`handler` は要求 1 件ぶんの処理を受け持つ
/// [`RequestHandler`] 実装（production は [`PlaceholderRouter`]、テストは
/// 任意のスタブ）。呼び出し元（`listener::accept_loop_with_handler`）が受理
/// 直後に一度だけ `read_timeout`／`write_timeout` を設定した後のソケットを
/// 渡す前提（[`crate::limits::apply_read_timeout`] が両方向へ同じ値を設定
/// 済み）。
///
/// panic 非伝播の設計はモジュール doc を参照。書き込み・[`drain_and_close`]・
/// クローズはすべて `catch_unwind` の**外**で行う。
///
/// `request_read_deadline` は要求の頭＋本文の読み取り全体に適用する絶対期限
/// （[`REQUEST_READ_DEADLINE`] の doc 参照。Slowloris 対策）。呼び出し元
/// （[`crate::http::listener::accept_loop_with_handler`]）が接続受理直後に
/// [`crate::limits::apply_read_timeout`] へ渡すのと**同じ** `read_timeout`
/// をここへも渡す契約とし、「受理直後に設定するタイムアウト」と「要求読み取り
/// 全体の期限」を同じ 1 つの値として扱う（2 つの別々の期限に分裂させない。
/// production では常に [`crate::limits::READ_TIMEOUT`]＝30 秒だが、値そのものは
/// 呼び出し元が決める）。本ファイルの単体テストはこの引数へ短縮値を渡すことで
/// [`crate::limits::READ_TIMEOUT`]（30 秒）を待たずに期限切れ経路を検証する。
pub(crate) fn handle_connection_with<S: WireStream, H: RequestHandler>(
    mut stream: S,
    handler: &H,
    request_read_deadline: Duration,
) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        build_outcome(&mut stream, handler, request_read_deadline)
    }));
    match outcome {
        Ok(Outcome::Respond {
            bytes,
            drain_budget,
        }) => {
            respond_and_close(&mut stream, &bytes, drain_budget);
        }
        Ok(Outcome::CloseSilently) => {
            // H7（Issue #968）: TLS 上では `close_notify` を送ってから
            // 両方向を閉じる（平文 `TcpStream` の `graceful_close` は
            // no-op のため既存挙動とビット同一のまま）。
            stream.graceful_close();
            let _ = stream.shutdown_both();
        }
        Err(_panic_payload) => {
            // panic payload（`Any`）はログ・応答のいずれにも出さない（内部詳細の
            // 非漏えい。P0）。固定文言の 1 行ログのみ残す。
            eprintln!(
                "wire-server: http connection handler panicked; responding with a fixed internal error and closing"
            );
            let bytes = response::encode_error(
                ErrorClass::InternalError,
                "internal error",
                SystemTime::now(),
            );
            // panic 発生時点でどこまで読み取りが進んでいたか（ヘッダ解析
            // 済みか）を安全に復元する手段が無いため、宣言長を知らない
            // 場合と同じフォールバック予算を使う（[`HTTP_LINGER_DRAIN_FALLBACK_BUDGET`]
            // の doc 参照）。
            respond_and_close(&mut stream, &bytes, HTTP_LINGER_DRAIN_FALLBACK_BUDGET);
        }
    }
}

/// 応答バイト列を 1 回 `write_all` し、未読データを有界に読み捨ててから戻る
/// （呼び出し元がその後 `stream` を drop することで接続が閉じる。SQL wire の
/// `protocol_dispatch::reject_and_close` と同じ「書き込み → drain → drop」の
/// 形）。書き込み失敗は無視する（新たなブロッキング点・panic を作らない）。
/// `drain_budget` は呼び出し元（[`build_outcome`]）が [`Outcome::Respond`]
/// ごとに決めた読み捨て上限バイト数。
///
/// 本関数は `handle_connection_with` の `catch_unwind` の**外側**（`Result` を
/// 見てから呼ばれる区間）で実行され、`build_outcome` の
/// `ResponseBoundaryGuard` の保護区間には含まれない（モジュール doc「panic
/// 非伝播と RECOVER-8（fail-fast）との関係」節参照）。ここへ到達する時点で
/// `Outcome::Respond` の `bytes` は既に確定済み（commit を伴う分岐は
/// `build_outcome` 側で完了している）であり、本関数自体は commit を一切
/// 行わないため保護対象の区間には当たらない。ガードをここまで広げようとして
/// `handle_connection_with` 側に外側ガードを追加で置くと、`ResponseBoundaryGuard`
/// のネスト規約（外側が境界を所有し内側は no-op で drop する設計。
/// `commit_boundary.rs` 参照）により、`catch_unwind` 内側の panic が
/// `catch_unwind` で止まった時点でスレッドは非 panicking に戻り、外側ガードの
/// 通常 drop は abort しない——保護がかえって失われるため意図的に行わない。
fn respond_and_close<S: WireStream>(stream: &mut S, bytes: &[u8], drain_budget: usize) {
    let _ = stream.write_all(bytes);
    // `drain_and_close` は内部で `shutdown_write` を呼ぶ。TLS 上ではこれが
    // `close_notify` の送出を兼ねる（`crate::wire_stream::WireStream::
    // shutdown_write` の doc・H7 参照。平文 `TcpStream` は従来どおり FIN のみ）。
    drain_and_close(stream, LINGER_DRAIN_TIMEOUT, drain_budget);
}

/// `catch_unwind` の中身。要求の頭（要求行＋ヘッダ）→ 本文長・`Content-Type`
/// の検証 → 本文読み取り → ルーティングの順に進め、[`Outcome`] を返す。
/// I/O・パースはすべてここで行うが、応答の書き込みは一切行わない
/// （呼び出し元の責務。モジュール doc 参照）。
///
/// 要求の読み取り開始時刻を基準に `request_read_deadline`
/// （呼び出し元 [`handle_connection_with`] の doc 参照）を 1 度だけ
/// 絶対期限へ変換し、頭・本文の読み取り（[`read_head`]・[`read_body`]）の
/// 双方へ同じ期限を渡す（期限はリセットしない。Slowloris 対策の doc 参照）。
fn build_outcome<S: WireStream, H: RequestHandler>(
    stream: &mut S,
    handler: &H,
    request_read_deadline: Duration,
) -> Outcome {
    let deadline = Instant::now() + request_read_deadline;
    let mut head_buf = [0u8; HEAD_BUF_LEN];
    let (line, headers, residual) = match read_head(stream, &mut head_buf, deadline) {
        HeadOutcome::Parsed {
            line,
            headers,
            residual,
        } => (line, headers, residual),
        // ヘッダ自体が未解析・不正なため `Content-Length` を知り得ない
        // （フォールバック予算。doc 参照）。
        HeadOutcome::Respond(bytes) => {
            return Outcome::Respond {
                bytes,
                drain_budget: HTTP_LINGER_DRAIN_FALLBACK_BUDGET,
            }
        }
        HeadOutcome::CloseSilently => return Outcome::CloseSilently,
    };

    if let Err(bytes) = reject_if_expect(&headers) {
        // ヘッダ解析済み（`Content-Length` 既知）のため未読分だけを狙って
        // 読み捨てる。
        return Outcome::Respond {
            bytes,
            drain_budget: drain_budget_after_headers(headers.content_length(), residual),
        };
    }

    let plan = match body::plan_body(&headers) {
        Ok(plan) => plan,
        // codex-review 再指摘（PR #810）の核心経路: `Content-Length` が
        // `MAX_BODY_LEN` を超える・`Content-Type` 不正等で本文を 1 バイトも
        // 読まずに拒否する場合、宣言長ぶんの未読データがまだソケットに
        // 残っている（または残りうる）ため、`MAX_BODY_LEN` ではなく宣言長
        // 基準の予算を使う。
        Err(e) => {
            return Outcome::Respond {
                bytes: frame_error_bytes(&e),
                drain_budget: drain_budget_after_headers(headers.content_length(), residual),
            }
        }
    };

    let body_bytes = match read_body(stream, residual, plan.content_length(), deadline) {
        Ok(BodyOutcome::Bytes(bytes)) => bytes,
        Ok(BodyOutcome::CloseSilently) => return Outcome::CloseSilently,
        // `read_body` の失敗は「宣言長との不一致」（超過／不足）であり、
        // 以後クライアントが `Content-Length` の宣言どおりに振る舞う保証が
        // 無い（プロトコル違反そのもの）ため、宣言長を根拠にした予算では
        // なくフォールバック予算（時間のみが実質的な上限）を使う。
        Err(bytes) => {
            return Outcome::Respond {
                bytes,
                drain_budget: HTTP_LINGER_DRAIN_FALLBACK_BUDGET,
            }
        }
    };

    // メソッドは要求行パーサ（`crate::http::request::parse_method`）が
    // 既に `POST` のみへ絞り込み済み（`Method` は単一 variant の閉じた語彙）。
    // ここでの分岐は将来 `Method` へ variant が増えた場合の fail-closed 安全弁。
    let Method::Post = line.method;

    let req = Request {
        line,
        headers,
        body: &body_bytes,
    };

    // commit 成功から `handler.handle` が応答バイト列を返し終えるまでの区間を
    // 覆う RAII ガード（RECOVER-5 (3)。`crate::simple_query::execute_and_respond`
    // が `ResponseBoundaryGuard` を使う契約と同じ機構を HTTP 側の書き込み経路
    // （`insert` op。`crate::http::query::insert::execute` が
    // `EngineCore::execute_bound_insert_in_session` 経由で commit する）にも
    // 適用する（codex-review P1 指摘・PR #829）。
    //
    // 本モジュール（`build_outcome`）は `handle_connection_with` の
    // `catch_unwind` の**中身**として呼ばれる（モジュール doc「panic 非伝播と
    // RECOVER-8（fail-fast）との関係」節参照）。production では
    // `main.rs::run_server` が起動時に導入する `engine::recovery::fail_fast`
    // の panic hook が unwind 前に `abort()` するためこの `catch_unwind` へは
    // 実際には到達しないが、`fail_fast::install` を呼ばない**ライブラリ利用**
    // （`wire-server` を lib として使う経路・テスト）では `catch_unwind` が
    // panic を捕捉し、書き込み未着手のまま `500`／`ErrorClass::InternalError`
    // へ縮退させてしまう（commit 済みの書き込みを通常失敗として応答する
    // RECOVER-5 違反）。`ResponseBoundaryGuard::drop` は
    // `std::thread::panicking()`（unwind 中かどうか）だけを見て
    // `std::process::abort()` するため、`catch_unwind` に頼らず
    // **`handler.handle` が unwind してこのスコープを抜ける時点**（＝
    // `catch_unwind` が payload を捕捉するより前）で作用する。これにより
    // `catch_unwind` の存在有無に関わらず、commit 成功後の panic は常に
    // プロセス終了へ倒れる（`fail_fast` と同じ帰結を構造的に保証する）。
    //
    // `_response_boundary` を `let _ = ...`（無名束縛）に書き換えると即座に
    // drop され、この保護区間全体が無効化される（`ResponseBoundaryGuard` は
    // `#[must_use]`。`handler.handle` の呼び出しを跨いで生存させるため必ず
    // この名前付き束縛のまま保つ。`simple_query.rs` の同型コメント参照）。
    let _response_boundary = engine::recovery::commit_boundary::ResponseBoundaryGuard::new();

    // 本文（宣言長ぶん）は `read_body` が既に読み切っている。これ以上
    // 届くバイト列は「宣言」を持たない非パイプライン設計上の想定外の
    // 追加データであり、量を見積もる根拠が無いためフォールバック予算
    // （時間のみが実質的な上限）を使う。
    Outcome::Respond {
        bytes: handler.handle(&req),
        drain_budget: HTTP_LINGER_DRAIN_FALLBACK_BUDGET,
    }
}

/// [`read_head`] の結果。`Parsed` の各フィールドは呼び出し元が渡した
/// バッファ（[`HEAD_BUF_LEN`] 長）からの借用。
///
/// `Parsed`（`Headers` が固定長配列を持つため大きい）と他 variant の
/// サイズ差は意図的（[`crate::http::headers::HeaderParse`] と同じ方針。
/// ヒープへ逃がすと本モジュール・`headers` モジュールの「ヒープ確保なし」
/// 方針に反する）。
#[allow(clippy::large_enum_variant)]
enum HeadOutcome<'a> {
    Parsed {
        line: RequestLine<'a>,
        headers: Headers<'a>,
        /// 要求の頭より後に既にバッファへ読み込まれていた本文の先頭部分
        /// （本文読み取りが 1 回の read で頭と本文をまたいで届いた場合）。
        residual: &'a [u8],
    },
    /// 応答バイト列を書いてから閉じる（不正フレーム・上限超過）。
    Respond(Vec<u8>),
    /// 応答を書かずに閉じる（EOF・タイムアウト）。
    CloseSilently,
}

/// [`head_parse_state`] の結果。借用データを一切保持しない（`buf` への
/// 借用に依存しない enum）ことが要点で、これにより `read_head` のループ内で
/// 「今のバイト列で頭が完成しているか」を確認する処理と、「追加のバイトを
/// 読むために `buf` を可変借用する」処理を同じ周回内で安全に共存させられる
/// （Rust の借用チェッカが単一の呼び出しへ 1 つのライフタイムしか割り当て
/// られない制約〔いわゆる NLL problem case #3〕を、借用を返さない設計で
/// 構造的に回避する）。実際に借用済みの `RequestLine`／`Headers` を得る
/// パースは、読み取りループが完全に終わった後に 1 度だけ行う
/// （[`parse_completed_head`]）。
enum HeadParseState {
    /// 要求行・ヘッダともに解析でき、頭の読み取りが完了している。
    Complete,
    /// 追加のバイトが必要（要求行・ヘッダのいずれかが `Incomplete`）。
    NeedMore,
    /// 応答バイト列を書いてから閉じる（不正フレーム・上限超過）。
    Respond(Vec<u8>),
}

/// `buf[..filled]` の時点で要求の頭を解析できるかを判定する（借用非依存）。
fn head_parse_state(buf: &[u8; HEAD_BUF_LEN], filled: usize) -> HeadParseState {
    let head_slice = match buf.get(..filled) {
        Some(s) => s,
        None => return HeadParseState::Respond(protocol_violation_bytes("request head bounds")),
    };

    match parse_request_line(head_slice) {
        Ok(RequestLineParse::Complete {
            consumed: line_consumed,
            ..
        }) => {
            let headers_input = match buf.get(line_consumed..filled) {
                Some(s) => s,
                None => {
                    return HeadParseState::Respond(protocol_violation_bytes("request head bounds"))
                }
            };
            match parse_headers(headers_input) {
                Ok(HeaderParse::Complete { .. }) => HeadParseState::Complete,
                Ok(HeaderParse::Incomplete) => HeadParseState::NeedMore,
                Err(e) => HeadParseState::Respond(frame_error_bytes(&e)),
            }
        }
        Ok(RequestLineParse::Incomplete) => HeadParseState::NeedMore,
        Err(e) => HeadParseState::Respond(frame_error_bytes(&e)),
    }
}

/// [`head_parse_state`] が `Complete` を返した後に、実際に借用済みの
/// `RequestLine`／`Headers`／本文残余を取り出す。読み取りループの外
/// （`buf` への可変借用が以後発生しない箇所）から 1 度だけ呼ぶ契約。
fn parse_completed_head(
    buf: &[u8; HEAD_BUF_LEN],
    filled: usize,
) -> Result<(RequestLine<'_>, Headers<'_>, &[u8]), Vec<u8>> {
    let head_slice = buf
        .get(..filled)
        .ok_or_else(|| protocol_violation_bytes("request head bounds"))?;
    let (line, line_consumed) = match parse_request_line(head_slice) {
        Ok(RequestLineParse::Complete { line, consumed }) => (line, consumed),
        _ => return Err(protocol_violation_bytes("request head bounds")),
    };
    let headers_input = buf
        .get(line_consumed..filled)
        .ok_or_else(|| protocol_violation_bytes("request head bounds"))?;
    let (headers, header_consumed) = match parse_headers(headers_input) {
        Ok(HeaderParse::Complete { headers, consumed }) => (headers, consumed),
        _ => return Err(protocol_violation_bytes("request head bounds")),
    };
    let head_len = line_consumed
        .checked_add(header_consumed)
        .ok_or_else(|| protocol_violation_bytes("request head length overflow"))?;
    let residual = buf
        .get(head_len..filled)
        .ok_or_else(|| protocol_violation_bytes("request head bounds"))?;
    Ok((line, headers, residual))
}

/// 要求の頭（要求行＋ヘッダ部）を有界に読み取り、解析する。
///
/// ループの各周回で「今持っているバイト列で頭が完成しているか
/// （[`head_parse_state`]。借用を返さない）を確認する→不足なら追加で 1 回
/// read する」を繰り返し、完成した時点でループを抜けてから
/// [`parse_completed_head`] で実際の借用データを 1 度だけ取り出す。各段の
/// 内部上限（`request::MAX_REQUEST_LINE_LEN`・`headers::
/// MAX_HEADER_SECTION_LEN`）は [`HEAD_BUF_LEN`] の doc が説明するとおり
/// バッファが満杯になる前に必ず先に効く契約。
///
/// `deadline` は要求全体（頭＋本文）の絶対読み取り期限（[`REQUEST_READ_DEADLINE`]
/// の doc 参照）。各 `read` 呼び出しの**前**に [`arm_read_timeout_for_deadline`]
/// で残り時間をソケットへ反映し、期限切れなら（個々の read が速く応答して
/// いても）無応答クローズへ倒す。
fn read_head<'buf, S: WireStream>(
    stream: &mut S,
    buf: &'buf mut [u8; HEAD_BUF_LEN],
    deadline: Instant,
) -> HeadOutcome<'buf> {
    let mut filled = 0usize;

    // このループは `buf` への可変借用（`stream.read` 用）と、借用非依存の
    // 判定（`head_parse_state`）だけを行い、借用済みデータは一切保持しない。
    enum LoopExit {
        Complete,
        Respond(Vec<u8>),
        CloseSilently,
    }
    let exit = loop {
        match head_parse_state(buf, filled) {
            HeadParseState::Complete => break LoopExit::Complete,
            HeadParseState::NeedMore => {
                // 追加のバイトを読んで次周回で再試行する。
            }
            HeadParseState::Respond(bytes) => break LoopExit::Respond(bytes),
        }

        if filled >= HEAD_BUF_LEN {
            // [`HEAD_BUF_LEN`] の doc が説明する契約が破れた場合の安全弁
            // （通常経路では到達しない）。
            break LoopExit::Respond(protocol_violation_bytes("request head exceeds limit"));
        }
        // 期限切れなら、ここまでにどれだけ read が完了していても打ち切る
        // （Slowloris 対策。[`REQUEST_READ_DEADLINE`] の doc 参照）。
        if arm_read_timeout_for_deadline(stream, deadline).is_none() {
            break LoopExit::CloseSilently;
        }
        let read_target = match buf.get_mut(filled..) {
            Some(s) => s,
            None => break LoopExit::Respond(protocol_violation_bytes("request head bounds")),
        };
        match stream.read(read_target) {
            Ok(0) => {
                if filled == 0 {
                    // 接続して何も送らずに切断（HTTP-11: 無応答で良い）。
                    break LoopExit::CloseSilently;
                }
                // 要求の途中で切断された。応答は書ける状態のため書く。
                break LoopExit::Respond(protocol_violation_bytes(
                    "connection closed before request head completed",
                ));
            }
            Ok(n) => {
                filled = match filled.checked_add(n) {
                    Some(v) => v,
                    None => {
                        break LoopExit::Respond(protocol_violation_bytes(
                            "request head length overflow",
                        ))
                    }
                };
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // HTTP-11: 段階を問わず、タイムアウトは無応答でクローズする。
                break LoopExit::CloseSilently;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => break LoopExit::CloseSilently,
        }
    };

    match exit {
        LoopExit::Complete => match parse_completed_head(buf, filled) {
            Ok((line, headers, residual)) => HeadOutcome::Parsed {
                line,
                headers,
                residual,
            },
            Err(bytes) => HeadOutcome::Respond(bytes),
        },
        LoopExit::Respond(bytes) => HeadOutcome::Respond(bytes),
        LoopExit::CloseSilently => HeadOutcome::CloseSilently,
    }
}

/// [`read_body`] の結果（正常系）。
enum BodyOutcome {
    Bytes(Vec<u8>),
    CloseSilently,
}

/// 本文（宣言済み `content_length` バイト）を読み取る。`residual` は
/// [`read_head`] が頭の読み取り時に既に取得していた本文の先頭部分（1 回の
/// read が頭と本文の境界をまたいだ場合）。
///
/// アロケーションサイズ（`vec![0u8; content_length]`）は呼び出し元
/// （[`build_outcome`]）が [`body::plan_body`] を通した後の `content_length`
/// のみを渡す契約（[`MAX_BODY_LEN`](body::MAX_BODY_LEN) 以下であることが
/// 型で保証された値）であり、未検証の宣言長を直接確保に使わない。
///
/// `deadline` は [`read_head`] と共有する要求全体の絶対読み取り期限
/// （[`REQUEST_READ_DEADLINE`] の doc 参照）。`std::io::Read::read_exact` は
/// 内部で複数回 `read` をループしても呼び出し前に設定した 1 つの
/// `read_timeout` しか効かせられず、[`read_head`] と同型の Slowloris 経路を
/// 残してしまうため使わず、[`read_head`] と同じ「各 `read` 前に残り時間を
/// 都度反映する」手書きループ（[`fill_remaining_body`]）に委ねる。
fn read_body<S: WireStream>(
    stream: &mut S,
    residual: &[u8],
    content_length: usize,
    deadline: Instant,
) -> Result<BodyOutcome, Vec<u8>> {
    if residual.len() > content_length {
        // 頭の読み取り時点で本文の宣言長を超えるバイト列が既に届いている
        // （Content-Length との不一致: 超過）。
        return Err(protocol_violation_bytes(
            "request body exceeds declared content-length",
        ));
    }

    let mut buf = vec![0u8; content_length];
    let copy_len = residual.len().min(content_length);
    let src = match residual.get(..copy_len) {
        Some(s) => s,
        None => return Err(protocol_violation_bytes("request body bounds")),
    };
    match buf.get_mut(..copy_len) {
        Some(dst) => dst.copy_from_slice(src),
        None => return Err(protocol_violation_bytes("request body bounds")),
    }

    let remaining = match buf.get_mut(copy_len..) {
        Some(s) => s,
        None => return Err(protocol_violation_bytes("request body bounds")),
    };
    if !remaining.is_empty() {
        match fill_remaining_body(stream, remaining, deadline)? {
            FillOutcome::Complete => {}
            FillOutcome::CloseSilently => return Ok(BodyOutcome::CloseSilently),
        }
    }

    Ok(BodyOutcome::Bytes(buf))
}

/// [`fill_remaining_body`] の結果（正常系）。
enum FillOutcome {
    Complete,
    CloseSilently,
}

/// `target` を宣言長ぶんの本文で埋めるまで、期限を都度反映しながら `read` を
/// 繰り返す（[`read_head`] の読み取りループと同じ Slowloris 対策パターン。
/// [`REQUEST_READ_DEADLINE`] の doc 参照）。
fn fill_remaining_body<S: WireStream>(
    stream: &mut S,
    target: &mut [u8],
    deadline: Instant,
) -> Result<FillOutcome, Vec<u8>> {
    let mut filled = 0usize;
    while filled < target.len() {
        // 期限切れなら、ここまでにどれだけ読めていても打ち切る。
        if arm_read_timeout_for_deadline(stream, deadline).is_none() {
            return Ok(FillOutcome::CloseSilently);
        }
        let dst = match target.get_mut(filled..) {
            Some(s) => s,
            None => return Err(protocol_violation_bytes("request body bounds")),
        };
        match stream.read(dst) {
            Ok(0) => {
                // Content-Length との不一致: 不足（相手が早期に切断した）。
                return Err(protocol_violation_bytes(
                    "request body ended before declared content-length",
                ));
            }
            Ok(n) => {
                filled = match filled.checked_add(n) {
                    Some(v) => v,
                    None => return Err(protocol_violation_bytes("request body length overflow")),
                };
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(FillOutcome::CloseSilently);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => return Ok(FillOutcome::CloseSilently),
        }
    }
    Ok(FillOutcome::Complete)
}

/// `Expect` ヘッダの名前（ASCII 大文字小文字非区別で [`Headers::get_single`]
/// へ渡す）。
const EXPECT_HEADER: &[u8] = b"expect";

/// 本文読み取り（[`body::plan_body`]・[`read_body`]）より前に呼び、`Expect`
/// ヘッダの拒否を行う。モジュール doc「`Expect` ヘッダの扱い」節参照。
///
/// 値を問わず（`100-continue` であっても、それ以外の未知のトークンで
/// あっても）1 件でも付いていれば拒否する。本モジュールは暫定応答
/// （`100 Continue`）を送出する経路を持たないため、値ごとに対応を分ける
/// 意味がなく、fail-closed に一律拒否する方が「対応する暫定応答を送るか
/// 非対応として最終エラー応答を返す」の後者を漏れなく満たせる。
/// `get_single` 自体が重複ヘッダを `FrameError::Malformed` として拒否する
/// ため、その経路も同じ最終エラー応答（[`frame_error_bytes`] 経由）へ倒す。
fn reject_if_expect(headers: &Headers<'_>) -> Result<(), Vec<u8>> {
    match headers.get_single(EXPECT_HEADER) {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(response::encode_error(
            ErrorClass::FeatureNotSupported,
            "Expect header is not supported on this connection",
            SystemTime::now(),
        )),
        Err(e) => Err(frame_error_bytes(&e)),
    }
}

/// [`FrameError`]（要求行・ヘッダ・本文長のいずれかのパーサが返す）を応答
/// バイト列へ変換する。`error_class()`／`client_message()` が `None`／空文字を
/// 返すのは `Truncated`／`Io`（本層のパーサ群は生成しない variant）のみの
/// ため、防御的に `ProtocolViolation`／固定文言へ fail-closed に縮退する。
fn frame_error_bytes(err: &FrameError) -> Vec<u8> {
    let class = err.error_class().unwrap_or(ErrorClass::ProtocolViolation);
    let message = err.client_message();
    let message = if message.is_empty() {
        "invalid request"
    } else {
        message
    };
    response::encode_error(class, message, SystemTime::now())
}

/// 固定文言の `08P01`（`ErrorClass::ProtocolViolation`）応答バイト列を組み立てる
/// 共通ヘルパ（codex-review 指摘・PR #810）。
///
/// `reason` は呼び出し元ごとの内部的な検証境界名（例: `"request head
/// bounds"`）であり、ワイヤへ送る `message` フィールドには使わない。
/// [`frame_error_bytes`] が `FrameError::client_message()` の空文字を
/// 汎用文言 `"invalid request"` へ縮退させているのと同じ契約に揃え、
/// 実装内部の検証境界名を常にログ専用（`eprintln!`）に留めてクライアント
/// へは固定の汎用文言のみを返す。
fn protocol_violation_bytes(reason: &str) -> Vec<u8> {
    eprintln!("wire-server: http protocol violation: {reason}");
    response::encode_error(
        ErrorClass::ProtocolViolation,
        "invalid request",
        SystemTime::now(),
    )
}

/// 同時接続数の枠を確保できなかった接続へ HTTP 503 ＋ JSON 本文
/// （`wire_code`＝`53300`）を書き込み、接続を閉じる。
///
/// `crate::limits::reject_too_many_connections`（SQL 表層）の HTTP 版。
/// [`reject_too_many_connections_on`] へ委譲する薄いラッパー（`TcpStream`
/// 直呼び出しの既存呼び出し元向け）。
pub(crate) fn reject_too_many_connections(mut stream: TcpStream) {
    reject_too_many_connections_on(&mut stream);
}

/// [`reject_too_many_connections`] の本体（`S: WireStream` で一般化）。平文
/// `TcpStream` に加え、`crate::http::tls_transport::reject_or_close_over_limit`
/// が TLS ハンドシェイク成功後の `TlsStream` へも同じ 503／`53300` 応答を
/// 送るために呼ぶ（Issue #968 codex-review P1 是正: `--tls-mode allow` の
/// 上限超過時、先頭バイトが TLS レコードでもハンドシェイクを完了してから
/// 拒否応答を返す経路が必要という指摘）。
///
/// 書き込みタイムアウトは [`REJECT_WRITE_TIMEOUT`] を使う（拒否応答自体が
/// accept ループのブロッキング点にならないよう小さく設定する契約を共有）。
/// 書き込み失敗は無視する（拒否経路で新たなブロッキング点・panic を作らない
/// ため）。
///
/// 応答を `write_all` した直後に `shutdown(Both)` で即座に閉じるのではなく、
/// [`respond_and_close`] と同じ「書き込み → 有界 lingering close（
/// [`drain_and_close`]） → drop」の形にする（codex-review・Cursor Bugbot
/// 指摘・Issue #968 是正）。呼び出し元（`tls_transport::serve_connection`／
/// `reject_or_close_over_limit`）は応答判定のために先頭 1 バイトを `peek`
/// 済みであり、そのバイト（および後続で既にパイプライン済みの要求バイト
/// 列）が未読のまま `shutdown(Both)` すると、OS が未読データありの
/// クローズを検知して TCP RST を送りうる（PoC-15 と同じ理屈）。RST を受けた
/// クライアントは既に送出済みの 503 応答を読めないまま接続断と誤認する
/// ため、`drain_and_close` で残データを有界に読み捨ててから戻る。
pub(crate) fn reject_too_many_connections_on<S: WireStream>(stream: &mut S) {
    let _ = stream.set_write_timeout(Some(REJECT_WRITE_TIMEOUT));
    let response = encode_reject_response();
    let _ = stream.write_all(&response);
    drain_and_close(
        stream,
        LINGER_DRAIN_TIMEOUT,
        HTTP_LINGER_DRAIN_FALLBACK_BUDGET,
    );
}

/// 同時接続数上限超過時の HTTP 応答バイト列を組み立てる純関数。
///
/// [`crate::http::response`]（Issue #746）が入る前の暫定実装（Issue #743）の
/// まま維持している最小エンベロープ。ステータス行
/// （`http::status::http_status(ErrorClass::ConnectionLimitExceeded)` ＝
/// 503 固定のため reason phrase も固定表記）・`Connection: close`・
/// `Content-Type: application/json; charset=utf-8`・`Content-Length`・
/// 空行・本文（`http::error_body::encode`）の順に組み立てる。
fn encode_reject_response() -> Vec<u8> {
    let class = ErrorClass::ConnectionLimitExceeded;
    debug_assert_eq!(status::http_status(class), 503);
    let body = error_body::encode(class, "too many connections");
    let body_bytes = body.as_bytes();

    let mut out = Vec::with_capacity(128 + body_bytes.len());
    out.extend_from_slice(b"HTTP/1.1 503 Service Unavailable\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"Content-Type: application/json; charset=utf-8\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body_bytes.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body_bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::Duration;

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (server, client)
    }

    /// `stream.read` が実際に EOF（`Ok(0)`）で終わったことを確認する。
    fn assert_eof(stream: &mut TcpStream) {
        let mut buf = [0u8; 8];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => panic!("expected EOF without extra bytes, got {n} bytes"),
            Err(e) => panic!("expected EOF (Ok(0)), got read error: {e:?}"),
        }
    }

    /// サーバー側が応答を書かずに接続を閉じたことを緩やかに確認する
    /// （クリーンな EOF・`ConnectionReset` のいずれも許容する）。
    ///
    /// [`assert_eof`] は「相手がまだ何か送っている最中ではない」クリーンな
    /// クローズのみを検証する既存テスト向けに残すが、Slowloris 回帰テスト
    /// （[`head_read_deadline_closes_connection_despite_steady_trickle`]・
    /// [`body_read_deadline_closes_connection_despite_steady_trickle`]）は
    /// クライアントが送信を続けている最中にサーバー側が期限切れで
    /// `shutdown` するため、OS が未読データの残ったソケットの close を
    /// RST として観測しうる（一般的な TCP の挙動。HTTP レベルの契約とは
    /// 無関係なテスト構成上の副作用）。本ヘルパは「サーバーが無期限に
    /// ハングせず接続を閉じたこと」だけを確認する。`set_read_timeout`
    /// 自体が（RST 後のソケット状態次第で）`EINVAL` を返すことがあるため
    /// 失敗は無視する（無視しても、既に閉じている接続への読み取りは
    /// タイムアウトを介さず即座に結果を返すため待機は発生しない）。
    fn assert_closed_without_hanging(stream: &mut TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 8];
        match stream.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => panic!("expected connection close without extra bytes, got {n} bytes"),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("expected EOF or connection reset, got read error: {e:?}"),
        }
    }

    fn read_all(stream: &mut TcpStream) -> Vec<u8> {
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        received
    }

    fn status_line(bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        text.lines().next().unwrap_or_default().to_string()
    }

    fn wire_code_of(bytes: &[u8]) -> String {
        let text = String::from_utf8_lossy(bytes);
        let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
        let body = &text[body_start..];
        let parsed = engine::json::parse_json(body).expect("body must be valid JSON");
        let engine::json::JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        let engine::json::JsonValue::Object(error_obj) = top.get("error").expect("error key")
        else {
            panic!("error value must be an object");
        };
        let engine::json::JsonValue::String(code) = error_obj
            .get("wire_code")
            .expect("wire_code present")
            .clone()
        else {
            panic!("wire_code must be a string");
        };
        code
    }

    /// production 相当の `PlaceholderRouter` を使い、正常形の要求が単一の
    /// `08P01` 応答（1 往復で完結）へ落ちること。
    #[test]
    fn placeholder_router_answers_08p01_for_well_formed_request() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
            .expect("write request");

        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 400 Bad Request");
        assert_eq!(wire_code_of(&received), "08P01");

        handle.join().expect("handler thread must not panic");
    }

    /// 要求行不正（非対応メソッド）は 400／`08P01` で 1 往復完結する。
    #[test]
    fn rejects_malformed_request_line() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client.write_all(b"GET / HTTP/1.1\r\n\r\n").expect("write");
        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 400 Bad Request");
        assert_eq!(wire_code_of(&received), "08P01");

        handle.join().expect("handler thread must not panic");
    }

    /// ヘッダ部不正（`Transfer-Encoding` 指定）は 400／`08P01`。
    #[test]
    fn rejects_malformed_headers() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(b"POST /v1/query HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n")
            .expect("write");
        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 400 Bad Request");
        assert_eq!(wire_code_of(&received), "08P01");

        handle.join().expect("handler thread must not panic");
    }

    /// codex-review 指摘（PR #810）の回帰テスト: `Expect: 100-continue` を
    /// 付けたクライアントが、本文を一切送信しなくても（暫定応答を待つ実装を
    /// 模した状態でも）最終エラー応答を受け取れる。本文を待って `read_body`
    /// へ進んでいれば、クライアントが送らない本文を待ち続け、この
    /// `read_all` は読み取りタイムアウトまで応答を受け取れずテストが失敗する。
    #[test]
    fn rejects_expect_100_continue_without_waiting_for_body() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        // 本文は宣言だけして実際には送らない（100-continue を待つ実装の模倣）。
        client
            .write_all(
                b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nExpect: 100-continue\r\n\r\n",
            )
            .expect("write head only");

        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 501 Not Implemented");
        assert_eq!(wire_code_of(&received), "0A000");

        handle.join().expect("handler thread must not panic");
    }

    /// `Expect` は値を問わず拒否する（未知のトークンでも同じ最終エラー応答）。
    #[test]
    fn rejects_expect_with_unknown_token() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(
                b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nExpect: unknown-token\r\n\r\n{}",
            )
            .expect("write");

        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 501 Not Implemented");
        assert_eq!(wire_code_of(&received), "0A000");

        handle.join().expect("handler thread must not panic");
    }

    /// RST 回帰の核心: `Content-Length` 超過（413）の応答が、クライアントが
    /// 本文を送り続けている最中でも末尾まで完全に到達する。
    #[test]
    fn oversized_content_length_response_arrives_completely_while_client_keeps_sending() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        // codex-review 再指摘（PR #810）の回帰: 宣言長（`declared`）と実際に
        // 送信する量を一致させ、旧実装（`body::MAX_BODY_LEN`＝1 MiB 固定の
        // drain 予算）なら 1 MiB を超えた時点で drain が打ち切られ、
        // 未読データを残した `close` により送信スレッドが `write_all` の
        // 失敗（`ECONNRESET`／`EPIPE`）を観測しうる量（2 MiB）を送る。
        let declared = 2 * 1024 * 1024usize;
        let header = format!(
            "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\n\r\n"
        );
        client.write_all(header.as_bytes()).expect("write head");

        let sender = std::thread::spawn(move || {
            let chunk = vec![b'a'; 8192];
            let chunks = declared / chunk.len();
            for _ in 0..chunks {
                // 修正後の実装は宣言長基準の drain 予算（実質無制限。1 秒の
                // 時間制限のみが上限）を使うため、送信側は 1 度も
                // エラーを観測せずに送り切れるはずである。
                client
                    .write_all(&chunk)
                    .expect("send full declared body without error");
            }
            client
        });

        let mut client = sender.join().expect("sender thread must not panic");
        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 413 Content Too Large");
        assert_eq!(wire_code_of(&received), "54000");

        handle.join().expect("handler thread must not panic");
    }

    /// codex-review 再指摘（PR #810）の回帰: 宣言長が `MAX_BODY_LEN` を
    /// 大幅に超える（実際には送らない）場合でも、実送信分（数 MiB）は
    /// 予算計算（`content_length.saturating_sub(residual.len())`）が
    /// 事実上無制限になるため drain され、応答が失われない。
    #[test]
    fn absurdly_large_declared_content_length_still_lets_response_arrive() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        // 1 GiB を宣言するが実際にはその一部（3 MiB）しか送らない
        // （現実のクライアントが誤って巨大な `Content-Length` を宣言した
        // ケースを模す）。
        let declared = 1024 * 1024 * 1024usize;
        let header = format!(
            "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\n\r\n"
        );
        client.write_all(header.as_bytes()).expect("write head");

        let sender = std::thread::spawn(move || {
            let chunk = vec![b'a'; 8192];
            for _ in 0..(3 * 1024 * 1024 / chunk.len()) {
                client.write_all(&chunk).expect("send bytes without error");
            }
            client
        });

        let mut client = sender.join().expect("sender thread must not panic");
        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 413 Content Too Large");
        assert_eq!(wire_code_of(&received), "54000");

        handle.join().expect("handler thread must not panic");
    }

    /// 本文不足（宣言長 > 実送信、クライアントが早期に書き込み方向を閉じる）は
    /// 400／`08P01`。
    #[test]
    fn rejects_body_shorter_than_declared_content_length() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(
                b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\nab",
            )
            .expect("write");
        client.shutdown(Shutdown::Write).expect("shutdown write");

        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 400 Bad Request");
        assert_eq!(wire_code_of(&received), "08P01");

        handle.join().expect("handler thread must not panic");
    }

    /// 本文余剰（宣言長 < head 内に既に届いた残余）は 400／`08P01`。
    #[test]
    fn rejects_body_longer_than_declared_content_length_within_head_buffer() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(
                b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\nabc",
            )
            .expect("write");

        let received = read_all(&mut client);
        assert_eq!(status_line(&received), "HTTP/1.1 400 Bad Request");
        assert_eq!(wire_code_of(&received), "08P01");

        handle.join().expect("handler thread must not panic");
    }

    /// 本文の途中で要求読み取りの絶対期限へ達した場合は無応答 EOF。
    ///
    /// production の期限（[`REQUEST_READ_DEADLINE`]。30 秒）をそのまま
    /// 待つと単体テストとして長すぎるため、[`handle_connection_with`]
    /// （`request_read_deadline` 引数。doc 参照）へ短い期限を渡す。かつての
    /// 実装は接続受理時に設定済みの `read_timeout`（ここでは 150ms へ
    /// 上書き）がそのまま本文読み取りの実効タイムアウトになっていたが、
    /// 本 PR の Slowloris 対策（`REQUEST_READ_DEADLINE` の doc 参照）で
    /// `build_outcome` が各 `read` 前に残り時間を都度 `set_read_timeout`
    /// へ反映するようになったため、ソケットへ事前設定した値は
    /// 上書きされる。したがって本テストの `server.set_read_timeout` 呼び出し
    /// 自体はもはや期限を左右しない（初期値として残すのみ）。
    #[test]
    fn closes_silently_when_body_read_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        server
            .set_read_timeout(Some(Duration::from_millis(150)))
            .expect("set server read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, Duration::from_millis(150));
        });

        client
            .write_all(
                b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\nab",
            )
            .expect("write partial body, then stall");

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client read timeout");
        assert_eof(&mut client);

        handle.join().expect("handler thread must not panic");
    }

    /// Slowloris 回帰テスト（codex-review 再指摘・PR #810）: 要求の頭を
    /// 1 バイトずつ、個々の `read` タイムアウトより短い間隔で送り続ける
    /// クライアントは、どの 1 回の `read` もタイムアウトしないため
    /// [`arm_read_timeout_for_deadline`] が無ければ無期限に接続枠を
    /// 占有できてしまう。要求全体の絶対期限（ここではテスト専用の
    /// 短縮値）に確実に到達し、無応答クローズへ倒れることを固定する。
    #[test]
    fn head_read_deadline_closes_connection_despite_steady_trickle() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        // 期限を 200ms とし、40ms 間隔（個々の read の待機上限より確実に
        // 短い）で 1 バイトずつ送り続けることで「個々の read は毎回進んで
        // いる」状態を作る。期限がなければこのループは要求行が完成する
        // （数十バイト × 40ms）よりずっと長く、テストが検証したい「期限切れ
        // で打ち切られる」経路には決して届かない。
        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, Duration::from_millis(200));
        });

        let trickle = b"GET /v1/query HTTP/1.1\r\n";
        for &byte in trickle {
            if client.write_all(&[byte]).is_err() {
                // サーバーが期限切れで先に閉じた（テストの目的どおり）。
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }

        assert_closed_without_hanging(&mut client);

        handle.join().expect("handler thread must not panic");
    }

    /// Slowloris 回帰テスト（codex-review 再指摘・PR #810）: 宣言済み本文
    /// （`Content-Length`）を 1 バイトずつ、個々の `read` タイムアウトより
    /// 短い間隔で送り続けるクライアントに対しても、要求全体の絶対期限に
    /// 到達し次第、無応答クローズへ倒れることを固定する
    /// （[`closes_silently_when_body_read_times_out`] は完全に停止する
    /// クライアントを検証するのに対し、本テストは「常に何か送っている」
    /// クライアントを検証する点が異なる）。
    #[test]
    fn body_read_deadline_closes_connection_despite_steady_trickle() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, Duration::from_millis(200));
        });

        client
            .write_all(b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\n")
            .expect("write head");

        // 宣言長 10 バイトのうち、40ms 間隔で 1 バイトずつ送り続ける
        // （期限 200ms を優に超える所要時間）。
        for byte in b"abcdefghij" {
            if client.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }

        assert_closed_without_hanging(&mut client);

        handle.join().expect("handler thread must not panic");
    }

    /// 接続して何も送らず切断した場合は無応答 EOF。
    #[test]
    fn closes_silently_when_client_sends_nothing() {
        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PlaceholderRouter, REQUEST_READ_DEADLINE);
        });

        client.shutdown(Shutdown::Write).expect("shutdown write");
        assert_eof(&mut client);

        handle.join().expect("handler thread must not panic");
    }

    /// panic 注入: `RequestHandler` 実装が `handle` で panic しても、
    /// `handle_connection_with` は panic せずに戻り（スレッドが正常終了し）、
    /// クライアントは 500／`XX000` 応答または EOF のいずれかを受け取る
    /// （書き込み後に相手が読み切れない場合もあるため、応答を最後まで受け
    /// 取れることまでは要求せず「サーバースレッドが panic で死なない」ことを
    /// 主眼にする。ここでは `join` の成功と、受信できた場合の内容を確認する）。
    #[test]
    fn handler_panic_does_not_propagate_and_thread_returns() {
        struct PanickingHandler;
        impl RequestHandler for PanickingHandler {
            fn handle(&self, _req: &Request<'_>) -> Vec<u8> {
                panic!("injected handler panic for HTTP-12 verification");
            }
        }

        let (server, mut client) = loopback_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");

        let handle = std::thread::spawn(move || {
            handle_connection_with(server, &PanickingHandler, REQUEST_READ_DEADLINE);
        });

        client
            .write_all(b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}")
            .expect("write request");

        let received = read_all(&mut client);
        if !received.is_empty() {
            assert_eq!(status_line(&received), "HTTP/1.1 500 Internal Server Error");
            assert_eq!(wire_code_of(&received), "XX000");
        }

        handle
            .join()
            .expect("handler thread must return normally despite the injected panic");
    }

    /// 同時接続数上限超過時の HTTP 応答バイト列（既存の Issue #743 テスト
    /// 内容をそのまま維持）。
    #[test]
    fn encode_reject_response_has_53300_body_and_matching_content_length() {
        let response = encode_reject_response();
        let text = String::from_utf8(response.clone()).expect("response must be valid utf-8");

        assert!(
            text.starts_with("HTTP/1.1 503 "),
            "unexpected status line: {text:?}"
        );
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Type: application/json; charset=utf-8\r\n"));

        let header_end = text
            .find("\r\n\r\n")
            .expect("must contain exactly one header terminator");
        assert_eq!(
            text.matches("\r\n\r\n").count(),
            1,
            "header terminator must appear exactly once"
        );
        let body = &text[header_end + 4..];

        let content_length_line = text
            .lines()
            .find(|l| l.starts_with("Content-Length:"))
            .expect("Content-Length header present");
        let declared_len: usize = content_length_line
            .trim_start_matches("Content-Length:")
            .trim()
            .parse()
            .expect("Content-Length must be a valid integer");
        assert_eq!(declared_len, body.len());

        let parsed = engine::json::parse_json(body).expect("body must be valid JSON");
        let engine::json::JsonValue::Object(top) = parsed else {
            panic!("top level must be an object");
        };
        let error_value = top.get("error").expect("error key present");
        let engine::json::JsonValue::Object(error_obj) = error_value else {
            panic!("error value must be an object");
        };
        let wire_code = error_obj.get("wire_code").expect("wire_code present");
        assert_eq!(
            wire_code,
            &engine::json::JsonValue::String(
                crate::limits::SQLSTATE_TOO_MANY_CONNECTIONS.to_string()
            )
        );
        assert!(
            !error_obj.contains_key("data"),
            "reject response must not carry the emergency-response-only data key"
        );
    }

    /// `reject_too_many_connections` は 503 応答を書き込んでから EOF になる。
    #[test]
    fn reject_too_many_connections_writes_response_and_closes() {
        let (server, mut client) = loopback_pair();

        std::thread::spawn(move || {
            reject_too_many_connections(server);
        });

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let received = read_all(&mut client);
        let text = String::from_utf8_lossy(&received);
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text:?}");
        assert!(text.contains("53300"), "got: {text:?}");
    }

    /// `reject_too_many_connections`（`tls_transport::serve_connection`／
    /// `reject_or_close_over_limit` の呼び出し元は先頭バイトを `peek` した
    /// だけで、要求本体は未読のままこの関数へ渡す契約）は、クライアントが
    /// 既に要求バイト列を送信済み（サーバー未読）の状態でも 503 応答を
    /// 完全に読める形で返すこと（Issue #968 codex-review・Cursor Bugbot
    /// 指摘の回帰防止: `write_all` 直後に未読データを残したまま
    /// `shutdown(Both)` すると OS が TCP RST を送りうり、クライアントが
    /// 送出済みの 503 応答を読めなくなる）。
    #[test]
    fn reject_too_many_connections_delivers_response_despite_unread_pipelined_request() {
        let (server, mut client) = loopback_pair();

        // 要求バイト列を送信するが、サーバー側は一切読まない
        // （`reject_too_many_connections` は要求を解釈しない契約）。
        client
            .write_all(b"POST /v1/session HTTP/1.1\r\nContent-Length: 4\r\n\r\nabcd")
            .expect("write pipelined request");

        std::thread::spawn(move || {
            reject_too_many_connections(server);
        });

        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
                Err(e) => panic!(
                    "client must read the full 503 response without a connection reset, got: {e:?}"
                ),
            }
        }
        let text = String::from_utf8_lossy(&received);
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text:?}");
        assert!(text.contains("53300"), "got: {text:?}");
    }
}
