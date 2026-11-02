//! PL/pgSQL の `RAISE` が生む「利用者が指定したエラー」と「クライアントへの通知」の型
//! （Issue #2227・SQL-70）。
//!
//! 役割:
//! - [`RaisedError`]: `RAISE EXCEPTION` が作るエラーの中身（利用者指定の SQLSTATE・文言・
//!   DETAIL・HINT・呼び出し文脈）。`sql::allowlist::SqlSurfaceError::RaisedException` が保持し、
//!   wire-server の `error_response::encode_sql_error` が `C`／`M`／`D`／`H`／`W` フィールドへ
//!   写す。`wire_code()`（`ErrorClass`）は常に静的な `P0001` で、利用者のコードは
//!   [`RaisedError::sqlstate`] に分けて持つ（`ErrorClass` を動的な値で汚さない）。
//! - [`Notice`]・[`StatementNotices`]: `RAISE NOTICE/INFO/WARNING` の通知を 1 文の実行区間だけ
//!   現スレッドへ溜める RAII 区間。wire-server が文の実行を [`StatementNotices::enter`] で包み、
//!   応答の前に [`StatementNotices::finish`] で取り出して `NoticeResponse`（'N'）へ写す。区間の
//!   外（HTTP／NoSQL 表層）での [`push_notice`] は何もしない（NoSQL の応答へ通知を含めない）。
//!
//! 安全性: 利用者テキストは生成時に NUL を除去し、長さを切り詰める（フレーム破壊と無制限確保の
//! 防止）。通知は件数・総バイト数に上限があり、超過分は捨てる。他テナントのデータは載せない
//! （載るのは呼び出し元の RLS の下で評価された値だけ）。

use std::cell::RefCell;

/// 1 件のメッセージ（文言・DETAIL・HINT）の最大バイト数。
pub const MAX_NOTICE_FIELD_BYTES: usize = 4096;
/// 1 文の実行区間で溜める通知の最大件数。
pub const MAX_NOTICES_PER_STATEMENT: usize = 1024;
/// 1 文の実行区間で溜める通知の総バイト数の上限。
pub const MAX_NOTICE_TOTAL_BYTES: usize = 1024 * 1024;
/// `W`（呼び出し文脈）に積む行の最大数。
pub const MAX_CONTEXT_LINES: usize = 64;

/// 通知の水準（クライアントへ送る 3 水準だけ。`DEBUG`／`LOG` は送らず捨てる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Notice,
    Warning,
}

impl NoticeLevel {
    /// `S`（severity）フィールドの値。
    pub fn severity(self) -> &'static str {
        match self {
            NoticeLevel::Info => "INFO",
            NoticeLevel::Notice => "NOTICE",
            NoticeLevel::Warning => "WARNING",
        }
    }
}

/// クライアントへ送る通知 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub level: NoticeLevel,
    /// 5 文字の SQLSTATE（既定は `NOTICE`／`INFO` が `00000`、`WARNING` が `01000`）。
    pub sqlstate: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
}

/// `RAISE EXCEPTION` が作るエラーの中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaisedError {
    /// 利用者指定（または既定 `P0001`）の 5 文字の SQLSTATE（検証済み）。
    sqlstate: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    /// 呼び出し文脈（`PL/pgSQL function ... line N at RAISE` 等。内側の枠が先頭）。
    context: Vec<String>,
}

impl RaisedError {
    /// 利用者テキストを安全化して作る（NUL 除去・長さ切り詰め）。`sqlstate` は呼び出し側
    /// （`plpgsql`）が [`is_valid_sqlstate`] で検証済みの値だけを渡す。
    pub(crate) fn new(
        sqlstate: String,
        message: &str,
        detail: Option<&str>,
        hint: Option<&str>,
    ) -> Self {
        Self {
            sqlstate,
            message: sanitize_text(message),
            detail: detail.map(sanitize_text),
            hint: hint.map(sanitize_text),
            context: Vec::new(),
        }
    }

