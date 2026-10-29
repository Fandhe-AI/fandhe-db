//! 集計 SELECT の結果行に窓関数をかける経路（SQL-64・Issue #1898 のポインタ）。
//!
//! 役割: `SELECT lang, count(*), rank() OVER (ORDER BY lang) FROM t GROUP BY lang` のように、
//! トップレベルの集計 SELECT（`GROUP BY`／`HAVING`／集計呼び出し）と窓項目が同じ SELECT
//! リストに並ぶ文を、評価順「WHERE → GROUP BY → HAVING → 窓 → ORDER BY → LIMIT/OFFSET」で
//! 実行できる形へ振り分ける。窓の母集合は集計後の結果行（1 グループ = 1 行）である。
//!
//! 方式: 第 2 の実行器を作らず、評価後射影形ビューの仕組み（[`Statement::BufferedView`]）を
//! 再利用する。本文は「窓項目を除いた集計 SELECT（＋窓が参照するグループ化キーの隠し項目）」
//! で既存の集計経路が実行し、外側（`sql::view_buffered`）が評価済みセルに窓を適用する。
//! 本文の結果列は外側の束縛専用に改名する（利用者の非窓項目は利用者が書けない内部名、隠し
//! キーは実キー列名）ため、窓が名前解決できるのはグループ化キー列と巻き上げ列だけになり、出力別名との
//! 衝突は起きない。利用者に見せる列名は `view_buffered::plan_outer` が改名前へ戻す。
//!
//! Issue #1899: 窓の引数・`PARTITION BY`・`ORDER BY` の中にある集計式（`rank() OVER (ORDER BY
//! sum(x))`・`sum(sum(x)) OVER ()` 等）は、集計呼び出しを含むセグメント全体を隠し項目として本文
//! （集計経路）へ巻き上げ（hoist）、窓側は利用者が書けない内部名の列参照へ置き換える。窓が
//! 名前解決できるのはグループ化キー列と巻き上げた集計列になる。
//!
//! 呼び出し元: [`super::validate_select_statement`] だけ。受理できない形は `Ok(None)` を返し、
//! 呼び出し元が従来のエラー（`42601`）へ倒す（fail-closed）。
//!
//! テナント境界: 可視行の確定は本文（既存の集計経路）が参照者の `PolicyContext` で行い、
//! 外側は `PolicyContext` を受け取らない。隠しキー列は射影しないため結果へ漏れない。
//! 本文・外側のトークン列は既存トークンの複写と `Token` の直接構築だけで組み立て、
//! 文字列連結や再字句解析はしない。

use super::*;

/// 非窓項目の本文結果列を外側で受ける内部名の接頭辞（制御文字入りで利用者の識別子と衝突しない）。
const ITEM_PREFIX: &str = "\u{1}gwitem";
/// 本文に補う隠しの `count(*)`（集計項目を 1 つ以上要求する本文の制約を満たすためだけの列）。
const FILL_NAME: &str = "\u{1}gwfill";
/// 窓の中から巻き上げた集計式を本文の隠し項目として受ける内部名の接頭辞（同上）。
const AGG_PREFIX: &str = "\u{1}gwagg";
/// 隠しキー項目の本文側の別名の接頭辞（出力名の衝突回避。外側へは実キー列名で渡す）。
const KEY_ALIAS_PREFIX: &str = "\u{1}gwkey";

fn ident(s: &str) -> Token {
    Token::Ident(s.to_string())
}

