//! 埋め込み SQL・式のトークン列に対する純粋な補助処理と、定義時ダミー検証（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が切り出した SQL トークン列に対して、`INTO` 句の除去・識別子位置の分類・
//! 本体内で禁止する句（`USING OPERATION_ID`／`PARTITIONED`。ADR D8）の検出・ダミートークンの選択を行う。
//! 検証の実体（許可リスト解析）は `EmbeddedSqlChecker` を介して core が実装する
//! （`core::plpgsql_validate`）。`plpgsql` はストレージにもカタログにも触れない。
//! [`is_ident_position`] は #2226 の呼び出し時の値置換でも同じ分類を共有する。
//! ADR D3: 定義時はダミー、呼び出し時は型付きの値から作ったトークンで置換し、SQL テキストへ連結しない。

use std::collections::HashMap;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::{parse_column_type_name, SqlColumnTypeName};
use crate::sql::lexer::{Keyword, Token};

use super::ast::{Block, Body, DeclType, EmbeddedKind, EmbeddedSql, SqlExpr, Stmt, VarRef};
use super::{oos_err, syn_err};

/// `Ident` が（大小無視で）`word` か。
pub(super) fn ident_is(t: &Token, word: &str) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(word))
}

/// キーワード判定用の「見え方」を作る。二重引用符付き識別子（`quoted[i]`）の内容の先頭に `"` を付け、
/// [`ident_is`] が決して一致しないようにする。SQL 字句解析器は `"into"` も `into` も `Ident("into")`
/// に潰すため、これが無いと列名・別名の `"into"`／`"partitioned"` を句のキーワードと取り違える。
/// 長さと添字は元の `tokens` と同一（構造の判定専用で、名前の取り出しには元の `tokens` を使う）。
/// `split_into`・`reject_forbidden_clauses`・`IdentCtx::new`・`classify_*` が内部で使う。
pub(super) fn keyword_view(tokens: &[Token], quoted: &[bool]) -> Vec<Token> {
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| match t {
            Token::Ident(s) if quoted.get(i).copied().unwrap_or(false) => {
                Token::Ident(format!("\"{s}"))
            }
            other => other.clone(),
        })
        .collect()
}

/// `INTO` 句の生の対象（修飾子, 名前）。
pub(super) struct RawInto {
    pub(super) strict: bool,
    pub(super) targets: Vec<(Option<String>, String)>,
}

/// [`split_into`] の結果（INTO 区間を除いたトークン列・同じ長さの引用情報・取り出した INTO 句）。
pub(super) type SplitInto = (Vec<Token>, Vec<bool>, Option<RawInto>);

