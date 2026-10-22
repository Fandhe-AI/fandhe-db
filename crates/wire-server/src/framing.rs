//! wire プロトコル入力のフレーミング検証（長さ上限・最小長・途中切断の分類）を
//! 1 箇所へ集約するモジュール。
//!
//! `handshake.rs` の接続ハンドラ（`negotiate_startup` / `read_password_message` /
//! `post_auth_loop`）から呼ばれ、untrusted なネットワークバイト列の「読み取り・
//! 検証・エラー分類」にのみ責務を限定する（応答メッセージの組み立ては
//! `handshake.rs` が担う）。読み取り API は `Read` トレイトへジェネリックにし、
//! `TcpStream` だけでなく `std::io::Cursor` でも動くようにしてある（ソケットなしの
//! 単体テストのため）。
//!
//! 対応: TASK-68（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-4, WIRE-10。
//! エラーコード写像は `docs/spec/04-behavior/error-format.md` の ERR-2）。
//!
//! 受信データ経路のため `unwrap`/`expect`/添字アクセスを用いず `get()`・
//! `checked_*` で処理する（`.claude/rules/coding-rust.md` P0）。

use std::cell::Cell;
use std::io::{self, Read};
use std::time::Instant;

/// 1 メッセージあたりの長さフィールド（自身の 4 バイトを含む）の上限（WIRE-4）。
/// これを超える宣言長は `TooLarge` として分類し、length 検証の時点で読み取りを
/// 打ち切る（本文を読まない＝アロケーションしない）。
pub const MAX_MESSAGE_LEN: usize = 1024 * 1024;

/// 認証前の最初のパケット（SSLRequest/GSSENCRequest/StartupMessage）に許す上限。
/// spec 値ではなく、未認証ピアに許すアロケーション量を絞る防御的な既定値である
/// （実際の StartupMessage は数百バイト程度で足りるため、通常利用は妨げない）。
pub const MAX_STARTUP_LEN: usize = 32 * 1024;

const _: () = assert!(
    MAX_STARTUP_LEN <= MAX_MESSAGE_LEN,
    "MAX_STARTUP_LEN must not exceed MAX_MESSAGE_LEN"
);

/// StartupMessage の最小長（length 4 バイト + protocol/code 4 バイト）。
pub const MIN_STARTUP_LEN: usize = 8;

/// 型付きメッセージ（type byte の後）の最小長（length 4 バイトのみ）。
pub const MIN_TYPED_MESSAGE_LEN: usize = 4;

/// SQLSTATE `54000`（program_limit_exceeded）。WIRE-4: メッセージ長が
/// `MAX_MESSAGE_LEN` を超過した場合に用いる。値は `engine::error_format::
/// ErrorClass`（SSOT。TASK-152・ERR-2）由来（TASK-153・ERR-1 の分散定数 SSOT 化）。
pub const SQLSTATE_PROGRAM_LIMIT_EXCEEDED: &str =
    engine::error_format::ErrorClass::PayloadTooLarge.wire_code();
/// SQLSTATE `08P01`（protocol_violation）。WIRE-10: 不正フレーム（負の長さ・
/// 最小値未満・StartupMessage 上限超過・型固有の形状違反等）に用いる。値は
/// `engine::error_format::ErrorClass`（SSOT）由来。
pub const SQLSTATE_PROTOCOL_VIOLATION: &str =
    engine::error_format::ErrorClass::ProtocolViolation.wire_code();

