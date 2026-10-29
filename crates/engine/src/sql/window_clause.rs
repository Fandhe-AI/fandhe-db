//! 名前付きウィンドウ（`WINDOW <名前> AS (...)`・`OVER <名前>`・`OVER (<名前> ...)`。
//! SQL-64 (3)・Issue #1892）の展開器。
//!
//! 役割: 許可リストの構文段（`sql::allowlist`）より前に、トークン列を「WINDOW 句を取り除き、
//! 名前の参照を等価なインライン `OVER ( <窓指定> )` へ置き換えた」列へ書き換える。構文段・束縛・
//! 実行（`sql::window`）・RLS の適用は展開済みの列だけを見るため、第 2 の実行経路は生まれない。
//! `) OVER (` を目印にする分類器が `allowlist.rs` に多数あるので、パーサー側で名前を直接扱うと
//! その全部の改修が要る。展開を 1 箇所（`validate_sql_tokens_impl`・`validate_session_with_tokens`
//! の先頭）に集約することで漏れを防ぐ。展開を通らない入口では従来どおり `42601` になる（fail-closed）。
//!
//! 名前はクエリ階層（サブクエリ・CTE 本文・派生表・集合演算の枝）をまたいで解決しない
//! （PostgreSQL と同じ）。エラーは PostgreSQL 17 に合わせる: 未定義名 `42704`、名前の重複・
//! PARTITION BY／ORDER BY の上書き・フレーム付き定義の括弧付き複写 `42P20`。展開はトークンの範囲を
//! 複写するだけで文字列を連結しない。展開後の総量には上限（`54000`）を設ける。

use std::borrow::Cow;

use super::allowlist::{validate_window_spec_tokens, SqlSurfaceError};
use super::lexer::{Keyword, Token};

/// 1 つの WINDOW 句に書ける定義数の上限（超過は `54000`）。
const MAX_WINDOW_DEFINITIONS: usize = 32;
/// 参照の置換で複写するトークンの合計上限（超過は `54000`。確保より前に検査する）。
const MAX_EXPANDED_WINDOW_TOKENS: usize = 65_536;
/// サブクエリ階層の再帰の深さ上限（超過は `54000`）。
const MAX_SCOPE_DEPTH: usize = 64;

/// 展開済みの窓定義 1 件（区間ごとに保持する）。
struct Def {
    name: String,
    partition: Vec<Token>,
    order: Vec<Token>,
    frame: Vec<Token>,
}

impl Def {
    /// [`Self::spec`] が返すトークン数（複写の前に予算を検査するために使う。確保はしない）。
    fn spec_len(&self) -> usize {
        self.partition.len() + self.order.len() + self.frame.len() + 2
    }

    /// `( <partition> <order> <frame> )` を組み立てる。
    fn spec(&self) -> Vec<Token> {
        let mut v =
            Vec::with_capacity(self.partition.len() + self.order.len() + self.frame.len() + 2);
        v.push(Token::Punct('('));
        v.extend(self.partition.iter().cloned());
        v.extend(self.order.iter().cloned());
        v.extend(self.frame.iter().cloned());
        v.push(Token::Punct(')'));
        v
    }
}

fn windowing(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::WindowingError { detail }
}

fn too_complex(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(detail)
}