/// 括弧の深さ 0 の文脈キーワード `INTO` と対象列を取り除く。`INSERT INTO` の `INTO` は対象外。
/// 引用付きの `"into"`（列名・別名）は句ではない。`INTO` が 2 個あれば `42601`。
/// 戻り値のトークン列と引用情報は常に同じ長さ（取り除いた区間は両方から落とす）。
pub(super) fn split_into(
    mut tokens: Vec<Token>,
    mut quoted: Vec<bool>,
) -> Result<SplitInto, SqlSurfaceError> {
    let view = keyword_view(&tokens, &quoted);
    let mut depth = 0usize;
    let mut found: Option<usize> = None;
    for (i, t) in view.iter().enumerate() {
        match t {
            Token::Punct('(') | Token::Punct('[') => depth = depth.saturating_add(1),
            Token::Punct(')') | Token::Punct(']') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && ident_is(t, "INTO") {
            let after_insert = i
                .checked_sub(1)
                .and_then(|p| view.get(p))
                .is_some_and(|p| ident_is(p, "INSERT"));
            if after_insert {
                continue;
            }
            if found.is_some() {
                return Err(syn_err("multiple INTO clauses in a PL/pgSQL statement"));
            }
            found = Some(i);
        }
    }
    let Some(at) = found else {
        return Ok((tokens, quoted, None));
    };
    let mut j = at.saturating_add(1);
    let mut strict = false;
    if view.get(j).is_some_and(|t| ident_is(t, "STRICT")) {
        strict = true;
        j = j.saturating_add(1);
    }
    let mut targets = Vec::new();
    loop {
        match tokens.get(j) {
            Some(Token::Ident(n)) => targets.push((None, n.clone())),
            Some(Token::QualifiedIdent { qualifier, name }) => {
                targets.push((Some(qualifier.clone()), name.clone()))
            }
            _ => return Err(syn_err("invalid INTO target in a PL/pgSQL statement")),
        }
        j = j.saturating_add(1);
        if matches!(tokens.get(j), Some(Token::Punct(','))) {
            j = j.saturating_add(1);
        } else {
            break;
        }
    }
    tokens.drain(at..j.min(tokens.len()));
    quoted.drain(at..j.min(quoted.len()));
    debug_assert_eq!(tokens.len(), quoted.len());
    Ok((tokens, quoted, Some(RawInto { strict, targets })))
}

/// 本体の中で禁止する句（`USING OPERATION_ID`・括弧の深さ 0 の `PARTITIONED`）を `0A000` で拒否する。
/// 引用付きの `"partitioned"`・`"operation_id"`（列名・別名）は句ではないので拒否しない。
pub(super) fn reject_forbidden_clauses(
    tokens: &[Token],
    quoted: &[bool],
) -> Result<(), SqlSurfaceError> {
    let tokens = keyword_view(tokens, quoted);
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') | Token::Punct('[') => depth = depth.saturating_add(1),
            Token::Punct(')') | Token::Punct(']') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if ident_is(t, "USING")
            && tokens
                .get(i.saturating_add(1))
                .is_some_and(|n| ident_is(n, "OPERATION_ID"))
        {
            return Err(oos_err(
                "USING OPERATION_ID is not supported inside a PL/pgSQL body",
            ));
        }
        if depth == 0 && ident_is(t, "PARTITIONED") {
            return Err(oos_err(
                "PARTITIONED is not supported inside a PL/pgSQL body",
            ));
        }
    }
    Ok(())
}

/// `INSERT INTO t ( cols ) ...` の列リスト（`(` の次から `)` の手前まで）の範囲。
/// `start` は `INSERT` トークンの位置。`close_of` は [`ParenInfo`] の対応括弧表（線形時間化のため）。
fn insert_column_list_at(
    tokens: &[Token],
    start: usize,
    close_of: &[Option<usize>],
) -> Option<std::ops::Range<usize>> {
    if !tokens.get(start).is_some_and(|t| ident_is(t, "INSERT")) {
        return None;
    }
    if !tokens
        .get(start.saturating_add(1))
        .is_some_and(|t| ident_is(t, "INTO"))
    {
        return None;
    }
    // 表名は `Ident` または `QualifiedIdent` の 1 トークン。
    let open = start.saturating_add(3);
    if !matches!(tokens.get(open), Some(Token::Punct('('))) {
        return None;
    }
    let first_at = open.saturating_add(1);
    let first = tokens.get(first_at)?;
    if matches!(first, Token::Keyword(Keyword::Select))
        || ident_is(first, "VALUES")
        || ident_is(first, "WITH")
    {
        return None;
    }
    let close = (*close_of.get(open)?)?;
    Some(first_at..close)
}

/// 括弧・CTE 列名リスト・INSERT 列リスト位置の事前計算結果。識別子ごとの前方／後方再走査
/// （二乗コスト）を避けるため、文ごとに線形走査で求める。
pub(super) struct ParenInfo {
    /// 各トークンを直接囲む `(` の 1 つ前のトークン位置（囲む括弧が無い／括弧が先頭なら `None`）。
    encl_prev: Vec<Option<usize>>,
    /// 各トークンを直接囲む `(` のトークン位置（囲む括弧が無ければ `None`）。
    encl_open: Vec<Option<usize>>,
    /// 各トークンが CTE 列名リスト `name (a, b) AS (` の内側か。
    cte_cols: Vec<bool>,
    /// 各トークンが `INSERT INTO t ( cols )` の列リスト内か。
    insert_cols: Vec<bool>,
}

impl ParenInfo {
    pub(super) fn new(tokens: &[Token]) -> Self {
        let n = tokens.len();
        let mut encl_open: Vec<Option<usize>> = vec![None; n];
        let mut close_of: Vec<Option<usize>> = vec![None; n];
        let mut stack: Vec<usize> = Vec::new();
        for (k, t) in tokens.iter().enumerate() {
            if let Some(o) = encl_open.get_mut(k) {
                *o = stack.last().copied();
            }
            match t {
                Token::Punct('(') => stack.push(k),
                Token::Punct(')') => {
                    if let Some(open) = stack.pop() {
                        if let Some(c) = close_of.get_mut(open) {
                            *c = Some(k);
                        }
                    }
                }
                _ => {}
            }
        }
        // 開き括弧ごとの「CTE 列名リストか」。
        let is_cte_open: Vec<bool> = (0..n)
            .map(|k| {
                close_of.get(k).copied().flatten().is_some_and(|m| {
                    cte_as_follows(tokens, m.saturating_add(1))
                        && k.checked_sub(1)
                            .and_then(|p| tokens.get(p))
                            .is_some_and(|p| {
                                matches!(
                                    p,
                                    Token::Ident(_)
                                        | Token::QuotedBoolWord(_)
                                        | Token::QuotedNull(_)
                                )
                            })
                })
            })
            .collect();
        let encl_prev = encl_open
            .iter()
            .map(|o| o.and_then(|k| k.checked_sub(1)))
            .collect();
        let cte_cols = encl_open
            .iter()
            .map(|o| o.is_some_and(|k| is_cte_open.get(k).copied().unwrap_or(false)))
            .collect();
        // INSERT 列リストの範囲は入れ子になりうるため、差分配列で線形に塗る。
        let mut diff = vec![0i64; n.saturating_add(1)];
        for (s, t) in tokens.iter().enumerate() {
            if !ident_is(t, "INSERT") {
                continue;
            }
            let at_stmt_start = s
                .checked_sub(1)
                .and_then(|p| tokens.get(p))
                .is_none_or(|p| matches!(p, Token::Punct('(') | Token::Punct(')')));
            if !at_stmt_start {
                continue;
            }
            if let Some(r) = insert_column_list_at(tokens, s, &close_of) {
                if let Some(d) = diff.get_mut(r.start) {
                    *d += 1;
                }
                if let Some(d) = diff.get_mut(r.end) {
                    *d -= 1;
                }
            }
        }
        let mut acc = 0i64;
        let insert_cols = (0..n)
            .map(|k| {
                acc += diff.get(k).copied().unwrap_or(0);
                acc > 0
            })
            .collect();
        Self {
            encl_prev,
            encl_open,
            cte_cols,
            insert_cols,
        }
    }

    /// `i` を直接囲む `(` の 1 つ前のトークン。
    fn enclosing_prev<'t>(&self, tokens: &'t [Token], i: usize) -> Option<&'t Token> {
        let p = self.encl_prev.get(i).copied().flatten()?;
        tokens.get(p)
    }
}