    /// 利用者指定の SQLSTATE（5 文字）。
    pub fn sqlstate(&self) -> &str {
        &self.sqlstate
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// 呼び出し文脈（`W` フィールドの値。無ければ `None`）。行は改行で連結する。
    pub fn context(&self) -> Option<String> {
        (!self.context.is_empty()).then(|| self.context.join("\n"))
    }

    /// 呼び出し文脈へ 1 行追記する（行数の上限を超える分は捨てる）。
    pub(crate) fn push_context(&mut self, line: &str) {
        if self.context.len() < MAX_CONTEXT_LINES {
            self.context.push(sanitize_text(line));
        }
    }
}

/// 5 文字・`[0-9A-Z]` か。
pub(crate) fn is_valid_sqlstate(s: &str) -> bool {
    s.len() == 5
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

/// NUL を取り除き、[`MAX_NOTICE_FIELD_BYTES`] 以内（文字境界）へ切り詰める。
pub(crate) fn sanitize_text(s: &str) -> String {
    // 全文の複製を作らず、NUL を除きながら上限に達した時点で打ち切る（長大入力でも確保は上限内）。
    let mut out = String::with_capacity(s.len().min(MAX_NOTICE_FIELD_BYTES));
    for c in s.chars() {
        if c == '\0' {
            continue;
        }
        if out.len().saturating_add(c.len_utf8()) > MAX_NOTICE_FIELD_BYTES {
            break;
        }
        out.push(c);
    }
    out
}

/// 文字列を [`MAX_NOTICE_FIELD_BYTES`] 以内（文字境界）へその場で切り詰め、余剰容量を解放する。
/// RAISE の引数・書式展開結果を保持前に抑え、長大な値の繰り返しによるメモリ枯渇を防ぐ。
pub(crate) fn clamp_field(s: &mut String) {
    if s.len() <= MAX_NOTICE_FIELD_BYTES {
        return;
    }
    let mut end = MAX_NOTICE_FIELD_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.shrink_to_fit();
}

#[derive(Default)]
struct Sink {
    notices: Vec<Notice>,
    bytes: usize,
    dropped: bool,
}

thread_local! {
    /// 現スレッドの通知区間（入れ子の区間は外側を退避して内側を張る）。
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// 通知 1 件を現スレッドの区間へ溜める。区間が無ければ何もしない。上限超過分は捨てる。
pub(crate) fn push_notice(n: Notice) {
    SINK.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(sink) = guard.as_mut() else {
            return;
        };
        let size = n
            .message
            .len()
            .saturating_add(n.detail.as_ref().map_or(0, String::len))
            .saturating_add(n.hint.as_ref().map_or(0, String::len));
        let next = sink.bytes.saturating_add(size);
        if sink.notices.len() >= MAX_NOTICES_PER_STATEMENT || next > MAX_NOTICE_TOTAL_BYTES {
            sink.dropped = true;
            return;
        }
        sink.bytes = next;
        sink.notices.push(n);
    });
}

/// 文 1 つの実行を包む通知の区間（RAII）。`finish` を呼ばずに落とした場合も区間を必ず戻す。
pub struct StatementNotices {
    saved: Option<Sink>,
    active: bool,
}

impl StatementNotices {
    /// 区間を張る（直前の区間があれば退避し、`finish`／drop で戻す）。
    pub fn enter() -> Self {
        let saved = SINK.with(|s| s.borrow_mut().replace(Sink::default()));
        Self {
            saved,
            active: true,
        }
    }

    /// 区間を閉じて溜まった通知を返す。上限超過で捨てた分があれば、末尾に固定文言の
    /// `WARNING` を 1 件足す。
    pub fn finish(mut self) -> Vec<Notice> {
        self.take()
    }

    fn take(&mut self) -> Vec<Notice> {
        if !self.active {
            return Vec::new();
        }
        self.active = false;
        let mine = SINK.with(|s| std::mem::replace(&mut *s.borrow_mut(), self.saved.take()));
        let Some(sink) = mine else {
            return Vec::new();
        };
        let mut out = sink.notices;
        if sink.dropped {
            out.push(Notice {
                level: NoticeLevel::Warning,
                sqlstate: "01000".to_string(),
                message: "some notices were dropped because the notice limit was exceeded"
                    .to_string(),
                detail: None,
                hint: None,
            });
        }
        out
    }
}

impl Drop for StatementNotices {
    fn drop(&mut self) {
        let _ = self.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(msg: &str) -> Notice {
        Notice {
            level: NoticeLevel::Notice,
            sqlstate: "00000".to_string(),
            message: msg.to_string(),
            detail: None,
            hint: None,
        }
    }

    #[test]
    fn push_outside_scope_is_noop() {
        push_notice(n("x"));
        let s = StatementNotices::enter();
        assert!(s.finish().is_empty());
    }

    #[test]
    fn collects_and_restores_outer_scope() {
        let outer = StatementNotices::enter();
        push_notice(n("a"));
        let inner = StatementNotices::enter();
        push_notice(n("b"));
        let got = inner.finish();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "b");
        push_notice(n("c"));
        let got = outer.finish();
        assert_eq!(
            got.iter().map(|x| x.message.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[test]
    fn count_limit_drops_and_warns_once() {
        let s = StatementNotices::enter();
        for _ in 0..(MAX_NOTICES_PER_STATEMENT + 10) {
            push_notice(n("x"));
        }
        let got = s.finish();
        assert_eq!(got.len(), MAX_NOTICES_PER_STATEMENT + 1);
        assert_eq!(got.last().map(|x| x.level), Some(NoticeLevel::Warning));
    }

    #[test]
    fn sanitize_removes_nul_and_truncates_on_char_boundary() {
        assert_eq!(sanitize_text("a\0b"), "ab");
        let long = "あ".repeat(MAX_NOTICE_FIELD_BYTES);
        let out = sanitize_text(&long);
        assert!(out.len() <= MAX_NOTICE_FIELD_BYTES);
        assert!(out.chars().all(|c| c == 'あ'));
    }

    #[test]
    fn sqlstate_validation() {
        assert!(is_valid_sqlstate("P0001"));
        assert!(is_valid_sqlstate("23505"));
        assert!(!is_valid_sqlstate("p0001"));
        assert!(!is_valid_sqlstate("P001"));
        assert!(!is_valid_sqlstate("P00012"));
    }

    #[test]
    fn context_lines_are_bounded() {
        let mut e = RaisedError::new("P0001".into(), "m", None, None);
        for _ in 0..(MAX_CONTEXT_LINES + 5) {
            e.push_context("line");
        }
        assert_eq!(
            e.context().map(|c| c.lines().count()),
            Some(MAX_CONTEXT_LINES)
        );
    }
}