/// フレーミング検証で検出したエラーの分類。sqlstate・クライアント向け定型
/// メッセージへの写像はそれぞれ [`FrameError::sqlstate`]・[`FrameError::client_message`]
/// が持つ（呼び出し元の `handshake.rs` はこれを見て応答するかどうかを判断する）。
#[derive(Debug)]
pub enum FrameError {
    /// WIRE-4: 宣言長が `MAX_MESSAGE_LEN` を超過。読み取りは length の 4 バイトの
    /// みで打ち切り、本文は未読・未確保。
    TooLarge { declared: usize, max: usize },
    /// WIRE-10: 負の長さ・最小値未満・StartupMessage 上限超過・型固有の形状違反
    /// （例: Terminate の body 非空）等の構文違反。`&'static str` はログ専用の
    /// 理由でクライアントへは返さない（詳細な違反理由を返すと内部実装の手がかりを
    /// 与えるため fail-closed で定型メッセージのみ返す）。
    Malformed(&'static str),
    /// WIRE-10: 宣言長より実際の送信が短く、`read_exact` が `UnexpectedEof` を
    /// 返した（相手が既に切断している）。応答なしで切断してよい
    /// （サーバー側の異常ではないため呼び出し元は `Ok(())` として扱う）。
    Truncated,
    /// タイムアウト等、上記以外の I/O 異常。呼び出し元でログに残す。
    Io(io::Error),
}

impl FrameError {
    /// クライアントへ返す SQLSTATE。`None` は応答を送らずに切断する種別
    /// （`Truncated`・`Io`）を表す。
    pub fn sqlstate(&self) -> Option<&'static str> {
        match self {
            FrameError::TooLarge { .. } => Some(SQLSTATE_PROGRAM_LIMIT_EXCEEDED),
            FrameError::Malformed(_) => Some(SQLSTATE_PROTOCOL_VIOLATION),
            FrameError::Truncated | FrameError::Io(_) => None,
        }
    }

    /// [`sqlstate`](Self::sqlstate) と同じ分岐を `engine::error_format::ErrorClass`
    /// で返す版。`handshake::respond_and_close` が `crate::error_response::encode`
    /// （`ErrorClass` 入力）へ横断写像を接続するために使う（codex-review P1 指摘
    /// 対応・PR #258。`sqlstate()` の `&str` 定数は `ErrorClass::wire_code()` 由来
    /// のため、写像の分岐自体は本メソッドと重複させず同じ `match` を保つ）。
    pub fn error_class(&self) -> Option<engine::error_format::ErrorClass> {
        match self {
            FrameError::TooLarge { .. } => Some(engine::error_format::ErrorClass::PayloadTooLarge),
            FrameError::Malformed(_) => Some(engine::error_format::ErrorClass::ProtocolViolation),
            FrameError::Truncated | FrameError::Io(_) => None,
        }
    }

    /// クライアントへ返す固定の英語メッセージ（内部理由・違反詳細は含めない）。
    pub fn client_message(&self) -> &'static str {
        match self {
            FrameError::TooLarge { .. } => "message length exceeds the per-message limit",
            FrameError::Malformed(_) => "invalid message frame",
            FrameError::Truncated => "",
            FrameError::Io(_) => "",
        }
    }
}

impl From<io::Error> for FrameError {
    /// `read_exact` が返す `UnexpectedEof`（宣言長より実送信が短い途中切断）は
    /// `Truncated` へ写像する。それ以外の I/O エラー（タイムアウト等）は `Io` の
    /// まま呼び出し元へ伝える。
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(e)
        }
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge { declared, max } => {
                write!(f, "message length {declared} exceeds limit {max}")
            }
            FrameError::Malformed(reason) => write!(f, "malformed frame: {reason}"),
            FrameError::Truncated => write!(f, "truncated frame"),
            FrameError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