/// `tokens[k]`（`FROM`）が式の内部の `FROM`（`IS [NOT] DISTINCT FROM`・`substring`/`extract`/`trim`/
/// `overlay` の `FROM`）か。表参照を導入する `FROM` ではないので、直後の語は変数でありうる。
fn is_expr_from(tokens: &[Token], k: usize, parens: &ParenInfo) -> bool {
    if k.checked_sub(1)
        .and_then(|p| tokens.get(p))
        .is_some_and(|p| ident_is(p, "DISTINCT"))
    {
        return true;
    }
    parens.enclosing_prev(tokens, k).is_some_and(|p| {
        ["SUBSTRING", "EXTRACT", "TRIM", "OVERLAY"]
            .iter()
            .any(|w| ident_is(p, w))
    })
}

/// `tokens[k]` が表参照を導入する `FROM` か。
fn is_table_from(tokens: &[Token], k: usize, parens: &ParenInfo) -> bool {
    matches!(tokens.get(k), Some(Token::Keyword(Keyword::From))) && !is_expr_from(tokens, k, parens)
}

/// `FROM` 句の関係リストを終える語。
fn ends_from_list(t: &Token) -> bool {
    matches!(
        t,
        Token::Keyword(Keyword::Where | Keyword::Select | Keyword::Order | Keyword::Limit)
    ) || [
        "GROUP",
        "HAVING",
        "OFFSET",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "WINDOW",
        "FOR",
        "FETCH",
        "RETURNING",
        "VALUES",
        "SET",
    ]
    .iter()
    .any(|w| ident_is(t, w))
}

/// JOIN の種別語（`ON` 条件式の終わり）。
fn is_join_word(t: &Token) -> bool {
    ["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL"]
        .iter()
        .any(|w| ident_is(t, w))
}

/// 各トークンが `FROM`／`USING` の関係リスト（カンマ区切りの表・JOIN 対象・別名）の内側にあるかを
/// 前方走査で求める。`ON` 条件式・括弧の内側・式内部の `FROM` は含めない（括弧ごとに状態を持つ）。
fn from_relation_slots(tokens: &[Token], parens: &ParenInfo) -> Vec<bool> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Idle,
        Rel,
        On,
    }
    let mut out = vec![false; tokens.len()];
    let mut stack = vec![St::Idle];
    // `DELETE ... USING t` だけが括弧なしの `USING` 関係リストを持つ。DELETE を見た後に限定する。
    let mut seen_delete = false;
    for (k, t) in tokens.iter().enumerate() {
        if ident_is(t, "DELETE") {
            seen_delete = true;
        }
        match t {
            // 角括弧（`ARRAY[1, v]`・添字）の内側のカンマが外側の関係リストの状態を変えないよう、
            // 丸括弧と同様に状態を分離する。
            Token::Punct('(') | Token::Punct('[') => {
                stack.push(St::Idle);
                continue;
            }
            Token::Punct(')') | Token::Punct(']') => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            _ => {}
        }
        let Some(st) = stack.last_mut() else { continue };
        if is_table_from(tokens, k, parens) {
            *st = St::Rel;
            continue;
        }
        if ends_from_list(t) {
            *st = St::Idle;
            continue;
        }
        let next_is_paren = matches!(tokens.get(k.saturating_add(1)), Some(Token::Punct('(')));
        if ident_is(t, "USING") && !next_is_paren && seen_delete {
            *st = St::Rel;
            continue;
        }
        match *st {
            St::Rel => {
                if ident_is(t, "ON") {
                    let conflict = tokens
                        .get(k.saturating_add(1))
                        .is_some_and(|n| ident_is(n, "CONFLICT"));
                    *st = if conflict { St::Idle } else { St::On };
                } else if let Some(o) = out.get_mut(k) {
                    *o = true;
                }
            }
            St::On => {
                let is_call = matches!(tokens.get(k.saturating_add(1)), Some(Token::Punct('(')));
                if matches!(t, Token::Punct(',')) || (is_join_word(t) && !is_call) {
                    *st = St::Rel;
                }
            }
            St::Idle => {}
        }
    }
    out
}

/// 表名位置（`FROM`・`JOIN`・`ONLY`・`UPDATE`・`INTO`・`TABLE`・`USING` の直後）の 1 トークン前か。
fn is_table_slot(tokens: &[Token], j: usize, parens: &ParenInfo) -> bool {
    match j.checked_sub(1).and_then(|p| tokens.get(p)) {
        Some(Token::Keyword(Keyword::From)) => is_table_from(tokens, j - 1, parens),
        Some(p) => ["JOIN", "INTO", "UPDATE", "TABLE", "ONLY", "USING"]
            .iter()
            .any(|w| ident_is(p, w)),
        None => false,
    }
}

