//! `ts_headline` 相当の純粋関数（Issue #2125・親 #2123。対象ビヘイビア SEARCH-14 の
//! ポインタ: `docs/spec/04-behavior/search.md`）。
//!
//! 責務境界: 「本文」「検索語」「オプション文字列」から、一致箇所を `StartSel`／`StopSel` で
//! 囲んだ抜粋文字列を返す。一致範囲は同じ親モジュールの [`super::match_span`]（#2124）が
//! 求め、本モジュールは語数の数え方・断片の選び方・出力の組み立てを担う。`sql::string_fn` の
//! `ts_headline` ラッパー（`sql::udf_call` から呼ばれる）が `HeadlineError` を `wire_code` へ
//! 写像する。storage／catalog／RLS とは結線しない（渡された 1 行の値だけを見る純関数。
//! 可視行・列権限の検査は呼び出し側の投影経路が先に済ませている）。
//!
//! 名前・オプション・既定値・エスケープしない方針は PostgreSQL 17 の `ts_headline` の公開
//! 挙動に合わせる（選択処理は公開挙動から再実装したもので、ソースの移植ではない）。
//! HTML などのエスケープはしない。表示する側で行う責務は利用者にある。
//!
//! # 単位モデル
//!
//! 本文を「ASCII 単語」「CJK 1 文字」「それ以外の区切り」の重ならない単位列へ分ける。
//! 分割規則は索引・クエリと共通の `walk_tokens` を唯一の真実源とし、バイグラムは捨てる。
//! 語数・`ShortWord` の長さは「単位」「文字数」で数える（CJK は 1 文字が 1 語）。
//! 検索語の解釈は当面 `MatchTerms::from_query`（任意語）で、フレーズ・除外・前方一致の
//! パーサ（#2113〜#2116）が入ったら cover（一致を含む単位範囲）の作り方をここで拡張する。
//!
//! # 資源上限（fail-closed）
//!
//! 入力（本文・検索語・一致数）は [`MatchSpanLimits`]、出力は [`HeadlineLimits`]、選択処理の
//! 仕事量は [`MAX_HEADLINE_STEPS`] の決定的な歩数で縛り、超過は切り詰めずエラーにする。
//! 受信データの経路のため `unwrap`／`expect`／添字アクセスを使わない。

use super::match_span::{match_spans, MatchSpan, MatchSpanError, MatchSpanLimits, MatchTerms};
use crate::headline_limits::HeadlineLimits;

/// 選択処理の歩数の上限（すべての内側ループが 1 歩ずつ消費する）。最悪入力でも概ね 1 秒未満で
/// 打ち切るための決定的な仕事量上限で、超過は [`HeadlineError::WorkLimitExceeded`]。
pub const MAX_HEADLINE_STEPS: u64 = 1 << 26;

/// 区切り文字列（`StartSel`／`StopSel`／`FragmentDelimiter`）の最大バイト長。
const MAX_DELIMITER_BYTES: usize = 32_767;

/// `ts_headline` のエラー。メッセージに本文・検索語・オプションの値は含めない（固定文言）。
///
/// 後続（検索語パーサ等）で variant を足すため `#[non_exhaustive]`
/// （`docs/design/error-enum-non-exhaustive-policy.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadlineError {
    /// 未知のオプション名（`22023`）。
    UnknownOption,
    /// オプションの値が不正（`22023`）。`detail` は固定文言。
    InvalidValue { detail: &'static str },
    /// オプション文字列の書式エラー（`42601`）。
    MalformedOptions,
    /// 一致範囲の算出エラー（本文・検索語・一致数の上限超過）。
    Span(MatchSpanError),
    /// 出力が `max_output_bytes` を超える（切り詰めない。`54000`）。
    OutputTooLong { max: usize },
    /// `MaxFragments` が上限を超える（`54000`）。
    TooManyFragments { max: usize },
    /// 選択処理の歩数が [`MAX_HEADLINE_STEPS`] を使い切った（`54000`）。
    WorkLimitExceeded,
    /// 作業領域を確保できなかった（`54000`）。
    ResourceExhausted,
}

impl std::fmt::Display for HeadlineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOption => f.write_str("unrecognized headline parameter"),
            Self::InvalidValue { detail } => f.write_str(detail),
            Self::MalformedOptions => f.write_str("invalid parameter list format"),
            Self::Span(e) => write!(f, "{e}"),
            Self::OutputTooLong { max } => {
                write!(f, "ts_headline result is too long (max {max} bytes)")
            }
            Self::TooManyFragments { max } => {
                write!(f, "MaxFragments exceeds the configured limit (max {max})")
            }
            Self::WorkLimitExceeded => f.write_str("ts_headline work limit exceeded"),
            Self::ResourceExhausted => f.write_str("ts_headline could not allocate working memory"),
        }
    }
}

impl std::error::Error for HeadlineError {}

impl From<MatchSpanError> for HeadlineError {
    fn from(e: MatchSpanError) -> Self {
        Self::Span(e)
    }
}

/// 解釈済みの `ts_headline` オプション。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlineOptions {
    /// 一致語の直前に挿入する文字列（既定 `<b>`）。
    pub start_sel: String,
    /// 一致語の直後に挿入する文字列（既定 `</b>`）。
    pub stop_sel: String,
    /// 断片あたりの最大語数（既定 35）。
    pub max_words: i32,
    /// 断片あたりの最小語数（既定 15）。
    pub min_words: i32,
    /// これ以下の長さの語は断片の端にしない（既定 3）。
    pub short_word: i32,
    /// 本文全体を出力するか（既定 false）。
    pub highlight_all: bool,
    /// 断片数（0 は単一断片方式。既定 0）。
    pub max_fragments: i32,
    /// 断片の区切り（既定 ` ... `）。
    pub fragment_delimiter: String,
}

