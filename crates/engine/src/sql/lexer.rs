//! 自作 SQL トークナイザ（TASK-74・SQL-8 参照。docs/spec/05-tasks.md）。
//!
//! `sql::allowlist` の構造検証（呼び出し元）が消費する字句列を作る前段。untrusted な
//! wire 入力を直接扱うため、`unwrap`/`expect`/添字アクセスを使わず（coding-rust.md）
//! 状態機械で線形走査する。再帰を持たないため入力長に対してスタック消費が増えない。
//!
//! 未対応の記号は許可リスト外として [`LexError`] で拒否する（拒否リストではなく、
//! 既知トークンのみを許可リストとして認識する構造）。
//!
//! `--` 行コメントと `/* */` ブロックコメント（PostgreSQL と同じく入れ子可）は空白と
//! 同様に読み飛ばす（WIRE-16・TASK-219。Issue #1346）。二重引用符識別子は、中身が
//! 引用符なしでもそのまま 1 個の [`Token::Ident`] として字句解析できる語（英数字と
//! `_` のみ・先頭は英字か `_`・予約語でない）に限って受理する。ビュー本体・CHECK 式は
//! 識別子を引用符なしで永続 SQL へ描画して再字句解析するため（`render_tokens` 等）、
//! 中身を自由にすると再解析で別の文・別のトークンへ化ける注入経路になる。
//! コメント・引用の終端走査（[`line_comment_end`]・[`block_comment_end`]・
//! [`quoted_identifier_end`]）は `sql::statement_splitter` と共有し、分割器と lexer の
//! 解釈が食い違わないようにしている。

/// トークナイザが認識する字句の種類。
///
/// キーワードは大文字小文字を区別せず ASCII 大文字へ正規化した上で、
/// 許可リストの文法が直接必要とする最小集合だけを [`Token::Keyword`] として
/// 区別する。それ以外の英字トークンはすべて [`Token::Ident`] として扱い、
/// 文法（許可リスト側）がこれらを期待しない位置に置くことで構造的に拒否させる。
///
/// `USING`・`SET`（TASK-161・SQL-12）は本レイヤでは予約語化しない。カタログ上は
/// 有効な識別子（テーブル名・列名）として従来どおり `Ident` になる語のため、
/// 字句解析の時点で無条件にキーワード化すると、その識別子が使えなくなる
/// 未告知の破壊的変更になる。構文上その語が必須の位置（`LIMIT` 直後の
/// `USING MODE ...`、statement 先頭の `SET search_mode = ...`）でのみ、
/// `allowlist` 側が `Ident` の文字列を大文字小文字を区別せず照合して
/// 文脈的にキーワードとして扱う（`allowlist.rs::parse_using_clause`・
/// `allowlist.rs::validate_sql` 参照）。`LIKE`（TASK-147・EXT-3）も同じ理由・同じ
/// 方式で `Keyword` へ含めない（`like` という列名の等価条件 `WHERE like = 'x'` を
/// 壊さないため。`allowlist.rs::Parser::parse_where` が `WHERE` 句内・`ident` の
/// 直後という位置でのみ文脈的に照合する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Keyword(Keyword),
    Ident(String),
    StringLiteral(String),
    Number(String),
    /// `(` `)` `,` `*` `=` `;` `+` `-` `/` `>` `<` `[` `]`（TASK-79・SQL-9 で `+ - / > <` を追加。
    /// `[`／`]` は配列型の列宣言 `<型>[N]`（SQL-23・TABLE-14。Issue #1348）専用で、
    /// 構文上の受理位置は `sql::ddl_column_type` の型名解析に限る（それ以外の位置に
    /// 現れた場合は各パーサーが `42601` で拒否する）。
    /// `*` は SELECT リストの `*` と式内の乗算の両方を表す。文脈による使い分けは
    /// `allowlist::Parser` の管轄）。
    Punct(char),
    /// `->`（SQL-42・Issue #1520: 多文字演算子の字句化）。構文段（`sql::allowlist`）は
    /// 本トークンを受理せず `42601` で拒否する（fail-closed）。以下 5 つの多文字演算子も同様。
    Arrow,
    /// `->>`（最長一致で `->` より優先。SQL-42・Issue #1520）。
    ArrowText,
    /// `#>`（SQL-42・Issue #1520）。
    HashArrow,
    /// `#>>`（最長一致で `#>` より優先。SQL-42・Issue #1520）。
    HashArrowText,
    /// `::`（SQL-42・Issue #1520）。
    TypeCast,
    /// `||`（SQL-42・Issue #1520）。
    Concat,
    /// `<=>`（密ベクトル距離演算子）
    DistanceOp,
    /// `<=`（TASK-79・SQL-9: 式述語の比較演算子）。
    Le,
    /// `>=`（TASK-79・SQL-9: 式述語の比較演算子）。
    Ge,
    /// `<qualifier>.<name>`（SQL-20・TASK-193・Issue #872）。`ON CONFLICT ...
    /// DO UPDATE SET` の右辺 `EXCLUDED.<col>` 専用の 2 語 1 トークン化。
    /// `lex_word` の直後に空白を挟まず `.` ＋識別子開始文字（英字・`_`）が続く
    /// 場合のみこの形になる（`.5`・`1.`・`1..2`・`a.`・`a. b`・`a.b.c` はいずれも
    /// 該当せず、従来どおり許可リスト外の `.` として `LexError` になる——
    /// `qualifier`／`name` の 2 段しか許さないため `a.b.c` は `a.b` の直後に
    /// 孤立した `.c` が続く形になり、`allowlist::Parser` 側でこの `.` が
    /// 未対応のトークン境界として構文的に拒否される）。許可リスト
    /// （`sql::allowlist::Parser`）は `qualifier` を `EXCLUDED` と大小無視で
    /// 照合し、`ON CONFLICT ... SET` の右辺以外の位置に現れた場合は
    /// `expect_ident` 系ヘルパーが受理せず `42601` へ落とす。
    QualifiedIdent {
        qualifier: String,
        name: String,
    },
    /// 拡張クエリプロトコルのパラメータプレースホルダ `$n`（1 始まり。
    /// Issue #935・WIRE-12・TASK-217）。既定の [`tokenize`] は本トークンを一切
    /// 生成せず、`$` を引き続き「未対応文字」として拒否する
    /// （[`tests::rejects_dollar_parameter_placeholder`] で固定）。生成するのは
    /// [`tokenize_with_params`] のみで、生成した `Param` を消費するのは
    /// `sql::params`（Bind 時に実値の [`Token::StringLiteral`] へ置換する）に
    /// 限られる。`sql::allowlist::Parser`（許可リスト構造検証）はこの variant を
    /// 一切知らない設計（置換後のトークン列のみを見る）。
    Param(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Select,
    From,
    Where,
    And,
    Order,
    By,
    Limit,
}

/// TASK-80・SQL-10 の `INSERT`/`INTO`/`VALUES`/`USING`/`OPERATION_ID` は
/// [`Keyword`] へ含めない（PR #189 レビュー指摘対応・P1）。この 5 語を無条件で
/// `Token::Keyword` 化すると、既存の SELECT 許可形状（`sql::allowlist`）が
/// 受理してきた同名のテーブル名・列名（`catalog::validate_identifier` は
/// これらを識別子として許可している）が `expect_ident` を通過できなくなり、
/// 引用識別子の回避策もないまま公開クエリ構文を無告知に破壊してしまう。
/// 代わりに常に [`Token::Ident`] として字句解析し、`sql::allowlist::Parser` が
/// INSERT 許可形状のパーサー位置でのみ文脈的に大文字小文字を無視して照合する
/// （`Parser::expect_contextual_keyword`）。
fn keyword_from_str(s: &str) -> Option<Keyword> {
    // 大文字小文字を区別しない ASCII 大文字比較（SQL 予約語の慣習に合わせる）。
    match s.to_ascii_uppercase().as_str() {
        "SELECT" => Some(Keyword::Select),
        "FROM" => Some(Keyword::From),
        "WHERE" => Some(Keyword::Where),
        "AND" => Some(Keyword::And),
        "ORDER" => Some(Keyword::Order),
        "BY" => Some(Keyword::By),
        "LIMIT" => Some(Keyword::Limit),
        _ => None,
    }
}

