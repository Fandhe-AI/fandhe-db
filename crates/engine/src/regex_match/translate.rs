//! PG の正規表現構文（ARE。C 照合の意味論）を `regex` クレートの構文へ写す字句変換器
//! （Issue #1786。ポインタ: `docs/spec/04-behavior/` SQL-55）。
//!
//! 親モジュール [`super`]（`regex_match`）の `CompiledRegex::compile` だけが呼ぶ非公開層。
//! パターンは非信頼の SQL 入力なので、**未検証の部分文字列を `regex` 構文へ連結しない**:
//! 自前のトークナイザが 1 文字ずつ解析し、リテラルとブラケット要素は個別にエスケープして
//! 出力する（`&&`・`--`・`~~`・入れ子の `[` が集合演算として解釈されるのを防ぐ）。
//! 走査は再帰せず（括弧の深さは明示カウンタ）、スタックを消費しない。
//!
//! 判定の方針（fail-closed）:
//!
//! - PG でも構文誤りになるもの → [`TranslateError::Syntax`]（`2201B`）
//! - PG は受理するが本実装が写像しないもの（後方参照・先読み・照合要素・一部の埋め込み
//!   オプション等）→ [`TranslateError::Unsupported`]（`0A000`）
//! - 括弧の入れ子が [`MAX_REGEX_NEST_DEPTH`] 超 → [`TranslateError::TooDeep`]（`54000`）
//!
//! 大文字小文字を区別しない場合は `regex` の `(?i)`（Unicode の simple case folding）を使わず、
//! ASCII 英字だけを自前で展開する。C 照合の PG は ASCII だけを畳み込むため
//! （`docs/design/like-wildcard-patterns.md` の ILIKE と同じ方針）。

use crate::regex_limits::MAX_REGEX_NEST_DEPTH;

/// 変換の失敗種別。親モジュールが `RegexError` へ写す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TranslateError {
    /// PG の構文としても不正（`2201B`）。
    Syntax,
    /// PG は受理するが本実装は写像しない（`0A000`）。
    Unsupported,
    /// 括弧の入れ子が上限超過（`54000`）。
    TooDeep,
}

type Res<T> = Result<T, TranslateError>;

/// 直前トークンの種別。量指定子の対象判定（PG は対象無し・二重指定を構文誤りにする）に使う。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    /// 先頭・`(` 直後・`|` 直後（量指定子の対象が無い）。
    Nothing,
    /// 量指定できる原子（リテラル・`.`・ブラケット・グループ・クラス）。
    Atom,
    /// 量指定できない零幅（`^`・`$`・`\A`・`\Z`・`\y`・`\m` 等）。
    Anchor,
    /// 量指定子そのもの（直後の量指定子は二重指定として拒否する）。
    Quantifier,
}

/// ブラケット内の 1 要素。
enum Item {
    /// 単一文字または範囲（両端を含む）。
    Range(char, char),
    /// 出力済みの ASCII クラス断片（`0-9` 等。ブラケット内にそのまま置ける形）。
    Raw(&'static str),
    /// 否定形の ASCII クラス（`\D`・`\S`・`\W`）。入れ子ブラケットとして出力する。
    Negated(&'static str),
}

/// 先頭の director と埋め込みオプションの解析結果。
struct Prefix {
    /// 先頭 director `***=`（以降すべてリテラル）または埋め込みオプション `q`。
    literal: bool,
    /// 大文字小文字を区別しない（`~*` または `(?i)`。`(?c)` は区別する側へ上書き）。
    insensitive: bool,
    /// パターン本体の開始位置（文字単位）。
    start: usize,
}

const DIGITS: &str = "0-9";
const SPACES: &str = "\\x{9}-\\x{d}\\x{20}";
const WORD: &str = "0-9A-Za-z_";

struct Cursor<'a> {
    chars: &'a [char],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos.checked_add(n)?).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos = self.pos.saturating_add(1);
        Some(c)
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(i, c)| self.peek_at(i) == Some(c))
    }
}

/// ブラケット・リテラル出力で使う 1 文字の安全な表現。ASCII 英数字と非 ASCII の通常文字は
/// そのまま、それ以外は `\x{HEX}` にして `regex` のメタ文字として解釈される余地を無くす。
fn push_char(out: &mut String, c: char) {
    if c.is_ascii_alphanumeric() || (!c.is_ascii() && !c.is_control() && !c.is_whitespace()) {
        out.push(c);
    } else {
        out.push_str(&format!("\\x{{{:x}}}", u32::from(c)));
    }
}

