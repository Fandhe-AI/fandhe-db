//! jsonpath の再帰下降構文解析器（`jsonpath.rs` の [`super::parse_jsonpath`] から呼ばれる）。
//!
//! 責務: `lexer.rs` のトークン列を [`SqlJsonPath`] へ組み立てる。PostgreSQL と同じく「式」と
//! 「述語」を別カテゴリとして扱い（`Kind`）、`&&`／`||`／`!`／比較の両辺のカテゴリ違反を
//! 構文エラーにする。`@`（フィルタ外）・`last`（添字外）の位置違反はここで検査し、
//! `like_regex` は構文を解析したうえで最後に 0A000 で拒否する（fail-closed）。
//! 再帰は `depth` 引数で入れ子深さを数え、式木の高さは `Parsed::h` で数えて上限を超える
//! 入力を拒否する。連続アクセサはループで畳むため再帰しない。

use super::ast::*;
use super::lexer::{Lexer, Token};
use super::{JsonPathParseError as E, MAX_JSONPATH_NESTING_DEPTH, MAX_JSONPATH_NODE_HEIGHT};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Expr,
    Pred,
}

/// 解析途中の部分木。`paren_pred` は `( 述語 )` で囲まれた述語（`is unknown` の左辺に
/// できる唯一の形）。`h` は式木の高さ。
struct Parsed {
    node: PathNode,
    kind: Kind,
    paren_pred: bool,
    h: usize,
}

struct Parser {
    lex: Lexer,
    peeked: Option<Token>,
    filter_depth: usize,
    subscript_depth: usize,
    saw_regex: bool,
}

pub(super) fn parse(input: &str) -> Result<SqlJsonPath, E> {
    let mut p = Parser {
        lex: Lexer::new(input),
        peeked: None,
        filter_depth: 0,
        subscript_depth: 0,
        saw_regex: false,
    };
    if *p.peek()? == Token::Eof {
        return Err(E::Empty);
    }
    let mode = match p.peek()? {
        Token::Word(w) if w.eq_ignore_ascii_case("lax") => Some(PathMode::Lax),
        Token::Word(w) if w.eq_ignore_ascii_case("strict") => Some(PathMode::Strict),
        _ => None,
    };
    if mode.is_some() {
        p.next()?;
    }
    let body = p.parse_or(0)?;
    if p.next()? != Token::Eof {
        return Err(E::Syntax);
    }
    if p.saw_regex {
        return Err(E::RegexNotSupported);
    }
    Ok(SqlJsonPath {
        mode: mode.unwrap_or(PathMode::Lax),
        expr: body.node,
    })
}

fn mk(node: PathNode, kind: Kind, h: usize) -> Result<Parsed, E> {
    if h > MAX_JSONPATH_NODE_HEIGHT {
        return Err(E::TooDeep);
    }
    Ok(Parsed {
        node,
        kind,
        paren_pred: false,
        h,
    })
}

fn need(p: &Parsed, kind: Kind) -> Result<(), E> {
    if p.kind == kind {
        Ok(())
    } else {
        Err(E::Syntax)
    }
}

fn enter(depth: usize) -> Result<usize, E> {
    if depth >= MAX_JSONPATH_NESTING_DEPTH {
        return Err(E::TooDeep);
    }
    Ok(depth + 1)
}

fn is_kw(t: &Token, kw: &str) -> bool {
    matches!(t, Token::Word(w) if w.eq_ignore_ascii_case(kw))
}

impl Parser {
    fn peek(&mut self) -> Result<&Token, E> {
        if self.peeked.is_none() {
            self.peeked = Some(self.lex.next_token()?);
        }
        self.peeked.as_ref().ok_or(E::Syntax)
    }

    fn next(&mut self) -> Result<Token, E> {
        match self.peeked.take() {
            Some(t) => Ok(t),
            None => self.lex.next_token(),
        }
    }

    fn expect(&mut self, want: Token) -> Result<(), E> {
        if self.next()? == want {
            Ok(())
        } else {
            Err(E::Syntax)
        }
    }