thread_local! {
    /// 現在のスレッドで受信中のフレームを受け取り終えるべき期限
    /// （[`FrameDeadlineGuard`] が設定する。`None` は期限なし）。
    static FRAME_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// フレーム（長さフィールド・本文）の受信全体に期限を課すガード（SQL-31・
/// TASK-221。PR #1041 レビュー指摘）。
///
/// `handshake::post_auth_loop` が、明示トランザクションが `Active`（単一ライタを
/// 保持中）の間だけ、型バイトを受信した直後に「持続時間上限（と接続の読み取り
/// タイムアウト）」までの期限で設定する。期限が設定されている間、本モジュールの
/// 長さ・本文の読み取りは期限を過ぎた時点で `TimedOut` の I/O エラーを返す
/// （呼び出し元は応答を送らずに接続を閉じ、`SessionTransaction` の drop で
/// ライタを解放する）。本文を少しずつ送り続けて期限後もライタを保持させる
/// 経路を塞ぐためのもので、読み取り 1 回あたりの待機はソケットの読み取り
/// タイムアウト（呼び出し元が短く設定する）で区切られる。wire-server は
/// 1 接続 1 スレッドのため、スレッドローカルで接続単位の期限になる。drop で
/// 直前の値へ戻す。
pub(crate) struct FrameDeadlineGuard {
    previous: Option<Instant>,
}

impl FrameDeadlineGuard {
    pub(crate) fn set(deadline: Instant) -> Self {
        let previous = FRAME_DEADLINE.with(|d| d.replace(Some(deadline)));
        Self { previous }
    }
}

impl Drop for FrameDeadlineGuard {
    fn drop(&mut self) {
        FRAME_DEADLINE.with(|d| d.set(self.previous));
    }
}

/// `buf` を満たすまで読む。[`FrameDeadlineGuard`] の期限が無ければ
/// `read_exact` そのもの（既存経路とビット同一）。期限がある場合は読み取りの
/// たびに期限を確認し、過ぎていれば `TimedOut` を返す。ソケットの読み取り
/// タイムアウトによる `WouldBlock`／`TimedOut` は期限内なら待機を続ける。
fn read_exact_within_deadline<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<()> {
    let Some(deadline) = FRAME_DEADLINE.with(Cell::get) else {
        return reader.read_exact(buf);
    };
    let mut filled = 0usize;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "frame receive deadline exceeded",
            ));
        }
        let dst = buf
            .get_mut(filled..)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        match reader.read(dst) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                filled = filled
                    .checked_add(n)
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_i32_be<R: Read>(reader: &mut R) -> Result<i32, FrameError> {
    let mut buf = [0u8; 4];
    read_exact_within_deadline(reader, &mut buf)?;
    Ok(i32::from_be_bytes(buf))
}

/// 先頭の length（4 バイト、自身を含む）を読み、`min_total..=max_total` の範囲か
/// 検証したうえで残りのボディを読み取る。SSLRequest/GSSENCRequest/CancelRequest/
/// StartupMessage・型付きメッセージ（`Q`・`p`・`X` 等）はいずれもこの共通フレームに
/// 従う。
///
/// `max_total` が `MAX_MESSAGE_LEN` を超えて指定されても `MAX_MESSAGE_LEN` に
/// 丸める（呼び出し側の誤用で全体上限が緩まらないようにする fail-closed）。
///
/// 検証を通過するまでアロケーションしない（length 検証前に `Vec` を確保しない。
/// coding-rust.md 準拠）。宣言長が `MAX_MESSAGE_LEN` を超えた場合は length の
/// 4 バイトのみを消費した時点で読み取りを打ち切る。
pub fn read_length_prefixed_body<R: Read>(
    reader: &mut R,
    min_total: usize,
    max_total: usize,
) -> Result<Vec<u8>, FrameError> {
    let max_total = max_total.min(MAX_MESSAGE_LEN);
    let total_len = read_i32_be(reader)?;
    if total_len < 0 {
        return Err(FrameError::Malformed("negative message length"));
    }
    let total_len = total_len as usize;
    if total_len > MAX_MESSAGE_LEN {
        return Err(FrameError::TooLarge {
            declared: total_len,
            max: MAX_MESSAGE_LEN,
        });
    }
    if total_len < min_total || total_len > max_total {
        return Err(FrameError::Malformed("message length out of bounds"));
    }
    // total_len は length フィールド自身の 4 バイトを含む。検証を通った total_len
    // は `MAX_MESSAGE_LEN` 以内であることが上で確定しているため、後続の
    // アロケーションも有界。
    let body_len = total_len
        .checked_sub(4)
        .ok_or(FrameError::Malformed("message length below header size"))?;
    let mut body = vec![0u8; body_len];
    read_exact_within_deadline(reader, &mut body)?;
    Ok(body)
}