impl Default for HeadlineOptions {
    fn default() -> Self {
        Self {
            start_sel: "<b>".to_string(),
            stop_sel: "</b>".to_string(),
            max_words: 35,
            min_words: 15,
            short_word: 3,
            highlight_all: false,
            max_fragments: 0,
            fragment_delimiter: " ... ".to_string(),
        }
    }
}

/// C の `isspace` と同じ 6 文字。
fn is_opt_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// `key = value` のカンマ／空白区切り列を `(key, value)` の列へ解く状態機械（入力長に線形）。
///
/// 値は引用符なし・`'…'`・`E'…'`・`"…"` の 4 通り。終端が「キー待ち」か「引用符なしの値の途中」
/// 以外なら書式エラー。
fn parse_deflist(input: &str) -> Result<Vec<(String, String)>, HeadlineError> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        WaitKey,
        InKey,
        InQKey,
        WaitEq,
        WaitValue,
        InValue,
        InQValue(char),
        AfterQuote,
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut st = St::WaitKey;
    let mut key = String::new();
    let mut val = String::new();
    let mut it = input.chars().peekable();
    let bad = || HeadlineError::MalformedOptions;
    while let Some(c) = it.next() {
        match st {
            St::WaitKey => {
                if is_opt_space(c) || c == ',' {
                    continue;
                } else if c == '"' {
                    key.clear();
                    st = St::InQKey;
                } else if c == '=' {
                    return Err(bad());
                } else {
                    key.clear();
                    key.push(c);
                    st = St::InKey;
                }
            }
            St::InKey => {
                if is_opt_space(c) {
                    st = St::WaitEq;
                } else if c == '=' {
                    st = St::WaitValue;
                } else {
                    key.push(c);
                }
            }
            St::InQKey => {
                if c == '"' {
                    if it.peek() == Some(&'"') {
                        it.next();
                        key.push('"');
                    } else {
                        st = St::WaitEq;
                    }
                } else {
                    key.push(c);
                }
            }
            St::WaitEq => {
                if is_opt_space(c) {
                    continue;
                } else if c == '=' {
                    st = St::WaitValue;
                } else {
                    return Err(bad());
                }
            }
            St::WaitValue => {
                if is_opt_space(c) {
                    continue;
                }
                val.clear();
                if c == '\'' || c == '"' {
                    st = St::InQValue(c);
                } else if c == 'E' && it.peek() == Some(&'\'') {
                    it.next();
                    st = St::InQValue('\'');
                } else {
                    // 先頭の文字はカンマでも値の一部（PostgreSQL と同じ）。
                    val.push(c);
                    st = St::InValue;
                }
            }
            St::InValue => {
                if is_opt_space(c) || c == ',' {
                    out.push((std::mem::take(&mut key), std::mem::take(&mut val)));
                    st = St::WaitKey;
                } else {
                    val.push(c);
                }
            }
            St::InQValue(q) => {
                if c == '\\' && it.peek() == Some(&'\\') {
                    // バックスラッシュは `\\` の 2 連だけが 1 文字になる（他は文字どおり）。
                    it.next();
                    val.push('\\');
                } else if c == q {
                    if it.peek() == Some(&q) {
                        it.next();
                        val.push(q);
                    } else {
                        out.push((std::mem::take(&mut key), std::mem::take(&mut val)));
                        st = St::AfterQuote;
                    }
                } else {
                    val.push(c);
                }
            }
            St::AfterQuote => {
                // 閉じ引用符の直後は空白かカンマだけを許す（`'a'b` は書式エラー）。
                if is_opt_space(c) || c == ',' {
                    st = St::WaitKey;
                } else {
                    return Err(bad());
                }
            }
        }
    }
    match st {
        St::WaitKey | St::AfterQuote => {}
        St::InValue => out.push((key, val)),
        _ => return Err(bad()),
    }
    Ok(out)
}

/// 整数オプションの値を i32 として解釈する。前後の空白、符号、10 進、桁間の `_`、
/// `0x`／`0o`／`0b` の接頭辞を受理する（PostgreSQL の整数入力に合わせる）。
fn parse_i32(v: &str) -> Result<i32, HeadlineError> {
    let invalid = HeadlineError::InvalidValue {
        detail: "invalid value for headline integer parameter",
    };
    let t = v.trim_matches(is_opt_space);
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) = {
        let lower = rest.get(..2).map(str::to_ascii_lowercase);
        match lower.as_deref() {
            Some("0x") => (16u32, rest.get(2..).unwrap_or("")),
            Some("0o") => (8, rest.get(2..).unwrap_or("")),
            Some("0b") => (2, rest.get(2..).unwrap_or("")),
            _ => (10, rest),
        }
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return Err(invalid);
    }
    let mut acc: i64 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(radix).ok_or_else(|| invalid.clone())?;
        acc = acc
            .checked_mul(i64::from(radix))
            .and_then(|a| a.checked_add(i64::from(d)))
            .ok_or_else(|| invalid.clone())?;
        if acc > i64::from(i32::MAX) + 1 {
            return Err(invalid);
        }
    }
    let signed = if neg { -acc } else { acc };
    i32::try_from(signed).map_err(|_| invalid)
}