/// 暗黙別名ではなく句の語になりうる語（表名の直後に現れる）。
const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "ON",
    "USING",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "GROUP",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "SELECT",
    "RETURNING",
    "FOR",
    "WINDOW",
    "FETCH",
    "OVERRIDING",
    "DEFAULT",
    "LATERAL",
    "TABLESAMPLE",
    "OUTER",
    "WHEN",
    "AS",
    "FROM",
    "DO",
    "AND",
    "OR",
    "NOT",
];

/// 各トークンが `UPDATE ... SET` の代入リスト内（直前までに有効な `SET` があり、`FROM`／`WHERE`／
/// `SELECT`／`RETURNING`／`ON`／`VALUES` や括弧の境界で途切れていない）かを前方走査 1 回で求める。
/// 括弧ごとに状態を持ち、トークンごとの後方再走査（二乗コスト）を避ける。値は「そのトークンを処理する
/// 直前の状態」。`SET` 節の開始は「直前が `,`／SELECT でなく、直後が `対象 =`」の `SET` だけ
/// （SELECT リストの `set` 列〔`SELECT set, v = 1`〕を SET 節と誤認しない）。
fn set_list_mask(tokens: &[Token]) -> Vec<bool> {
    let mut out = vec![false; tokens.len()];
    let mut stack: Vec<bool> = vec![false];
    for (k, t) in tokens.iter().enumerate() {
        if let (Some(o), Some(st)) = (out.get_mut(k), stack.last()) {
            *o = *st;
        }
        match t {
            Token::Punct('(') | Token::Punct('[') => {
                stack.push(false);
                continue;
            }
            Token::Punct(')') | Token::Punct(']') => {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            _ => {}
        }
        let Some(st) = stack.last_mut() else { continue };
        if ident_is(t, "SET") {
            let prev_ok = k
                .checked_sub(1)
                .and_then(|p| tokens.get(p))
                .is_some_and(|p| !matches!(p, Token::Punct(',') | Token::Keyword(Keyword::Select)));
            let next_ok = matches!(
                tokens.get(k.saturating_add(1)),
                Some(Token::Ident(_) | Token::QualifiedIdent { .. })
            ) && matches!(tokens.get(k.saturating_add(2)), Some(Token::Punct('=')));
            if prev_ok && next_ok {
                *st = true;
            }
        }
        if matches!(
            t,
            Token::Keyword(Keyword::From | Keyword::Where | Keyword::Select)
        ) || ["RETURNING", "ON", "VALUES"].iter().any(|w| ident_is(t, w))
        {
            *st = false;
        }
    }
    out
}

/// 派生表・関数の列別名リスト（`(SELECT 1) AS d(v)`・`(SELECT 1) d(v)`・`t AS a(x)`・`t a(x)`）の
/// 内側のトークンを求める。`(` の直前が関係リスト内の別名名で、その前が `)`・`AS`・表名のときだけ。
/// `FROM generate_series(1, v)`（直前の前が `FROM`）や `JOIN f(..)`・`ONLY`・`LATERAL` の呼び出し
/// 引数は含めない（引数の変数は参照のまま）。
fn alias_column_mask(tokens: &[Token], slots: &[bool], parens: &ParenInfo) -> Vec<bool> {
    let n = tokens.len();
    let mut open_is_alias = vec![false; n];
    for (k, t) in tokens.iter().enumerate() {
        if !matches!(t, Token::Punct('(')) || k < 2 {
            continue;
        }
        let name_ok = matches!(tokens.get(k - 1), Some(Token::Ident(_)))
            && slots.get(k - 1).copied().unwrap_or(false);
        if !name_ok {
            continue;
        }
        let before = tokens.get(k - 2);
        let after_clause = match before {
            Some(Token::Punct(')')) => true,
            Some(p) if ident_is(p, "AS") => true,
            Some(p @ (Token::Ident(_) | Token::QualifiedIdent { .. })) => {
                k >= 3
                    && is_table_slot(tokens, k - 2, parens)
                    && !ident_is(p, "ONLY")
                    && !matches!(p, Token::Ident(w) if CLAUSE_WORDS.iter().any(|c| w.eq_ignore_ascii_case(c)))
            }
            _ => false,
        };
        if after_clause {
            if let Some(o) = open_is_alias.get_mut(k) {
                *o = true;
            }
        }
    }
    parens
        .encl_open
        .iter()
        .map(|o| o.is_some_and(|k| open_is_alias.get(k).copied().unwrap_or(false)))
        .collect()
}

/// `i` 以降が `AS [[NOT] MATERIALIZED] (` の形か（CTE 定義の入口）。`from` は `AS` の位置。
fn cte_as_follows(tokens: &[Token], from: usize) -> bool {
    if !tokens.get(from).is_some_and(|t| ident_is(t, "AS")) {
        return false;
    }
    let mut j = from.saturating_add(1);
    if tokens.get(j).is_some_and(|t| ident_is(t, "NOT")) {
        j = j.saturating_add(1);
    }
    if tokens.get(j).is_some_and(|t| ident_is(t, "MATERIALIZED")) {
        j = j.saturating_add(1);
    }
    matches!(tokens.get(j), Some(Token::Punct('(')))
}

/// `tokens[i]` が CTE の宣言名（直後が `AS (` 系）か。`SELECT c AS x` の別名元とは `(` の有無で区別する。
fn is_cte_decl_name(tokens: &[Token], i: usize) -> bool {
    cte_as_follows(tokens, i.saturating_add(1))
}

/// `WITH` で始まる文の主文の種別と、データ変更 CTE（`( INSERT|UPDATE|DELETE`）の有無を返す。
///
/// CTE 定義（`[RECURSIVE] 名前 [(列リスト)] AS [[NOT] MATERIALIZED] (本体)` のカンマ区切り）を構造的に
/// 読み飛ばし、その直後の主文の語で種別を決める。CTE の宣言名は `"update"` のように主文の語と同名に
/// なりうるため、深さ 0 の最初の語を見る方式は使わない。定義の形を読み解けない・主文が
/// `SELECT`／`VALUES`／`INSERT`／`UPDATE`／`DELETE`／`(` 以外のときは `42601` で拒否する
/// （書き込み経路の事前決定なので判定不能は fail-closed）。
pub(super) fn classify_with(
    tokens: &[Token],
    quoted: &[bool],
) -> Result<(EmbeddedKind, bool), SqlSurfaceError> {
    let tokens = keyword_view(tokens, quoted);
    let tokens = tokens.as_slice();
    let bad = || syn_err("unsupported WITH statement in a PL/pgSQL body");
    // `open` の `(` に対応する `)` の次の位置。
    let skip_parens = |open: usize| -> Option<usize> {
        let mut depth = 0usize;
        for (k, t) in tokens.iter().enumerate().skip(open) {
            match t {
                Token::Punct('(') => depth = depth.saturating_add(1),
                Token::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(k.saturating_add(1));
                    }
                }
                _ => {}
            }
        }
        None
    };
    if !tokens.first().is_some_and(|t| ident_is(t, "WITH")) {
        return Err(bad());
    }
    let mut j = 1usize;
    // 引用付きの `"recursive"` は `keyword_view` で `RECURSIVE` と一致しない（CTE 名として読む）。
    if tokens.get(j).is_some_and(|t| ident_is(t, "RECURSIVE")) {
        j = j.saturating_add(1);
    }
    loop {
        // CTE 名（1 トークン）。
        if !matches!(
            tokens.get(j),
            Some(Token::Ident(_) | Token::QuotedBoolWord(_) | Token::QuotedNull(_))
        ) {
            return Err(bad());
        }
        j = j.saturating_add(1);
        if matches!(tokens.get(j), Some(Token::Punct('('))) {
            j = skip_parens(j).ok_or_else(bad)?;
        }
        if !tokens.get(j).is_some_and(|t| ident_is(t, "AS")) {
            return Err(bad());
        }
        j = j.saturating_add(1);
        if tokens.get(j).is_some_and(|t| ident_is(t, "NOT")) {
            j = j.saturating_add(1);
        }
        if tokens.get(j).is_some_and(|t| ident_is(t, "MATERIALIZED")) {
            j = j.saturating_add(1);
        }
        if !matches!(tokens.get(j), Some(Token::Punct('('))) {
            return Err(bad());
        }
        j = skip_parens(j).ok_or_else(bad)?;
        if matches!(tokens.get(j), Some(Token::Punct(','))) {
            j = j.saturating_add(1);
        } else {
            break;
        }
    }
    let kind = match tokens.get(j) {
        Some(t) if ident_is(t, "INSERT") => EmbeddedKind::Insert,
        Some(t) if ident_is(t, "UPDATE") => EmbeddedKind::Update,
        Some(t) if ident_is(t, "DELETE") => EmbeddedKind::Delete,
        Some(t) if ident_is(t, "VALUES") => EmbeddedKind::Select,
        Some(Token::Keyword(Keyword::Select)) | Some(Token::Punct('(')) => EmbeddedKind::Select,
        _ => return Err(bad()),
    };
    Ok((kind, has_write_cte(tokens, &[])))
}