/// 型付きメッセージの長さフィールド（4 バイト、自身を含む）のみを読み検証する。
/// `read_length_prefixed_body` と同じ分類（`TooLarge`/`Malformed`）を用いるが、
/// 本文は読まない（呼び出し元が本文を解釈せず有界 lingering drain へ委ねる用途、
/// 例: `protocol_dispatch::reject_and_close` 前の未対応メッセージ拒否）。
///
/// 未対応の型バイトであっても、宣言長を検証せずに固定応答（`0A000`）を返すと、
/// 長さフィールド欠落・範囲外の malformed frame まで「未対応機能」として扱って
/// しまい、既存の framing/protocol error 契約（`54000`/`08P01`）を迂回してしまう
/// （レビュー指摘の回帰防止。ポインタ: TASK-71・WIRE-8）。
pub fn validate_typed_message_length_prefix<R: Read>(
    reader: &mut R,
    min_total: usize,
    max_total: usize,
) -> Result<usize, FrameError> {
    let max_total = max_total.min(MAX_MESSAGE_LEN);
    let total_len = read_i32_be(reader)?;
    if total_len < 0 {
        return Err(FrameError::Malformed("negative message length"));
    }
    let total_len = total_len as usize;
    if total_len > MAX_MESSAGE_LEN {
        return Err(FrameError::TooLarge {
            declared: total_len,
            max: MAX_MESSAGE_LEN,
        });
    }
    if total_len < min_total || total_len > max_total {
        return Err(FrameError::Malformed("message length out of bounds"));
    }
    Ok(total_len)
}

/// 指定バイト数だけ読み捨てる（アロケーションせず `io::copy` で `io::sink()` へ
/// 流す）。拡張クエリプロトコルのエラー後の同期回復（Issue #934・WIRE-11。
/// `handshake::post_auth_loop` の `ignore_till_sync` モード）が、後続メッセージの
/// 本文を解釈せず読み飛ばすために使う。`len` は呼び出し元が
/// [`validate_typed_message_length_prefix`] 等で `MAX_MESSAGE_LEN` 以内と
/// 確認済みの値であること（本関数自身は上限を検証しない）。
///
/// `Read::take(len)` に対する `io::copy` は、基底 reader が `len` バイト未満で
/// EOF に達しても `Err` にならず、実際にコピーできたバイト数を返して正常
/// 完了する（`std::io::Take` の仕様）。この戻り値を確認せず捨てると、
/// 宣言された `len` より短い本文で接続が切断された場合に、フレームが
/// 途中で切り詰められた事実を見逃し正規の読み捨てとして受理してしまう
/// （AGENTS.md P0「wire プロトコル入力の未検証処理」・codex P0 指摘・
/// PR #1013）。ここでは実コピー長を `len` と照合し、一致しなければ
/// [`FrameError::Truncated`] へ fail-closed に写像する。
pub fn discard_body<R: Read>(reader: &mut R, len: usize) -> Result<(), FrameError> {
    // フレーム受信期限（[`FrameDeadlineGuard`]）の設定中は、期限を確認しながら
    // 固定長のスタックバッファで読み捨てる（途中切断は `Truncated`）。
    if FRAME_DEADLINE.with(Cell::get).is_some() {
        let mut chunk = [0u8; 8192];
        let mut remaining = len;
        while remaining > 0 {
            let n = remaining.min(chunk.len());
            let dst = chunk
                .get_mut(..n)
                .ok_or(FrameError::Malformed("discard chunk out of range"))?;
            read_exact_within_deadline(reader, dst)?;
            remaining = remaining.saturating_sub(n);
        }
        return Ok(());
    }
    let mut limited = reader.take(len as u64);
    let copied = io::copy(&mut limited, &mut io::sink())?;
    if copied != len as u64 {
        return Err(FrameError::Truncated);
    }
    Ok(())
}