impl HeadlineOptions {
    /// オプション文字列を解釈し、PostgreSQL と同じ条件・順序で検証する。
    ///
    /// 同じキーが複数あれば後勝ち。名前の大小文字は区別しない。`HighlightAll` が真のときは
    /// 語数などの整合性検査を行わない（PostgreSQL と同じ）。
    pub fn parse(options: Option<&str>) -> Result<Self, HeadlineError> {
        let mut o = Self::default();
        let Some(text) = options else {
            return Ok(o);
        };
        let too_long = HeadlineError::InvalidValue {
            detail: "headline delimiter string is too long",
        };
        for (k, v) in parse_deflist(text)? {
            let k = k.to_ascii_lowercase();
            match k.as_str() {
                "startsel" => o.start_sel = v,
                "stopsel" => o.stop_sel = v,
                "fragmentdelimiter" => o.fragment_delimiter = v,
                "maxwords" => o.max_words = parse_i32(&v)?,
                "minwords" => o.min_words = parse_i32(&v)?,
                "shortword" => o.short_word = parse_i32(&v)?,
                "maxfragments" => o.max_fragments = parse_i32(&v)?,
                "highlightall" => {
                    let l = v.to_ascii_lowercase();
                    o.highlight_all = matches!(l.as_str(), "1" | "on" | "true" | "t" | "y" | "yes");
                }
                _ => return Err(HeadlineError::UnknownOption),
            }
        }
        for s in [&o.start_sel, &o.stop_sel, &o.fragment_delimiter] {
            if s.len() > MAX_DELIMITER_BYTES {
                return Err(too_long);
            }
        }
        if !o.highlight_all {
            if o.min_words >= o.max_words {
                return Err(HeadlineError::InvalidValue {
                    detail: "MinWords should be less than MaxWords",
                });
            }
            if o.min_words <= 0 {
                return Err(HeadlineError::InvalidValue {
                    detail: "MinWords should be positive",
                });
            }
            if o.short_word < 0 {
                return Err(HeadlineError::InvalidValue {
                    detail: "ShortWord should be >= 0",
                });
            }
            if o.max_fragments < 0 {
                return Err(HeadlineError::InvalidValue {
                    detail: "MaxFragments should be >= 0",
                });
            }
        }
        Ok(o)
    }
}

/// 単位の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// ASCII 単語または CJK 1 文字。
    Word,
    /// 数字だけの ASCII 単語（断片の端にならない）。
    Digits,
    /// 空白・句読点・制御文字などの区切り（語数に数えない）。
    Separator,
}

/// 本文を隙間なく覆う重ならない単位 1 個。
#[derive(Debug, Clone, Copy)]
struct Unit {
    start: u32,
    end: u32,
    kind: Kind,
    /// 元の部分文字列の Unicode スカラー値の数。
    char_len: u32,
    /// 属する統合済み一致範囲の番号。一致しなければ [`NO_SPAN`]。
    span: u32,
}

const NO_SPAN: u32 = u32::MAX;

/// 範囲外参照の保険（添字アクセスを使わないための番兵）。語ではない区切りとして振る舞う。
const SENTINEL: Unit = Unit {
    start: 0,
    end: 0,
    kind: Kind::Separator,
    char_len: 0,
    span: NO_SPAN,
};

impl Unit {
    fn interesting(&self) -> bool {
        self.span != NO_SPAN
    }
    fn nonword(&self) -> bool {
        self.kind == Kind::Separator
    }
}

/// 歩数の予算。内側ループごとに [`Budget::tick`] を呼ぶ。
struct Budget(u64);

impl Budget {
    fn tick(&mut self) -> Result<(), HeadlineError> {
        if self.0 == 0 {
            return Err(HeadlineError::WorkLimitExceeded);
        }
        self.0 -= 1;
        Ok(())
    }
}

fn try_vec<T: Clone>(len: usize, fill: T) -> Result<Vec<T>, HeadlineError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len)
        .map_err(|_| HeadlineError::ResourceExhausted)?;
    v.resize(len, fill);
    Ok(v)
}

fn try_push<T>(v: &mut Vec<T>, item: T) -> Result<(), HeadlineError> {
    v.try_reserve(1)
        .map_err(|_| HeadlineError::ResourceExhausted)?;
    v.push(item);
    Ok(())
}

/// 本文から単位列を作る。`spans` は `match_spans` の結果（昇順・非重複）。
fn build_units(text: &str, spans: &[MatchSpan]) -> Result<Vec<Unit>, HeadlineError> {
    // 1. walk_tokens の ASCII 単語と CJK 1 文字だけを採る（範囲は元本文の文字境界）。
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut oom = false;
    super::walk_tokens(text, false, |token, r| {
        let mut chars = token.chars();
        let first = chars.next();
        let is_cjk = first.is_some_and(super::is_cjk_char);
        if is_cjk && chars.next().is_some() {
            return true; // バイグラムは捨てる
        }
        if words.try_reserve(1).is_err() {
            oom = true;
            return false;
        }
        match words.last_mut() {
            // 防御: 直前と重なる範囲は直前の単位に併合する。
            Some(prev) if r.start < prev.1 => prev.1 = prev.1.max(r.end),
            _ => words.push((r.start, r.end)),
        }
        true
    });
    if oom {
        return Err(HeadlineError::ResourceExhausted);
    }
    // 2. 単語の間の隙間を区切り単位にして、隙間なく覆う。
    let mut units: Vec<Unit> = Vec::new();
    let mut pos = 0usize;
    let mut sp = 0usize; // spans の走査位置
    let to_u32 = |v: usize| u32::try_from(v).map_err(|_| HeadlineError::ResourceExhausted);
    let mut push_unit = |units: &mut Vec<Unit>,
                         start: usize,
                         end: usize,
                         is_word: bool|
     -> Result<(), HeadlineError> {
        if start >= end {
            return Ok(());
        }
        let slice = text.get(start..end).unwrap_or("");
        let kind = if !is_word {
            Kind::Separator
        } else if slice.bytes().all(|b| b.is_ascii_digit()) {
            Kind::Digits
        } else {
            Kind::Word
        };
        // 区間 [start,end) と交わる一致範囲を探す（両方とも昇順なので 2 ポインタ）。
        while let Some(s) = spans.get(sp) {
            if s.end <= start {
                sp += 1;
            } else {
                break;
            }
        }
        let span = match spans.get(sp) {
            Some(s) if s.start < end && is_word => to_u32(sp)?,
            _ => NO_SPAN,
        };
        let char_len = u32::try_from(slice.chars().count()).unwrap_or(u32::MAX);
        try_push(
            units,
            Unit {
                start: to_u32(start)?,
                end: to_u32(end)?,
                kind,
                char_len,
                span,
            },
        )
    };
    for (s, e) in words {
        push_unit(&mut units, pos, s, false)?;
        push_unit(&mut units, s, e, true)?;
        pos = e;
    }
    push_unit(&mut units, pos, text.len(), false)?;
    Ok(units)
}