fn is_ident(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

/// `OVER (` の直後が窓名ではなく窓指定の語（または名前になりえないトークン）か。
fn is_spec_word(t: Option<&Token>) -> bool {
    match t {
        Some(Token::Ident(s)) => ["PARTITION", "ROWS", "RANGE", "GROUPS"]
            .iter()
            .any(|w| s.eq_ignore_ascii_case(w)),
        _ => true,
    }
}

/// 展開が必要になりうる入力か（高速経路の判定）。
fn needs_expansion(tokens: &[Token]) -> bool {
    for (i, t) in tokens.iter().enumerate() {
        if is_ident(Some(t), "WINDOW") {
            return true;
        }
        if is_ident(Some(t), "OVER")
            && i > 0
            && matches!(tokens.get(i - 1), Some(Token::Punct(')')))
        {
            match tokens.get(i + 1) {
                Some(Token::Punct('(')) => {
                    if !is_spec_word(tokens.get(i + 2)) {
                        return true;
                    }
                }
                Some(_) => return true,
                None => {}
            }
        }
    }
    false
}

/// 括弧がちょうど釣り合っているか。
fn balanced(tokens: &[Token]) -> bool {
    let mut depth = 0usize;
    for t in tokens {
        match t {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    depth == 0
}

/// 名前付きウィンドウを等価なインライン `OVER (...)` へ展開する（モジュール先頭の説明参照）。
/// 名前も WINDOW 句も無い入力・括弧が釣り合わない入力は `Cow::Borrowed` で素通しする（後者は
/// 下流が従来どおり拒否する）。
pub(crate) fn expand_named_windows(tokens: &[Token]) -> Result<Cow<'_, [Token]>, SqlSurfaceError> {
    if !needs_expansion(tokens) || !balanced(tokens) {
        return Ok(Cow::Borrowed(tokens));
    }
    let mut budget = 0usize;
    let out = expand_scope(tokens, 0, &mut budget)?;
    Ok(Cow::Owned(out))
}

/// `(` の位置 `open` が、サブクエリ・CTE 本文・派生表等の独立スコープの開きか。
fn is_scope_open(tokens: &[Token], open: usize) -> bool {
    let mut k = open;
    while matches!(tokens.get(k), Some(Token::Punct('('))) {
        k += 1;
    }
    matches!(tokens.get(k), Some(Token::Keyword(Keyword::Select)))
        || is_ident(tokens.get(k), "WITH")
        // データ変更 CTE の本体も独立スコープ。WINDOW 句の誤配置を `process_branch` が検出できる
        // ようにする（`(update + 1)` のような列名の式を誤認しない最小限の先読み）。
        || (is_ident(tokens.get(k), "DELETE")
            && matches!(tokens.get(k + 1), Some(Token::Keyword(Keyword::From))))
        || (is_ident(tokens.get(k), "INSERT") && is_ident(tokens.get(k + 1), "INTO"))
        || (is_ident(tokens.get(k), "UPDATE")
            && matches!(tokens.get(k + 1), Some(Token::Ident(_)))
            && is_ident(tokens.get(k + 2), "SET"))
}

/// `open` の `(` に対応する `)` の位置。
fn matching_close(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, t) in tokens.iter().enumerate().skip(open) {
        match t {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

fn expand_scope(
    tokens: &[Token],
    depth: usize,
    budget: &mut usize,
) -> Result<Vec<Token>, SqlSurfaceError> {
    if depth > MAX_SCOPE_DEPTH {
        return Err(too_complex("window scope nesting too deep"));
    }
    // 1. 内側のスコープを先に展開して平らな列へ戻す（内側は WINDOW 句も名前も残らない）。
    let mut flat: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while let Some(t) = tokens.get(i) {
        if matches!(t, Token::Punct('(')) && is_scope_open(tokens, i) {
            let close = matching_close(tokens, i)
                .ok_or_else(|| SqlSurfaceError::unsupported("unbalanced parentheses"))?;
            let inner = tokens.get(i + 1..close).unwrap_or(&[]);
            let expanded = expand_scope(inner, depth + 1, budget)?;
            flat.push(Token::Punct('('));
            flat.extend(expanded);
            flat.push(Token::Punct(')'));
            i = close + 1;
        } else {
            flat.push(t.clone());
            i += 1;
        }
    }
    // 2. 深さ 0 の集合演算子で枝に分け、枝ごとに処理する。
    let mut out = Vec::with_capacity(flat.len());
    let mut start = 0;
    let mut d = 0usize;
    for (k, t) in flat.iter().enumerate() {
        match t {
            Token::Punct('(') => d += 1,
            Token::Punct(')') => d = d.saturating_sub(1),
            Token::Ident(_) if d == 0 && is_set_operator(&flat, k) => {
                let branch = flat.get(start..k).unwrap_or(&[]);
                out.extend(process_branch(branch, budget)?);
                out.push(t.clone());
                start = k + 1;
            }
            _ => {}
        }
    }
    out.extend(process_branch(flat.get(start..).unwrap_or(&[]), budget)?);
    Ok(out)
}

/// 深さ 0 の位置 `k` の `UNION`／`INTERSECT`／`EXCEPT` が集合演算子か。
/// 同名の列（`SELECT union FROM t`・`WHERE except = 1` 等。lexer では通常の識別子）を
/// 分割しないよう、直後（`ALL`／`DISTINCT` を除く）が問合せの先頭（`SELECT`・`VALUES`・`WITH`・
/// 独立スコープの `(`）である場合だけ演算子とみなす。誤って演算子と判定しないだけで、
/// 本物の演算子の見逃しは WINDOW 句の位置検査（`process_branch`）が `42601` で拒否する。
fn is_set_operator(tokens: &[Token], k: usize) -> bool {
    if !["UNION", "INTERSECT", "EXCEPT"]
        .iter()
        .any(|w| is_ident(tokens.get(k), w))
    {
        return false;
    }
    let mut n = k + 1;
    if is_ident(tokens.get(n), "ALL") || is_ident(tokens.get(n), "DISTINCT") {
        n += 1;
    }
    match tokens.get(n) {
        Some(Token::Keyword(Keyword::Select)) => true,
        Some(Token::Punct('(')) => is_scope_open(tokens, n),
        t => is_ident(t, "VALUES") || is_ident(t, "WITH"),
    }
}

/// 位置 `i` の `OFFSET`／`FETCH`／`FOR` が句の開始か（直前が式の末尾か）。
/// 列名として値の位置に現れる場合（`WHERE offset = 1`・`AND for`・`= fetch` 等。直前が
/// キーワード・演算子・論理語）は句ではない。
fn starts_trailing_clause(tokens: &[Token], i: usize) -> bool {
    if !["OFFSET", "FETCH", "FOR"]
        .iter()
        .any(|x| is_ident(tokens.get(i), x))
    {
        return false;
    }
    let Some(prev) = i.checked_sub(1).and_then(|j| tokens.get(j)) else {
        return false;
    };
    // オペランド終端の判定は `fetch_clause` と共通（修飾名・パラメータ等も終端）。
    // 素の識別子のうち論理・比較の語は演算子側なので終端から除く。
    if let Token::Ident(w) = prev {
        if [
            "AND", "OR", "NOT", "IS", "IN", "LIKE", "ILIKE", "BETWEEN", "ON", "WHEN", "THEN",
            "ELSE", "CASE", "DISTINCT", "AS",
        ]
        .iter()
        .any(|x| w.eq_ignore_ascii_case(x))
        {
            return false;
        }
    }
    super::fetch_clause::follows_operand(Some(prev))
}

/// 枝の文の動詞（`SELECT`・`DELETE` 等）の位置。先頭が `WITH` なら CTE リスト
/// （`name [(列, ...)] AS [NOT] [MATERIALIZED] ( 本体 ) [, ...]`）を読み飛ばした直後を返す。
/// CTE 名・列名が `update` 等でも動詞と誤認しない。形が想定外なら `None`（呼び出し側は
/// WINDOW 句を拒否する＝fail-closed）。
fn statement_verb_position(branch: &[Token]) -> Option<usize> {
    // 文頭の接頭（集合演算子で分けた後続の枝の `ALL`／`DISTINCT`、`EXPLAIN`・`CREATE ... AS`・
    // `PREPARE ... AS` 等）を飛ばし、深さ 0 で最初に現れる文の開始語を動詞とする。
    let start = depth0_positions(branch).into_iter().find(|&i| {
        matches!(branch.get(i), Some(Token::Keyword(Keyword::Select)))
            || ["WITH", "DELETE", "UPDATE", "INSERT"]
                .iter()
                .any(|w| is_ident(branch.get(i), w))
    })?;
    if !is_ident(branch.get(start), "WITH") {
        return Some(start);
    }
    let mut p = start;
    p += 1;
    if is_ident(branch.get(p), "RECURSIVE") {
        p += 1;
    }
    loop {
        if !matches!(branch.get(p), Some(Token::Ident(_))) {
            return None;
        }
        p += 1;
        if matches!(branch.get(p), Some(Token::Punct('('))) {
            p = matching_close(branch, p)? + 1;
        }
        if !is_ident(branch.get(p), "AS") {
            return None;
        }
        p += 1;
        if is_ident(branch.get(p), "NOT") {
            p += 1;
        }
        if is_ident(branch.get(p), "MATERIALIZED") {
            p += 1;
        }
        if !matches!(branch.get(p), Some(Token::Punct('('))) {
            return None;
        }
        p = matching_close(branch, p)? + 1;
        // 再帰 CTE の `SEARCH ... SET ...`／`CYCLE ... SET ... USING ...` は、次の定義の区切り
        // （`, 名前 AS|(`）か主クエリの `SELECT` の手前までを句として読み飛ばす
        // （`allowlist` の再帰 CTE 解析と同じ終端規則。句の中身の検証はそちらの責務）。
        if is_ident(branch.get(p), "SEARCH") || is_ident(branch.get(p), "CYCLE") {
            loop {
                match branch.get(p) {
                    None => return None,
                    Some(Token::Keyword(Keyword::Select)) => break,
                    Some(Token::Punct(',')) => {
                        let after_name = branch.get(p + 2);
                        if matches!(branch.get(p + 1), Some(Token::Ident(_)))
                            && (matches!(after_name, Some(Token::Punct('(')))
                                || is_ident(after_name, "AS"))
                        {
                            break;
                        }
                        p += 1;
                    }
                    Some(_) => p += 1,
                }
            }
        }
        if matches!(branch.get(p), Some(Token::Punct(','))) {
            p += 1;
            continue;
        }
        return Some(p);
    }
}

/// 括弧の外（深さ 0）にあるトークンの位置（括弧そのものを含む）。
fn depth0_positions(tokens: &[Token]) -> Vec<usize> {
    let mut d = 0usize;
    let mut v = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => {
                if d == 0 {
                    v.push(i);
                }
                d += 1;
            }
            Token::Punct(')') => {
                d = d.saturating_sub(1);
                if d == 0 {
                    v.push(i);
                }
            }
            _ => {
                if d == 0 {
                    v.push(i);
                }
            }
        }
    }
    v
}

fn is_clause_terminator(t: Option<&Token>) -> bool {
    match t {
        None | Some(Token::Punct(';')) => true,
        Some(Token::Keyword(Keyword::Order | Keyword::Limit)) => true,
        Some(Token::Ident(s)) => ["OFFSET", "FETCH", "FOR", "UNION", "INTERSECT", "EXCEPT"]
            .iter()
            .any(|w| s.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

/// 1 つの枝（深さ 0 に集合演算子を含まない）を処理する。
fn process_branch(branch: &[Token], budget: &mut usize) -> Result<Vec<Token>, SqlSurfaceError> {
    let d0 = depth0_positions(branch);
    let from = d0
        .iter()
        .copied()
        .find(|&i| matches!(branch.get(i), Some(Token::Keyword(Keyword::From))));
    let clause_at = from.and_then(|f| {
        d0.iter().copied().find(|&i| {
            i > f
                && is_ident(branch.get(i), "WINDOW")
                && matches!(branch.get(i + 1), Some(Token::Ident(_)))
                && is_ident(branch.get(i + 2), "AS")
        })
    });

    // WINDOW 句は SELECT 枝にしか置けない。DELETE／UPDATE／INSERT（データ変更 CTE の本体を含む）
    // の枝に深さ 0 の `WINDOW <名> AS` があれば、除去すると不正な句が消えて有効な更新文に
    // なってしまうため、除去せず拒否する（fail-closed。INSERT ... SELECT の SELECT 部も同様）。
    let has_window_clause = d0.iter().copied().any(|i| {
        is_ident(branch.get(i), "WINDOW")
            && matches!(branch.get(i + 1), Some(Token::Ident(_)))
            && is_ident(branch.get(i + 2), "AS")
    });
    if has_window_clause {
        let verb = statement_verb_position(branch);
        if !matches!(
            verb.and_then(|v| branch.get(v)),
            Some(Token::Keyword(Keyword::Select))
        ) {
            return Err(SqlSurfaceError::unsupported("misplaced WINDOW clause"));
        }
    }

    let mut defs: Vec<Def> = Vec::new();
    let (head, tail): (&[Token], &[Token]) = match (from, clause_at) {
        (Some(f), Some(w)) => {
            // 句より前（FROM 以降）に ORDER BY・LIMIT 等があると、句を除いた後に通ってしまう。
            let bad_before = d0.iter().copied().any(|i| {
                i > f
                    && i < w
                    && (matches!(
                        branch.get(i),
                        Some(Token::Keyword(Keyword::Order | Keyword::Limit))
                    ) || starts_trailing_clause(branch, i)
                        // `JOIN ... USING (...)` は WINDOW の前に正当に現れる。拒否するのは
                        // 検索 SELECT の `USING PLAN(...)` だけ。
                        || (is_ident(branch.get(i), "USING")
                            && is_ident(branch.get(i + 1), "PLAN")))
            });
            if bad_before {
                return Err(SqlSurfaceError::unsupported("misplaced WINDOW clause"));
            }
            let mut p = w + 1;
            loop {
                let name = match branch.get(p) {
                    Some(Token::Ident(n)) => n.clone(),
                    _ => return Err(SqlSurfaceError::unsupported("malformed WINDOW clause")),
                };
                if !is_ident(branch.get(p + 1), "AS")
                    || !matches!(branch.get(p + 2), Some(Token::Punct('(')))
                {
                    return Err(SqlSurfaceError::unsupported("malformed WINDOW clause"));
                }
                let open = p + 2;
                let close = matching_close(branch, open)
                    .ok_or_else(|| SqlSurfaceError::unsupported("unbalanced parentheses"))?;
                if defs.len() >= MAX_WINDOW_DEFINITIONS {
                    return Err(too_complex("too many window definitions"));
                }
                if defs.iter().any(|d| d.name.eq_ignore_ascii_case(&name)) {
                    return Err(windowing("window name is already defined"));
                }
                let inner = branch.get(open + 1..close).unwrap_or(&[]);
                let def = build_def(name, inner, &defs)?;
                validate_window_spec_tokens(&def.spec())?;
                defs.push(def);
                p = close + 1;
                if matches!(branch.get(p), Some(Token::Punct(','))) {
                    p += 1;
                    continue;
                }
                break;
            }
            if !is_clause_terminator(branch.get(p)) {
                return Err(SqlSurfaceError::unsupported("misplaced WINDOW clause"));
            }
            (
                branch.get(..w).unwrap_or(&[]),
                branch.get(p..).unwrap_or(&[]),
            )
        }
        _ => (branch, &[][..]),
    };

    // 参照の置換（句を除いた head・tail を連結した列に対して行う）。
    let mut joined: Vec<Token> = Vec::with_capacity(head.len() + tail.len());
    joined.extend(head.iter().cloned());
    joined.extend(tail.iter().cloned());
    replace_refs(&joined, &defs, budget)
}

/// `WINDOW` 句または `OVER (...)` の括弧の中身を 4 区間へ分けたもの。
struct Parts<'a> {
    refname: Option<&'a str>,
    partition: &'a [Token],
    order: &'a [Token],
    frame: &'a [Token],
}

fn split_spec(inner: &[Token]) -> Result<Parts<'_>, SqlSurfaceError> {
    let mut start = 0;
    let mut refname = None;
    if let Some(Token::Ident(n)) = inner.first() {
        if !is_spec_word(inner.first()) {
            refname = Some(n.as_str());
            start = 1;
        }
    }
    // 深さ 0 の区間開始位置を集める。
    let mut p_at = None;
    let mut o_at = None;
    let mut f_at = None;
    let mut d = 0usize;
    for k in start..inner.len() {
        let t = inner.get(k);
        match t {
            Some(Token::Punct('(')) => d += 1,
            Some(Token::Punct(')')) => d = d.saturating_sub(1),
            _ if d == 0 => {
                if is_ident(t, "PARTITION")
                    && matches!(inner.get(k + 1), Some(Token::Keyword(Keyword::By)))
                {
                    if p_at.is_none() && o_at.is_none() && f_at.is_none() {
                        p_at = Some(k);
                    }
                } else if matches!(t, Some(Token::Keyword(Keyword::Order))) {
                    if o_at.is_none() && f_at.is_none() {
                        o_at = Some(k);
                    }
                } else if f_at.is_none()
                    && ["ROWS", "RANGE", "GROUPS"].iter().any(|w| is_ident(t, w))
                {
                    // `ORDER BY range` や `PARTITION BY a, rows` の列名を境界と誤認しない。
                    let prev = k.checked_sub(1).and_then(|j| inner.get(j));
                    if !matches!(
                        prev,
                        Some(Token::Keyword(Keyword::By)) | Some(Token::Punct(','))
                    ) {
                        f_at = Some(k);
                    }
                }
            }
            _ => {}
        }
    }
    let first = [p_at, o_at, f_at].into_iter().flatten().min();
    // 先頭区間の前に未知のトークンがあれば拒否する（`(w x ...)` 等）。
    match first {
        Some(f) if f != start => {
            return Err(SqlSurfaceError::unsupported(
                "malformed window specification",
            ))
        }
        None if start < inner.len() => {
            return Err(SqlSurfaceError::unsupported(
                "malformed window specification",
            ))
        }
        _ => {}
    }
    let starts = [p_at, o_at, f_at];
    let slice = |at: Option<usize>| -> &[Token] {
        match at {
            Some(a) => {
                let end = starts
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|&n| n > a)
                    .min()
                    .unwrap_or(inner.len());
                inner.get(a..end).unwrap_or(&[])
            }
            None => &[],
        }
    };
    Ok(Parts {
        refname,
        partition: slice(p_at),
        order: slice(o_at),
        frame: slice(f_at),
    })
}

/// 参照先 `r` に対する括弧付き継承の規則（PostgreSQL の判定順: PARTITION BY → ORDER BY → フレーム）。
fn check_derive(parts: &Parts<'_>, r: &Def) -> Result<(), SqlSurfaceError> {
    if !parts.partition.is_empty() {
        return Err(windowing("cannot override PARTITION BY clause of window"));
    }
    if !parts.order.is_empty() && !r.order.is_empty() {
        return Err(windowing("cannot override ORDER BY clause of window"));
    }
    if !r.frame.is_empty() {
        return Err(windowing(
            "cannot copy window because it has a frame clause",
        ));
    }
    Ok(())
}

/// 派生の結果の `spec()` 長（確保なし。`check_derive` 通過後に呼ぶ）。
fn derived_spec_len(parts: &Parts<'_>, r: &Def) -> usize {
    let order = if parts.order.is_empty() {
        r.order.len()
    } else {
        parts.order.len()
    };
    r.partition.len() + order + parts.frame.len() + 2
}

/// 検査（[`check_derive`]）の後で派生定義を複写して組み立てる。
fn derive(parts: &Parts<'_>, r: &Def) -> Result<Def, SqlSurfaceError> {
    check_derive(parts, r)?;
    Ok(Def {
        name: String::new(),
        partition: r.partition.clone(),
        order: if parts.order.is_empty() {
            r.order.clone()
        } else {
            parts.order.to_vec()
        },
        frame: parts.frame.to_vec(),
    })
}

fn find_def<'a>(defs: &'a [Def], name: &str) -> Result<&'a Def, SqlSurfaceError> {
    defs.iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| SqlSurfaceError::UndefinedWindow {
            name: name.to_string(),
        })
}