/// 括弧の深さ 0 で最初の `FROM` の添字。括弧が閉じない場合は `None`。
fn find_top_level_from(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(1) {
        match t {
            Token::Punct('(' | '[') => depth = depth.saturating_add(1),
            Token::Punct(')' | ']') => depth = depth.checked_sub(1)?,
            Token::Keyword(Keyword::From) if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// `tokens[start..end]` を深さ 0 の `,` で項目に分け、`(開始, 終了)` の組を返す。空項目は `None`。
fn split_items(tokens: &[Token], start: usize, end: usize) -> Option<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut item_start = start;
    for i in start..end {
        match tokens.get(i)? {
            Token::Punct('(' | '[') => depth = depth.saturating_add(1),
            Token::Punct(')' | ']') => depth = depth.checked_sub(1)?,
            Token::Punct(',') if depth == 0 => {
                if i == item_start {
                    return None;
                }
                out.push((item_start, i));
                item_start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    if item_start >= end || depth != 0 {
        return None;
    }
    out.push((item_start, end));
    Some(out)
}

/// 項目が集計関数の呼び出し（`name '('`）を含むか。
fn item_has_aggregate_call(item: &[Token]) -> bool {
    item.windows(2).any(|w| {
        matches!(&w[0], Token::Ident(n) if is_aggregate_function_name(n))
            && matches!(w[1], Token::Punct('('))
    })
}

/// 窓項目の引数・`PARTITION BY`・`ORDER BY` に集計呼び出しがあるか（Issue #1899。振り分け条件の
/// 判定専用で、巻き上げ本体は [`hoist_window_item`]）。先頭の関数名、
/// `FILTER (...)` の括弧内、フレーム句以降は対象外（それぞれ既存の検査が担う）。
fn window_item_has_inner_aggregate(item: &[Token]) -> bool {
    let mut depth = 0usize;
    let mut in_over = false;
    let mut i = 1usize;
    while let Some(t) = item.get(i) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Ident(n) => {
                let next_is_open = matches!(item.get(i.saturating_add(1)), Some(Token::Punct('(')));
                if depth == 0 && n.eq_ignore_ascii_case("OVER") {
                    in_over = true;
                } else if depth == 0 && n.eq_ignore_ascii_case("FILTER") && next_is_open {
                    // FILTER の括弧は丸ごと読み飛ばす。
                    let mut d = 0usize;
                    let mut j = i.saturating_add(1);
                    while let Some(u) = item.get(j) {
                        match u {
                            Token::Punct('(') => d = d.saturating_add(1),
                            Token::Punct(')') => {
                                d = d.saturating_sub(1);
                                if d == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j = j.saturating_add(1);
                    }
                    i = j;
                } else if in_over
                    && depth == 1
                    && (n.eq_ignore_ascii_case("ROWS")
                        || n.eq_ignore_ascii_case("RANGE")
                        || n.eq_ignore_ascii_case("GROUPS"))
                {
                    return false;
                } else if is_aggregate_function_name(n) && next_is_open {
                    return true;
                }
            }
            _ => {}
        }
        i = i.saturating_add(1);
    }
    false
}

/// 直前のトークンが式の終端か（`ROWS`／`RANGE`／`GROUPS` をフレーム開始と見なす条件）。演算子・
/// `,`・`BY` の直後の同名識別子（`sum(k) + rows` 等）は式の途中の識別子なのでフレームではない。
fn ends_expression(prev: Option<&Token>) -> bool {
    match prev {
        Some(Token::Ident(n)) => ![
            "AND", "OR", "NOT", "IS", "LIKE", "ILIKE", "IN", "BETWEEN", "DISTINCT",
        ]
        .iter()
        .any(|k| n.eq_ignore_ascii_case(k)),
        Some(
            Token::Number(_)
            | Token::StringLiteral(_)
            | Token::QuotedBoolWord(_)
            | Token::Punct(')' | ']'),
        ) => true,
        _ => false,
    }
}

/// `open` の `(` に対応する `)` の添字。閉じない場合は `None`。
fn find_close(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// 巻き上げ段で構造を読み取れない窓項目の拒否（fail-closed。固定文言）。
fn hoist_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "unsupported aggregate placement inside a window function over a grouped query",
    )
}

/// `item[s..e]` を深さ 0 の `,` で分け、集計呼び出しを含むセグメントの `(開始, 終了)` だけ返す。
/// 集計を含まない区間は分割せず空を返す（構造の検証は従来どおり窓の構文解析が担う）。
fn aggregate_segments(
    item: &[Token],
    s: usize,
    e: usize,
) -> Result<Vec<(usize, usize)>, SqlSurfaceError> {
    let Some(range) = item.get(s..e) else {
        return Err(hoist_unsupported());
    };
    if !item_has_aggregate_call(range) {
        return Ok(Vec::new());
    }
    let segs = split_items(item, s, e).ok_or_else(hoist_unsupported)?;
    Ok(segs
        .into_iter()
        .filter(|&(a, b)| item.get(a..b).is_some_and(item_has_aggregate_call))
        .collect())
}

/// 窓項目の引数・`PARTITION BY`・`ORDER BY` にある集計式を含むセグメント全体を `hoisted` へ
/// 登録（同一トークン列は重複排除）し、内部名の識別子 1 トークンへ置き換えたトークン列を返す
/// （Issue #1899。呼び出し元は [`try_build`] だけ）。セグメント単位で置換するのは、窓キーが識別子
/// しか受理しないため（`ORDER BY sum(k) + 1` 等）。`FILTER (...)` の内側・フレーム句以降・先頭の
/// `DISTINCT` 付き引数は触らず、既存の検査／構文解析に委ねる。構造が想定外なら `0A000`。
fn hoist_window_item(
    item: &[Token],
    hoisted: &mut Vec<Vec<Token>>,
) -> Result<Vec<Token>, SqlSurfaceError> {
    if !matches!(item.get(1), Some(Token::Punct('('))) {
        return Err(hoist_unsupported());
    }
    let close = find_close(item, 1).ok_or_else(hoist_unsupported)?;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    if close > 2 && !is_distinct_modifier(item, 2) {
        spans.extend(aggregate_segments(item, 2, close)?);
    }
    let mut c = close;
    if matches!(item.get(c.saturating_add(1)), Some(Token::Ident(n)) if n.eq_ignore_ascii_case("FILTER"))
        && matches!(item.get(c.saturating_add(2)), Some(Token::Punct('(')))
    {
        c = find_close(item, c.saturating_add(2)).ok_or_else(hoist_unsupported)?;
    }
    if !matches!(item.get(c.saturating_add(1)), Some(Token::Ident(n)) if n.eq_ignore_ascii_case("OVER"))
    {
        return Err(hoist_unsupported());
    }
    let over_open = c.saturating_add(2);
    if !matches!(item.get(over_open), Some(Token::Punct('('))) {
        return Err(hoist_unsupported());
    }
    let over_close = find_close(item, over_open).ok_or_else(hoist_unsupported)?;
    let spec_start = over_open.saturating_add(1);

    // OVER の括弧直下で PARTITION BY／ORDER BY の範囲とフレーム句の開始を求める。
    let mut part_start: Option<usize> = None;
    let mut order_kw: Option<usize> = None;
    let mut frame_start = over_close;
    let mut depth = 0usize;
    for i in spec_start..over_close {
        let t = item.get(i).ok_or_else(hoist_unsupported)?;
        let next_is_by = matches!(
            item.get(i.saturating_add(1)),
            Some(Token::Keyword(Keyword::By))
        );
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.checked_sub(1).ok_or_else(hoist_unsupported)?,
            Token::Keyword(Keyword::Order) if depth == 0 && order_kw.is_none() && next_is_by => {
                order_kw = Some(i);
            }
            Token::Ident(n)
                if depth == 0
                    && i == spec_start
                    && next_is_by
                    && n.eq_ignore_ascii_case("PARTITION") =>
            {
                part_start = Some(i.saturating_add(2));
            }
            Token::Ident(n)
                if depth == 0
                    && i > spec_start
                    && (n.eq_ignore_ascii_case("ROWS")
                        || n.eq_ignore_ascii_case("RANGE")
                        || n.eq_ignore_ascii_case("GROUPS"))
                    && ends_expression(item.get(i.saturating_sub(1))) =>
            {
                frame_start = i;
                break;
            }
            _ => {}
        }
    }
    if let Some(ps) = part_start {
        let pe = order_kw.unwrap_or(frame_start);
        spans.extend(aggregate_segments(item, ps, pe)?);
    }
    if let Some(ok) = order_kw {
        let os = ok.saturating_add(2);
        for (a, b) in aggregate_segments(item, os, frame_start)? {
            // 末尾の `ASC|DESC`・`NULLS FIRST|LAST` は式の外に残す。
            let mut e = b;
            if let (Some(Token::Ident(x)), Some(Token::Ident(y))) =
                (item.get(e.saturating_sub(2)), item.get(e.saturating_sub(1)))
            {
                if e.saturating_sub(2) > a
                    && x.eq_ignore_ascii_case("NULLS")
                    && (y.eq_ignore_ascii_case("FIRST") || y.eq_ignore_ascii_case("LAST"))
                {
                    e = e.saturating_sub(2);
                }
            }
            if let Some(Token::Ident(x)) = item.get(e.saturating_sub(1)) {
                // 直前がオペランドで終わる場合のみ修飾子（`sum(k) + desc` の `desc` は列名）。
                if e.saturating_sub(1) > a
                    && (x.eq_ignore_ascii_case("ASC") || x.eq_ignore_ascii_case("DESC"))
                    && ends_expression(item.get(e.saturating_sub(2)))
                {
                    e = e.saturating_sub(1);
                }
            }
            if item.get(a..e).is_some_and(item_has_aggregate_call) {
                spans.push((a, e));
            }
        }
    }

    spans.sort_unstable();
    let mut out: Vec<Token> = Vec::with_capacity(item.len());
    let mut prev = 0usize;
    for (a, b) in spans {
        let seg = item.get(a..b).ok_or_else(hoist_unsupported)?;
        if a < prev || seg.is_empty() {
            return Err(hoist_unsupported());
        }
        // 集計呼び出しの引数の中の集計呼び出し（入れ子集計）は PostgreSQL と同じ `42803`。
        for (i, w) in seg.windows(2).enumerate() {
            if matches!(&w[0], Token::Ident(n) if is_aggregate_function_name(n))
                && matches!(w[1], Token::Punct('('))
            {
                let open = i.saturating_add(1);
                let inner_close = find_close(seg, open).ok_or_else(hoist_unsupported)?;
                if seg
                    .get(open.saturating_add(1)..inner_close)
                    .is_some_and(item_has_aggregate_call)
                {
                    return Err(SqlSurfaceError::GroupingError {
                        detail: "aggregate function calls cannot be nested",
                    });
                }
            }
        }
        if tokens_contain_window_over(seg) {
            return Err(SqlSurfaceError::GroupingError {
                detail: "aggregate function calls cannot contain window function calls",
            });
        }
        let idx = match hoisted.iter().position(|h| h.as_slice() == seg) {
            Some(i) => i,
            None => {
                hoisted.push(seg.to_vec());
                hoisted.len().saturating_sub(1)
            }
        };
        out.extend(
            item.get(prev..a)
                .ok_or_else(hoist_unsupported)?
                .iter()
                .cloned(),
        );
        out.push(ident(&format!("{AGG_PREFIX}{idx}")));
        prev = b;
    }
    out.extend(
        item.get(prev..)
            .ok_or_else(hoist_unsupported)?
            .iter()
            .cloned(),
    );
    Ok(out)
}

/// 分類済みの SELECT リスト項目。
enum ListItem<'a> {
    /// 窓項目（巻き上げ後のトークンと解析結果。`position` は SELECT リスト全体での位置）。
    Window(Vec<Token>, Box<WindowSelectItem>),
    /// 非窓項目（本文へそのまま渡す）。
    Plain(&'a [Token]),
}

/// カンマ区切りの 2 項目目以降の前に `,` を積む。
fn push_sep(out: &mut Vec<Token>, n: &mut usize) {
    if *n > 0 {
        out.push(Token::Punct(','));
    }
    *n = n.saturating_add(1);
}

/// FROM 以降に `LIMIT`／`OFFSET`／`FETCH` があるか。
fn tail_has_row_limit(tail: &[Token]) -> bool {
    tail.iter().any(|t| match t {
        Token::Keyword(Keyword::Limit) => true,
        Token::Ident(n) => n.eq_ignore_ascii_case("OFFSET") || n.eq_ignore_ascii_case("FETCH"),
        _ => false,
    })
}

/// 項目内の列参照候補（関数名・別名・型名・演算子／`CASE` 系キーワード・`NULL`／`TRUE`／`FALSE` 以外の識別子）。
fn column_ref_candidates(item: &[Token]) -> Vec<&str> {
    item.iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let Token::Ident(n) = t else { return None };
            let next_is_call = matches!(item.get(i.saturating_add(1)), Some(Token::Punct('(')));
            let prev = i.checked_sub(1).and_then(|j| item.get(j));
            let after_as = matches!(prev, Some(Token::Ident(p)) if p.eq_ignore_ascii_case("AS"));
            let after_cast = matches!(prev, Some(Token::TypeCast));
            let reserved = [
                "AS",
                "CASE",
                "WHEN",
                "THEN",
                "ELSE",
                "END",
                "NULL",
                "TRUE",
                "FALSE",
                "OR",
                "AND",
                "NOT",
                "IS",
                "LIKE",
                "ILIKE",
                "IN",
                "BETWEEN",
                "ESCAPE",
                "DISTINCT",
                "PRECISION",
                "VARYING",
            ]
            .iter()
            .any(|k| n.eq_ignore_ascii_case(k));
            (!(next_is_call || after_as || after_cast || reserved)).then_some(n.as_str())
        })
        .collect()
}

/// 集計 SELECT と窓項目の併用を [`Statement::BufferedView`]（`origin = GroupedWindow`）へ振り分ける。
///
/// `Ok(None)` は「この経路の対象外」（呼び出し元が従来の振り分け・エラーへ進む）。対象と確定した後の
/// 構文・束縛上の誤りは `Err` で返す。`has_group_by_or_having` は呼び出し元が求めた
/// `GROUP BY`／`HAVING` の有無で、これ・非窓項目の集計呼び出し・窓の内側の集計呼び出しの
/// いずれも無ければ対象外とする。
pub(super) fn try_build(
    tokens: &[Token],
    lookup: &impl TableLookup,
    subquery_ctx: Option<usize>,
    limits: crate::nesting_limits::NestingLimits,
    has_group_by_or_having: bool,
) -> Result<Option<Statement>, SqlSurfaceError> {
    // トップレベル文・非 DISTINCT・サブクエリなしの形だけが対象。
    if !subquery_ctx.is_none_or(|d| d == 0)
        || !matches!(tokens.first(), Some(Token::Keyword(Keyword::Select)))
        || is_distinct_modifier(tokens, 1)
        || tokens
            .iter()
            .skip(1)
            .any(|t| matches!(t, Token::Keyword(Keyword::Select)))
    {
        return Ok(None);
    }
    let Some(from_idx) = find_top_level_from(tokens) else {
        return Ok(None);
    };
    // FROM 以降（WHERE／GROUP BY／HAVING／ORDER BY）の窓は従来どおり拒否側へ。
    let Some(tail) = tokens.get(from_idx..) else {
        return Ok(None);
    };
    if tokens_contain_window_over(tail) {
        return Ok(None);
    }
    let Some(ranges) = split_items(tokens, 1, from_idx) else {
        return Ok(None);
    };
    // 窓項目の有無と、非窓項目の集計呼び出しでこの経路の対象かを先に確定する（解析前）。
    let mut any_window = false;
    let mut plain_has_aggregate = false;
    let mut window_has_aggregate = false;
    for &(s, e) in &ranges {
        let Some(item) = tokens.get(s..e) else {
            return Ok(None);
        };
        if tokens_contain_window_over(item) {
            any_window = true;
            if window_item_has_inner_aggregate(item) {
                window_has_aggregate = true;
            }
        } else {
            if item_has_aggregate_call(item) {
                plain_has_aggregate = true;
            }
        }
    }
    if !any_window || !(has_group_by_or_having || plain_has_aggregate || window_has_aggregate) {
        return Ok(None);
    }
    // 窓内集計だけが根拠で GROUP BY／HAVING も非窓の集計も無い文は暗黙の単一グループになる。
    // 非窓項目が列参照を持たない（定数のみ）なら受理し、未定義列は束縛段の `42703`、未グループ化列は
    // 束縛側の `42803` に任せる。ただし `LIMIT`／`OFFSET` 付きでは本文の形検査が先に `42601` へ倒れて
    // しまうため、その場合に限り列参照を持つ非窓項目を先に `42803` で拒否する。
    // 未定義名は `42803` に倒さず `42703` にする（テーブルの列集合が分かるときのみ判別できる）。
    if !has_group_by_or_having && !plain_has_aggregate && tail_has_row_limit(tail) {
        let known_cols = match tail.get(1) {
            Some(Token::Ident(t)) => lookup.table_columns(t)?,
            _ => None,
        };
        let mut grouping = false;
        for &(s, e) in &ranges {
            let Some(item) = tokens.get(s..e) else {
                continue;
            };
            if tokens_contain_window_over(item) {
                continue;
            }
            for n in column_ref_candidates(item) {
                match &known_cols {
                    Some(ks)
                        if !n.eq_ignore_ascii_case("id")
                            && !ks.iter().any(|c| c.eq_ignore_ascii_case(n)) =>
                    {
                        return Err(SqlSurfaceError::UndefinedColumn {
                            name: n.to_string(),
                        });
                    }
                    _ => grouping = true,
                }
            }
        }
        if grouping {
            return Err(SqlSurfaceError::GroupingError {
                detail:
                    "column must appear in the GROUP BY clause or be used in an aggregate function",
            });
        }
    }
    // Describe 経路は本文が `$n` を含まない前提でダミーフラグを渡すため、不一致を避けて拒否する。
    if tokens.iter().any(|t| matches!(t, Token::Param(_))) {
        return Err(SqlSurfaceError::feature_not_supported(
            "parameters are not supported in a grouped query with window functions",
        ));
    }

    // 項目の分類と窓項目の解析。
    let mut list: Vec<ListItem<'_>> = Vec::with_capacity(ranges.len());
    let mut hoisted: Vec<Vec<Token>> = Vec::new();
    for (pos, &(s, e)) in ranges.iter().enumerate() {
        let Some(item) = tokens.get(s..e) else {
            return Ok(None);
        };
        if !tokens_contain_window_over(item) {
            list.push(ListItem::Plain(item));
            continue;
        }
        if !Parser::new(item).peek_is_window_item_start() {
            return Ok(None);
        }
        let rewritten = hoist_window_item(item, &mut hoisted)?;
        let mut p = Parser::new(&rewritten).with_nesting_limits(limits);
        let mut w = p.parse_window_item()?;
        if p.peek().is_some() {
            return Ok(None);
        }
        w.position = pos;
        list.push(ListItem::Window(rewritten, Box::new(w)));
    }

    // 窓が参照するグループ化キー列（重複排除・名前順）。
    let mut referenced = std::collections::HashSet::new();
    for it in &list {
        if let ListItem::Window(_, w) = it {
            w.collect_referenced_idents(&mut referenced);
        }
    }
    // 巻き上げた集計列は本文の隠し項目として別に積むため、キー列候補から除く。
    let mut keys: Vec<String> = referenced
        .into_iter()
        .filter(|k| !k.starts_with(AGG_PREFIX))
        .collect();
    keys.sort();

    // 本文: 非窓項目（元の順序）＋必要なら隠し count(*) ＋隠しキー項目。
    let need_fill = !plain_has_aggregate && hoisted.is_empty();
    let mut body: Vec<Token> = vec![Token::Keyword(Keyword::Select)];
    let mut aliases: Vec<String> = Vec::new();
    let mut body_items = 0usize;
    let mut plain_idx = 0usize;
    for it in &list {
        if let ListItem::Plain(item) = it {
            push_sep(&mut body, &mut body_items);
            body.extend(item.iter().cloned());
            aliases.push(format!("{ITEM_PREFIX}{plain_idx}"));
            plain_idx = plain_idx.saturating_add(1);
        }
    }
    if need_fill {
        push_sep(&mut body, &mut body_items);
        body.extend([
            ident("count"),
            Token::Punct('('),
            Token::Punct('*'),
            Token::Punct(')'),
            ident("AS"),
            ident(FILL_NAME),
        ]);
        aliases.push(FILL_NAME.to_string());
    }
    for (j, seg) in hoisted.iter().enumerate() {
        push_sep(&mut body, &mut body_items);
        body.extend(seg.iter().cloned());
        body.push(ident("AS"));
        body.push(ident(&format!("{AGG_PREFIX}{j}")));
        aliases.push(format!("{AGG_PREFIX}{j}"));
    }
    for (j, k) in keys.iter().enumerate() {
        push_sep(&mut body, &mut body_items);
        body.extend([
            ident(k),
            ident("AS"),
            ident(&format!("{KEY_ALIAS_PREFIX}{j}")),
        ]);
        aliases.push(k.clone());
    }
    body.extend(tail.iter().cloned());

    let shape = parse_aggregate_shape(&body, subquery_ctx, false, true, limits)?;
    if shape.items.len() != body_items {
        return Err(SqlSurfaceError::unsupported(
            "window functions over a grouped query: unsupported select list",
        ));
    }
    let ParsedAggregateShape {
        table_name,
        items,
        where_predicates,
        mut group_by,
    } = shape;

    // 文全体の ORDER BY は本文に残す（窓は行順を変えない）。序数はユーザーのリスト基準なので
    // 本文基準へ付け替え、LIMIT／OFFSET は窓の後に掛かるよう外側へ移す。
    let mut limit = None;
    let mut offset = 0u32;
    if let Some(g) = group_by.as_mut() {
        if g.grouping_sets.is_some() {
            return Err(SqlSurfaceError::unsupported(
                "window functions cannot be combined with ROLLUP, CUBE or GROUPING SETS",
            ));
        }
        if g.with_ties {
            return Err(SqlSurfaceError::feature_not_supported(
                "WITH TIES is not supported with window functions over a grouped query",
            ));
        }
        for key in g.order_by.iter_mut() {
            let Some(k) = key.ordinal else { continue };
            let out_of_range = || SqlSurfaceError::OrderByPositionNotInSelectList { position: k };
            // 序数は 1 始まり。0・負数は範囲外として 42P10 で拒否する（fail-closed）。
            let u = k
                .checked_sub(1)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(out_of_range)?;
            match list.get(u) {
                None => return Err(out_of_range()),
                Some(ListItem::Window(..)) => return Err(SqlSurfaceError::unsupported(
                    "ORDER BY position of a window function is not supported over a grouped query",
                )),
                Some(ListItem::Plain(_)) => {
                    let before = list
                        .iter()
                        .take(u)
                        .filter(|i| matches!(i, ListItem::Plain(_)))
                        .count();
                    key.ordinal =
                        Some(i64::try_from(before.saturating_add(1)).map_err(|_| out_of_range())?);
                }
            }
        }
        limit = g.limit.take();
        offset = std::mem::take(&mut g.offset);
    }

    // 実テーブル以外（ビュー等）は本 Issue の対象外（fail-closed）。
    let resolved = crate::sql::view::resolve_from(lookup, &table_name)?;
    if !matches!(resolved, crate::sql::view::Resolved::Table) {
        return Err(SqlSurfaceError::unsupported(
            "window functions over a grouped query are supported only on a table",
        ));
    }
    let agg = build_aggregate_from_resolved(
        table_name.clone(),
        resolved,
        items,
        where_predicates,
        group_by,
    )?;

    // 外側: 非窓項目は内部名の列参照、窓項目は元のトークンのまま（位置はリスト基準）。
    let mut outer_tokens: Vec<Token> = vec![Token::Keyword(Keyword::Select)];
    let mut n = 0usize;
    let mut plain_idx = 0usize;
    for it in &list {
        push_sep(&mut outer_tokens, &mut n);
        match it {
            ListItem::Plain(_) => {
                outer_tokens.push(ident(&format!("{ITEM_PREFIX}{plain_idx}")));
                plain_idx = plain_idx.saturating_add(1);
            }
            ListItem::Window(toks, _) => outer_tokens.extend(toks.iter().cloned()),
        }
    }
    outer_tokens.push(Token::Keyword(Keyword::From));
    outer_tokens.push(ident(&table_name));
    let ParsedSelect::Scan(mut outer_shape) =
        parse_select_shape(&outer_tokens, None, false, limits)?
    else {
        return Err(SqlSurfaceError::unsupported(
            "window functions over a grouped query: unsupported select list",
        ));
    };
    outer_shape.limit = limit;
    outer_shape.offset = offset;
    let outer = buffered_scan_outer(outer_shape)?;

    Ok(Some(Statement::BufferedView(ValidatedBufferedView {
        view_name: table_name,
        body: Box::new(Statement::Aggregate(agg)),
        outer,
        column_aliases: aliases,
        origin: BufferedOrigin::GroupedWindow,
    })))
}