/// 原子としてのリテラル 1 文字。大文字小文字を区別しない場合は ASCII 英字を `[Aa]` に展開する。
fn push_literal(out: &mut String, c: char, insensitive: bool) {
    if insensitive && c.is_ascii_alphabetic() {
        out.push('[');
        out.push(c.to_ascii_uppercase());
        out.push(c.to_ascii_lowercase());
        out.push(']');
    } else {
        push_char(out, c);
    }
}

fn hex_val(c: char) -> Option<u32> {
    c.to_digit(16)
}

/// 先頭の director（`***:`／`***=`）と埋め込みオプション（`(?letters)`）を解析する。
fn parse_prefix(chars: &[char], base_insensitive: bool) -> Res<Prefix> {
    let mut cur = Cursor { chars, pos: 0 };
    let mut literal = false;
    let mut insensitive = base_insensitive;
    if cur.starts_with("***") {
        match cur.peek_at(3) {
            Some(':') => cur.pos = 4,
            Some('=') => {
                cur.pos = 4;
                literal = true;
            }
            // `***` に続くのが director でない並びは PG でも構文誤り。
            _ => return Err(TranslateError::Syntax),
        }
    }
    if !literal && cur.starts_with("(?") {
        if let Some(c) = cur.peek_at(2) {
            if c.is_ascii_alphabetic() {
                cur.pos = cur.pos.saturating_add(2);
                loop {
                    match cur.bump() {
                        Some(')') => break,
                        Some('c') => insensitive = false,
                        Some('i') => insensitive = true,
                        Some('q') => literal = true,
                        // 受理される既定・同義のオプション。
                        Some('s') | Some('t') => {}
                        // PG は受理するが意味論が異なる（BRE/ERE・newline-sensitive・拡張構文等）。
                        Some('b' | 'e' | 'm' | 'n' | 'p' | 'w' | 'x') => {
                            return Err(TranslateError::Unsupported)
                        }
                        // 未知の文字・`)` 欠落は PG でも構文誤り。
                        _ => return Err(TranslateError::Syntax),
                    }
                }
            }
        }
    }
    Ok(Prefix {
        literal,
        insensitive,
        start: cur.pos,
    })
}

/// PG の POSIX クラス名 → ASCII 範囲断片（C 照合の意味論。`cntrl` のみ PG の実測で
/// U+007F〜U+009F まで含む）。未知の名前は `None`。
fn posix_class(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "A-Za-z",
        "alnum" => "0-9A-Za-z",
        "blank" => "\\x{9}\\x{20}",
        "cntrl" => "\\x{0}-\\x{1f}\\x{7f}-\\x{9f}",
        "digit" => "0-9",
        "graph" => "\\x{21}-\\x{7e}",
        "lower" => "a-z",
        "print" => "\\x{20}-\\x{7e}",
        "punct" => "\\x{21}-\\x{2f}\\x{3a}-\\x{40}\\x{5b}-\\x{60}\\x{7b}-\\x{7e}",
        "space" => SPACES,
        "upper" => "A-Z",
        "xdigit" => "0-9A-Fa-f",
        "word" => WORD,
        _ => return None,
    })
}

/// 数値エスケープの値を `char` へ。NUL・サロゲート・範囲外は本実装が写像しない。
fn scalar(v: Option<u32>) -> Res<char> {
    match v.filter(|&n| n != 0).and_then(char::from_u32) {
        Some(c) => Ok(c),
        None => Err(TranslateError::Unsupported),
    }
}

/// `\x`・`\u`・`\U` の値を読む（`\` と種別文字は消費済み）。`\x` は 1 桁以上の 16 進数を
/// 貪欲に読み（PG と同じ）、桁あふれは `None`。`\u`／`\U` はちょうど 4／8 桁。
fn read_hex(cur: &mut Cursor<'_>, kind: char) -> Res<Option<u32>> {
    let (min, max) = match kind {
        'u' => (4usize, 4usize),
        'U' => (8, 8),
        _ => (1, usize::MAX),
    };
    let mut value: Option<u32> = Some(0);
    let mut n = 0usize;
    while n < max {
        let Some(d) = cur.peek().and_then(hex_val) else {
            break;
        };
        cur.pos = cur.pos.saturating_add(1);
        n = n.saturating_add(1);
        value = value
            .and_then(|v| v.checked_mul(16))
            .and_then(|v| v.checked_add(d));
    }
    if n < min {
        return Err(TranslateError::Syntax);
    }
    Ok(value)
}