/// 選択処理の作業状態。`ins[i]` は単位 `i` を出力に含めるか。
struct Selector<'a> {
    units: &'a [Unit],
    n: i64,
    short_word: i64,
    min_words: i64,
    max_words: i64,
    budget: Budget,
}

impl Selector<'_> {
    fn u(&self, i: i64) -> Unit {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.units.get(i).copied())
            .unwrap_or(SENTINEL)
    }

    /// 断片の端にしてはいけない単位か（一致語ではなく、区切り・数字・短い語）。
    fn bad_end(&self, i: i64) -> bool {
        let u = self.u(i);
        (u.kind != Kind::Word || i64::from(u.char_len) <= self.short_word) && !u.interesting()
    }
}

/// 一致範囲ごとの cover（単位番号の閉区間）。
fn build_covers(units: &[Unit], span_count: usize) -> Result<Vec<(i64, i64)>, HeadlineError> {
    let mut covers: Vec<(i64, i64)> = try_vec(span_count, (i64::MAX, -1))?;
    for (i, u) in units.iter().enumerate() {
        if !u.interesting() {
            continue;
        }
        let idx = usize::try_from(u.span).unwrap_or(usize::MAX);
        let i = i64::try_from(i).unwrap_or(i64::MAX);
        if let Some(c) = covers.get_mut(idx) {
            c.0 = c.0.min(i);
            c.1 = c.1.max(i);
        }
    }
    covers.retain(|c| c.1 >= 0);
    Ok(covers)
}

/// 単一断片方式（`MaxFragments == 0`）の選択。選んだ区間 `[begin, end]` を返す。
fn select_single(
    sel: &mut Selector<'_>,
    covers: &[(i64, i64)],
) -> Result<(i64, i64), HeadlineError> {
    let (mut best_b, mut best_e, mut best_len) = (-1i64, -1i64, -1i64);
    let mut best_good = false;
    for &(p, q) in covers {
        sel.budget.tick()?;
        // cover の語数を max_words まで数える。
        let mut curlen = 0i64;
        let mut poslen = 0i64;
        let mut pose = p;
        let mut i = p;
        while i <= q && curlen < sel.max_words {
            sel.budget.tick()?;
            let u = sel.u(i);
            if !u.nonword() {
                curlen += 1;
            }
            if u.interesting() {
                poslen += 1;
            }
            pose = i;
            i += 1;
        }
        let mut posb = p;
        if curlen < sel.max_words {
            // 良い終端を探して後ろへ伸ばす。
            i -= 1;
            while i < sel.n && curlen < sel.max_words {
                sel.budget.tick()?;
                if i != q {
                    let u = sel.u(i);
                    if !u.nonword() {
                        curlen += 1;
                    }
                    if u.interesting() {
                        poslen += 1;
                    }
                }
                pose = i;
                if !sel.bad_end(i) && curlen >= sel.min_words {
                    break;
                }
                i += 1;
            }
            if curlen < sel.min_words {
                // 良い始端を探して前へ伸ばす。
                i = p - 1;
                while i >= 0 {
                    sel.budget.tick()?;
                    let u = sel.u(i);
                    if !u.nonword() {
                        curlen += 1;
                    }
                    if u.interesting() {
                        poslen += 1;
                    }
                    if curlen >= sel.max_words {
                        break;
                    }
                    if !sel.bad_end(i) && curlen >= sel.min_words {
                        break;
                    }
                    i -= 1;
                }
                posb = i.max(0);
            }
        } else {
            // cover が max_words 以上: 終端側を詰める。
            if i > q {
                i = q;
            }
            while curlen > sel.min_words {
                sel.budget.tick()?;
                if !sel.bad_end(i) {
                    break;
                }
                let u = sel.u(i);
                if !u.nonword() {
                    curlen -= 1;
                }
                if u.interesting() {
                    poslen -= 1;
                }
                pose = i - 1;
                i -= 1;
            }
        }
        let good = !sel.bad_end(pose);
        if best_len < 0 || poslen > best_len || (poslen == best_len && good && !best_good) {
            best_b = posb;
            best_e = pose;
            best_len = poslen;
            best_good = good;
        }
    }
    if best_len < 0 {
        // 一致が無い: 先頭から min_words 語。
        let mut curlen = 0i64;
        let mut pose = 0i64;
        let mut i = 0i64;
        while i < sel.n && curlen < sel.min_words {
            sel.budget.tick()?;
            if !sel.u(i).nonword() {
                curlen += 1;
            }
            pose = i;
            i += 1;
        }
        best_b = 0;
        best_e = pose;
    }
    Ok((best_b, best_e))
}

/// 断片方式の 1 候補。
#[derive(Clone, Copy)]
struct Cand {
    start: i64,
    end: i64,
    curlen: i64,
    poslen: i64,
    chosen: bool,
    excluded: bool,
}

/// cover を `max_words` 以下の断片に分ける 1 回分（端は一致語に揃える）。
fn next_fragment(
    sel: &mut Selector<'_>,
    startpos: &mut i64,
    endpos: &mut i64,
) -> Result<(i64, i64), HeadlineError> {
    let mut i = *startpos;
    while i <= *endpos {
        sel.budget.tick()?;
        *startpos = i;
        if sel.u(i).interesting() {
            break;
        }
        i += 1;
    }
    let mut curlen = 0i64;
    let mut poslen = 0i64;
    i = *startpos;
    while i <= *endpos && curlen < sel.max_words {
        sel.budget.tick()?;
        let u = sel.u(i);
        if !u.nonword() {
            curlen += 1;
        }
        if u.interesting() {
            poslen += 1;
        }
        i += 1;
    }
    if *endpos > i {
        *endpos = i;
        while i >= *startpos {
            sel.budget.tick()?;
            *endpos = i;
            let u = sel.u(i);
            if u.interesting() {
                break;
            }
            if !u.nonword() {
                curlen -= 1;
            }
            i -= 1;
        }
    }
    Ok((curlen, poslen))
}