/// WINDOW 句の 1 定義を、それより前の定義だけから解決して組み立てる。
fn build_def(name: String, inner: &[Token], earlier: &[Def]) -> Result<Def, SqlSurfaceError> {
    let parts = split_spec(inner)?;
    let mut def = match parts.refname {
        Some(r) => derive(&parts, find_def(earlier, r)?)?,
        None => Def {
            name: String::new(),
            partition: parts.partition.to_vec(),
            order: parts.order.to_vec(),
            frame: parts.frame.to_vec(),
        },
    };
    def.name = name;
    Ok(def)
}

/// `OVER <名前>`・`OVER (<名前> ...)` を、定義から組み立てたインライン `OVER (...)` へ置き換える。
/// 対象は直前が関数呼び出しの `)` の `OVER` だけ（`over` という別名・列名を誤検出しない）。
fn replace_refs(
    tokens: &[Token],
    defs: &[Def],
    budget: &mut usize,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while let Some(t) = tokens.get(i) {
        let after_call = is_ident(Some(t), "OVER")
            && i > 0
            && matches!(tokens.get(i - 1), Some(Token::Punct(')')))
            && is_call_close(tokens, i - 1);
        if !after_call {
            out.push(t.clone());
            i += 1;
            continue;
        }
        match tokens.get(i + 1) {
            Some(Token::Ident(name)) => {
                let def = find_def(defs, name)?;
                charge(budget, def.spec_len())?;
                let spec = def.spec();
                out.push(t.clone());
                out.extend(spec);
                i += 2;
            }
            Some(Token::Punct('(')) if !is_spec_word(tokens.get(i + 2)) => {
                let close = matching_close(tokens, i + 1)
                    .ok_or_else(|| SqlSurfaceError::unsupported("unbalanced parentheses"))?;
                let inner = tokens.get(i + 2..close).unwrap_or(&[]);
                let parts = split_spec(inner)?;
                let r = match parts.refname {
                    Some(r) => find_def(defs, r)?,
                    None => {
                        return Err(SqlSurfaceError::unsupported(
                            "malformed window specification",
                        ))
                    }
                };
                check_derive(&parts, r)?;
                charge(budget, derived_spec_len(&parts, r))?;
                let spec = derive(&parts, r)?.spec();
                out.push(t.clone());
                out.extend(spec);
                i = close + 1;
            }
            _ => {
                out.push(t.clone());
                i += 1;
            }
        }
    }
    Ok(out)
}