/// 文字を表すエスケープ（`\a \b \e \f \n \r \t \v \cX \xh+ \uhhhh \Uhhhhhhhh`、および
/// 英数字以外の `\k`）を解釈する。文字エスケープでなければ `Ok(None)`（`\` と `c` は未消費に
/// せず、呼び出し側が先に `c` を消費している前提で、`c` の種別だけ判定する）。
fn char_escape(cur: &mut Cursor<'_>, c: char) -> Res<Option<char>> {
    let ch = match c {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        'c' => {
            // `\cX`: X の下位 5 ビットの制御文字。X が無い・非 ASCII は写像しない。
            match cur.bump() {
                Some(x) if x.is_ascii() => scalar(Some(u32::from(x) & 0x1f))?,
                Some(_) => return Err(TranslateError::Unsupported),
                None => return Err(TranslateError::Syntax),
            }
        }
        'x' | 'u' | 'U' => scalar(read_hex(cur, c)?)?,
        c if !c.is_ascii_alphanumeric() => c,
        _ => return Ok(None),
    };
    Ok(Some(ch))
}

/// ブラケット内の範囲（折り畳み前）を大文字小文字を区別しない形へ広げる。ASCII 英字の
/// 区間と交わる部分に対応する反対ケースの区間を足す。
fn fold_ranges(items: &mut Vec<Item>) {
    let mut extra = Vec::new();
    for it in items.iter() {
        if let Item::Range(lo, hi) = it {
            let (lo, hi) = (u32::from(*lo), u32::from(*hi));
            let (a, b) = (lo.max(u32::from('A')), hi.min(u32::from('Z')));
            if a <= b {
                if let (Some(x), Some(y)) = (char::from_u32(a + 32), char::from_u32(b + 32)) {
                    extra.push(Item::Range(x, y));
                }
            }
            let (a, b) = (lo.max(u32::from('a')), hi.min(u32::from('z')));
            if a <= b {
                if let (Some(x), Some(y)) = (char::from_u32(a - 32), char::from_u32(b - 32)) {
                    extra.push(Item::Range(x, y));
                }
            }
        }
    }
    items.extend(extra);
}