/// 断片方式（`MaxFragments != 0`）の選択。出力に含める単位を `ins` に立てる。
fn select_fragments(
    sel: &mut Selector<'_>,
    covers: &[(i64, i64)],
    max_fragments: i64,
    ins: &mut [bool],
) -> Result<(), HeadlineError> {
    let mut cands: Vec<Cand> = Vec::new();
    for &(p, q) in covers {
        let mut startpos = p;
        let mut endpos = q;
        while startpos <= endpos {
            sel.budget.tick()?;
            let (curlen, poslen) = next_fragment(sel, &mut startpos, &mut endpos)?;
            try_push(
                &mut cands,
                Cand {
                    start: startpos,
                    end: endpos,
                    curlen,
                    poslen,
                    chosen: false,
                    excluded: false,
                },
            )?;
            startpos = endpos + 1;
            endpos = q;
        }
    }
    let mut num_f = 0i64;
    let mut f = 0i64;
    while f < max_fragments {
        f += 1;
        let (mut maxitems, mut minwords, mut min_i) = (0i64, i64::MAX, usize::MAX);
        for (idx, c) in cands.iter().enumerate() {
            sel.budget.tick()?;
            if !c.chosen
                && !c.excluded
                && (maxitems < c.poslen || (maxitems == c.poslen && minwords > c.curlen))
            {
                maxitems = c.poslen;
                minwords = c.curlen;
                min_i = idx;
            }
        }
        let Some(cur) = cands.get(min_i).copied() else {
            break;
        };
        let (mut startpos, mut endpos, mut curlen) = (cur.start, cur.end, cur.curlen);
        if curlen < sel.max_words {
            let maxstretch = (sel.max_words - curlen) / 2;
            let mut stretch = 0i64;
            let mut posmarker = startpos;
            let mut i = startpos - 1;
            while i >= 0 && stretch < maxstretch && !in_at(ins, i) {
                sel.budget.tick()?;
                if !sel.u(i).nonword() {
                    curlen += 1;
                    stretch += 1;
                }
                posmarker = i;
                i -= 1;
            }
            i = posmarker;
            while i < startpos && sel.bad_end(i) {
                sel.budget.tick()?;
                if !sel.u(i).nonword() {
                    curlen -= 1;
                }
                i += 1;
            }
            startpos = i;
            posmarker = endpos;
            i = endpos + 1;
            while i < sel.n && curlen < sel.max_words && !in_at(ins, i) {
                sel.budget.tick()?;
                if !sel.u(i).nonword() {
                    curlen += 1;
                }
                posmarker = i;
                i += 1;
            }
            i = posmarker;
            while i > endpos && sel.bad_end(i) {
                sel.budget.tick()?;
                if !sel.u(i).nonword() {
                    curlen -= 1;
                }
                i -= 1;
            }
            endpos = i;
        }
        if let Some(c) = cands.get_mut(min_i) {
            c.chosen = true;
            c.start = startpos;
            c.end = endpos;
            c.curlen = curlen;
        }
        let mut k = startpos;
        while k <= endpos {
            sel.budget.tick()?;
            if let Some(slot) = usize::try_from(k).ok().and_then(|k| ins.get_mut(k)) {
                *slot = true;
            }
            k += 1;
        }
        num_f += 1;
        for (idx, c) in cands.iter_mut().enumerate() {
            sel.budget.tick()?;
            if idx != min_i
                && ((c.start >= startpos && c.start <= endpos)
                    || (c.end >= startpos && c.end <= endpos))
            {
                c.excluded = true;
            }
        }
    }
    if num_f <= 0 {
        let mut curlen = 0i64;
        let mut i = 0i64;
        while i < sel.n && curlen < sel.min_words {
            sel.budget.tick()?;
            if !sel.u(i).nonword() {
                curlen += 1;
            }
            if let Some(slot) = usize::try_from(i).ok().and_then(|k| ins.get_mut(k)) {
                *slot = true;
            }
            i += 1;
        }
    }
    Ok(())
}

fn in_at(ins: &[bool], i: i64) -> bool {
    usize::try_from(i)
        .ok()
        .and_then(|i| ins.get(i).copied())
        .unwrap_or(false)
}

/// 出力バッファへ追記する。追記前に上限を検査し、超えるなら確保せずに `Err`（切り詰めない）。
fn push_checked(out: &mut String, piece: &str, max: usize) -> Result<(), HeadlineError> {
    let total = out
        .len()
        .checked_add(piece.len())
        .ok_or(HeadlineError::OutputTooLong { max })?;
    if total > max {
        return Err(HeadlineError::OutputTooLong { max });
    }
    out.try_reserve(piece.len())
        .map_err(|_| HeadlineError::ResourceExhausted)?;
    out.push_str(piece);
    Ok(())
}