/// 検証済みトークン列を、再トークン化で元と同一のトークン列に戻る正規化 SQL
/// テキストへ描画する（TABLE-18・Issue #1192）。`CREATE VIEW` の本文のうち、
/// 集計・`LIMIT`・`ORDER BY`・JOIN を含む形は AST から再描画すると `f64` の
/// 丸めや述語形状の取りこぼしが起きうるため、許可リスト構造検証を通過した
/// トークン列そのものを描画して永続化する（`sql::allowlist::
/// validate_create_view_tokens` が呼ぶ。参照時は `sql::view::resolve_from` が
/// 同じ字句解析・許可リストを再度通す）。
///
/// 規則: トークンを半角スペース 1 つで連結する（隣接する `-` が `--` コメントに、
/// `Punct('-')`＋`Punct('>')` が `->` に（SQL-42）、
/// `<` と `=` が `<=` に化けるのを防ぐ）。`Keyword` は大文字、`StringLiteral` は
/// `'` の二重化で囲む。`Token::Param` は DDL では Parse 時点で拒否済みのため
/// `None`（fail-closed）。
pub(crate) fn render_tokens(tokens: &[Token]) -> Option<String> {
    let mut out = String::new();
    for (i, t) in tokens.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match t {
            Token::Keyword(k) => out.push_str(match k {
                Keyword::Select => "SELECT",
                Keyword::From => "FROM",
                Keyword::Where => "WHERE",
                Keyword::And => "AND",
                Keyword::Order => "ORDER",
                Keyword::By => "BY",
                Keyword::Limit => "LIMIT",
            }),
            Token::Ident(s) | Token::Number(s) => out.push_str(s),
            Token::StringLiteral(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            Token::Punct(c) => out.push(*c),
            Token::DistanceOp => out.push_str("<=>"),
            Token::Le => out.push_str("<="),
            Token::Ge => out.push_str(">="),
            Token::Arrow => out.push_str("->"),
            Token::ArrowText => out.push_str("->>"),
            Token::HashArrow => out.push_str("#>"),
            Token::HashArrowText => out.push_str("#>>"),
            Token::TypeCast => out.push_str("::"),
            Token::Concat => out.push_str("||"),
            Token::QualifiedIdent { qualifier, name } => {
                out.push_str(qualifier);
                out.push('.');
                out.push_str(name);
            }
            Token::Param(_) => return None,
        }
    }
    Some(out)
}

/// 字句解析エラー。位置情報はデバッグ用途に限り、応答メッセージへは
/// `allowlist` 側が長さを切り詰めて含める（security.md「情報漏えい」対応）。
#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub byte_offset: usize,
}

/// 字句解析対象のバイト長上限。構造検証自体を線形時間で終わらせるための
/// 防御的上限（security.md「DoS」対応。値レベルの検証は本モジュールの管轄外）。
pub const MAX_INPUT_LEN: usize = 1_048_576;

/// 1 文で許容するトークン数上限。無制限 `Vec` 確保を避けるための防御的上限
/// （security.md「不安全な設計｜無制限リソース確保」対応）。
pub const MAX_TOKEN_COUNT: usize = 20_000;

/// SQL テキストをトークン列へ変換する。`unwrap`/`expect`/添字アクセスを使わず、
/// `chars()` イテレータの先読み（`Peekable`）のみで走査する。`$n` プレースホルダ
/// （Issue #935・WIRE-12）は常に「未対応文字」として拒否する（[`tokenize_with_params`]
/// のみが受理する。両者は [`tokenize_impl`] を共有し、この関数の既存の受理・拒否
/// 契約は一切変えない）。
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    tokenize_impl(input, false)
}

/// [`tokenize`] の `$n` プレースホルダ対応版（Issue #935・WIRE-12・TASK-217）。
/// 拡張クエリプロトコルの Parse（`sql::params`）だけがこの関数を呼ぶ。簡易クエリ
/// プロトコル・許可リスト構造検証（`sql::allowlist`）は引き続き [`tokenize`]
/// （`$` を拒否する既定の字句解析）を使う。
pub fn tokenize_with_params(input: &str) -> Result<Vec<Token>, LexError> {
    tokenize_impl(input, true)
}

