//! jsonpath の字句解析器（`parser.rs` から 1 トークンずつ遅延して呼ばれる）。
//!
//! 責務: 文字列・識別子・変数・数値・記号・コメントのトークン化と、エスケープの展開、
//! 数値の 10 進正規化まで。構文（式か述語か等）の判断は `parser.rs` が担う。
//! 入力は untrusted のため `[]`・`unwrap` を使わず `get()` で走査し、どのループも 1 回ごとに
//! 必ず 1 文字以上を消費する。数値リテラルは長さの上限を基数変換の前に検査する。

use super::{JsonPathParseError as E, MAX_JSONPATH_NUMERIC_LITERAL_LEN};

/// トークン。キーワードは `Word` として返し、解釈は構文解析側で文脈に応じて行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    Eof,
    Str(String),
    Word(String),
    Var(String),
    /// `is_int` は整数形（10 進整数・基数つき整数）。`text` は 10 進正規化済み。
    Num {
        text: String,
        is_int: bool,
    },
    Root,
    At,
    Question,
    Dot,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    LParen,
    RParen,
    Comma,
    Star,
    DStar,
    Plus,
    Minus,
    Slash,
    Percent,
    Bang,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
}

pub(super) struct Lexer {
    chars: Vec<char>,
    pos: usize,
}

fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000C}')
}

/// 識別子・数値の直後に置けない「その他」の文字（空白でも記号でもない文字）。
fn is_other(c: char) -> bool {
    !is_blank(c)
        && !matches!(
            c,
            '?' | '%'
                | '$'
                | '.'
                | '['
                | ']'
                | '{'
                | '}'
                | '('
                | ')'
                | '|'
                | '&'
                | '!'
                | '='
                | '<'
                | '>'
                | '@'
                | '#'
                | ','
                | '*'
                | ':'
                | '-'
                | '+'
                | '/'
                | '\\'
                | '"'
        )
}

fn hex_val(c: char) -> Option<u32> {
    c.to_digit(16)
}