/// `(` の直後が `INSERT`／`UPDATE`／`DELETE` のトークンがあるか（データ変更 CTE）。
/// `RETURN QUERY`・問い合わせ `FOR` の主文を分類する（ADR D5: 書き込み経路を事前決定するため）。
/// `SELECT`／`VALUES`／括弧始まりは `Select`、`INSERT`／`UPDATE`／`DELETE` はその種別、
/// `WITH` は主文で判定。これら以外は `42601` で明示的に拒否する。
pub(super) fn classify_query(
    tokens: &[Token],
    quoted: &[bool],
) -> Result<EmbeddedKind, SqlSurfaceError> {
    // 先頭の語は引用付きなら識別子（`"insert"` は問い合わせの語ではない）。
    let view = keyword_view(tokens, quoted);
    let Some(first) = view.first() else {
        return Err(syn_err("empty query in a PL/pgSQL statement"));
    };
    if matches!(first, Token::Keyword(Keyword::Select) | Token::Punct('('))
        || ident_is(first, "VALUES")
    {
        Ok(EmbeddedKind::Select)
    } else if ident_is(first, "INSERT") {
        Ok(EmbeddedKind::Insert)
    } else if ident_is(first, "UPDATE") {
        Ok(EmbeddedKind::Update)
    } else if ident_is(first, "DELETE") {
        Ok(EmbeddedKind::Delete)
    } else if ident_is(first, "WITH") {
        Ok(classify_with(tokens, quoted)?.0)
    } else {
        Err(syn_err("unsupported query in a PL/pgSQL statement"))
    }
}