/// 本文 `len` バイトを固定長バッファで読み捨てつつ、PostgreSQL の `String` 型（末尾が単一の NUL で、
/// それより前に NUL を含まない C 文字列）の形をしているかを返す（Issue #1761。同期回復中の
/// CopyFail 本文の検証用。`copy.rs` の CopyFail 検査と同じ構造判定を、アロケートせず有界に行う）。
/// 本文は常に最後まで消費する（形が不正でも呼び出し側が応答して切断するだけでよい）。
/// 途中切断は [`FrameError::Truncated`]。`len == 0`（本文なし）は不正形として `false`。
pub fn discard_c_string_body<R: Read>(reader: &mut R, len: usize) -> Result<bool, FrameError> {
    let mut chunk = [0u8; 8192];
    let mut remaining = len;
    let mut valid = len > 0;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        let dst = chunk
            .get_mut(..n)
            .ok_or(FrameError::Malformed("discard chunk out of range"))?;
        read_exact_within_deadline(reader, dst)?;
        remaining = remaining.saturating_sub(n);
        let filled = chunk
            .get(..n)
            .ok_or(FrameError::Malformed("discard chunk out of range"))?;
        // 末尾バイト（最終チャンクの最後）だけが NUL でなければならず、それ以外の NUL は不正。
        let (body, tail) = if remaining == 0 {
            filled.split_at(n.saturating_sub(1))
        } else {
            (filled, &[][..])
        };
        if body.contains(&0) {
            valid = false;
        }
        if remaining == 0 && tail.first() != Some(&0) {
            valid = false;
        }
    }
    Ok(valid)
}

/// StartupMessage（SSLRequest/GSSENCRequest/CancelRequest を含む、認証前の最初の
/// パケット）を読み取る。`MIN_STARTUP_LEN..=MAX_STARTUP_LEN` の範囲外は
/// `Malformed`（`08P01`）に写像する（WIRE-10。`MAX_MESSAGE_LEN` 超過であっても
/// StartupMessage 段階では起動メッセージ不正の分類とし、WIRE-4 の `54000` とは
/// 区別する）。
pub fn read_startup_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    match read_length_prefixed_body(reader, MIN_STARTUP_LEN, MAX_STARTUP_LEN) {
        Err(FrameError::TooLarge { .. }) => Err(FrameError::Malformed(
            "startup packet exceeds startup limit",
        )),
        other => other,
    }
}