/// `text` の中で `query` に一致する箇所を強調した抜粋を返す（SEARCH-14 ポインタ）。
///
/// `options` は `StartSel=<em>, StopSel=</em>` 形式の文字列（`None` は指定なし）。エスケープは
/// しない。上限超過は切り詰めずエラー。
pub fn ts_headline(
    text: &str,
    query: &str,
    options: Option<&str>,
    limits: &HeadlineLimits,
) -> Result<String, HeadlineError> {
    let opts = HeadlineOptions::parse(options)?;
    let max_fragments = usize::try_from(opts.max_fragments.max(0)).unwrap_or(usize::MAX);
    if max_fragments > limits.max_fragments() {
        return Err(HeadlineError::TooManyFragments {
            max: limits.max_fragments(),
        });
    }
    let span_limits = MatchSpanLimits::default();
    let terms = MatchTerms::from_query(query)?;
    let spans = match_spans(text, &terms, &span_limits)?;
    let units = build_units(text, &spans)?;
    let n = i64::try_from(units.len()).unwrap_or(i64::MAX);
    let mut ins: Vec<bool> = try_vec(units.len(), false)?;
    let mut sel = Selector {
        units: &units,
        n,
        short_word: i64::from(opts.short_word),
        min_words: i64::from(opts.min_words),
        max_words: i64::from(opts.max_words),
        budget: Budget(MAX_HEADLINE_STEPS),
    };
    if opts.highlight_all && opts.max_fragments == 0 {
        ins.fill(true);
    } else {
        let covers = build_covers(&units, spans.len())?;
        if opts.max_fragments == 0 {
            let (b, e) = select_single(&mut sel, &covers)?;
            let mut k = b.max(0);
            while k <= e {
                sel.budget.tick()?;
                if let Some(slot) = usize::try_from(k).ok().and_then(|k| ins.get_mut(k)) {
                    *slot = true;
                }
                k += 1;
            }
        } else {
            select_fragments(&mut sel, &covers, i64::from(opts.max_fragments), &mut ins)?;
        }
    }
    render(text, &units, &ins, &opts, limits.max_output_bytes())
}