pub(super) fn has_write_cte(tokens: &[Token], quoted: &[bool]) -> bool {
    let tokens = keyword_view(tokens, quoted);
    tokens.windows(2).any(|w| {
        matches!(w.first(), Some(Token::Punct('(')))
            && w.get(1).is_some_and(|t| {
                ident_is(t, "INSERT") || ident_is(t, "UPDATE") || ident_is(t, "DELETE")
            })
    })
}

/// `tokens[i]` の `Ident` が識別子の位置（関数名・表名・列名・別名・暗黙別名・`USING` の列／表・
/// `ON CONFLICT` の対象・`UPDATE ... SET`／`DO UPDATE SET` の代入先）にあるか。そうなら変数参照として
/// 置換しない。#2226 の呼び出し時の置換と共有する単一の分類関数。
///
/// 引用情報を持たない呼び出し（全て未引用として扱う）。引用付き識別子を区別したい呼び出し元は
/// [`is_ident_position_quoted`] を使う。
pub(crate) fn is_ident_position(tokens: &[Token], i: usize) -> bool {
    is_ident_position_quoted(tokens, &vec![false; tokens.len()], i)
}

/// [`is_ident_position`] の引用情報付き版。`quoted` は `sql::lexer::tokenize_with_params_quoted` の
/// 並走配列（引用付き識別子は句のキーワードではなく常に識別子として扱う）。
pub(crate) fn is_ident_position_quoted(tokens: &[Token], quoted: &[bool], i: usize) -> bool {
    is_ident_position_with(i, &IdentCtx::new(tokens, quoted))
}

/// 識別子位置の分類に使う文単位の事前計算結果（関係リスト位置・括弧／CTE／INSERT 列位置）。
pub(super) struct IdentCtx {
    /// 引用付き識別子を句のキーワードと区別する見え方（[`keyword_view`]）。判定はすべてこれに対して行う。
    view: Vec<Token>,
    slots: Vec<bool>,
    parens: ParenInfo,
    /// `::型` と `CAST(式 AS 型)` の型指定の範囲（複数語・修飾子・`[]` を含む）に入るトークン。
    type_range: Vec<bool>,
    /// 派生表・関数の列別名リスト（`AS d(v)`）の内側のトークン。
    alias_cols: Vec<bool>,
    /// `UPDATE ... SET` の代入リスト内のトークン。
    set_list: Vec<bool>,
}

impl IdentCtx {
    pub(super) fn new(tokens: &[Token], quoted: &[bool]) -> Self {
        let view = keyword_view(tokens, quoted);
        let parens = ParenInfo::new(&view);
        let type_range = type_ranges(&view, tokens, &parens);
        let slots = from_relation_slots(&view, &parens);
        let alias_cols = alias_column_mask(&view, &slots, &parens);
        let set_list = set_list_mask(&view);
        Self {
            view,
            slots,
            parens,
            type_range,
            alias_cols,
            set_list,
        }
    }
}

/// 型指定の範囲に入るトークンを塗る。範囲は SQL 側の列型パーサー
/// （`sql::ddl_column_type::parse_column_type_name`。`double precision`・`character varying(n)`・
/// `timestamp without time zone`・`numeric(p,s)`・`[]` を受理する）に決めさせ、1 トークンの
/// 先読み特例を持たない。型として読めない場合（未対応の型名）は先頭 1 トークンだけを範囲にする
/// （後続の許可リスト解析が拒否する）。`IdentCtx::new` から 1 文に 1 回だけ呼ばれる。
fn type_ranges(view: &[Token], tokens: &[Token], parens: &ParenInfo) -> Vec<bool> {
    let mut out = vec![false; tokens.len()];
    for (i, t) in view.iter().enumerate() {
        let starts = match t {
            Token::TypeCast => true,
            // `CAST(式 AS 型)` の `AS`。別名の `AS` と区別するため直接囲む括弧の直前が `CAST`。
            _ if ident_is(t, "AS") => parens
                .enclosing_prev(view, i)
                .is_some_and(|p| ident_is(p, "CAST")),
            _ => false,
        };
        if !starts {
            continue;
        }
        let start = i.saturating_add(1);
        let mut pos = start;
        let end = match parse_column_type_name(tokens, &mut pos) {
            Ok(_) if pos > start => pos,
            _ => start.saturating_add(1),
        };
        for slot in out.iter_mut().take(end).skip(start) {
            *slot = true;
        }
    }
    out
}