/// ブラケット式（`[` を消費済みの位置から）を解析して `out` へ出力する。
fn bracket(cur: &mut Cursor<'_>, out: &mut String, insensitive: bool) -> Res<()> {
    let negated = cur.peek() == Some('^');
    if negated {
        cur.pos = cur.pos.saturating_add(1);
    }
    let mut items: Vec<Item> = Vec::new();
    let mut first = true;
    // 直前要素が範囲・クラスで、直後の `-` が範囲の続きにならない（構文誤り）状態。
    let mut closed_range = false;
    loop {
        let Some(c) = cur.bump() else {
            return Err(TranslateError::Syntax);
        };
        if c == ']' && !first {
            break;
        }
        first = false;
        // 単一端点（範囲の始点になりうる文字）を決める。クラス要素は即 push して続行。
        let start: char = match c {
            '[' => match cur.peek() {
                Some('.') | Some('=') => return Err(TranslateError::Unsupported),
                Some(':') => {
                    cur.pos = cur.pos.saturating_add(1);
                    let mut name = String::new();
                    loop {
                        match cur.bump() {
                            Some(':') if cur.peek() == Some(']') => {
                                cur.pos = cur.pos.saturating_add(1);
                                break;
                            }
                            Some(ch) if ch.is_ascii_alphabetic() || ch == '<' || ch == '>' => {
                                name.push(ch)
                            }
                            _ => return Err(TranslateError::Syntax),
                        }
                    }
                    let Some(frag) = posix_class(&name) else {
                        return Err(TranslateError::Syntax);
                    };
                    items.push(Item::Raw(frag));
                    if insensitive && (name == "upper" || name == "lower") {
                        items.push(Item::Raw("A-Za-z"));
                    }
                    closed_range = true;
                    // クラス直後の `-`（末尾以外）は構文誤り。
                    if cur.peek() == Some('-') && cur.peek_at(1) != Some(']') {
                        return Err(TranslateError::Syntax);
                    }
                    continue;
                }
                _ => '[',
            },
            '\\' => {
                let Some(e) = cur.bump() else {
                    return Err(TranslateError::Syntax);
                };
                match e {
                    'd' => {
                        items.push(Item::Raw(DIGITS));
                        closed_range = true;
                        if cur.peek() == Some('-') && cur.peek_at(1) != Some(']') {
                            return Err(TranslateError::Syntax);
                        }
                        continue;
                    }
                    's' => {
                        items.push(Item::Raw(SPACES));
                        closed_range = true;
                        if cur.peek() == Some('-') && cur.peek_at(1) != Some(']') {
                            return Err(TranslateError::Syntax);
                        }
                        continue;
                    }
                    'w' => {
                        items.push(Item::Raw(WORD));
                        closed_range = true;
                        if cur.peek() == Some('-') && cur.peek_at(1) != Some(']') {
                            return Err(TranslateError::Syntax);
                        }
                        continue;
                    }
                    'D' | 'S' | 'W' => {
                        let frag = match e {
                            'D' => DIGITS,
                            'S' => SPACES,
                            _ => WORD,
                        };
                        items.push(Item::Negated(frag));
                        closed_range = true;
                        if cur.peek() == Some('-') && cur.peek_at(1) != Some(']') {
                            return Err(TranslateError::Syntax);
                        }
                        continue;
                    }
                    d if d.is_ascii_digit() => return Err(TranslateError::Unsupported),
                    _ => match char_escape(cur, e)? {
                        Some(ch) => ch,
                        None => return Err(TranslateError::Syntax),
                    },
                }
            }
            '-' if closed_range && cur.peek() != Some(']') => {
                // 範囲・クラスの直後に現れた `-` が末尾でなければ構文誤り（末尾なら下で
                // `]` が先に来るためここへは来ない）。
                return Err(TranslateError::Syntax);
            }
            other => other,
        };
        closed_range = false;
        // 範囲かどうか: 直後が `-` で、その次が `]` でない。
        if cur.peek() == Some('-') && cur.peek_at(1).is_some_and(|n| n != ']') {
            cur.pos = cur.pos.saturating_add(1);
            let Some(ec) = cur.bump() else {
                return Err(TranslateError::Syntax);
            };
            let end: char = match ec {
                '[' => match cur.peek() {
                    Some('.') | Some('=') => return Err(TranslateError::Unsupported),
                    // 範囲の終点にクラスは置けない。
                    Some(':') => return Err(TranslateError::Syntax),
                    _ => '[',
                },
                '\\' => {
                    let Some(e) = cur.bump() else {
                        return Err(TranslateError::Syntax);
                    };
                    if e.is_ascii_digit() {
                        return Err(TranslateError::Unsupported);
                    }
                    match char_escape(cur, e)? {
                        Some(ch) => ch,
                        // `\d` 等のクラスは範囲の終点にできない。
                        None => return Err(TranslateError::Syntax),
                    }
                }
                other => other,
            };
            if start > end {
                return Err(TranslateError::Syntax);
            }
            items.push(Item::Range(start, end));
            closed_range = true;
        } else {
            items.push(Item::Range(start, start));
        }
    }
    if insensitive {
        fold_ranges(&mut items);
    }
    out.push('[');
    if negated {
        out.push('^');
    }
    for it in &items {
        match it {
            Item::Range(lo, hi) => {
                push_char(out, *lo);
                if lo != hi {
                    out.push('-');
                    push_char(out, *hi);
                }
            }
            Item::Raw(s) => out.push_str(s),
            Item::Negated(s) => {
                out.push_str("[^");
                out.push_str(s);
                out.push(']');
            }
        }
    }
    out.push(']');
    Ok(())
}

/// 量指定子 `{m}`・`{m,}`・`{m,n}` を読む（`{` は消費済みで、直後が数字であることを確認済み）。
/// 回数は 0〜255（PG の DUPMAX）で `m <= n`。形が崩れていれば構文誤り。
fn read_bound(cur: &mut Cursor<'_>, out: &mut String) -> Res<()> {
    let read_num = |cur: &mut Cursor<'_>| -> Res<u32> {
        let mut v: u32 = 0;
        let mut n = 0usize;
        while let Some(d) = cur.peek().and_then(|c| c.to_digit(10)) {
            cur.pos = cur.pos.saturating_add(1);
            n = n.saturating_add(1);
            v = v.saturating_mul(10).saturating_add(d);
        }
        if n == 0 || v > 255 {
            return Err(TranslateError::Syntax);
        }
        Ok(v)
    };
    let m = read_num(cur)?;
    match cur.bump() {
        Some('}') => out.push_str(&format!("{{{m}}}")),
        Some(',') => {
            if cur.peek() == Some('}') {
                cur.pos = cur.pos.saturating_add(1);
                out.push_str(&format!("{{{m},}}"));
            } else {
                let n = read_num(cur)?;
                if cur.bump() != Some('}') || m > n {
                    return Err(TranslateError::Syntax);
                }
                out.push_str(&format!("{{{m},{n}}}"));
            }
        }
        _ => return Err(TranslateError::Syntax),
    }
    Ok(())
}