/// [`tokenize`]／[`tokenize_with_params`] が共有する走査本体。`allow_params` が
/// `false` の場合の受理・拒否契約は本分割の前と完全に同一（`$` は下の「未対応
/// 文字」へフォールスルーする）。
fn tokenize_impl(input: &str, allow_params: bool) -> Result<Vec<Token>, LexError> {
    if input.len() > MAX_INPUT_LEN {
        return Err(LexError {
            message: format!("input too large: {} bytes", input.len()),
            byte_offset: 0,
        });
    }

    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some(&(offset, c)) = chars.peek() {
        // 空白の読み飛ばしはトークンを生成しないため、上限判定より先に処理する。
        // 先に判定すると、ちょうど MAX_TOKEN_COUNT 個のトークンを生成する入力が
        // 末尾の空白 1 文字の有無だけで成否が変わってしまう（読み飛ばしのみで
        // ループが終わる場合と、空白を読む前に上限判定へ触れてしまう場合の非対称）。
        if c.is_whitespace() {
            chars.next();
            continue;
        }

        // コメントもトークンを生成しないため空白と同じ位置（上限判定より前）で
        // 読み飛ばす。`--`／`/*` の検出は `-`／`/` を `Punct` 化する分岐より必ず先に
        // 行う（順序を入れ替えるとコメントが演算子の列に化ける）。
        if c == '-' && input.get(offset..).is_some_and(|r| r.starts_with("--")) {
            advance_to(&mut chars, line_comment_end(input, offset));
            continue;
        }
        if c == '/' && input.get(offset..).is_some_and(|r| r.starts_with("/*")) {
            let Some(end) = block_comment_end(input, offset) else {
                return Err(LexError {
                    message: "unterminated block comment".to_string(),
                    byte_offset: offset,
                });
            };
            advance_to(&mut chars, end);
            continue;
        }

        if tokens.len() >= MAX_TOKEN_COUNT {
            return Err(LexError {
                message: "too many tokens".to_string(),
                byte_offset: offset,
            });
        }

        // 多文字演算子（SQL-42・Issue #1520）。`--` コメントは上で先に消費済みのため
        // ここへ来る `-` は演算子。最長一致は `->>` → `->` → `-`。空白を挟んだ
        // `- >` は従来どおり 2 トークン。`<->` は `<` と `->` に分かれる（構文段は
        // 変更前後とも 42601 で拒否）。
        if c == '-' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '>'))) {
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '>'))) {
                    lookahead.next();
                    tokens.push(Token::ArrowText);
                } else {
                    tokens.push(Token::Arrow);
                }
                chars = lookahead;
                continue;
            }
            tokens.push(Token::Punct('-'));
            chars.next();
            continue;
        }
        // `#>>` → `#>`。`#` 単独は従来どおり未対応文字として拒否（扱いは #1521）。
        if c == '#' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '>'))) {
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '>'))) {
                    lookahead.next();
                    tokens.push(Token::HashArrowText);
                } else {
                    tokens.push(Token::HashArrow);
                }
                chars = lookahead;
                continue;
            }
        }
        // `::`・`||`。単独の `:`・`|` は未対応文字として拒否する。
        if c == ':' || c == '|' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, d)) if d == c) {
                lookahead.next();
                tokens.push(if c == ':' {
                    Token::TypeCast
                } else {
                    Token::Concat
                });
                chars = lookahead;
                continue;
            }
        }
        if c == '/' {
            tokens.push(Token::Punct('/'));
            chars.next();
            continue;
        }

        // 二重引用符識別子（Issue #1346）。中身は引用符なしでも 1 個の `Ident` に
        // なる語に限る（モジュールドキュメント参照）。それ以外は fail-closed に拒否。
        if c == '"' {
            let Some(end) = quoted_identifier_end(input, offset) else {
                return Err(LexError {
                    message: "unterminated quoted identifier".to_string(),
                    byte_offset: offset,
                });
            };
            let content = input
                .get(offset.saturating_add(1)..end.saturating_sub(1))
                .unwrap_or("");
            if content.is_empty() {
                return Err(LexError {
                    message: "zero-length quoted identifier".to_string(),
                    byte_offset: offset,
                });
            }
            if !is_plain_identifier(content) {
                return Err(LexError {
                    message: "unsupported quoted identifier".to_string(),
                    byte_offset: offset,
                });
            }
            tokens.push(Token::Ident(content.to_string()));
            advance_to(&mut chars, end);
            continue;
        }

        if c == '\'' {
            let (literal, next_offset) = lex_string_literal(input, offset)?;
            tokens.push(Token::StringLiteral(literal));
            advance_to(&mut chars, next_offset);
            continue;
        }

        if c == '<' {
            // 最長一致: `<=>`（距離演算子）→ `<=`（比較演算子）→ `<`（比較演算子）の順で
            // 判定する（TASK-79・SQL-9 で `<=`・裸の `<` を式述語の比較演算子として
            // 追加。`<>` はどの分岐にも一致しないため 2 つの `Punct` トークンに分かれ、
            // 許可リスト（`allowlist::Parser`）側の文法が受理しないことで構造的に
            // 拒否される＝字句解析段階では拒否しない）。
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '='))) {
                lookahead.next();
                if matches!(lookahead.peek(), Some(&(_, '>'))) {
                    lookahead.next();
                    tokens.push(Token::DistanceOp);
                    chars = lookahead;
                    continue;
                }
                tokens.push(Token::Le);
                chars = lookahead;
                continue;
            }
            tokens.push(Token::Punct('<'));
            chars.next();
            continue;
        }

        if c == '!' {
            // `!=` は PostgreSQL では `<>` の別名（Issue #1431・SQL-24 ポインタ）。
            // 字句段で `<>` と同じ 2 つの `Punct` へ写し、構文段の `<>` 文法
            // （`allowlist::Parser`）にそのまま合流させる（新しい `Token` は作らない）。
            // `!` 単独は従来どおり未対応文字として拒否する（fail-closed）。
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '='))) {
                lookahead.next();
                tokens.push(Token::Punct('<'));
                tokens.push(Token::Punct('>'));
                chars = lookahead;
                continue;
            }
            return Err(LexError {
                message: format!("unsupported character: {c:?}"),
                byte_offset: offset,
            });
        }

        if c == '>' {
            // `>=`（比較演算子）→ `>`（比較演算子）の最長一致（TASK-79・SQL-9）。
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, '='))) {
                lookahead.next();
                tokens.push(Token::Ge);
                chars = lookahead;
                continue;
            }
            tokens.push(Token::Punct('>'));
            chars.next();
            continue;
        }

        if matches!(c, '(' | ')' | ',' | '*' | '=' | ';' | '+' | '[' | ']') {
            tokens.push(Token::Punct(c));
            chars.next();
            continue;
        }

        if c.is_ascii_digit() {
            let (number, next_offset) = lex_number(input, offset);
            tokens.push(Token::Number(number));
            advance_to(&mut chars, next_offset);
            continue;
        }

        // `$n`（Issue #935・WIRE-12）: `allow_params` が `false` のとき（既定の
        // `tokenize`）は何もせずこの if を素通りし、下の「未対応文字」へ
        // フォールスルーする（既存契約を変えない）。
        if c == '$' && allow_params {
            let (index, next_offset) = lex_param(input, offset)?;
            tokens.push(Token::Param(index));
            advance_to(&mut chars, next_offset);
            continue;
        }

        // 先頭 `.` の小数リテラル（`.5` 等。TABLE-13〔検討中〕・TASK-197、Issue #885・
        // D5。`NUMERIC` 列の受理文法 `[+-]?(digits)?(\.digits?)?` が「整数部 0 桁」を
        // 許すため、字句解析でも `.` の直後に数字が続く場合は数値トークンの開始として
        // 扱う。数字が続かない孤立した `.`（`a.b` の `QualifiedIdent` 判定対象外の
        // 位置に現れたもの等）は従来どおり許可リスト外の文字として拒否する）。
        if c == '.' {
            let mut lookahead = chars.clone();
            lookahead.next();
            if matches!(lookahead.peek(), Some(&(_, d)) if d.is_ascii_digit()) {
                let (number, next_offset) = lex_number(input, offset);
                tokens.push(Token::Number(number));
                advance_to(&mut chars, next_offset);
                continue;
            }
            return Err(LexError {
                message: format!("unsupported character: {c:?}"),
                byte_offset: offset,
            });
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let (word, next_offset) = lex_word(input, offset);
            // `Token::QualifiedIdent`（Issue #872・SQL-20）: 予約語（`Keyword`）は
            // 修飾子として使わせない（`EXCLUDED` はそもそも `Keyword` 化されて
            // いない語であり、予約語直後の `.` を新たに受理対象へ広げる必要が
            // ないため）。空白を挟まず「`.` の直後に識別子開始文字（英字・`_`）」
            // が続く場合のみ 2 語 1 トークンへまとめる（1 文字先読みで確定させ、
            // `a.`（末尾 `.`）・`a. b`（空白を挟む）を誤って飲み込まない。
            // `lex_number` の小数点先読みと同じ設計）。
            if keyword_from_str(&word).is_none() {
                if let Some(rest) = input.get(next_offset..) {
                    let mut dot_lookahead = rest.char_indices();
                    if matches!(dot_lookahead.next(), Some((_, '.'))) {
                        let name_start_in_rest = dot_lookahead.next();
                        if matches!(name_start_in_rest, Some((_, nc)) if nc.is_ascii_alphabetic() || nc == '_')
                        {
                            let name_start = next_offset + 1;
                            let (name, name_end) = lex_word(input, name_start);
                            tokens.push(Token::QualifiedIdent {
                                qualifier: word,
                                name,
                            });
                            advance_to(&mut chars, name_end);
                            continue;
                        }
                    }
                }
            }
            match keyword_from_str(&word) {
                Some(kw) => tokens.push(Token::Keyword(kw)),
                None => tokens.push(Token::Ident(word)),
            }
            advance_to(&mut chars, next_offset);
            continue;
        }

        return Err(LexError {
            message: format!("unsupported character: {c:?}"),
            byte_offset: offset,
        });
    }

    Ok(tokens)
}

/// `--` で始まる行コメントの終端（`\n`／`\r` の位置。無ければ入力末尾）を返す。
/// `start` は `--` の先頭。改行自体は含めず、呼び出し側が空白として扱う。
/// lexer の読み飛ばしと `sql::statement_splitter::split_statements` が共有する。
pub(crate) fn line_comment_end(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let mut j = start.saturating_add(2);
    while let Some(&c) = bytes.get(j) {
        if c == b'\n' || c == b'\r' {
            break;
        }
        j = j.saturating_add(1);
    }
    j.min(bytes.len())
}