    fn eat(&mut self, want: &Token) -> Result<bool, E> {
        if self.peek()? == want {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn eat_kw(&mut self, kw: &str) -> Result<bool, E> {
        if is_kw(self.peek()?, kw) {
            self.next()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // or := and ('||' and)*
    fn parse_or(&mut self, depth: usize) -> Result<Parsed, E> {
        let mut left = self.parse_and(depth)?;
        while *self.peek()? == Token::OrOr {
            need(&left, Kind::Pred)?;
            self.next()?;
            let right = self.parse_and(depth)?;
            need(&right, Kind::Pred)?;
            let h = left.h.max(right.h) + 1;
            left = mk(
                PathNode::Or(Box::new(left.node), Box::new(right.node)),
                Kind::Pred,
                h,
            )?;
        }
        Ok(left)
    }

    // and := cmp ('&&' cmp)*
    fn parse_and(&mut self, depth: usize) -> Result<Parsed, E> {
        let mut left = self.parse_cmp(depth)?;
        while *self.peek()? == Token::AndAnd {
            need(&left, Kind::Pred)?;
            self.next()?;
            let right = self.parse_cmp(depth)?;
            need(&right, Kind::Pred)?;
            let h = left.h.max(right.h) + 1;
            left = mk(
                PathNode::And(Box::new(left.node), Box::new(right.node)),
                Kind::Pred,
                h,
            )?;
        }
        Ok(left)
    }

    // cmp := '!' delimited | additive [ (compop additive) | starts with ... | like_regex ... ]
    fn parse_cmp(&mut self, depth: usize) -> Result<Parsed, E> {
        if self.eat(&Token::Bang)? {
            let inner = self.parse_delimited(depth)?;
            let h = inner.h + 1;
            return mk(PathNode::Not(Box::new(inner.node)), Kind::Pred, h);
        }
        let lhs = self.parse_additive(depth)?;
        if lhs.kind == Kind::Pred {
            if lhs.paren_pred && self.eat_kw("is")? {
                if !self.eat_kw("unknown")? {
                    return Err(E::Syntax);
                }
                let h = lhs.h + 1;
                return mk(PathNode::IsUnknown(Box::new(lhs.node)), Kind::Pred, h);
            }
            return Ok(lhs);
        }
        let op = match self.peek()? {
            Token::Eq => Some(CompareOp::Eq),
            Token::Ne => Some(CompareOp::Ne),
            Token::Lt => Some(CompareOp::Lt),
            Token::Le => Some(CompareOp::Le),
            Token::Gt => Some(CompareOp::Gt),
            Token::Ge => Some(CompareOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next()?;
            let rhs = self.parse_additive(depth)?;
            need(&rhs, Kind::Expr)?;
            let h = lhs.h.max(rhs.h) + 1;
            return mk(
                PathNode::Compare {
                    op,
                    lhs: Box::new(lhs.node),
                    rhs: Box::new(rhs.node),
                },
                Kind::Pred,
                h,
            );
        }
        if self.eat_kw("starts")? {
            if !self.eat_kw("with")? {
                return Err(E::Syntax);
            }
            let prefix = match self.next()? {
                Token::Str(s) => StartsWithOperand::String(s.into()),
                Token::Var(s) => StartsWithOperand::Variable(s.into()),
                _ => return Err(E::Syntax),
            };
            let h = lhs.h + 1;
            return mk(
                PathNode::StartsWith {
                    lhs: Box::new(lhs.node),
                    prefix,
                },
                Kind::Pred,
                h,
            );
        }
        if self.eat_kw("like_regex")? {
            let Token::Str(pattern) = self.next()? else {
                return Err(E::Syntax);
            };
            let mut flags = RegexFlags::default();
            if self.eat_kw("flag")? {
                let Token::Str(f) = self.next()? else {
                    return Err(E::Syntax);
                };
                for c in f.chars() {
                    match c {
                        'i' => flags.case_insensitive = true,
                        's' => flags.dot_all = true,
                        'm' => flags.multiline = true,
                        'q' => flags.literal = true,
                        'x' => flags.extended = true,
                        _ => return Err(E::Syntax),
                    }
                }
            }
            self.saw_regex = true;
            let h = lhs.h + 1;
            return mk(
                PathNode::LikeRegex {
                    lhs: Box::new(lhs.node),
                    pattern: pattern.into(),
                    flags,
                },
                Kind::Pred,
                h,
            );
        }
        Ok(lhs)
    }

    /// `!` の直後: `( 述語 )` または `exists( 式 )`（アクセサ・`is unknown` は続けられない）。
    fn parse_delimited(&mut self, depth: usize) -> Result<Parsed, E> {
        match self.next()? {
            Token::LParen => {
                let d = enter(depth)?;
                let inner = self.parse_or(d)?;
                need(&inner, Kind::Pred)?;
                self.expect(Token::RParen)?;
                Ok(Parsed {
                    paren_pred: false,
                    ..inner
                })
            }
            Token::Word(w) if w.eq_ignore_ascii_case("exists") => self.parse_exists(depth),
            _ => Err(E::Syntax),
        }
    }

    /// `exists` を消費した直後から `( 式 )` を読む。
    fn parse_exists(&mut self, depth: usize) -> Result<Parsed, E> {
        self.expect(Token::LParen)?;
        let d = enter(depth)?;
        let inner = self.parse_additive(d)?;
        need(&inner, Kind::Expr)?;
        self.expect(Token::RParen)?;
        let h = inner.h + 1;
        mk(PathNode::Exists(Box::new(inner.node)), Kind::Pred, h)
    }

    fn parse_additive(&mut self, depth: usize) -> Result<Parsed, E> {
        let mut left = self.parse_mul(depth)?;
        loop {
            let op = match self.peek()? {
                Token::Plus => ArithOp::Add,
                Token::Minus => ArithOp::Sub,
                _ => return Ok(left),
            };
            need(&left, Kind::Expr)?;
            self.next()?;
            let right = self.parse_mul(depth)?;
            need(&right, Kind::Expr)?;
            let h = left.h.max(right.h) + 1;
            left = mk(
                PathNode::Arith {
                    op,
                    lhs: Box::new(left.node),
                    rhs: Box::new(right.node),
                },
                Kind::Expr,
                h,
            )?;
        }
    }

    fn parse_mul(&mut self, depth: usize) -> Result<Parsed, E> {
        let mut left = self.parse_unary(depth)?;
        loop {
            let op = match self.peek()? {
                Token::Star => ArithOp::Mul,
                Token::Slash => ArithOp::Div,
                Token::Percent => ArithOp::Mod,
                _ => return Ok(left),
            };
            need(&left, Kind::Expr)?;
            self.next()?;
            let right = self.parse_unary(depth)?;
            need(&right, Kind::Expr)?;
            let h = left.h.max(right.h) + 1;
            left = mk(
                PathNode::Arith {
                    op,
                    lhs: Box::new(left.node),
                    rhs: Box::new(right.node),
                },
                Kind::Expr,
                h,
            )?;
        }
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Parsed, E> {
        let op = match self.peek()? {
            Token::Plus => UnaryOp::Plus,
            Token::Minus => UnaryOp::Minus,
            _ => return self.parse_primary(depth),
        };
        let d = enter(depth)?;
        self.next()?;
        let operand = self.parse_unary(d)?;
        need(&operand, Kind::Expr)?;
        let h = operand.h + 1;
        mk(
            PathNode::Unary {
                op,
                operand: Box::new(operand.node),
            },
            Kind::Expr,
            h,
        )
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Parsed, E> {
        let node = match self.next()? {
            Token::Num { text, .. } => PathNode::Numeric(PathNumeric(text.into())),
            Token::Str(s) => PathNode::String(s.into()),
            Token::Var(s) => PathNode::Variable(s.into()),
            Token::Root => PathNode::Root,
            Token::At => {
                if self.filter_depth == 0 {
                    return Err(E::Syntax);
                }
                PathNode::Current
            }
            Token::Word(w) => match w.as_str() {
                "true" => PathNode::Bool(true),
                "false" => PathNode::Bool(false),
                "null" => PathNode::Null,
                _ if w.eq_ignore_ascii_case("last") => {
                    if self.subscript_depth == 0 {
                        return Err(E::Syntax);
                    }
                    PathNode::Last
                }
                _ if w.eq_ignore_ascii_case("exists") => return self.parse_exists(depth),
                _ => return Err(E::Syntax),
            },
            Token::LParen => {
                let d = enter(depth)?;
                let inner = self.parse_or(d)?;
                self.expect(Token::RParen)?;
                if self.at_accessor_start()? {
                    return self.parse_accessors(inner, depth);
                }
                let paren_pred = inner.kind == Kind::Pred;
                return Ok(Parsed {
                    paren_pred,
                    ..inner
                });
            }
            _ => return Err(E::Syntax),
        };
        let base = mk(node, Kind::Expr, 1)?;
        self.parse_accessors(base, depth)
    }

    fn at_accessor_start(&mut self) -> Result<bool, E> {
        Ok(matches!(
            self.peek()?,
            Token::Dot | Token::LBracket | Token::Question
        ))
    }

    /// アクセサの連鎖を平坦な `ops` へ畳む（再帰しない）。無ければ `base` をそのまま返す。
    fn parse_accessors(&mut self, base: Parsed, depth: usize) -> Result<Parsed, E> {
        let mut ops: Vec<AccessorOp> = Vec::new();
        let mut max_h = base.h;
        loop {
            match self.peek()? {
                Token::Dot => {
                    self.next()?;
                    let op = self.parse_dot_accessor()?;
                    ops.push(op);
                }
                Token::LBracket => {
                    self.next()?;
                    let (op, h) = self.parse_subscripts(depth)?;
                    max_h = max_h.max(h);
                    ops.push(op);
                }
                Token::Question => {
                    self.next()?;
                    self.expect(Token::LParen)?;
                    let d = enter(depth)?;
                    self.filter_depth += 1;
                    let inner = self.parse_or(d)?;
                    self.filter_depth -= 1;
                    need(&inner, Kind::Pred)?;
                    self.expect(Token::RParen)?;
                    max_h = max_h.max(inner.h);
                    ops.push(AccessorOp::Filter(Box::new(inner.node)));
                }
                _ => break,
            }
        }
        if ops.is_empty() {
            return Ok(base);
        }
        mk(
            PathNode::Path {
                base: Box::new(base.node),
                ops,
            },
            Kind::Expr,
            max_h + 1,
        )
    }

    /// `.` の直後: キー・`*`・`**`・メソッド。
    fn parse_dot_accessor(&mut self) -> Result<AccessorOp, E> {
        match self.next()? {
            Token::Star => Ok(AccessorOp::MemberWildcard),
            Token::DStar => self.parse_any_path(),
            Token::Str(s) => Ok(AccessorOp::Member(s.into())),
            Token::Word(w) => {
                if *self.peek()? == Token::LParen {
                    if let Some(m) = self.parse_method(&w)? {
                        return Ok(AccessorOp::Method(m));
                    }
                }
                Ok(AccessorOp::Member(w.into()))
            }
            _ => Err(E::Syntax),
        }
    }

    fn parse_any_path(&mut self) -> Result<AccessorOp, E> {
        if !self.eat(&Token::LBrace)? {
            return Ok(AccessorOp::AnyPath {
                from: AnyLevel::Level(0),
                to: AnyLevel::Last,
            });
        }
        let from = self.parse_any_level()?;
        let to = if self.eat_kw("to")? {
            self.parse_any_level()?
        } else {
            from
        };
        self.expect(Token::RBrace)?;
        Ok(AccessorOp::AnyPath { from, to })
    }

    fn parse_any_level(&mut self) -> Result<AnyLevel, E> {
        match self.next()? {
            Token::Num { text, is_int: true } => {
                let v: u64 = text.parse().map_err(|_| E::IntegerOutOfRange)?;
                let v = u32::try_from(v)
                    .ok()
                    .filter(|v| i32::try_from(*v).is_ok())
                    .ok_or(E::IntegerOutOfRange)?;
                Ok(AnyLevel::Level(v))
            }
            Token::Word(w) if w.eq_ignore_ascii_case("last") => Ok(AnyLevel::Last),
            _ => Err(E::Syntax),
        }
    }

    /// `[` の直後から `]` まで。戻り値の `usize` は添字式の高さの最大。
    fn parse_subscripts(&mut self, depth: usize) -> Result<(AccessorOp, usize), E> {
        if self.eat(&Token::Star)? {
            self.expect(Token::RBracket)?;
            return Ok((AccessorOp::ArrayWildcard, 0));
        }
        let d = enter(depth)?;
        let mut items = Vec::new();
        let mut max_h = 0;
        self.subscript_depth += 1;
        loop {
            let from = self.parse_additive(d)?;
            need(&from, Kind::Expr)?;
            max_h = max_h.max(from.h);
            let to = if self.eat_kw("to")? {
                let t = self.parse_additive(d)?;
                need(&t, Kind::Expr)?;
                max_h = max_h.max(t.h);
                Some(t.node)
            } else {
                None
            };
            items.push(Subscript {
                from: from.node,
                to,
            });
            if self.eat(&Token::Comma)? {
                continue;
            }
            self.expect(Token::RBracket)?;
            break;
        }
        self.subscript_depth -= 1;
        Ok((AccessorOp::Subscripts(items), max_h))
    }

    /// メソッド名なら `(` を含めて引数まで読む。メソッド名でなければ `None`（キーとして扱う）。
    fn parse_method(&mut self, name: &str) -> Result<Option<Method>, E> {
        let lower = name.to_ascii_lowercase();
        let no_arg = match lower.as_str() {
            "abs" => Some(Method::Abs),
            "size" => Some(Method::Size),
            "type" => Some(Method::Type),
            "floor" => Some(Method::Floor),
            "ceiling" => Some(Method::Ceiling),
            "double" => Some(Method::Double),
            "keyvalue" => Some(Method::KeyValue),
            "bigint" => Some(Method::BigInt),
            "boolean" => Some(Method::Boolean),
            "date" => Some(Method::Date),
            "integer" => Some(Method::Integer),
            "number" => Some(Method::Number),
            "string" => Some(Method::String),
            _ => None,
        };
        if let Some(m) = no_arg {
            self.expect(Token::LParen)?;
            self.expect(Token::RParen)?;
            return Ok(Some(m));
        }
        match lower.as_str() {
            "datetime" => {
                self.expect(Token::LParen)?;
                let arg = match self.peek()? {
                    Token::Str(_) => match self.next()? {
                        Token::Str(s) => Some(Box::<str>::from(s)),
                        _ => return Err(E::Syntax),
                    },
                    _ => None,
                };
                self.expect(Token::RParen)?;
                Ok(Some(Method::Datetime(arg)))
            }
            "decimal" => {
                self.expect(Token::LParen)?;
                let mut args: Vec<Box<str>> = Vec::new();
                if *self.peek()? != Token::RParen {
                    loop {
                        let sign = if self.eat(&Token::Minus)? {
                            "-"
                        } else {
                            self.eat(&Token::Plus)?;
                            ""
                        };
                        let Token::Num { text, is_int: true } = self.next()? else {
                            return Err(E::Syntax);
                        };
                        args.push(format!("{sign}{text}").into());
                        if args.len() > 2 {
                            return Err(E::Syntax);
                        }
                        if !self.eat(&Token::Comma)? {
                            break;
                        }
                    }
                }
                self.expect(Token::RParen)?;
                let mut it = args.into_iter();
                Ok(Some(Method::Decimal {
                    precision: it.next(),
                    scale: it.next(),
                }))
            }
            "time" | "time_tz" | "timestamp" | "timestamp_tz" => {
                self.expect(Token::LParen)?;
                let arg = match self.peek()? {
                    Token::Num { is_int: true, .. } => match self.next()? {
                        Token::Num { text, .. } => Some(Box::<str>::from(text)),
                        _ => return Err(E::Syntax),
                    },
                    _ => None,
                };
                self.expect(Token::RParen)?;
                Ok(Some(match lower.as_str() {
                    "time" => Method::Time(arg),
                    "time_tz" => Method::TimeTz(arg),
                    "timestamp" => Method::Timestamp(arg),
                    _ => Method::TimestampTz(arg),
                }))
            }
            _ => Ok(None),
        }
    }
}