/// 選ばれた単位を、断片の区切りと強調を付けて文字列にする。
fn render(
    text: &str,
    units: &[Unit],
    ins: &[bool],
    opts: &HeadlineOptions,
    max: usize,
) -> Result<String, HeadlineError> {
    let mut out = String::new();
    let mut prev_in = false;
    let mut open_span = NO_SPAN;
    let mut wrote_any = false;
    for (idx, u) in units.iter().enumerate() {
        let is_in = ins.get(idx).copied().unwrap_or(false);
        if !is_in {
            if open_span != NO_SPAN {
                push_checked(&mut out, &opts.stop_sel, max)?;
                open_span = NO_SPAN;
            }
            prev_in = false;
            continue;
        }
        if !prev_in && wrote_any {
            push_checked(&mut out, &opts.fragment_delimiter, max)?;
        }
        let want = if u.kind == Kind::Separator {
            NO_SPAN
        } else {
            u.span
        };
        if open_span != NO_SPAN && open_span != want {
            push_checked(&mut out, &opts.stop_sel, max)?;
            open_span = NO_SPAN;
        }
        if want != NO_SPAN && open_span == NO_SPAN {
            push_checked(&mut out, &opts.start_sel, max)?;
            open_span = want;
        }
        let piece = text
            .get(usize::try_from(u.start).unwrap_or(0)..usize::try_from(u.end).unwrap_or(0))
            .unwrap_or("");
        push_checked(&mut out, piece, max)?;
        prev_in = true;
        wrote_any = true;
    }
    if open_span != NO_SPAN {
        push_checked(&mut out, &opts.stop_sel, max)?;
    }
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn hl(text: &str, query: &str, opts: Option<&str>) -> String {
        ts_headline(text, query, opts, &HeadlineLimits::default()).expect("headline ok")
    }

    fn opts(s: &str) -> HeadlineOptions {
        HeadlineOptions::parse(Some(s)).expect("options ok")
    }

    #[test]
    fn defaults_follow_postgresql_public_values() {
        let d = HeadlineOptions::parse(None).expect("default");
        assert_eq!(d, HeadlineOptions::default());
        assert_eq!((d.start_sel.as_str(), d.stop_sel.as_str()), ("<b>", "</b>"));
        assert_eq!((d.max_words, d.min_words, d.short_word), (35, 15, 3));
        assert!(!d.highlight_all);
        assert_eq!(d.max_fragments, 0);
        assert_eq!(d.fragment_delimiter, " ... ");
        // 空のオプション文字列は「指定なし」と同じ。
        assert_eq!(HeadlineOptions::parse(Some("")).expect("empty"), d);
    }

    #[test]
    fn option_names_are_case_insensitive_and_last_wins() {
        let o = opts("MAXWORDS=9, minwords=2, MaxWords=7");
        assert_eq!((o.max_words, o.min_words), (7, 2));
    }

    #[test]
    fn value_quoting_forms() {
        assert_eq!(opts("StartSel=<i>").start_sel, "<i>");
        assert_eq!(opts("StartSel='it''s'").start_sel, "it's");
        assert_eq!(opts("StartSel=E'a\\\\b'").start_sel, "a\\b");
        // 単独のバックスラッシュは文字どおり（`\n` は改行にならない）。
        assert_eq!(opts("StartSel=E'a\\nb'").start_sel, "a\\nb");
        assert_eq!(
            opts("StartSel=\"<em class=\"\"x\"\">\"").start_sel,
            "<em class=\"x\">"
        );
        // 小文字の e' は E 文字列ではなく、引用符なしの値。
        assert_eq!(opts("StartSel=e'a'").start_sel, "e'a'");
        assert_eq!(opts("\"StartSel\"='x' , StopSel = y").stop_sel, "y");
        assert_eq!(opts("StartSel=,StopSel=z").start_sel, ",StopSel=z");
        assert_eq!(opts("StartSel='',StopSel=z").stop_sel, "z");
    }

    #[test]
    fn malformed_option_lists_are_rejected_as_format_errors() {
        for bad in [
            "MaxWords",
            "MaxWords=",
            "=5",
            "foo bar=1",
            "StartSel='unterminated",
            "StartSel=\"unterminated",
            "StartSel='a'b",
            "StartSel=\"a\"b",
            "StartSel=E'a\\'b'",
            "MaxWords 5",
        ] {
            assert_eq!(
                HeadlineOptions::parse(Some(bad)),
                Err(HeadlineError::MalformedOptions),
                "{bad}"
            );
        }
    }

    #[test]
    fn unknown_option_and_bad_values_are_invalid_parameter() {
        assert_eq!(
            HeadlineOptions::parse(Some("Bogus=1")),
            Err(HeadlineError::UnknownOption)
        );
        // 制御文字だけのキーも未知のオプション。
        assert_eq!(
            HeadlineOptions::parse(Some("\x01=1")),
            Err(HeadlineError::UnknownOption)
        );
        for bad in [
            "MaxWords=abc",
            "MaxWords=1.5",
            "MaxWords=1e1",
            "MaxWords=2147483648",
            "MaxWords=-2147483649",
            "MaxWords=1__0",
            "MaxWords=_1",
            "MaxWords=1_",
            "MaxWords=0x",
            "MaxWords=--1",
        ] {
            assert!(
                matches!(
                    HeadlineOptions::parse(Some(bad)),
                    Err(HeadlineError::InvalidValue { .. })
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn integer_grammar_matches_postgresql() {
        for (v, expected) in [
            ("+20", 20),
            ("0020", 20),
            ("0x14", 20),
            ("0X14", 20),
            ("0o24", 20),
            ("0b10100", 20),
            ("1_0", 10),
            ("'  20 '", 20),
            ("\"2_0\"", 20),
        ] {
            let o = opts(&format!("MaxWords={v}, MinWords=1"));
            assert_eq!(o.max_words, expected, "{v}");
        }
        assert_eq!(opts("MaxWords=2147483647").max_words, i32::MAX);
        assert_eq!(opts("MaxWords=3, MinWords=1, ShortWord=0").short_word, 0);
    }

    #[test]
    fn consistency_checks_follow_postgresql_order_and_highlight_all_skips_them() {
        for bad in [
            "MinWords=15, MaxWords=15",
            "MinWords=0",
            "MinWords=-3",
            "ShortWord=-1",
            "MaxFragments=-1",
        ] {
            assert!(
                matches!(
                    HeadlineOptions::parse(Some(bad)),
                    Err(HeadlineError::InvalidValue { .. })
                ),
                "{bad}"
            );
        }
        // HighlightAll が真なら検査しない。
        assert!(
            HeadlineOptions::parse(Some("HighlightAll=true, MinWords=0, ShortWord=-1")).is_ok()
        );
        // 真偽値は 1/on/true/t/y/yes のみ真。それ以外はエラーにせず偽。
        for t in ["1", "on", "TRUE", "t", "Y", "yes"] {
            assert!(opts(&format!("HighlightAll={t}")).highlight_all, "{t}");
        }
        for f in ["0", "off", "false", "maybe", "2", ""] {
            assert!(!opts(&format!("HighlightAll='{f}'")).highlight_all, "{f}");
        }
    }

    #[test]
    fn delimiter_strings_are_bounded() {
        let ok = format!("StartSel={}", "x".repeat(MAX_DELIMITER_BYTES));
        assert!(HeadlineOptions::parse(Some(&ok)).is_ok());
        let ng = format!("FragmentDelimiter={}", "x".repeat(MAX_DELIMITER_BYTES + 1));
        assert!(matches!(
            HeadlineOptions::parse(Some(&ng)),
            Err(HeadlineError::InvalidValue { .. })
        ));
    }

    #[test]
    fn basic_highlight_does_not_escape_markup() {
        assert_eq!(
            hl("the quick brown fox", "fox", None),
            "the quick brown <b>fox</b>"
        );
        assert_eq!(
            hl("<i>fox</i> & dog", "fox", None),
            "<i><b>fox</b></i> & dog"
        );
        assert_eq!(hl("foo foo bar", "foo", None), "<b>foo</b> <b>foo</b> bar");
        assert_eq!(hl("", "fox", None), "");
        assert_eq!(hl("   ", "fox", None), "   ");
    }

    #[test]
    fn control_characters_are_separators_and_kept_verbatim() {
        let text = "tab\there fox\x01ctl\x7fend\0nul\r\nline";
        let out = hl(text, "fox", None);
        assert_eq!(out, "tab\there <b>fox</b>\x01ctl\x7fend\0nul\r\nline");
        // オプション文字列中の制御文字: isspace の 6 文字は区切り、それ以外は値の一部。
        let o = opts("StartSel=\x1b[1m\u{b}StopSel=\x1b[0m");
        assert_eq!(
            (o.start_sel.as_str(), o.stop_sel.as_str()),
            ("\x1b[1m", "\x1b[0m")
        );
        let out = hl("a fox", "fox", Some("StartSel=\x1b[1m, StopSel=\x1b[0m"));
        assert_eq!(out, "a \x1b[1mfox\x1b[0m");
    }

    #[test]
    fn lowercasing_length_changes_do_not_shift_ranges() {
        // `İ`（小文字化で 2 文字）・ケルビン記号・絵文字を含んでも、強調は元の文字で囲む。
        let text = "İstanbul K fox \u{1F600} smile";
        let out = hl(text, "fox", None);
        assert_eq!(out, "İstanbul K <b>fox</b> \u{1F600} smile");
        let out = hl("İ fox", "i", None);
        assert!(out.starts_with("<b>İ</b>"), "{out}");
    }

    #[test]
    fn cjk_matches_group_into_one_pair_and_particles_count_as_words() {
        assert_eq!(hl("東京都に行く", "東京都", None), "<b>東京都</b>に行く");
        // 助詞 1 文字の検索語は単独では一致しない。
        assert_eq!(hl("東京に行く", "に", None), "東京に行く");
        // 語数は文字単位: MinWords=3 は先頭 3 文字。
        assert_eq!(
            hl("一二三四五六", "なし", Some("MinWords=3, MaxWords=9")),
            "一二三"
        );
    }

    #[test]
    fn cjk_fragment_edges_stay_on_char_boundaries() {
        let text = "吾輩は猫である。名前はまだ無い。どこで生れたかとんと見当がつかぬ。";
        for (q, o) in [
            ("猫", "MaxWords=4, MinWords=2, ShortWord=0"),
            (
                "名前",
                "MaxFragments=2, MaxWords=3, MinWords=1, ShortWord=0",
            ),
            ("見当", "MaxWords=6, MinWords=3, ShortWord=1"),
        ] {
            let out = hl(text, q, Some(o));
            let plain = out.replace("<b>", "").replace("</b>", "");
            for part in plain.split(" ... ") {
                assert!(text.contains(part), "{part:?}");
            }
        }
    }

    #[test]
    fn adjacent_cjk_spans_are_wrapped_separately() {
        // 接するだけの一致範囲は併合されないため別々に囲む（#2124 の仕様）。
        let out = hl("東京京都", "東京 京都", Some("MinWords=4, MaxWords=8"));
        assert_eq!(out, "<b>東京</b><b>京都</b>");
        assert!(
            out.replace("<b>", "").replace("</b>", "") == "東京京都",
            "{out}"
        );
    }

    #[test]
    fn output_limit_is_enforced_before_allocation_and_never_truncates() {
        let limits = HeadlineLimits::new(12, 4);
        assert_eq!(
            ts_headline("a fox", "fox", None, &limits).expect("exact"),
            "a <b>fox</b>"
        );
        let limits = HeadlineLimits::new(11, 4);
        assert_eq!(
            ts_headline("a fox", "fox", None, &limits),
            Err(HeadlineError::OutputTooLong { max: 11 })
        );
    }

    #[test]
    fn max_fragments_over_the_configured_limit_is_rejected() {
        let limits = HeadlineLimits::new(1 << 20, 2);
        assert!(ts_headline(
            "a fox",
            "fox",
            Some("MaxFragments=2, MaxWords=3, MinWords=1"),
            &limits
        )
        .is_ok());
        assert_eq!(
            ts_headline(
                "a fox",
                "fox",
                Some("MaxFragments=3, MaxWords=3, MinWords=1"),
                &limits
            ),
            Err(HeadlineError::TooManyFragments { max: 2 })
        );
    }

    #[test]
    fn input_limits_surface_as_span_errors() {
        let big = "a ".repeat(600_000);
        assert!(matches!(
            ts_headline(&big, "a", None, &HeadlineLimits::default()),
            Err(HeadlineError::Span(MatchSpanError::TextTooLong { .. }))
        ));
        let many = "a ".repeat(70_000);
        assert!(matches!(
            ts_headline(&many, "a", None, &HeadlineLimits::default()),
            Err(HeadlineError::Span(MatchSpanError::TooManySpans { .. }))
        ));
        let long_query = "word ".repeat(10_000);
        assert!(matches!(
            ts_headline("a word", &long_query, None, &HeadlineLimits::default()),
            Err(HeadlineError::Span(MatchSpanError::QueryTooLong { .. }))
        ));
    }

    #[test]
    fn worst_case_work_is_cut_off_by_the_step_budget() {
        // 6 万件の一致が散らばる 1 MiB 弱の本文で、MinWords を最大にすると cover ごとに本文末まで
        // 前方走査する。歩数予算で決定的に打ち切り、切り詰めた結果は返さない。
        let mut text = String::new();
        while text.len() < 900_000 {
            text.push_str("a bb bb bb bb bb bb bb bb ");
        }
        let started = std::time::Instant::now();
        let r = ts_headline(
            &text,
            "a",
            Some("MaxWords=2147483647, MinWords=2147483646"),
            &HeadlineLimits::default(),
        );
        assert_eq!(r, Err(HeadlineError::WorkLimitExceeded));
        assert!(started.elapsed().as_secs() < 10, "{:?}", started.elapsed());
    }

    #[test]
    fn highlight_all_returns_the_whole_body_without_work_on_covers() {
        let text = "x ".repeat(1000) + "fox";
        let out = hl(&text, "fox", Some("HighlightAll=true"));
        assert_eq!(out, format!("{}<b>fox</b>", "x ".repeat(1000)));
    }

    /// 擬似乱数（xorshift）で作った入力でも、出力は常に UTF-8 として正しく、強調タグを除くと
    /// 本文の部分文字列の連結（断片の区切りで分割）になる。
    #[test]
    fn random_inputs_keep_output_a_concatenation_of_body_substrings() {
        let alphabet: Vec<&str> = vec![
            "a",
            "b",
            "fox",
            "dog",
            " ",
            " ",
            ", ",
            ". ",
            "\t",
            "東",
            "京",
            "都",
            "の",
            "ー",
            "İ",
            "K",
            "\u{1F600}",
            "9",
            "_",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for case in 0..2000 {
            let len = usize::try_from(next() % 60).unwrap_or(0);
            let mut text = String::new();
            for _ in 0..len {
                text.push_str(alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()]);
            }
            let query = format!(
                "{} {}",
                alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()],
                alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()]
            );
            let max_words = 2 + next() % 12;
            let min_words = 1 + next() % (max_words - 1);
            let frags = next() % 4;
            let o = format!(
                "MaxWords={max_words}, MinWords={min_words}, MaxFragments={frags}, ShortWord={}, FragmentDelimiter=|",
                next() % 3
            );
            let out = ts_headline(&text, &query, Some(&o), &HeadlineLimits::default())
                .unwrap_or_else(|e| panic!("case {case}: {e}"));
            let plain = out.replace("<b>", "").replace("</b>", "");
            assert_eq!(
                out.matches("<b>").count(),
                out.matches("</b>").count(),
                "case {case}"
            );
            for part in plain.split('|') {
                assert!(text.contains(part), "case {case}: {part:?} in {text:?}");
            }
        }
    }
}