/// `/*` で始まるブロックコメントの終端の直後の位置を返す。PostgreSQL と同じく
/// 入れ子を数える（再帰ではなく深さカウンタ。追加の確保なし）。未終端なら `None`。
/// `start` は `/*` の先頭。lexer と分割器が共有する。
pub(crate) fn block_comment_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut j = start.saturating_add(2);
    let mut depth: usize = 1;
    loop {
        let c = *bytes.get(j)?;
        let n = bytes.get(j.saturating_add(1)).copied();
        if c == b'/' && n == Some(b'*') {
            depth = depth.saturating_add(1);
            j = j.saturating_add(2);
        } else if c == b'*' && n == Some(b'/') {
            depth = depth.saturating_sub(1);
            j = j.saturating_add(2);
            if depth == 0 {
                return Some(j);
            }
        } else {
            j = j.saturating_add(1);
        }
    }
}

/// `"` で始まる二重引用符識別子の閉じ `"` の直後の位置を返す。連続 2 個の `"` は
/// エスケープとして継続する（領域の境界を PostgreSQL と一致させる）。未終端なら
/// `None`。中身の妥当性は検証しない（lexer の `is_plain_identifier` が担う）。
/// `start` は開き `"` の位置。lexer と分割器が共有する。
pub(crate) fn quoted_identifier_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut j = start.saturating_add(1);
    loop {
        let c = *bytes.get(j)?;
        if c == b'"' {
            if bytes.get(j.saturating_add(1)) == Some(&b'"') {
                j = j.saturating_add(2);
                continue;
            }
            return Some(j.saturating_add(1));
        }
        j = j.saturating_add(1);
    }
}

/// 二重引用符識別子の中身が、引用符なしで字句解析しても同じ 1 個の `Ident` に
/// なる語か（空でない・`[A-Za-z_][A-Za-z0-9_]*`・予約語でない）。
fn is_plain_identifier(content: &str) -> bool {
    let mut it = content.chars();
    let Some(first) = it.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && it.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && keyword_from_str(content).is_none()
}

/// 字句解析するとトークンが 1 つも残らない（空白・コメントのみ）SQL か。字句解析に
/// 失敗する入力は `false`（通常の parse 経路でエラーにする。fail-closed）。
/// `wire-server::extended_query` の Parse が、コメントだけの文を空文
/// （EmptyQueryResponse）として扱う判定に使う。`$n` を含み得るため
/// [`tokenize_with_params`] で判定する。
pub fn is_effectively_empty(sql: &str) -> bool {
    matches!(tokenize_with_params(sql), Ok(t) if t.is_empty())
}

/// `chars` イテレータを `byte_offset` の直前まで読み飛ばす。文字列リテラル・数値・
/// 識別子の走査を個別のバイトオフセットベース関数（`lex_string_literal` 等）で
/// 行った後、メインループの `Peekable<CharIndices>` を同じ位置まで同期させるために使う。
fn advance_to(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, byte_offset: usize) {
    while let Some(&(offset, _)) = chars.peek() {
        if offset >= byte_offset {
            break;
        }
        chars.next();
    }
}

/// `'...'` 文字列リテラルを読む。`''` を単一の `'` へのエスケープとして扱う。
/// 閉じ引用符が見つからない場合は `Err`。リテラルの内容自体の意味論的妥当性は
/// 検証しない（本レイヤは文字列として正しく閉じているかのみを構造的に見る）。
fn lex_string_literal(input: &str, start: usize) -> Result<(String, usize), LexError> {
    let bytes = input.as_bytes();
    // start は呼び出し元で確認済みの `'` の位置。
    let mut idx = start + 1;
    let mut content = String::new();
    loop {
        let Some(&b) = bytes.get(idx) else {
            return Err(LexError {
                message: "unterminated string literal".to_string(),
                byte_offset: start,
            });
        };
        if b == b'\'' {
            // 直後がもう 1 つの `'` ならエスケープ（リテラル内の `'` 1 文字）。
            if bytes.get(idx + 1) == Some(&b'\'') {
                content.push('\'');
                idx += 2;
                continue;
            }
            return Ok((content, idx + 1));
        }
        // マルチバイト文字を安全に取り出す（添字直接アクセスをせず char_indices 経由）。
        let rest = input.get(idx..).ok_or_else(|| LexError {
            message: "invalid string literal encoding".to_string(),
            byte_offset: idx,
        })?;
        let ch = rest.chars().next().ok_or_else(|| LexError {
            message: "invalid string literal encoding".to_string(),
            byte_offset: idx,
        })?;
        content.push(ch);
        idx += ch.len_utf8();
    }
}