/// [`is_ident_position`] の本体。`ctx` は同じトークン列に対する [`IdentCtx::new`] の結果。
pub(super) fn is_ident_position_with(i: usize, ctx: &IdentCtx) -> bool {
    let tokens = ctx.view.as_slice();
    let slots = &ctx.slots;
    if ctx.type_range.get(i).copied().unwrap_or(false)
        || ctx.alias_cols.get(i).copied().unwrap_or(false)
    {
        return true;
    }
    if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('('))) {
        return true;
    }
    let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
    // `EXTRACT(field FROM v)` の field は日時の部品名であり値ではない。
    if matches!(prev, Some(Token::Punct('(')))
        && matches!(
            tokens.get(i.saturating_add(1)),
            Some(Token::Keyword(Keyword::From))
        )
        && i.checked_sub(2)
            .and_then(|p| tokens.get(p))
            .is_some_and(|p| ident_is(p, "EXTRACT"))
    {
        return true;
    }
    match prev {
        // `::型名` は型の位置であり値ではない。
        Some(Token::TypeCast) => return true,
        // 式内部の `FROM`（`IS DISTINCT FROM v` 等）の直後は変数でありうるので識別子位置にしない。
        Some(Token::Keyword(Keyword::From)) if !is_expr_from(tokens, i - 1, &ctx.parens) => {
            return true
        }
        Some(p)
            if [
                "JOIN",
                "INTO",
                "UPDATE",
                "TABLE",
                "AS",
                "ONLY",
                "USING",
                "CONSTRAINT",
            ]
            .iter()
            .any(|w| ident_is(p, w)) =>
        {
            return true
        }
        _ => {}
    }
    // `FROM t, u`・JOIN 対象・別名など、関係リスト内の語。
    if slots.get(i).copied().unwrap_or(false) {
        return true;
    }
    // CTE の宣言名（`WITH c AS (`・`c AS MATERIALIZED (`）と列名リスト（`c (a, b) AS (`）。
    if is_cte_decl_name(tokens, i) || ctx.parens.cte_cols.get(i).copied().unwrap_or(false) {
        return true;
    }
    // 暗黙別名: 表名の直後の語（`FROM t x`）。句の語は別名ではない。
    if let (Some(p @ (Token::Ident(_) | Token::QualifiedIdent { .. })), Some(Token::Ident(me))) =
        (prev, tokens.get(i))
    {
        if i >= 1
            && is_table_slot(tokens, i - 1, &ctx.parens)
            && !ident_is(p, "ONLY")
            && !CLAUSE_WORDS.iter().any(|w| me.eq_ignore_ascii_case(w))
        {
            return true;
        }
    }
    // `JOIN ... USING (cols)`・`ON CONFLICT (cols)` の列。
    if ctx
        .parens
        .enclosing_prev(tokens, i)
        .is_some_and(|p| ident_is(p, "USING") || ident_is(p, "CONFLICT"))
    {
        return true;
    }
    // `UPDATE ... SET` / `DO UPDATE SET` の代入先。
    if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('=')))
        && prev.is_some_and(|p| ident_is(p, "SET") || matches!(p, Token::Punct(',')))
        && ctx.set_list.get(i).copied().unwrap_or(false)
    {
        return true;
    }
    ctx.parens.insert_cols.get(i).copied().unwrap_or(false)
}

/// 定義時検証に使うダミートークン。整数・浮動小数は `0`、真偽値は `false`、それ以外は文字列 `'0'`。
pub(crate) fn dummy_token(ty: &DeclType) -> Token {
    fn named(t: &SqlColumnTypeName) -> Token {
        match t {
            SqlColumnTypeName::SmallInt
            | SqlColumnTypeName::Integer
            | SqlColumnTypeName::BigInt
            | SqlColumnTypeName::Real
            | SqlColumnTypeName::Double
            | SqlColumnTypeName::Numeric { .. } => Token::Number("0".to_string()),
            SqlColumnTypeName::Boolean => Token::Ident("false".to_string()),
            SqlColumnTypeName::Alias { target, .. } => named(target),
            _ => Token::StringLiteral("0".to_string()),
        }
    }
    match ty {
        DeclType::Named(t) => named(t),
        _ => Token::StringLiteral("0".to_string()),
    }
}

/// 埋め込み SQL の検証口。core が `parse_tokens`（許可リスト解析）へ委譲して実装する
/// （`core::plpgsql_validate`）。受け取るのは変数をダミーへ置換したトークン列。
pub(crate) trait EmbeddedSqlChecker {
    /// 構文検証。定義時に拒否すべき失敗（`42601`／`0A000`／`54000`／`54001`）だけを `Err` にする。
    fn check_syntax(&self, tokens: Vec<Token>) -> Result<(), SqlSurfaceError>;
}

fn substituted(tokens: &[Token], refs: &[(usize, VarRef)], body: &Body) -> Vec<Token> {
    let map: HashMap<usize, &VarRef> = refs.iter().map(|(i, r)| (*i, r)).collect();
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| match map.get(&i) {
            Some(VarRef::Var(id)) => match body.vars.get(id.0 as usize) {
                Some(v) => dummy_token(&v.ty),
                None => Token::StringLiteral("0".to_string()),
            },
            Some(VarRef::Field(..)) => Token::StringLiteral("0".to_string()),
            None => t.clone(),
        })
        .collect()
}

/// 条件式の検証に使う仮の表名と列名。許可リストの `WHERE` は、`FROM` の無い `SELECT` では単一の述語しか
/// 受理せず（ダミー置換で定数になった `a > 0 AND b < 3` が `42601`）、`FROM` があれば複合述語を受理する。
/// また `IS NULL`・`BETWEEN`・`IN` は左辺に列を要求するため、定数では拒否される。未知の表・列の
/// `42P01`／`42703` は定義時には握りつぶされる（late binding）ので、仮の表名・列名で構文だけを検査できる。
const DUMMY_RELATION: &str = "plpgsql_validate_dummy_relation";
const DUMMY_COLUMN: &str = "plpgsql_validate_dummy_column";

/// `refs` の位置を `replace` で置き換えたトークン列を作る（`substituted` の列名版を兼ねる）。
fn substituted_with_columns(tokens: &[Token], refs: &[(usize, VarRef)]) -> Vec<Token> {
    let set: std::collections::HashSet<usize> = refs.iter().map(|(i, _)| *i).collect();
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if set.contains(&i) {
                Token::Ident(DUMMY_COLUMN.to_string())
            } else {
                t.clone()
            }
        })
        .collect()
}