/// 位置 `close` の `)` が関数呼び出し（開き括弧の直前が識別子）を閉じているか。
fn is_call_close(tokens: &[Token], close: usize) -> bool {
    let mut depth = 0usize;
    let mut k = close + 1;
    while k > 0 {
        k -= 1;
        match tokens.get(k) {
            Some(Token::Punct(')')) => depth += 1,
            Some(Token::Punct('(')) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k > 0 && matches!(tokens.get(k - 1), Some(Token::Ident(_)));
                }
            }
            _ => {}
        }
    }
    false
}

fn charge(budget: &mut usize, n: usize) -> Result<(), SqlSurfaceError> {
    *budget = budget
        .checked_add(n)
        .filter(|&b| b <= MAX_EXPANDED_WINDOW_TOKENS)
        .ok_or_else(|| too_complex("expanded window specification too large"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn run(sql: &str) -> Result<Vec<Token>, String> {
        let t = tokenize(sql).expect("lex");
        expand_named_windows(&t)
            .map(|c| c.into_owned())
            .map_err(|e| e.wire_code().to_string())
    }

    fn same(named: &str, inline: &str) {
        assert_eq!(
            run(named).expect(named),
            tokenize(inline).expect("lex"),
            "{named}"
        );
    }

    fn code(sql: &str) -> String {
        run(sql).expect_err(sql)
    }

    #[test]
    fn fast_path_borrows() {
        let t = tokenize("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t").unwrap();
        assert!(matches!(
            expand_named_windows(&t).unwrap(),
            Cow::Borrowed(_)
        ));
        let t = tokenize("SELECT id FROM t").unwrap();
        assert!(matches!(
            expand_named_windows(&t).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn expands_reference_and_removes_clause() {
        same(
            "SELECT id, ROW_NUMBER() OVER w FROM t WINDOW w AS (PARTITION BY g ORDER BY s) LIMIT 5",
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY g ORDER BY s) FROM t LIMIT 5",
        );
        same(
            "SELECT id, SUM(s) OVER W FROM t WINDOW w AS (ORDER BY s ROWS 1 PRECEDING)",
            "SELECT id, SUM(s) OVER (ORDER BY s ROWS 1 PRECEDING) FROM t",
        );
    }

    #[test]
    fn expands_derivation_and_chain() {
        same(
            "SELECT id, SUM(s) OVER (w ORDER BY s) FROM t WINDOW w AS (PARTITION BY g)",
            "SELECT id, SUM(s) OVER (PARTITION BY g ORDER BY s) FROM t",
        );
        same(
            "SELECT id, SUM(s) OVER (w ROWS 1 PRECEDING) FROM t WINDOW w AS (ORDER BY s)",
            "SELECT id, SUM(s) OVER (ORDER BY s ROWS 1 PRECEDING) FROM t",
        );
        same(
            "SELECT id, RANK() OVER w2 FROM t WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY s)",
            "SELECT id, RANK() OVER (PARTITION BY g ORDER BY s) FROM t",
        );
    }

    #[test]
    fn multiple_refs_and_unused_definitions() {
        same(
            "SELECT RANK() OVER w, SUM(s) OVER w FROM t WINDOW w AS (ORDER BY s), u AS (ORDER BY id)",
            "SELECT RANK() OVER (ORDER BY s), SUM(s) OVER (ORDER BY s) FROM t",
        );
    }

    #[test]
    fn scopes_are_independent() {
        same(
            "WITH c AS (SELECT id, RANK() OVER w r FROM t WINDOW w AS (ORDER BY s)) SELECT id FROM c",
            "WITH c AS (SELECT id, RANK() OVER (ORDER BY s) r FROM t) SELECT id FROM c",
        );
        assert_eq!(
            code("SELECT id FROM t WHERE id IN (SELECT id FROM u WHERE RANK() OVER w = 1) WINDOW w AS (ORDER BY s)"),
            "42704"
        );
    }

    #[test]
    fn undefined_and_forward_refs() {
        assert_eq!(code("SELECT RANK() OVER nosuch FROM t"), "42704");
        assert_eq!(
            code("SELECT RANK() OVER (nosuch ORDER BY s) FROM t"),
            "42704"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t WINDOW w AS (u ORDER BY s), u AS (PARTITION BY g)"),
            "42704"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t WINDOW w AS (w ORDER BY s)"),
            "42704"
        );
    }

    #[test]
    fn windowing_errors() {
        assert_eq!(
            code("SELECT 1 FROM t WINDOW w AS (ORDER BY s), W AS (ORDER BY id)"),
            "42P20"
        );
        assert_eq!(
            code("SELECT RANK() OVER (w PARTITION BY g) FROM t WINDOW w AS (PARTITION BY g ORDER BY s)"),
            "42P20"
        );
        assert_eq!(
            code("SELECT RANK() OVER (w ORDER BY id) FROM t WINDOW w AS (ORDER BY s)"),
            "42P20"
        );
        assert_eq!(
            code("SELECT RANK() OVER (w) FROM t WINDOW w AS (ORDER BY s ROWS 1 PRECEDING)"),
            "42P20"
        );
        assert_eq!(
            code("SELECT 1 FROM t WINDOW a AS (ORDER BY s ROWS 1 PRECEDING), b AS (a)"),
            "42P20"
        );
        // 括弧なしの参照はフレーム付きでも可
        assert!(
            run("SELECT SUM(s) OVER w FROM t WINDOW w AS (ORDER BY s ROWS 1 PRECEDING)").is_ok()
        );
    }

    #[test]
    fn clause_position_errors() {
        assert_eq!(
            code("SELECT RANK() OVER w FROM t WINDOW w AS (ORDER BY s) WHERE id = 1"),
            "42601"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t ORDER BY id WINDOW w AS (ORDER BY s) LIMIT 10"),
            "42601"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t WINDOW w AS ORDER BY s"),
            "42601"
        );
    }

    #[test]
    fn unused_definition_is_validated() {
        // 未使用でもフレーム境界の誤りは PostgreSQL と同じ SQLSTATE
        assert_eq!(
            code("SELECT 1 FROM t WINDOW w AS (ORDER BY s ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW)"),
            "42P20"
        );
        assert_eq!(
            code("SELECT 1 FROM t WINDOW w AS (ORDER BY s LIMIT)"),
            "42601"
        );
    }

    #[test]
    fn over_alias_is_not_a_reference() {
        let t = tokenize("SELECT id FROM (SELECT id FROM t) over WHERE id = 1").unwrap();
        assert_eq!(expand_named_windows(&t).unwrap().as_ref(), t.as_slice());
    }

    #[test]
    fn limits() {
        let defs: Vec<String> = (0..=MAX_WINDOW_DEFINITIONS)
            .map(|i| format!("w{i} AS (ORDER BY s)"))
            .collect();
        let sql = format!("SELECT 1 FROM t WINDOW {}", defs.join(", "));
        assert_eq!(code(&sql), "54000");
    }

    #[test]
    fn keyword_like_columns_are_not_clause_boundaries() {
        // 列名の `offset`／`for`／`fetch`・`union` は句・集合演算子ではない（WHERE・select リスト）。
        same(
            "SELECT id, RANK() OVER w FROM t WHERE offset = 1 AND for = 2 OR fetch = 3 WINDOW w AS (ORDER BY id) LIMIT 10",
            "SELECT id, RANK() OVER (ORDER BY id) FROM t WHERE offset = 1 AND for = 2 OR fetch = 3 LIMIT 10",
        );
        same(
            "SELECT union, except, RANK() OVER w FROM t WHERE union = 1 WINDOW w AS (ORDER BY id)",
            "SELECT union, except, RANK() OVER (ORDER BY id) FROM t WHERE union = 1",
        );
        // 本物の集合演算子は引き続き枝に分かれ、枝ごとに WINDOW 句を持てる。
        same(
            "SELECT RANK() OVER w FROM t WINDOW w AS (ORDER BY id) UNION ALL SELECT RANK() OVER w FROM t WINDOW w AS (ORDER BY s)",
            "SELECT RANK() OVER (ORDER BY id) FROM t UNION ALL SELECT RANK() OVER (ORDER BY s) FROM t",
        );
        // 本来の誤配置は fail-closed のまま。
        assert_eq!(
            code("SELECT RANK() OVER w FROM t OFFSET 5 WINDOW w AS (ORDER BY id)"),
            "42601"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t LIMIT 5 WINDOW w AS (ORDER BY id)"),
            "42601"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t FOR UPDATE WINDOW w AS (ORDER BY id)"),
            "42601"
        );
        assert_eq!(
            code("SELECT RANK() OVER w FROM t USING PLAN(x) WINDOW w AS (ORDER BY id)"),
            "42601"
        );
    }

    #[test]
    fn join_using_before_window_clause() {
        same(
            "SELECT RANK() OVER w FROM a JOIN b USING (id) WINDOW w AS (ORDER BY id)",
            "SELECT RANK() OVER (ORDER BY id) FROM a JOIN b USING (id)",
        );
    }

    #[test]
    fn spec_lengths_match_materialized_specs() {
        let d = Def {
            name: "w".into(),
            partition: tokenize("PARTITION BY g").unwrap(),
            order: tokenize("ORDER BY s").unwrap(),
            frame: Vec::new(),
        };
        assert_eq!(d.spec_len(), d.spec().len());
        let inner = tokenize("w ROWS 1 PRECEDING").unwrap();
        let parts = split_spec(&inner).unwrap();
        assert_eq!(
            derived_spec_len(&parts, &d),
            derive(&parts, &d).unwrap().spec().len()
        );
    }

    #[test]
    fn expansion_budget_rejects_with_54000() {
        const WIDE: &str = "PARTITION BY a, b, c, d, e, f, g, h ORDER BY a, b, c, d, e, f, g, h";
        let cols = vec!["RANK() OVER w"; 3_000].join(", ");
        let sql = format!("SELECT {cols} FROM t WINDOW w AS ({WIDE})");
        assert_eq!(code(&sql), "54000");
    }

    #[test]
    fn trailing_clause_after_qualified_name_is_rejected() {
        for tail in ["OFFSET 1", "FETCH FIRST 1 ROWS ONLY", "FOR UPDATE"] {
            assert_eq!(
                code(&format!(
                    "SELECT t.id, RANK() OVER w FROM t ORDER BY t.id {tail} WINDOW w AS (ORDER BY id)"
                )),
                "42601",
                "{tail}"
            );
        }
        assert_eq!(
            code("SELECT RANK() OVER w FROM t WHERE t.a = t.b OFFSET 1 WINDOW w AS (ORDER BY id)"),
            "42601"
        );
    }

    #[test]
    fn window_clause_on_non_select_branches_is_rejected() {
        for sql in [
            "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id WINDOW w AS ()) SELECT * FROM d LIMIT 10",
            "WITH d AS (UPDATE t SET s = 1 WHERE id = 1 RETURNING id WINDOW w AS (ORDER BY id)) SELECT * FROM d",
            "WITH d AS (INSERT INTO t (id) VALUES (1) RETURNING id WINDOW w AS (ORDER BY id)) SELECT * FROM d",
            "DELETE FROM t WHERE id = 1 RETURNING id WINDOW w AS (ORDER BY id)",
            "INSERT INTO t (id) SELECT id FROM u WINDOW w AS (ORDER BY id)",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
        // SELECT 枝（CTE 付き主クエリ・集合演算の枝）は従来どおり受理する。
        assert!(run(
            "WITH c AS (SELECT id FROM t) SELECT RANK() OVER w FROM c WINDOW w AS (ORDER BY id)"
        )
        .is_ok());
    }

    #[test]
    fn statement_prefixes_before_select_are_accepted() {
        for prefix in [
            "EXPLAIN ",
            "EXPLAIN ANALYZE ",
            "CREATE VIEW v AS ",
            "CREATE MATERIALIZED VIEW mv AS ",
            "CREATE TABLE c AS ",
            "PREPARE p AS ",
        ] {
            assert!(
                run(&format!(
                    "{prefix}SELECT id, RANK() OVER w FROM t WINDOW w AS (ORDER BY id)"
                ))
                .is_ok(),
                "{prefix}"
            );
        }
        assert_eq!(
            code("EXPLAIN DELETE FROM t RETURNING id WINDOW w AS (ORDER BY id)"),
            "42601"
        );
    }

    #[test]
    fn recursive_cte_search_and_cycle_clauses_keep_the_window_clause_valid() {
        let cte = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3)";
        for clause in [
            "SEARCH DEPTH FIRST BY n SET ord",
            "CYCLE n SET is_cycle USING path",
            "SEARCH BREADTH FIRST BY n SET ord CYCLE n SET is_cycle TO 1 DEFAULT 0 USING path",
        ] {
            assert!(
                run(&format!(
                    "{cte} {clause} SELECT n, RANK() OVER w FROM r WINDOW w AS (ORDER BY n)"
                ))
                .is_ok(),
                "{clause}"
            );
        }
        // 後続の CTE 定義が続く形と、主文が SELECT でない形(fail-closed)。
        assert!(run(&format!(
            "{cte} SEARCH DEPTH FIRST BY n SET ord, c AS (SELECT 1) SELECT RANK() OVER w FROM c WINDOW w AS (ORDER BY n)"
        ))
        .is_ok());
        assert_eq!(
            code(&format!(
                "{cte} CYCLE n SET m USING p DELETE FROM t RETURNING id WINDOW w AS (ORDER BY id)"
            )),
            "42601"
        );
    }
}