/// PG の ARE を `regex` 構文へ変換する。`base_insensitive` は `~*` 相当（`AsciiInsensitive`）。
/// 呼び出し側（`regex_match`）がパターン長の上限を検査済みである前提。
pub(super) fn translate(pattern: &str, base_insensitive: bool) -> Res<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let prefix = parse_prefix(&chars, base_insensitive)?;
    let insensitive = prefix.insensitive;
    let mut cur = Cursor {
        chars: &chars,
        pos: prefix.start,
    };
    let mut out = String::new();
    // `.`・`^` 等は変換後に必ず `(?s:` 等で包むためフラグは持ち越さない。
    if prefix.literal {
        while let Some(c) = cur.bump() {
            push_literal(&mut out, c, insensitive);
        }
        return Ok(out);
    }
    let mut depth: usize = 0;
    let mut last = Last::Nothing;
    while let Some(c) = cur.bump() {
        match c {
            '(' => {
                if cur.peek() == Some('?') {
                    match cur.peek_at(1) {
                        Some(':') => cur.pos = cur.pos.saturating_add(2),
                        // 先読み・後読み・コメント: PG は受理するが本実装は写像しない。
                        Some('=') | Some('!') | Some('#') => {
                            return Err(TranslateError::Unsupported)
                        }
                        Some('<') if matches!(cur.peek_at(2), Some('=') | Some('!')) => {
                            return Err(TranslateError::Unsupported)
                        }
                        // 先頭以外の埋め込みオプション・未知の `(?` 形は PG でも構文誤り。
                        _ => return Err(TranslateError::Syntax),
                    }
                }
                depth = depth.saturating_add(1);
                if depth > MAX_REGEX_NEST_DEPTH {
                    return Err(TranslateError::TooDeep);
                }
                out.push_str("(?:");
                last = Last::Nothing;
            }
            ')' => {
                if depth == 0 {
                    return Err(TranslateError::Syntax);
                }
                depth -= 1;
                out.push(')');
                last = Last::Atom;
            }
            '|' => {
                out.push('|');
                last = Last::Nothing;
            }
            '*' | '+' | '?' => {
                if last != Last::Atom {
                    return Err(TranslateError::Syntax);
                }
                out.push(c);
                if cur.peek() == Some('?') {
                    cur.pos = cur.pos.saturating_add(1);
                    out.push('?');
                }
                last = Last::Quantifier;
            }
            '{' if cur.peek().is_some_and(|d| d.is_ascii_digit()) => {
                if last != Last::Atom {
                    return Err(TranslateError::Syntax);
                }
                read_bound(&mut cur, &mut out)?;
                if cur.peek() == Some('?') {
                    cur.pos = cur.pos.saturating_add(1);
                    out.push('?');
                }
                last = Last::Quantifier;
            }
            '.' => {
                out.push_str("(?s:.)");
                last = Last::Atom;
            }
            '^' => {
                out.push_str("\\A");
                last = Last::Anchor;
            }
            '$' => {
                out.push_str("\\z");
                last = Last::Anchor;
            }
            '[' => {
                if cur.starts_with("[:<:]]") {
                    cur.pos = cur.pos.saturating_add(6);
                    out.push_str("(?-u:\\b{start})");
                    last = Last::Anchor;
                } else if cur.starts_with("[:>:]]") {
                    cur.pos = cur.pos.saturating_add(6);
                    out.push_str("(?-u:\\b{end})");
                    last = Last::Anchor;
                } else {
                    bracket(&mut cur, &mut out, insensitive)?;
                    last = Last::Atom;
                }
            }
            '\\' => {
                let Some(e) = cur.bump() else {
                    return Err(TranslateError::Syntax);
                };
                match e {
                    'd' => {
                        out.push_str("[0-9]");
                        last = Last::Atom;
                    }
                    'D' => {
                        out.push_str("[^0-9]");
                        last = Last::Atom;
                    }
                    's' => {
                        out.push_str(&format!("[{SPACES}]"));
                        last = Last::Atom;
                    }
                    'S' => {
                        out.push_str(&format!("[^{SPACES}]"));
                        last = Last::Atom;
                    }
                    'w' => {
                        out.push_str(&format!("[{WORD}]"));
                        last = Last::Atom;
                    }
                    'W' => {
                        out.push_str(&format!("[^{WORD}]"));
                        last = Last::Atom;
                    }
                    'A' => {
                        out.push_str("\\A");
                        last = Last::Anchor;
                    }
                    'Z' => {
                        out.push_str("\\z");
                        last = Last::Anchor;
                    }
                    'm' => {
                        out.push_str("(?-u:\\b{start})");
                        last = Last::Anchor;
                    }
                    'M' => {
                        out.push_str("(?-u:\\b{end})");
                        last = Last::Anchor;
                    }
                    'y' => {
                        out.push_str("(?-u:\\b)");
                        last = Last::Anchor;
                    }
                    'Y' => {
                        out.push_str("(?-u:\\B)");
                        last = Last::Anchor;
                    }
                    // ARE では `\B` は「バックスラッシュ 1 個」の同義語。
                    'B' => {
                        push_literal(&mut out, '\\', insensitive);
                        last = Last::Atom;
                    }
                    // 後方参照（`\1`〜）と 8 進（`\0`）: 写像しない。
                    d if d.is_ascii_digit() => return Err(TranslateError::Unsupported),
                    _ => match char_escape(&mut cur, e)? {
                        Some(ch) => {
                            push_literal(&mut out, ch, insensitive);
                            last = Last::Atom;
                        }
                        // 未知の英数字エスケープは PG でも構文誤り。
                        None => return Err(TranslateError::Syntax),
                    },
                }
            }
            other => {
                push_literal(&mut out, other, insensitive);
                last = Last::Atom;
            }
        }
    }
    if depth != 0 {
        return Err(TranslateError::Syntax);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(p: &str) -> Result<String, TranslateError> {
        translate(p, false)
    }

    #[test]
    fn literal_and_anchors_map() {
        assert_eq!(t("a^b$").unwrap(), "a\\Ab\\z");
        assert_eq!(t("a.b").unwrap(), "a(?s:.)b");
    }

    #[test]
    fn insensitive_expands_ascii_letters_only() {
        assert_eq!(translate("aé", true).unwrap(), "[Aa]é");
        assert_eq!(translate("[a-c]", true).unwrap(), "[a-cA-C]");
    }

    #[test]
    fn meta_in_literals_are_escaped() {
        assert_eq!(t("\\.").unwrap(), "\\x{2e}");
        assert_eq!(t("[&&a]").unwrap(), "[\\x{26}\\x{26}a]");
    }

    #[test]
    fn unsupported_constructs_are_0a000() {
        for p in [
            "(a)\\1", "(?=a)", "(?!a)", "(?<=a)", "(?<!a)", "[[.a.]]", "[[=a=]]", "(?x)a",
            "(?#c)a", "\\0",
        ] {
            assert_eq!(t(p), Err(TranslateError::Unsupported), "{p}");
        }
    }

    #[test]
    fn syntax_errors_are_2201b() {
        for p in [
            "(",
            "a)",
            "*a",
            "a**",
            "a{1",
            "a{256}",
            "a{2,1}",
            "[a",
            "[]",
            "[z-a]",
            "\\",
            "\\q",
            "a(?i)b",
            "(?i",
            "***a",
            "[[:foo:]]",
            "^*",
            "a{1}{2}",
        ] {
            assert_eq!(t(p), Err(TranslateError::Syntax), "{p}");
        }
    }

    #[test]
    fn nesting_limit_is_enforced_without_recursion() {
        let ok = format!(
            "{}a{}",
            "(".repeat(MAX_REGEX_NEST_DEPTH),
            ")".repeat(MAX_REGEX_NEST_DEPTH)
        );
        assert!(t(&ok).is_ok());
        let ng = format!(
            "{}a{}",
            "(".repeat(MAX_REGEX_NEST_DEPTH + 1),
            ")".repeat(MAX_REGEX_NEST_DEPTH + 1)
        );
        assert_eq!(t(&ng), Err(TranslateError::TooDeep));
    }

    #[test]
    fn bounds_boundaries() {
        assert!(t("a{255}").is_ok());
        assert_eq!(t("a{256}"), Err(TranslateError::Syntax));
        assert!(t("a{0,255}").is_ok());
    }
}