/// 型付きメッセージ（'Q'・'p'・'X' 等）の型バイト 1 バイトを読む。読み取り前に
/// クライアントが正常切断していれば `Ok(None)`（`post_auth_loop` の通常終了）、
/// 型バイトの途中で切断されていれば `Truncated` を返す。
pub fn read_typed_frame_header<R: Read>(reader: &mut R) -> Result<Option<u8>, FrameError> {
    let mut type_byte = [0u8; 1];
    loop {
        match reader.read(&mut type_byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(type_byte[0])),
            // シグナル割り込み等による `Interrupted` は接続断ではないため、
            // `read_exact` 系の他経路（`post_auth_loop` / `read_password_message`）
            // と同様にリトライする（Bugbot 指摘: type byte 読み取りのみ再試行が
            // 抜けていた）。
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// SQL-31・TASK-221: フレーム受信期限が過ぎていれば、長さ・本文の読み取りは
    /// データの有無に関わらず `TimedOut` の I/O エラーになる（期限なしなら従来どおり）。
    #[test]
    fn frame_deadline_in_the_past_times_out_body_reads() {
        let bytes = length_prefixed(9, b"hello");
        let past = Instant::now()
            .checked_sub(std::time::Duration::from_millis(1))
            .unwrap_or_else(Instant::now);
        {
            let _guard = FrameDeadlineGuard::set(past);
            let err = read_length_prefixed_body(&mut Cursor::new(bytes.clone()), 4, 64)
                .expect_err("expired deadline must fail");
            assert!(
                matches!(&err, FrameError::Io(e) if e.kind() == io::ErrorKind::TimedOut),
                "unexpected error: {err:?}"
            );
            let err = discard_body(&mut Cursor::new(bytes.clone()), 4)
                .expect_err("expired deadline must fail");
            assert!(matches!(&err, FrameError::Io(e) if e.kind() == io::ErrorKind::TimedOut));
        }
        // ガードの drop 後は期限なしに戻る。
        let body = read_length_prefixed_body(&mut Cursor::new(bytes), 4, 64).expect("read");
        assert_eq!(body, b"hello");
    }

    /// 期限内なら期限付きの読み取りも期限なしと同じ結果を返す（途中切断も同じ分類）。
    #[test]
    fn frame_deadline_in_the_future_matches_unbounded_reads() {
        let future = Instant::now() + std::time::Duration::from_secs(60);
        let _guard = FrameDeadlineGuard::set(future);
        let body = read_length_prefixed_body(&mut Cursor::new(length_prefixed(9, b"hello")), 4, 64)
            .expect("read");
        assert_eq!(body, b"hello");
        assert!(matches!(
            read_length_prefixed_body(&mut Cursor::new(length_prefixed(9, b"he")), 4, 64),
            Err(FrameError::Truncated)
        ));
        assert!(discard_body(&mut Cursor::new(vec![0u8; 20_000]), 20_000).is_ok());
        assert!(matches!(
            discard_body(&mut Cursor::new(vec![0u8; 10]), 20),
            Err(FrameError::Truncated)
        ));
    }

    fn length_prefixed(total_len: i32, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&total_len.to_be_bytes());
        buf.extend_from_slice(body);
        buf
    }

    /// WIRE-4: 宣言長が上限を超えると `TooLarge` を返し、かつ本文を読まない
    /// （Cursor の position が length の 4 バイトのみで止まっていること＝
    /// 読み取り打ち切りの確認）。
    #[test]
    fn too_large_is_detected_before_allocation() {
        let declared = (MAX_MESSAGE_LEN + 1) as i32;
        let mut cursor = Cursor::new(length_prefixed(declared, &[]));
        let err = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN)
            .expect_err("must be rejected");
        assert!(matches!(err, FrameError::TooLarge { .. }));
        assert_eq!(err.sqlstate(), Some(SQLSTATE_PROGRAM_LIMIT_EXCEEDED));
        assert_eq!(cursor.position(), 4, "body must not be read");
    }

    /// 境界値: `MAX_MESSAGE_LEN` ちょうどは受理されること。
    #[test]
    fn exactly_max_message_len_is_accepted() {
        let body = vec![b'a'; MAX_MESSAGE_LEN - 4];
        let mut cursor = Cursor::new(length_prefixed(MAX_MESSAGE_LEN as i32, &body));
        let result = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN);
        assert!(result.is_ok(), "boundary value must be accepted");
        assert_eq!(result.expect("ok").len(), MAX_MESSAGE_LEN - 4);
    }

    #[test]
    fn negative_length_is_malformed() {
        let mut cursor = Cursor::new(length_prefixed(-1, &[]));
        let err = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN)
            .expect_err("negative length must be rejected");
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some(SQLSTATE_PROTOCOL_VIOLATION));
    }

    #[test]
    fn i32_max_is_too_large() {
        let mut cursor = Cursor::new(length_prefixed(i32::MAX, &[]));
        let err = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN)
            .expect_err("i32::MAX must be rejected");
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[test]
    fn below_min_total_is_malformed() {
        let mut cursor = Cursor::new(length_prefixed(4, &[]));
        let err = read_length_prefixed_body(&mut cursor, 8, MAX_MESSAGE_LEN)
            .expect_err("below min_total must be rejected");
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some(SQLSTATE_PROTOCOL_VIOLATION));
    }

    /// 型固有の形状上限（`max_total`）を全体上限より小さく指定した場合、それを
    /// 超える宣言長は `TooLarge`（54000）ではなく `Malformed`（08P01）側に分類
    /// されること（全体のリソース超過ではなく、その型としての形状違反のため）。
    #[test]
    fn per_type_max_below_global_is_malformed_not_too_large() {
        let mut cursor = Cursor::new(length_prefixed(100, &[0u8; 96]));
        let err = read_length_prefixed_body(&mut cursor, 4, 4)
            .expect_err("must exceed the per-type max_total");
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some(SQLSTATE_PROTOCOL_VIOLATION));
    }

    /// WIRE-10: 宣言長より実送信が短い（途中切断）場合は `Truncated` に写像され、
    /// panic しないこと。
    #[test]
    fn truncated_body_maps_to_truncated() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&64i32.to_be_bytes());
        raw.extend_from_slice(&[0u8; 10]); // 宣言 64（body 60）だが実際は 10 バイトのみ
        let mut cursor = Cursor::new(raw);
        let err = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN)
            .expect_err("truncated body must be rejected");
        assert!(matches!(err, FrameError::Truncated));
        assert_eq!(err.sqlstate(), None);
    }

    /// codex P0 指摘（PR #1013）の回帰防止: `discard_body` は宣言長どおりの
    /// バイト数を読み捨てた場合のみ成功し、`Read::take` が基底 reader の
    /// EOF で切り詰められた場合（宣言長より短い本文で接続が切断された場合）
    /// は `FrameError::Truncated` へ fail-closed に写像する。`io::copy` の
    /// 戻り値（実コピー長）を確認しないと、この切り詰めが正常な読み捨てと
    /// して受理されてしまう。
    #[test]
    fn discard_body_accepts_exact_length() {
        let mut cursor = Cursor::new(vec![0u8; 10]);
        discard_body(&mut cursor, 10).expect("exact length must be accepted");
        assert_eq!(cursor.position(), 10);
    }

    #[test]
    fn discard_body_rejects_short_read_as_truncated() {
        // 宣言長 10 に対し実際に読める本文は 3 バイトしかない
        // （`ignore_till_sync` 中に接続が切断されたケースの再現）。
        let mut cursor = Cursor::new(vec![0u8; 3]);
        let err = discard_body(&mut cursor, 10).expect_err("short read must be rejected");
        assert!(matches!(err, FrameError::Truncated));
        assert_eq!(err.sqlstate(), None);
    }

    #[test]
    fn discard_body_accepts_zero_length() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        discard_body(&mut cursor, 0).expect("zero length must be accepted");
        assert_eq!(cursor.position(), 0);
    }

    #[test]
    fn startup_over_limit_is_malformed() {
        let declared = (MAX_STARTUP_LEN + 1) as i32;
        let mut cursor = Cursor::new(length_prefixed(declared, &[]));
        let err = read_startup_frame(&mut cursor).expect_err("must be rejected");
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some(SQLSTATE_PROTOCOL_VIOLATION));
    }

    #[test]
    fn startup_below_min_is_malformed() {
        let mut cursor = Cursor::new(length_prefixed(4, &[]));
        let err = read_startup_frame(&mut cursor).expect_err("must be rejected");
        assert!(matches!(err, FrameError::Malformed(_)));
    }

    #[test]
    fn typed_header_clean_eof_is_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_typed_frame_header(&mut cursor).expect("clean EOF is not an error");
        assert_eq!(result, None);
    }

    /// Bugbot 指摘: `read_typed_frame_header` はシグナル割り込み相当の
    /// `ErrorKind::Interrupted` を再試行し、接続を切断しないこと
    /// （`post_auth_loop` / `read_password_message` の `read_exact` 経路と同様の
    /// リトライ挙動を type byte 読み取りにも揃える）。
    #[test]
    fn typed_header_retries_on_interrupted() {
        struct InterruptOnceThenByte {
            interrupted: bool,
        }

        impl Read for InterruptOnceThenByte {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if let Some(slot) = buf.first_mut() {
                    *slot = b'Q';
                }
                Ok(1)
            }
        }

        let mut reader = InterruptOnceThenByte { interrupted: false };
        let result =
            read_typed_frame_header(&mut reader).expect("interrupted read must be retried");
        assert_eq!(result, Some(b'Q'));
    }

    /// `max_total` に `MAX_MESSAGE_LEN` を超える値を渡しても、全体上限で丸められる
    /// こと（呼び出し側の誤用で上限が緩まらない fail-closed の確認）。
    #[test]
    fn max_total_larger_than_global_is_clamped() {
        let declared = (MAX_MESSAGE_LEN + 1) as i32;
        let mut cursor = Cursor::new(length_prefixed(declared, &[]));
        let err = read_length_prefixed_body(&mut cursor, 4, MAX_MESSAGE_LEN + 1_000_000)
            .expect_err("must still be rejected as too large");
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }
}