/// `input.get(start..)` が `None` を返す（`start` が文字境界でない・範囲外）ことは
/// 呼び出し元がメインループで先読みした ASCII 文字境界の直後からしか呼ばないため
/// 通常あり得ないが、untrusted 入力経路では添字直接アクセス（`input[start..]`）を
/// 使わず `get()` で明示的に処理する（coding-rust.md）。
/// TASK-79・SQL-9: 整数に加え `<digits>.<digits>` の小数リテラルを 1 トークンとして
/// 認識する。TABLE-13〔検討中〕・TASK-197、Issue #885・D5 で `NUMERIC` 列の受理文法
/// `[+-]?(digits)?(\.digits?)?` に合わせ、先頭 `.`（`.5`。呼び出し元のメインループが
/// `.` の直後に数字が続く場合にこの関数を呼ぶ）・末尾 `.`（`1.`。整数部を 1 桁以上
/// 消費済みなら小数部が空でも `.` を消費する）も 1 トークンとして受理するよう拡張した。
/// 2 個目以降の `.`（`1..2`）は本関数が最初の `.` を消費した時点で走査を止めるため
/// 対象外のまま（残った `.` は新たな数値トークンの開始、または「未対応文字」として
/// `tokenize` のメインループが扱う）。
/// Issue #1187 で指数部（`e`／`E`・任意の符号・1 桁以上の数字）も同じ数値トークンとして
/// 読むよう拡張した（`REAL`／`DOUBLE PRECISION` 列の `1.5e3` 受理と揃える）。
/// 指数部に数字が無い `1e`／`1e+` は指数部を消費せず、残った `e` を後続トークンとして
/// 扱わせる（数値の直後に識別子が続く形は構文エラーになり PostgreSQL の拒否と一致する）。
fn lex_number(input: &str, start: usize) -> (String, usize) {
    let Some(rest) = input.get(start..) else {
        return (String::new(), start);
    };
    let mut chars = rest.char_indices().peekable();
    let mut end = 0usize;
    while let Some(&(_, c)) = chars.peek() {
        if c.is_ascii_digit() {
            end += c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    // 小数点は「直前に整数部の桁を 1 桁以上消費済み（`1.` 形）」または
    // 「直後に少なくとも 1 桁の数字が続く（`1.5`／`.5` 形）」場合のみ消費する
    // （1 文字先読みで確定させ、孤立した `.`〔`1..2` の 2 個目〕を誤って
    // 飲み込まない）。
    if let Some(&(_, '.')) = chars.peek() {
        let mut lookahead = chars.clone();
        lookahead.next();
        let next_is_digit = matches!(lookahead.peek(), Some(&(_, d)) if d.is_ascii_digit());
        if end > 0 || next_is_digit {
            end += '.'.len_utf8();
            chars.next();
            while let Some(&(_, c)) = chars.peek() {
                if c.is_ascii_digit() {
                    end += c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
        }
    }
    // 指数部: `e`／`E` + 任意の `+`／`-` + 1 桁以上の数字が揃った場合のみ消費する
    // （仮読みで確定させ、揃わない場合は `end` を進めない）。整数部・小数部が共に空
    // （`e5` 等）はメインループが識別子として扱うためここには来ない。
    if end > 0 && matches!(chars.peek(), Some(&(_, 'e' | 'E'))) {
        let mut probe = chars.clone();
        let mut exp_len = 'e'.len_utf8();
        probe.next();
        if let Some(&(_, sign @ ('+' | '-'))) = probe.peek() {
            exp_len += sign.len_utf8();
            probe.next();
        }
        let mut digits = 0usize;
        while let Some(&(_, c)) = probe.peek() {
            if c.is_ascii_digit() {
                exp_len += c.len_utf8();
                digits += 1;
                probe.next();
            } else {
                break;
            }
        }
        if digits > 0 {
            end += exp_len;
        }
    }
    let word = rest.get(..end).unwrap_or_default().to_string();
    (word, start + end)
}

/// `s` 全体が字句解析器の数値トークン 1 個（符号なし）と完全に一致するかを返す
/// （Issue #1406。`sql::params` の REAL／DOUBLE 型付き束縛が、値をリテラルで書いた
/// SQL と同一の `Token::Number` だけを受理するために使う。第 2 の数値文法を作らず
/// `lex_number` の規則を再利用する）。メインループの進入条件（先頭が数字、または
/// `.` の直後が数字）を満たし、かつ `lex_number` が `s` を丸ごと消費する場合のみ真。
pub(crate) fn is_single_number_literal(s: &str) -> bool {
    let mut chars = s.chars();
    let starts_number = match chars.next() {
        Some(c) if c.is_ascii_digit() => true,
        Some('.') => matches!(chars.next(), Some(d) if d.is_ascii_digit()),
        _ => false,
    };
    if !starts_number {
        return false;
    }
    let (_, end) = lex_number(s, 0);
    end == s.len()
}

/// `$<digits>`（Issue #935・WIRE-12）を読み取り、1 始まりのパラメータ番号を返す。
/// 呼び出し元は `$` の位置（`start`）を確認済み。`sql::allowlist::Parser` は
/// この形状を一切知らず、`sql::params` が Bind 時に置換するトークンとしてのみ
/// 消費する。
///
/// - 数字が 1 桁も続かない（`$`・`$a`・`$$`）／先頭ゼロ（`$01`）／`$0` 自体は
///   いずれも `LexError`（呼び出し元 `sql::allowlist` 経由で `42601`）。
/// - 数字の直後に識別子継続文字（英数字・`_`）が続く形（`$1a`）も曖昧な形として
///   `LexError` にする（1 文字先読みで確定させる。`lex_word` の `.` 先読みと同じ
///   設計）。
/// - 桁数字が `u16::MAX` を超える場合は `u16::MAX` へ飽和させる（未定義動作にせず
///   `sql::params::MAX_PARAMS`（64）超過は呼び出し元が別途 `54000` で拒否するため、
///   ここでの飽和は安全側に倒すだけでよい）。
fn lex_param(input: &str, start: usize) -> Result<(u16, usize), LexError> {
    let Some(rest) = input.get(start + 1..) else {
        return Err(LexError {
            message: "malformed parameter placeholder".to_string(),
            byte_offset: start,
        });
    };
    let mut chars = rest.char_indices().peekable();
    let mut digits_end = 0usize;
    while let Some(&(_, c)) = chars.peek() {
        if c.is_ascii_digit() {
            digits_end += c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    if digits_end == 0 {
        return Err(LexError {
            message: "expected digits after $".to_string(),
            byte_offset: start,
        });
    }
    let digits = rest.get(..digits_end).unwrap_or_default();
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(LexError {
            message: "parameter placeholder must not have a leading zero".to_string(),
            byte_offset: start,
        });
    }
    if let Some(&(_, c)) = chars.peek() {
        if c.is_ascii_alphanumeric() || c == '_' {
            return Err(LexError {
                message: "malformed parameter placeholder".to_string(),
                byte_offset: start,
            });
        }
    }
    let mut value: u32 = 0;
    for b in digits.bytes() {
        // `digits` は上のループで ASCII 数字のみを集めたバイト列のため
        // `b - b'0'` は必ず 0..=9 に収まる。
        let d = u32::from(b.saturating_sub(b'0'));
        value = value.saturating_mul(10).saturating_add(d);
        if value > u32::from(u16::MAX) {
            value = u32::from(u16::MAX);
        }
    }
    if value == 0 {
        return Err(LexError {
            message: "$0 is not a valid parameter placeholder".to_string(),
            byte_offset: start,
        });
    }
    let end = start + 1 + digits_end;
    Ok((value as u16, end))
}

fn lex_word(input: &str, start: usize) -> (String, usize) {
    let Some(rest) = input.get(start..) else {
        return (String::new(), start);
    };
    let mut end = 0usize;
    for c in rest.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            end += c.len_utf8();
        } else {
            break;
        }
    }
    let word = rest.get(..end).unwrap_or_default().to_string();
    (word, start + end)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bang_equal_lexes_as_not_equal_punct_pair() {
        // Issue #1431: `!=` は `<>` と同じトークン列。`!` 単独・文字列内の `!` は影響しない。
        let ne = tokenize("a != 'x'").expect("tokenize");
        assert_eq!(ne, tokenize("a <> 'x'").expect("tokenize"));
        assert!(tokenize("a ! 'x'").is_err());
        assert!(tokenize("a !").is_err());
        assert_eq!(
            tokenize("'a!=b'").expect("tokenize"),
            vec![Token::StringLiteral("a!=b".to_string())]
        );
    }

    use super::is_single_number_literal;

    #[test]
    fn single_number_literal_accepts_and_rejects() {
        for ok in ["1", "1.5", ".5", "1.", "1.5e3", "1E-3", "3.e+4", "0"] {
            assert!(is_single_number_literal(ok), "{ok}");
        }
        for ng in [
            "",
            "+1",
            " 1",
            "1 ",
            "-1",
            "1e",
            "1e+",
            "1.5.3",
            "NaN",
            "Infinity",
            ".",
            "e5",
            "1a",
            "0x10",
            "1.5 OR 1=1",
        ] {
            assert!(!is_single_number_literal(ng), "{ng}");
        }
    }

    /// TABLE-18・Issue #1192: 描画 → 再トークン化で同一トークン列に戻る。
    #[test]
    fn render_tokens_round_trips() {
        let cases = [
            "SELECT lang, COUNT(*), SUM(n) FROM docs WHERE a = 'it''s' GROUP BY lang HAVING COUNT(*) >= 1.5 ORDER BY lang DESC LIMIT 10 OFFSET 5",
            "select * from a inner join b on a.id = b.doc_id where a.x < 3 and b.y <= 4 limit 7",
            "SELECT x FROM t WHERE n > - 1 AND m = - - 2 LIMIT 3",
            "SELECT x FROM t WHERE v <=> '[1,2]' LIMIT 3",
            "SELECT x FROM t WHERE a < = 1 LIMIT 3",
            "SELECT x FROM t WHERE a = '' LIMIT 3",
        ];
        for sql in cases {
            let tokens = tokenize(sql).expect("tokenize");
            let rendered = render_tokens(&tokens).expect("render");
            let again = tokenize(&rendered).expect("re-tokenize");
            assert_eq!(tokens, again, "round trip failed for {sql}");
            assert!(!rendered.contains("--"));
        }
    }

    #[test]
    fn render_tokens_rejects_param() {
        assert!(render_tokens(&[Token::Param(1)]).is_none());
    }

    use super::*;

    #[test]
    fn tokenizes_simple_select() {
        let tokens = tokenize("SELECT * FROM docs LIMIT 10").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Punct('*'),
                Token::Keyword(Keyword::From),
                Token::Ident("docs".to_string()),
                Token::Keyword(Keyword::Limit),
                Token::Number("10".to_string()),
            ]
        );
    }

    #[test]
    fn qualified_ident_lexes_excluded_dot_column() {
        // Issue #872・SQL-20: `ON CONFLICT ... DO UPDATE SET` の右辺
        // `EXCLUDED.<col>` の規範形。大小混在（`excluded`）も同様に扱う
        // （照合自体は `allowlist::Parser` 側で大小無視する）。
        let tokens = tokenize("EXCLUDED.embedding").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![Token::QualifiedIdent {
                qualifier: "EXCLUDED".to_string(),
                name: "embedding".to_string(),
            }]
        );
        let tokens = tokenize("excluded.lang").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![Token::QualifiedIdent {
                qualifier: "excluded".to_string(),
                name: "lang".to_string(),
            }]
        );
    }

    #[test]
    fn qualified_ident_rejects_malformed_dot_forms() {
        // 識別子側の拒否形状: `a.`（末尾 `.`）・`a. b`（空白を挟む）・
        // `a.b.c`（3 段。`a.b` を `QualifiedIdent` 化した直後に孤立した `.c` が
        // 残り「未対応文字」として拒否される）はいずれも既存契約のまま。
        assert!(tokenize("a.").is_err());
        assert!(tokenize("a. b").is_err());
        assert!(tokenize("a.b.c").is_err());
    }

    #[test]
    fn dot_digit_after_ident_lexes_as_separate_number_token() {
        // `a.5`（Issue #885・D5 での数値側拡張の副作用）: `.` の直後が数字の
        // ため `QualifiedIdent` の条件（`.` の直後が英字・`_`）には合致せず、
        // `a` は独立した `Ident` になる。続く `.5` は本 Issue で新たに受理する
        // 先頭 `.` の数値リテラルとして字句解析されるため、`tokenize` 自体は
        // エラーにならない（`Ident` の直後に `Number` が続く並びを許可リストの
        // 文法が受理するかどうかは `sql::allowlist::Parser` 側の管轄）。
        let tokens = tokenize("a.5").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("a".to_string()),
                Token::Number(".5".to_string())
            ]
        );
    }

    #[test]
    fn insert_operation_id_words_lex_as_plain_idents() {
        // PR #189 レビュー指摘対応（P1）: INSERT 許可形状・USING OPERATION_ID
        // 文末句が使う 5 語は Token::Keyword 化せず、常に Token::Ident として
        // 字句解析されることを固定する（文脈的キーワード化は
        // `sql::allowlist::Parser` 側の責務）。
        let tokens =
            tokenize("INSERT INTO t VALUES USING OPERATION_ID").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("INSERT".to_string()),
                Token::Ident("INTO".to_string()),
                Token::Ident("t".to_string()),
                Token::Ident("VALUES".to_string()),
                Token::Ident("USING".to_string()),
                Token::Ident("OPERATION_ID".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_using_and_set_as_plain_idents() {
        // TASK-161（SQL-12）: `USING`／`SET` は字句解析の時点ではキーワード化しない
        // （`allowlist` 側が LIMIT 直後／statement 先頭という文脈でのみキーワードとして
        // 扱う。カタログ上有効な識別子としての `using`／`set` を字句解析段階で
        // 破壊的に奪わないための設計）。
        let tokens = tokenize("using MODE 'recall'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("using".to_string()),
                Token::Ident("MODE".to_string()),
                Token::StringLiteral("recall".to_string()),
            ]
        );
        let tokens = tokenize("SET search_mode = 'precision'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("SET".to_string()),
                Token::Ident("search_mode".to_string()),
                Token::Punct('='),
                Token::StringLiteral("precision".to_string()),
            ]
        );
    }

    #[test]
    fn insert_operation_id_words_remain_usable_as_ordinary_identifiers() {
        // PR #189 レビュー指摘対応（P1）: `catalog::validate_identifier` が許可する
        // 同名のテーブル名・列名（例: `values`）が、SELECT 許可形状の
        // `expect_ident` 位置で引き続き受理できることを固定する
        // （`SELECT values FROM documents` が構文破壊しない回帰テスト）。
        let tokens = tokenize("SELECT values FROM documents").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Ident("values".to_string()),
                Token::Keyword(Keyword::From),
                Token::Ident("documents".to_string()),
            ]
        );
    }

    #[test]
    fn using_and_set_remain_valid_identifiers_outside_their_keyword_positions() {
        // P1 修正の回帰: `using`／`set` はテーブル名・列名としての `Ident` 位置
        // （`FROM`・投影・`ORDER BY` 等）で従来どおり使用できる。
        let tokens = tokenize("SELECT using FROM set").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Keyword(Keyword::Select),
                Token::Ident("using".to_string()),
                Token::Keyword(Keyword::From),
                Token::Ident("set".to_string()),
            ]
        );
    }

    #[test]
    fn keyword_matching_is_case_insensitive() {
        let tokens = tokenize("select * from docs limit 1").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Keyword(Keyword::Select));
        assert_eq!(tokens[2], Token::Keyword(Keyword::From));
        assert_eq!(tokens[4], Token::Keyword(Keyword::Limit));
    }

    #[test]
    fn hint_is_a_context_dependent_ident_not_a_reserved_keyword() {
        // HINT ORDER(...) は LIMIT 直後の所定位置でのみ allowlist 側が文脈依存で
        // 認識する語であり、字句解析の時点では常に通常の識別子として扱う
        // （後方互換性: `hint` を列名・テーブル名として使う既存 SQL を拒否しない）。
        let tokens = tokenize("HINT ORDER(RLS)").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Ident("HINT".to_string()));
        assert_eq!(tokens[1], Token::Keyword(Keyword::Order));

        let tokens = tokenize("hint order(rls)").expect("tokenize should succeed");
        assert_eq!(tokens[0], Token::Ident("hint".to_string()));
    }

    #[test]
    fn tokenizes_distance_operator() {
        let tokens = tokenize("embedding <=> 'x'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("embedding".to_string()),
                Token::DistanceOp,
                Token::StringLiteral("x".to_string()),
            ]
        );
    }

    #[test]
    fn string_literal_handles_escaped_quote() {
        let tokens = tokenize("'it''s'").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::StringLiteral("it's".to_string())]);
    }

    #[test]
    fn rejects_unterminated_string_literal() {
        assert!(tokenize("'abc").is_err());
    }

    /// SQL-42・Issue #1520: 多文字演算子の字句化（最長一致・境界）。
    #[test]
    fn multichar_operators_lex_as_single_tokens() {
        use Token::*;
        let a = || ident("a");
        let b = || ident("b");
        assert_eq!(toks("a -> b"), vec![a(), Arrow, b()]);
        assert_eq!(toks("a ->> b"), vec![a(), ArrowText, b()]);
        assert_eq!(toks("a #> b"), vec![a(), HashArrow, b()]);
        assert_eq!(toks("a #>> b"), vec![a(), HashArrowText, b()]);
        assert_eq!(toks("a::b"), vec![a(), TypeCast, b()]);
        assert_eq!(toks("a || b"), vec![a(), Concat, b()]);
        assert_eq!(toks("a->>b"), vec![a(), ArrowText, b()]);
        assert_eq!(toks("a->b"), vec![a(), Arrow, b()]);
        assert_eq!(toks("a#>>b"), vec![a(), HashArrowText, b()]);
        assert_eq!(toks("a#>b"), vec![a(), HashArrow, b()]);
        assert_eq!(toks("->>>"), vec![ArrowText, Punct('>')]);
        assert_eq!(toks("#>>>"), vec![HashArrowText, Punct('>')]);
        assert_eq!(toks("a<->b"), vec![a(), Punct('<'), Arrow, b()]);
        assert_eq!(
            toks("1::int"),
            vec![Number("1".into()), TypeCast, ident("int")]
        );
        assert_eq!(
            tokenize_with_params("$1::int").expect("tokenize"),
            vec![Param(1), TypeCast, ident("int")]
        );
    }

    /// SQL-42・Issue #1520: 空白・コメント・文字列・単独文字の境界は従来どおり。
    #[test]
    fn multichar_operator_boundaries_keep_legacy_behavior() {
        use Token::*;
        let a = || ident("a");
        let b = || ident("b");
        assert_eq!(toks("a - > b"), vec![a(), Punct('-'), Punct('>'), b()]);
        assert_eq!(toks("a - >b"), vec![a(), Punct('-'), Punct('>'), b()]);
        assert_eq!(toks("a - -> b"), vec![a(), Punct('-'), Arrow, b()]);
        assert_eq!(toks("a-->b"), vec![a()]);
        assert_eq!(toks("a /*->*/ b"), vec![a(), b()]);
        assert_eq!(toks("a->/*c*/>b"), vec![a(), Arrow, Punct('>'), b()]);
        for lit in ["a->b", "x||y", "::", "#>>"] {
            assert_eq!(toks(&format!("'{lit}'")), vec![StringLiteral(lit.into())]);
        }
        for bad in ["a # b", "a : b", "a | b", "a:::b", "a|||b"] {
            assert!(tokenize(bad).is_err(), "{bad} should be rejected");
        }
    }

    /// SQL-42・Issue #1520: 新トークンの描画 → 再字句化で同一列に戻る（最小ケース）。
    #[test]
    fn render_tokens_round_trips_multichar_operators() {
        let sql = "a -> b ->> c #> d #>> e :: f || g - > h";
        let tokens = tokenize(sql).expect("tokenize");
        let rendered = render_tokens(&tokens).expect("render");
        assert_eq!(tokenize(&rendered).expect("re-tokenize"), tokens);
    }

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql).expect("tokenize should succeed")
    }

    fn ident(s: &str) -> Token {
        Token::Ident(s.to_string())
    }

    #[test]
    fn skips_line_comment() {
        assert_eq!(
            toks("SELECT * FROM docs -- comment"),
            toks("SELECT * FROM docs")
        );
        assert_eq!(toks("SELECT -- c\n 1"), toks("SELECT 1"));
        assert_eq!(toks("SELECT -- c\r 1"), toks("SELECT 1"));
        assert_eq!(toks("-- only"), vec![]);
    }

    #[test]
    fn skips_nested_block_comment() {
        assert_eq!(
            toks("SELECT /* c */ * FROM docs"),
            toks("SELECT * FROM docs")
        );
        assert_eq!(toks("SELECT /* a /* b */ c */ 1"), toks("SELECT 1"));
        assert_eq!(toks("/* a ; b */"), vec![]);
    }

    #[test]
    fn rejects_unterminated_block_comment() {
        assert!(tokenize("SELECT /* c").is_err());
        assert!(tokenize("SELECT /* a /* b */ c").is_err());
        assert!(tokenize("/*/").is_err());
    }

    #[test]
    fn comment_detection_precedes_operator_tokens() {
        assert_eq!(toks("1--1"), vec![Token::Number("1".to_string())]);
        assert_eq!(toks("1 - -1").len(), 4);
        assert_eq!(
            toks("a */ b"),
            vec![ident("a"), Token::Punct('*'), Token::Punct('/'), ident("b")]
        );
    }

    #[test]
    fn comment_contents_are_ignored() {
        assert_eq!(toks("SELECT 1 -- ' \" $1"), toks("SELECT 1"));
        assert_eq!(
            tokenize_with_params("SELECT 1 -- $1").expect("ok"),
            toks("SELECT 1")
        );
        assert_eq!(toks("SELECT /* ' \" */ 1"), toks("SELECT 1"));
    }

    #[test]
    fn comment_markers_inside_string_literal_are_content() {
        assert_eq!(
            toks("'a -- b /* c'"),
            vec![Token::StringLiteral("a -- b /* c".to_string())]
        );
    }

    #[test]
    fn comments_do_not_count_toward_token_limit() {
        let base = "*".repeat(MAX_TOKEN_COUNT);
        assert!(tokenize(&format!("{base}/* c */")).is_ok());
        assert!(tokenize(&format!("{base}-- c")).is_ok());
    }

    #[test]
    fn accepts_plain_quoted_identifier() {
        assert_eq!(toks("SELECT * FROM \"docs\""), toks("SELECT * FROM docs"));
        assert_eq!(toks("\"Docs\""), vec![ident("Docs")]);
        assert_eq!(toks("\"_a1\""), vec![ident("_a1")]);
    }

    #[test]
    fn rejects_unsafe_quoted_identifiers() {
        for sql in [
            "\"\"",
            "\"a b\"",
            "\"a\"\"b\"",
            "\"\u{e9}\"",
            "\"1a\"",
            "\"select\"",
            "\"SELECT\"",
            "\"Limit\"",
            "\"abc",
            "\"a.b\"",
            "\"a;b\"",
            "\"a--\"",
        ] {
            assert!(tokenize(sql).is_err(), "should reject: {sql}");
        }
    }

    #[test]
    fn quoted_identifier_error_does_not_leak_content() {
        let err = tokenize("\"a b\"").expect_err("rejected");
        assert!(!err.message.contains('a'));
    }

    #[test]
    fn is_effectively_empty_cases() {
        assert!(is_effectively_empty(""));
        assert!(is_effectively_empty("  -- c"));
        assert!(is_effectively_empty("/* c */ -- d"));
        assert!(!is_effectively_empty("SELECT 1"));
        assert!(!is_effectively_empty("/* unterminated"));
        assert!(!is_effectively_empty("'x"));
    }

    #[test]
    fn rejects_dollar_parameter_placeholder() {
        assert!(tokenize("embedding <=> $1").is_err());
    }

    // --- tokenize_with_params（Issue #935・WIRE-12）---------------------------

    #[test]
    fn tokenize_with_params_accepts_placeholder() {
        let tokens =
            tokenize_with_params("embedding <=> $1").expect("tokenize_with_params should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("embedding".to_string()),
                Token::DistanceOp,
                Token::Param(1),
            ]
        );
    }

    #[test]
    fn tokenize_with_params_parses_max_and_saturates_beyond_u16_max() {
        let tokens = tokenize_with_params("$64").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Param(64)]);

        let tokens = tokenize_with_params("$65").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Param(65)]);

        // 桁数字が u16::MAX を超える巨大な入力でも panic せず飽和する
        // （オーバーフローを未定義動作にしない。coding-rust.md）。
        let huge = format!("${}", "9".repeat(30));
        let tokens = tokenize_with_params(&huge).expect("tokenize should saturate, not panic");
        assert_eq!(tokens, vec![Token::Param(u16::MAX)]);
    }

    #[test]
    fn tokenize_with_params_rejects_malformed_placeholders() {
        assert!(tokenize_with_params("$").is_err());
        assert!(tokenize_with_params("$$").is_err());
        assert!(tokenize_with_params("$a").is_err());
        assert!(tokenize_with_params("$0").is_err());
        assert!(tokenize_with_params("$01").is_err());
        assert!(tokenize_with_params("$1a").is_err());
    }

    #[test]
    fn tokenize_with_params_treats_dollar_digit_inside_string_literal_as_literal_text() {
        let tokens =
            tokenize_with_params("'$1'").expect("string literal content is not a placeholder");
        assert_eq!(tokens, vec![Token::StringLiteral("$1".to_string())]);
    }

    #[test]
    fn tokenize_without_params_still_rejects_dollar_even_via_shared_impl() {
        // `tokenize`（`allow_params = false`）は `tokenize_impl` 分割の前後で
        // 挙動が完全に不変であることを固定する（既存
        // `rejects_dollar_parameter_placeholder` の重複確認）。
        assert!(tokenize("$1").is_err());
    }

    #[test]
    fn accepts_lone_less_than_as_comparison_operator() {
        // TASK-79・SQL-9: 式述語の比較演算子として単独の `<` を受理するようになった
        // （旧 `rejects_lone_less_than` の置き換え。構文上その位置を受理するかどうかは
        // `allowlist::Parser` の管轄で、本テストは字句解析段階の受理のみを確認する）。
        let tokens = tokenize("a < b").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("a".to_string()),
                Token::Punct('<'),
                Token::Ident("b".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_arithmetic_and_comparison_operators() {
        // TASK-79・SQL-9: `+ - / > < >= <=` を式演算子として追加。`<=>`（距離演算子）
        // との最長一致・`*`（乗算と SELECT * の両方に使う既存トークン）を確認する。
        let tokens = tokenize("1.5 + 2 - 3 * 4 / 5 > 6 >= 7 < 8 <= 9 <=> 10")
            .expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Number("1.5".to_string()),
                Token::Punct('+'),
                Token::Number("2".to_string()),
                Token::Punct('-'),
                Token::Number("3".to_string()),
                Token::Punct('*'),
                Token::Number("4".to_string()),
                Token::Punct('/'),
                Token::Number("5".to_string()),
                Token::Punct('>'),
                Token::Number("6".to_string()),
                Token::Ge,
                Token::Number("7".to_string()),
                Token::Punct('<'),
                Token::Number("8".to_string()),
                Token::Le,
                Token::Number("9".to_string()),
                Token::DistanceOp,
                Token::Number("10".to_string()),
            ]
        );
    }

    #[test]
    fn tokenizes_decimal_number_literal() {
        let tokens = tokenize("2.0").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Number("2.0".to_string())]);
    }

    #[test]
    fn accepts_trailing_dot_number_literal() {
        // TABLE-13〔検討中〕・TASK-197、Issue #885・D5（PR #1020 codex-review
        // 指摘対応）: `NUMERIC` 列の受理文法 `[+-]?(digits)?(\.digits?)?` に
        // 合わせ、`1.` は整数部 `1` と小数部 0 桁の 1 トークン `Number("1.")`
        // として受理する（従来は残った `.` が「未対応文字」として拒否されていた）。
        let tokens = tokenize("1. ").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Number("1.".to_string())]);
    }

    #[test]
    fn accepts_leading_dot_number_literal() {
        // 同上（D5）: `.5` は整数部 0 桁・小数部 `5` の 1 トークン
        // `Number(".5")` として受理する。
        let tokens = tokenize(".5").expect("tokenize should succeed");
        assert_eq!(tokens, vec![Token::Number(".5".to_string())]);
    }

    #[test]
    fn double_dot_number_literal_splits_into_two_number_tokens() {
        // `1..2` は字句解析エラーにはならない（`lex_number` は最初の `.` を
        // 消費した時点で走査を止めるため）。`Number("1.")` と `Number(".2")` の
        // 2 トークンに分かれ、複数の `.` を持つ単一の数値リテラルとしては
        // 構造的に組み立たない。VALUES リストの 1 要素は 1 リテラルのみを
        // 期待するため、この 2 トークン形は許可リスト（`sql::allowlist::Parser`）
        // 側で構文エラーとして拒否される（字句解析層の担当外）。
        let tokens = tokenize("1..2").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Number("1.".to_string()),
                Token::Number(".2".to_string())
            ]
        );
    }

    #[test]
    fn rejects_input_exceeding_max_len() {
        let huge = "a".repeat(MAX_INPUT_LEN + 1);
        assert!(tokenize(&huge).is_err());
    }

    #[test]
    fn accepts_exactly_max_token_count_without_trailing_whitespace() {
        let input = "*".repeat(MAX_TOKEN_COUNT);
        let tokens = tokenize(&input).expect("exactly MAX_TOKEN_COUNT tokens should be accepted");
        assert_eq!(tokens.len(), MAX_TOKEN_COUNT);
    }

    #[test]
    fn accepts_exactly_max_token_count_with_trailing_whitespace() {
        // 上限判定は空白の読み飛ばしより後に行うため、ちょうど MAX_TOKEN_COUNT 個の
        // トークンを生成する入力は末尾空白の有無に関係なく同じ結果になる。
        let input = format!("{} ", "*".repeat(MAX_TOKEN_COUNT));
        let tokens = tokenize(&input).expect("trailing whitespace must not affect the boundary");
        assert_eq!(tokens.len(), MAX_TOKEN_COUNT);
    }

    #[test]
    fn rejects_more_than_max_token_count() {
        let input = "*".repeat(MAX_TOKEN_COUNT + 1);
        assert!(tokenize(&input).is_err());
    }

    #[test]
    fn handles_multibyte_characters_in_string_literal_without_panicking() {
        let tokens = tokenize("'日本語データ'").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![Token::StringLiteral("日本語データ".to_string())]
        );
    }

    #[test]
    fn does_not_panic_on_truncated_utf8_like_input() {
        // 不正なバイト列を意図的に含む入力でも panic せず Err を返すことを確認する
        // （String は常に有効な UTF-8 のため、ここでは実用上あり得るケースとして
        // 閉じられない文字列リテラル中に非 ASCII 文字を混在させる形で検証する）。
        let input = "'\u{e9}\u{e9}";
        assert!(tokenize(input).is_err());
    }

    #[test]
    fn tokenize_reads_exponent_as_part_of_number() {
        // Issue #1187: 指数部（e/E・任意の符号・数字）を同じ数値トークンとして読む。
        for lit in ["1.5e3", "1E3", "2e-2", "3.e+4", ".5e1", "10e0"] {
            let tokens = tokenize(lit).expect("tokenize should succeed");
            assert_eq!(tokens, vec![Token::Number(lit.to_string())], "{lit}");
        }
        let tokens = tokenize("1.5e3, 2").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Number("1.5e3".to_string()),
                Token::Punct(','),
                Token::Number("2".to_string())
            ]
        );
    }

    #[test]
    fn tokenize_does_not_consume_incomplete_exponent() {
        // 指数部に数字が無い `1e`／`1e+` は指数部を消費せず、`e` は識別子として残る
        // （後段のパーサが構文エラーとして拒否する）。
        let tokens = tokenize("1e").expect("tokenize should succeed");
        assert_eq!(tokens.first(), Some(&Token::Number("1".to_string())));
        assert_eq!(tokens.len(), 2);
        let tokens = tokenize("1e+").expect("tokenize should succeed");
        assert_eq!(tokens.first(), Some(&Token::Number("1".to_string())));
        assert!(tokens.len() >= 2);
    }

    #[test]
    fn tokenize_reads_square_brackets_as_punct() {
        // Issue #1348: 配列型 `<型>[N]` の宣言用に `[`／`]` を `Punct` として字句化する。
        let tokens = tokenize("INTEGER[]").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("INTEGER".to_string()),
                Token::Punct('['),
                Token::Punct(']')
            ]
        );
        let tokens = tokenize("TEXT[3]").expect("tokenize should succeed");
        assert_eq!(
            tokens,
            vec![
                Token::Ident("TEXT".to_string()),
                Token::Punct('['),
                Token::Number("3".to_string()),
                Token::Punct(']')
            ]
        );
    }
}