/// 式 1 個の定義時検証。次の形を順に試し、最初に受理された時点で成功とする。
///
/// 1. 値の式なら `SELECT <expr>`（変数は型に応じたダミーの値）
/// 2. 条件式なら `SELECT 1 FROM <仮の表> WHERE <expr>`（変数はダミーの値）
/// 3. 同じ形で変数を仮の列名に置換（`IS NULL`・`BETWEEN`・`IN` の左辺が変数のとき）
///
/// 許可リストは比較・論理演算を `WHERE` の中でしか受理しないため、値の式として拒否された式も条件式の形
/// で試す（`b := 1 = 1`・`RETURN a AND b`・比較式の初期値）。すべて拒否なら、本来の構文誤りとして最初の
/// エラーを返す（`42601` のまま）。
fn check_expr(e: &SqlExpr, body: &Body, c: &dyn EmbeddedSqlChecker) -> Result<(), SqlSurfaceError> {
    let cond_form = |subst: Vec<Token>| {
        let mut toks = vec![
            Token::Keyword(Keyword::Select),
            Token::Number("1".to_string()),
            Token::Keyword(Keyword::From),
            Token::Ident(DUMMY_RELATION.to_string()),
            Token::Keyword(Keyword::Where),
        ];
        toks.extend(subst);
        toks
    };
    let literal = substituted(&e.tokens, &e.var_refs, body);
    let mut first: Option<SqlSurfaceError> = None;
    let mut record = |r: Result<(), SqlSurfaceError>| -> bool {
        match r {
            Ok(()) => true,
            Err(err) => {
                first.get_or_insert(err);
                false
            }
        }
    };
    if !e.cond {
        let mut toks = vec![Token::Keyword(Keyword::Select)];
        toks.extend(literal.iter().cloned());
        if record(c.check_syntax(toks)) {
            return Ok(());
        }
    }
    if record(c.check_syntax(cond_form(literal)))
        || record(c.check_syntax(cond_form(substituted_with_columns(&e.tokens, &e.var_refs))))
    {
        return Ok(());
    }
    Err(first.unwrap_or_else(|| syn_err("invalid expression in a PL/pgSQL body")))
}

fn check_sql(
    s: &EmbeddedSql,
    body: &Body,
    c: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    c.check_syntax(substituted(&s.tokens, &s.var_refs, body))
}

fn check_stmts(
    stmts: &[Stmt],
    body: &Body,
    c: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    for s in stmts {
        match s {
            Stmt::Block(b) => check_block(b, body, c)?,
            Stmt::Assign { expr, .. } => check_expr(expr, body, c)?,
            Stmt::If { branches, else_ } => {
                for (cond, st) in branches {
                    check_expr(cond, body, c)?;
                    check_stmts(st, body, c)?;
                }
                if let Some(st) = else_ {
                    check_stmts(st, body, c)?;
                }
            }
            Stmt::Case {
                selector,
                whens,
                else_,
            } => {
                if let Some(sel) = selector {
                    check_expr(sel, body, c)?;
                }
                for (exprs, st) in whens {
                    for e in exprs {
                        check_expr(e, body, c)?;
                    }
                    check_stmts(st, body, c)?;
                }
                if let Some(st) = else_ {
                    check_stmts(st, body, c)?;
                }
            }
            Stmt::Loop { body: st, .. } => check_stmts(st, body, c)?,
            Stmt::While { cond, body: st, .. } => {
                check_expr(cond, body, c)?;
                check_stmts(st, body, c)?;
            }
            Stmt::ForInt {
                lower,
                upper,
                step,
                body: st,
                ..
            } => {
                check_expr(lower, body, c)?;
                check_expr(upper, body, c)?;
                if let Some(e) = step {
                    check_expr(e, body, c)?;
                }
                check_stmts(st, body, c)?;
            }
            Stmt::ForQuery {
                query, body: st, ..
            } => {
                check_sql(query, body, c)?;
                check_stmts(st, body, c)?;
            }
            Stmt::Exit { when, .. } | Stmt::Continue { when, .. } => {
                if let Some(e) = when {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::Return(e) | Stmt::ReturnNext(e) => {
                if let Some(e) = e {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::ReturnQuery(q) | Stmt::Perform(q) | Stmt::Sql(q) => check_sql(q, body, c)?,
            Stmt::Execute { query, using, .. } => {
                check_expr(query, body, c)?;
                for e in using {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::GetDiagnostics { .. } | Stmt::Null => {}
        }
    }
    Ok(())
}

fn check_block(b: &Block, body: &Body, c: &dyn EmbeddedSqlChecker) -> Result<(), SqlSurfaceError> {
    for id in &b.decls {
        if let Some(d) = body
            .vars
            .get(id.0 as usize)
            .and_then(|v| v.default.as_ref())
        {
            check_expr(d, body, c)?;
        }
    }
    check_stmts(&b.stmts, body, c)
}

/// 本体の全ての埋め込み SQL・式を、変数をダミーへ置換して `checker` へ渡す（ADR D3）。
/// 解析木の深さは `parse_body` が上限で抑えているため、再帰で辿ってよい。
pub(crate) fn validate_embedded(
    body: &Body,
    checker: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    check_block(&body.block, body, checker)
}