impl Lexer {
    pub(super) fn new(input: &str) -> Self {
        Lexer {
            chars: input.chars().collect(),
            pos: 0,
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos.saturating_add(off)).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// 次のトークンを返す。終端では `Eof` を返し続ける。
    pub(super) fn next_token(&mut self) -> Result<Token, E> {
        self.skip_blank_and_comments()?;
        let Some(c) = self.peek_char() else {
            return Ok(Token::Eof);
        };
        if c.is_ascii_digit() || (c == '.' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit())) {
            return self.scan_number();
        }
        self.pos += 1;
        let tok = match c {
            '$' => match self.peek_char() {
                Some('"') => {
                    self.pos += 1;
                    Token::Var(self.scan_quoted()?)
                }
                Some(n) if is_other(n) => Token::Var(self.scan_ident()?),
                _ => Token::Root,
            },
            '@' => Token::At,
            '?' => Token::Question,
            '.' => Token::Dot,
            '[' => Token::LBracket,
            ']' => Token::RBracket,
            '{' => Token::LBrace,
            '}' => Token::RBrace,
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '*' => {
                if self.peek_char() == Some('*') {
                    self.pos += 1;
                    Token::DStar
                } else {
                    Token::Star
                }
            }
            '!' => {
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Token::Ne
                } else {
                    Token::Bang
                }
            }
            '=' => {
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Token::Eq
                } else {
                    return Err(E::Syntax);
                }
            }
            '<' => match self.peek_char() {
                Some('=') => {
                    self.pos += 1;
                    Token::Le
                }
                Some('>') => {
                    self.pos += 1;
                    Token::Ne
                }
                _ => Token::Lt,
            },
            '>' => {
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Token::Ge
                } else {
                    Token::Gt
                }
            }
            '&' => {
                if self.peek_char() == Some('&') {
                    self.pos += 1;
                    Token::AndAnd
                } else {
                    return Err(E::Syntax);
                }
            }
            '|' => {
                if self.peek_char() == Some('|') {
                    self.pos += 1;
                    Token::OrOr
                } else {
                    return Err(E::Syntax);
                }
            }
            '"' => Token::Str(self.scan_quoted()?),
            '#' | ':' => return Err(E::Syntax),
            _ => {
                // 識別子（`\` エスケープで始まる場合を含む）。1 文字戻して走査し直す。
                self.pos -= 1;
                let w = self.scan_ident()?;
                // PostgreSQL は識別子の直後の `/*` で識別子の文字列を黙って捨てる（既知の
                // 癖）。同じ入力を受理側に倒さないよう fail-closed で拒否する。
                if self.peek_char() == Some('/') && self.peek_at(1) == Some('*') {
                    return Err(E::Syntax);
                }
                Token::Word(w)
            }
        };
        Ok(tok)
    }

    fn skip_blank_and_comments(&mut self) -> Result<(), E> {
        loop {
            match self.peek_char() {
                Some(c) if is_blank(c) => self.pos += 1,
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.pos += 2;
                    loop {
                        match self.bump() {
                            None => return Err(E::Syntax),
                            Some('*') if self.peek_char() == Some('/') => {
                                self.pos += 1;
                                break;
                            }
                            Some(_) => {}
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// 引用符なしの識別子（キー・キーワード・変数名）。`\` エスケープを展開する。
    fn scan_ident(&mut self) -> Result<String, E> {
        let mut out = String::new();
        while let Some(c) = self.peek_char() {
            if is_other(c) {
                out.push(c);
                self.pos += 1;
            } else if c == '\\' {
                self.pos += 1;
                self.scan_escape(&mut out)?;
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// 開き `"` の直後から閉じ `"` までを読む。
    fn scan_quoted(&mut self) -> Result<String, E> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(E::Syntax),
                Some('"') => return Ok(out),
                Some('\\') => self.scan_escape(&mut out)?,
                Some(c) => out.push(c),
            }
        }
    }

    /// `\` の直後のエスケープ 1 個を展開して `out` へ追加する。
    fn scan_escape(&mut self, out: &mut String) -> Result<(), E> {
        let Some(c) = self.bump() else {
            return Err(E::Syntax);
        };
        match c {
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\u{000B}'),
            'x' => {
                let mut v: u32 = 0;
                for _ in 0..2 {
                    let d = self.bump().and_then(hex_val).ok_or(E::Syntax)?;
                    v = v * 16 + d;
                }
                push_scalar(out, v)?;
            }
            'u' => {
                let cp = self.scan_unicode_escape_body()?;
                if (0xD800..=0xDBFF).contains(&cp) {
                    // 上位サロゲートは直後の `\u` 下位サロゲートと対でなければならない。
                    if self.peek_char() != Some('\\') || self.peek_at(1) != Some('u') {
                        return Err(E::InvalidUnicode);
                    }
                    self.pos += 2;
                    let lo = self.scan_unicode_escape_body()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(E::InvalidUnicode);
                    }
                    let combined = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    push_scalar(out, combined)?;
                } else {
                    push_scalar(out, cp)?;
                }
            }
            other => out.push(other),
        }
        Ok(())
    }

    /// `\u` の直後の `XXXX` または `{X…}`（1〜6 桁）を値として読む。
    fn scan_unicode_escape_body(&mut self) -> Result<u32, E> {
        let mut v: u32 = 0;
        if self.peek_char() == Some('{') {
            self.pos += 1;
            let mut n = 0;
            loop {
                match self.bump() {
                    Some('}') if n > 0 => return Ok(v),
                    Some(c) => {
                        let d = hex_val(c).ok_or(E::Syntax)?;
                        n += 1;
                        if n > 6 {
                            return Err(E::Syntax);
                        }
                        v = v * 16 + d;
                    }
                    None => return Err(E::Syntax),
                }
            }
        }
        for _ in 0..4 {
            let d = self.bump().and_then(hex_val).ok_or(E::Syntax)?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn scan_number(&mut self) -> Result<Token, E> {
        let start = self.pos;
        if self.peek_char() == Some('0') {
            let radix = match self.peek_at(1) {
                Some('x' | 'X') => Some(16),
                Some('o' | 'O') => Some(8),
                Some('b' | 'B') => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                if self.peek_at(2).is_some_and(|c| c.is_digit(radix)) {
                    self.pos += 2;
                    let digits = self.scan_digits(radix);
                    self.check_literal(start)?;
                    self.check_no_junk()?;
                    return Ok(Token::Num {
                        text: radix_to_decimal(&digits, radix),
                        is_int: true,
                    });
                }
            }
        }
        let mut int = String::new();
        match self.peek_char() {
            Some('0') => {
                self.pos += 1;
                int.push('0');
            }
            Some(c) if c.is_ascii_digit() => int = self.scan_digits(10),
            _ => {}
        }
        let mut frac = String::new();
        let mut has_dot = false;
        if self.peek_char() == Some('.') {
            has_dot = true;
            self.pos += 1;
            frac = self.scan_digits(10);
        }
        let mut exp = String::new();
        let mut has_exp = false;
        if matches!(self.peek_char(), Some('e' | 'E')) {
            let mut off = 1;
            let mut neg = false;
            if let Some(s @ ('+' | '-')) = self.peek_at(off) {
                neg = s == '-';
                off += 1;
            }
            if self.peek_at(off).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += off;
                has_exp = true;
                if neg {
                    exp.push('-');
                }
                exp.push_str(&self.scan_digits(10));
            }
        }
        self.check_literal(start)?;
        self.check_no_junk()?;
        let mut text = if int.is_empty() { "0".to_string() } else { int };
        if !frac.is_empty() {
            text.push('.');
            text.push_str(&frac);
        }
        if has_exp {
            text.push('e');
            text.push_str(&exp);
        }
        Ok(Token::Num {
            text,
            is_int: !has_dot && !has_exp,
        })
    }

    /// `d(_?d)*` を読み、`_` を除いた桁列を返す。先頭が桁でなければ空。
    fn scan_digits(&mut self, radix: u32) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_digit(radix) {
                out.push(c);
                self.pos += 1;
            } else if c == '_'
                && !out.is_empty()
                && self.peek_at(1).is_some_and(|d| d.is_digit(radix))
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        out
    }

    fn check_literal(&self, start: usize) -> Result<(), E> {
        if self.pos.saturating_sub(start) > MAX_JSONPATH_NUMERIC_LITERAL_LEN {
            return Err(E::LiteralTooLong);
        }
        Ok(())
    }

    /// 数値の直後に識別子の文字が続く形（`1a`・`00`・`1_`）を拒否する。
    fn check_no_junk(&self) -> Result<(), E> {
        match self.peek_char() {
            Some(c) if is_other(c) => Err(E::Syntax),
            _ => Ok(()),
        }
    }
}

fn push_scalar(out: &mut String, cp: u32) -> Result<(), E> {
    if cp == 0 {
        return Err(E::UnsupportedUnicodeEscape);
    }
    if cp > 0x10_FFFF {
        return Err(E::Syntax);
    }
    let c = char::from_u32(cp).ok_or(E::InvalidUnicode)?;
    out.push(c);
    Ok(())
}

/// 基数つき整数の桁列（`_` 除去済み・1 桁以上）を 10 進テキストへ変換する。桁数は
/// リテラル長の上限で抑えられているため O(n^2) でも小さい。
fn radix_to_decimal(digits: &str, radix: u32) -> String {
    // 10 進の桁を下位から保持する。
    let mut dec: Vec<u8> = vec![0];
    for c in digits.chars() {
        let mut carry = c.to_digit(radix).unwrap_or(0);
        for d in dec.iter_mut() {
            let v = u32::from(*d) * radix + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            dec.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    while dec.len() > 1 && dec.last() == Some(&0) {
        dec.pop();
    }
    dec.iter().rev().map(|d| char::from(b'0' + *d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_all(s: &str) -> Result<Vec<Token>, E> {
        let mut l = Lexer::new(s);
        let mut v = Vec::new();
        loop {
            let t = l.next_token()?;
            if t == Token::Eof {
                return Ok(v);
            }
            v.push(t);
        }
    }

    fn num(s: &str) -> String {
        match lex_all(s).unwrap().as_slice() {
            [Token::Num { text, .. }] => text.clone(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn numbers_normalize_to_decimal_text() {
        assert_eq!(num("0x1F"), "31");
        assert_eq!(num("0o17"), "15");
        assert_eq!(num("0b101"), "5");
        assert_eq!(num("1_000"), "1000");
        assert_eq!(num(".1"), "0.1");
        assert_eq!(num("1."), "1");
        assert_eq!(num("1.e3"), "1e3");
        assert_eq!(num("1e+3"), "1e3");
        assert_eq!(num("1.5e-3"), "1.5e-3");
        assert_eq!(num("0xFFFFFFFFFFFFFFFFFFFF"), "1208925819614629174706175");
    }

    #[test]
    fn numeric_junk_is_rejected() {
        for s in ["00", "01", "1e", "1_", "1__0", "1.a", "0x", "1a"] {
            assert_eq!(lex_all(s), Err(E::Syntax), "{s}");
        }
    }

    #[test]
    fn escapes_and_surrogates() {
        assert_eq!(
            lex_all(r#""A\x42\u{43}\q""#).unwrap(),
            vec![Token::Str("ABCq".into())]
        );
        assert_eq!(
            lex_all(r#""😀""#).unwrap(),
            vec![Token::Str("\u{1F600}".into())]
        );
        assert_eq!(lex_all(r#""\ud800""#), Err(E::InvalidUnicode));
        assert_eq!(lex_all(r#""\ude00""#), Err(E::InvalidUnicode));
        assert_eq!(lex_all(r#""\u0000""#), Err(E::UnsupportedUnicodeEscape));
        assert_eq!(lex_all(r#""\x4""#), Err(E::Syntax));
        assert_eq!(lex_all(r#""abc"#), Err(E::Syntax));
    }

    #[test]
    fn comments_and_variables() {
        assert_eq!(lex_all("/* c */ $").unwrap(), vec![Token::Root]);
        assert_eq!(lex_all("$ /* c"), Err(E::Syntax));
        assert_eq!(
            lex_all(r#"$x $1 $"x y""#).unwrap(),
            vec![
                Token::Var("x".into()),
                Token::Var("1".into()),
                Token::Var("x y".into())
            ]
        );
    }
}
