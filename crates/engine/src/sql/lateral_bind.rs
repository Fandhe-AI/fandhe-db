//! LATERAL 派生表の本文が参照する「左側の relation の列」の束縛（SQL-62・Issue #3321。
//! 設計記録 `docs/design/lateral-derived-table.md` の L3・L4。親 #3319）。
//!
//! 責務境界: `sql::derived_table::resolve_level` が LATERAL の本文トークンを CTE 本文のパーサーへ
//! 渡す**前**に [`bind_lateral_body`] を 1 回呼ぶ。本モジュールは次の 3 つだけを行う。
//!
//! 1. 本文の深さ 0 にある、左側 relation の列への参照（`d.lang` の修飾形と、内側に無く左側にだけ
//!    ある非修飾名）を予約内部名（[`crate::sql::correlation::lateral_ref_placeholder`]）の `Ident` へ
//!    置き換えて記録する。外側の値を SQL 文字列・トークン列・AST のリテラルへ連結しない（L3）。
//! 2. 外側参照を置ける位置を最上位 `WHERE` に限る（L4）。それ以外の位置・入れ子サブクエリ内の
//!    外側参照は固定文言の `0A000`。
//! 3. 外側参照を含む `WHERE` の連言を本文から取り除く（既存の本文検証は内側だけで通す）。取り除いた
//!    連言の内側の識別子は、ここで内側の列集合に対して存在を検査する（`42703`／`42P01`）。
//!
//! 左側以外（LATERAL 自身・右側・後続の FROM 項目）の修飾子は左側の公開名に一致しないため
//! 書き換えられず、後段の修飾列正規化（`qualified_column`）が `42P01` にする。束縛はカタログの構造
//! （列集合）だけを見て行・テナントに依存しない（RLS-10 (b)）。エラー文は固定文言と利用者自身が
//! 書いた名前だけを含み、予約内部名は出さない。走査は反復処理・添字は `get()`。
//!
//! 外側の行ごとの再評価（実行）は #3322 以降。本 Issue の束縛結果（[`LateralBinding`]）は
//! `derived_table::build_statement` が「外側参照の有無」を判定して実行形を選ぶために使う。

use crate::sql::allowlist::{probe_from_table_ref, SqlSurfaceError};
use crate::sql::correlation::{lateral_ref_placeholder, parse_lateral_ref_placeholder};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::qualified_column::{is_non_column_position, is_syntax_word};
use crate::sql::relation::TableRef;

/// 本文が参照する左側 relation の列 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct LateralOuterRef {
    /// 左側 relation の並び（結合連鎖の 0 始まり）の添字。
    pub(crate) rel_index: usize,
    /// 左側 relation の列名（左側の列集合にある綴り）。
    pub(crate) column: String,
}

/// 束縛結果。`outer_refs` は `(rel_index, column)` の昇順で重複を除く。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LateralBinding {
    pub(crate) outer_refs: Vec<LateralOuterRef>,
}

impl LateralBinding {
    /// 左側の列を 1 つでも参照するか（真なら外側の行ごとの再評価が要る形）。
    pub(crate) fn is_correlated(&self) -> bool {
        !self.outer_refs.is_empty()
    }
}

/// [`bind_lateral_body`] の結果。
#[derive(Debug)]
pub(crate) struct BoundBody {
    /// 外側参照を含む連言を取り除いた本文（既存の本文検証へ渡す）。
    pub(crate) tokens: Vec<Token>,
    pub(crate) binding: LateralBinding,
    /// 取り除いた相関連言だけを `WHERE` に持つ本文（`SELECT <元の投影> FROM <内側> WHERE <相関連言>`。
    /// 外側参照は予約内部名のまま。外側参照が無ければ `None`）。`derived_table` が内側の別名を
    /// 正規化したうえで述語へ解析し、外側タプルごとに評価する相関述語にする（Issue #3322。L2）。
    pub(crate) correlated_body: Option<Vec<Token>>,
    /// 内側 relation の列名（カタログの綴り。疑似列 `id` を含みうる。列集合を返せなければ `None`）。
    pub(crate) inner_columns: Option<Vec<String>>,
}

fn outer_position_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "outer column references are only supported in the WHERE clause of a LATERAL subquery",
    )
}

fn unresolvable_left_columns() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "columns of the relation referenced by a LATERAL subquery cannot be resolved",
    )
}

fn is_literal_word(n: &str) -> bool {
    ["true", "false", "null", "nulls", "first", "last"]
        .iter()
        .any(|w| n.eq_ignore_ascii_case(w))
}

fn find_col<'a>(cols: &'a [String], name: &str) -> Option<&'a String> {
    cols.iter().find(|c| c.eq_ignore_ascii_case(name))
}

/// 左側 relation の列集合の遅延キャッシュ。`None` は「列集合を返せない」。
struct LeftCols<'a, F> {
    left: &'a [TableRef],
    columns: &'a mut F,
    cache: Vec<Option<Option<Vec<String>>>>,
}

impl<F> LeftCols<'_, F>
where
    F: FnMut(&TableRef) -> Result<Option<Vec<String>>, SqlSurfaceError>,
{
    fn get(&mut self, idx: usize) -> Result<Option<&[String]>, SqlSurfaceError> {
        let Some(rel) = self.left.get(idx) else {
            return Ok(None);
        };
        if self.cache.get(idx).is_some_and(Option::is_none) {
            let cols = (self.columns)(rel)?;
            if let Some(slot) = self.cache.get_mut(idx) {
                *slot = Some(cols);
            }
        }
        Ok(self
            .cache
            .get(idx)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.as_deref()))
    }
}

/// 深さ 0 の最初の `FROM` の位置（`IS DISTINCT FROM` は除く）。
fn find_top_from(body: &[Token]) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in body.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Keyword(Keyword::From) if depth == 0 => {
                let after_distinct = i.checked_sub(1).and_then(|p| body.get(p)).is_some_and(
                    |t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("DISTINCT")),
                );
                if !after_distinct {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// 深さ 0 の `WHERE` 句の範囲 `[WHERE の位置, 句の終端)`。
fn find_where(tokens: &[Token]) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth != 0 => {}
            Token::Keyword(Keyword::Where) if start.is_none() => start = Some(i),
            Token::Keyword(Keyword::Order | Keyword::Limit) | Token::Punct(';')
                if start.is_some() =>
            {
                return start.map(|s| (s, i));
            }
            Token::Ident(n) if start.is_some() => {
                let next = tokens.get(i.saturating_add(1));
                let ends = ["GROUP", "HAVING", "OFFSET", "FETCH", "WINDOW"]
                    .iter()
                    .any(|w| n.eq_ignore_ascii_case(w))
                    || (n.eq_ignore_ascii_case("USING") && matches!(next, Some(Token::Ident(_))));
                if ends {
                    return start.map(|s| (s, i));
                }
            }
            _ => {}
        }
    }
    start.map(|s| (s, tokens.len()))
}

/// WHERE の中身 `[from, to)` を深さ 0 の `AND` で連言に分ける。深さ 0 に `OR`／`BETWEEN` があれば
/// 分けずに 1 つの連言として返す（優先順位・`BETWEEN .. AND ..` を壊さない）。
fn split_conjuncts(tokens: &[Token], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut depth = 0usize;
    let mut cuts: Vec<usize> = Vec::new();
    let mut unsplittable = false;
    for i in from..to {
        match tokens.get(i) {
            Some(Token::Punct('(')) => depth = depth.saturating_add(1),
            Some(Token::Punct(')')) => depth = depth.saturating_sub(1),
            Some(Token::Keyword(Keyword::And)) if depth == 0 => cuts.push(i),
            Some(Token::Ident(n))
                if depth == 0
                    && (n.eq_ignore_ascii_case("OR") || n.eq_ignore_ascii_case("BETWEEN")) =>
            {
                unsplittable = true;
            }
            _ => {}
        }
    }
    if unsplittable {
        return vec![(from, to)];
    }
    let mut out = Vec::new();
    let mut s = from;
    for c in cuts {
        out.push((s, c));
        s = c.saturating_add(1);
    }
    out.push((s, to));
    out
}

/// LATERAL の本文 `body`（`SELECT ...`。括弧なし）が参照する左側 relation `left` の列を束縛する。
/// `columns` は relation の列集合（実表・利用者の CTE・先行の派生表）を返す。`Ok(None)` は列集合を
/// 返せない relation で、その relation への修飾参照は `0A000`、非修飾名の推定からは外す（fail-closed）。
/// `reject_left_ref` は RIGHT／FULL JOIN の LATERAL（Issue #3323）で、左側 relation を参照した時点
/// （列が存在するかを問わない。PostgreSQL と同じ）で `42P10` を返す。内側の未知列（`42703`）・未知の
/// 修飾子（`42P01`）は本文のテキスト順で先に現れた方が先に返る。
pub(crate) fn bind_lateral_body<F>(
    body: &[Token],
    left: &[TableRef],
    columns: &mut F,
    reject_left_ref: bool,
) -> Result<BoundBody, SqlSurfaceError>
where
    F: FnMut(&TableRef) -> Result<Option<Vec<String>>, SqlSurfaceError>,
{
    let unchanged = || BoundBody {
        tokens: body.to_vec(),
        binding: LateralBinding::default(),
        correlated_body: None,
        inner_columns: None,
    };
    let Some(from_idx) = find_top_from(body) else {
        return Ok(unchanged());
    };
    let probe_end = body.len().min(from_idx.saturating_add(5));
    let Some((inner_ref, consumed)) = body
        .get(from_idx.saturating_add(1)..probe_end)
        .and_then(probe_from_table_ref)
    else {
        return Ok(unchanged());
    };
    let from_item = (
        from_idx.saturating_add(1),
        from_idx.saturating_add(1).saturating_add(consumed),
    );
    let inner_cols = columns(&inner_ref)?;
    let mut lc = LeftCols {
        left,
        columns,
        cache: vec![None; left.len()],
    };

    let mut out: Vec<Token> = body.to_vec();
    let mut reserved: Vec<usize> = Vec::new();
    let mut refs: Vec<LateralOuterRef> = Vec::new();
    // 入れ子として数えるのはサブクエリの括弧（`( SELECT`）だけ。式の括弧は深さに数えない
    // （`WHERE a AND (x = d.k OR ...)` の外側参照は最上位 WHERE の中）。
    let mut parens: Vec<bool> = Vec::new();
    let mut depth = 0usize;
    for (i, tok) in body.iter().enumerate() {
        match tok {
            Token::Punct('(') => {
                let sub = matches!(
                    body.get(i.saturating_add(1)),
                    Some(Token::Keyword(Keyword::Select))
                );
                parens.push(sub);
                if sub {
                    depth = depth.saturating_add(1);
                }
                continue;
            }
            Token::Punct(')') => {
                if parens.pop() == Some(true) {
                    depth = depth.saturating_sub(1);
                }
                continue;
            }
            _ => {}
        }
        if i >= from_item.0 && i < from_item.1 {
            continue;
        }
        match tok {
            Token::QualifiedIdent { qualifier, name } => {
                if inner_ref.matches_qualifier(qualifier) {
                    // RIGHT／FULL では内側の未知列を、後続の左側参照（`42P10`）より先に出た順で `42703` にする。
                    if reject_left_ref
                        && !name.eq_ignore_ascii_case("id")
                        && !is_non_column_position(body, i)
                        && inner_cols
                            .as_deref()
                            .is_some_and(|ic| find_col(ic, name).is_none())
                    {
                        return Err(SqlSurfaceError::undefined_column(name.clone()));
                    }
                    continue;
                }
                let Some(ridx) = left.iter().position(|r| r.matches_qualifier(qualifier)) else {
                    continue;
                };
                if depth > 0 {
                    // 入れ子サブクエリ内の外側参照（`levels_up >= 2`）は対象外（L4）。
                    return Err(outer_position_unsupported());
                }
                if is_non_column_position(body, i) {
                    continue;
                }
                if is_syntax_word(name) || is_literal_word(name) {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified column reference named like a SQL keyword is not supported",
                    ));
                }
                if reject_left_ref {
                    return Err(SqlSurfaceError::LateralReferenceInvalidJoinType);
                }
                let Some(cols) = lc.get(ridx)? else {
                    return Err(unresolvable_left_columns());
                };
                let Some(col) = find_col(cols, name).cloned() else {
                    return Err(SqlSurfaceError::undefined_column(name.clone()));
                };
                if let Some(slot) = out.get_mut(i) {
                    *slot = Token::Ident(lateral_ref_placeholder(ridx, &col));
                }
                reserved.push(i);
                refs.push(LateralOuterRef {
                    rel_index: ridx,
                    column: col,
                });
            }
            Token::Ident(n) if depth == 0 => {
                if is_syntax_word(n) || is_literal_word(n) || is_non_column_position(body, i) {
                    continue;
                }
                let Some(ic) = inner_cols.as_deref() else {
                    continue;
                };
                if find_col(ic, n).is_some() {
                    continue;
                }
                let mut hit: Option<(usize, String)> = None;
                for ridx in 0..left.len() {
                    if let Some(col) = lc.get(ridx)?.and_then(|cols| find_col(cols, n)) {
                        if hit.is_some() {
                            return Err(SqlSurfaceError::AmbiguousColumn { name: n.clone() });
                        }
                        hit = Some((ridx, col.clone()));
                    }
                }
                if let Some((ridx, col)) = hit {
                    if reject_left_ref {
                        return Err(SqlSurfaceError::LateralReferenceInvalidJoinType);
                    }
                    if let Some(slot) = out.get_mut(i) {
                        *slot = Token::Ident(lateral_ref_placeholder(ridx, &col));
                    }
                    reserved.push(i);
                    refs.push(LateralOuterRef {
                        rel_index: ridx,
                        column: col,
                    });
                }
            }
            _ => {}
        }
    }
    if reserved.is_empty() {
        return Ok(BoundBody {
            tokens: out,
            binding: LateralBinding::default(),
            correlated_body: None,
            inner_columns: None,
        });
    }
    refs.sort();
    refs.dedup();

    // L4: 外側参照は最上位 WHERE の中だけ。
    let Some((where_at, where_end)) = find_where(&out) else {
        return Err(outer_position_unsupported());
    };
    if reserved.iter().any(|&i| i <= where_at || i >= where_end) {
        return Err(outer_position_unsupported());
    }

    let conjuncts = split_conjuncts(&out, where_at.saturating_add(1), where_end);
    let is_correlated = |s: usize, e: usize| {
        out.get(s..e)
            .unwrap_or(&[])
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if parse_lateral_ref_placeholder(n).is_some()))
    };
    let mut kept: Vec<(usize, usize)> = Vec::new();
    let mut dropped: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in &conjuncts {
        if is_correlated(s, e) {
            check_dropped_conjunct(&out, s, e, &inner_ref, inner_cols.as_deref())?;
            dropped.push((s, e));
        } else {
            kept.push((s, e));
        }
    }
    let mut tokens: Vec<Token> = out.get(..where_at).unwrap_or(&[]).to_vec();
    for (n, &(s, e)) in kept.iter().enumerate() {
        tokens.push(if n == 0 {
            Token::Keyword(Keyword::Where)
        } else {
            Token::Keyword(Keyword::And)
        });
        tokens.extend_from_slice(out.get(s..e).unwrap_or(&[]));
    }
    tokens.extend_from_slice(out.get(where_end..).unwrap_or(&[]));
    // 取り除いた連言を `AND` で連結した本文（元の投影・FROM を保つ。別名の正規化は呼び出し側）。
    // 投影は `*` に置き換える（取り除いた連言の解析にだけ使うため、元の投影の形に依存させない）。
    let mut correlated_body: Vec<Token> = vec![Token::Keyword(Keyword::Select), Token::Punct('*')];
    correlated_body.extend_from_slice(out.get(from_idx..where_at).unwrap_or(&[]));
    for (n, &(s, e)) in dropped.iter().enumerate() {
        correlated_body.push(if n == 0 {
            Token::Keyword(Keyword::Where)
        } else {
            Token::Keyword(Keyword::And)
        });
        correlated_body.extend_from_slice(out.get(s..e).unwrap_or(&[]));
    }
    Ok(BoundBody {
        tokens,
        binding: LateralBinding { outer_refs: refs },
        correlated_body: Some(correlated_body),
        inner_columns: inner_cols,
    })
}

/// 相関 LATERAL の実行形（Issue #3322。設計記録 L2）。内側を「外側参照を含まない基底」と
/// 「外側タプルごとに適用する後処理」の 2 つの本文へ分ける。
pub(crate) struct ExecutionBodies {
    /// `SELECT <参照される内側の列> FROM <内側> [WHERE <外側参照を含まない連言>]`。tail は持たない。
    pub(crate) base: Vec<Token>,
    /// 元の投影・`ORDER BY`・`LIMIT`・`OFFSET` を基底の名前の relation へ適用する本文。
    pub(crate) post: Vec<Token>,
}

fn star_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "SELECT * in a LATERAL subquery referencing outer columns is not supported yet",
    )
}

/// 相関 LATERAL の本文（`body`。内側の別名除去済みで、外側参照を含む連言は取り除かれている）を
/// 基底本文と後処理本文へ分ける。`base_name` は基底の合成 CTE 名、`inner_cols` は内側 relation の
/// 列名（正規の綴り。疑似列 `id` を含みうる）、`correlated_body` は取り除いた連言の本文
/// （[`BoundBody::correlated_body`] を正規化したもの）。基底の投影は、本文のどこかで参照される内側の
/// 列だけにする（参照されない VECTOR 列などを実体化しない）。走査は反復処理・添字は `get()`。
pub(crate) fn split_for_execution(
    body: &[Token],
    base_name: &str,
    inner_cols: &[String],
    correlated_body: &[Token],
) -> Result<ExecutionBodies, SqlSurfaceError> {
    let Some(from_idx) = find_top_from(body) else {
        return Err(star_unsupported());
    };
    let projection = body.get(1..from_idx).unwrap_or(&[]);
    if projection
        .iter()
        .any(|t| matches!(t, Token::Punct('*') | Token::QualifiedStar { .. }))
    {
        return Err(star_unsupported());
    }
    let probe_end = body.len().min(from_idx.saturating_add(5));
    let Some((_inner_ref, consumed)) = body
        .get(from_idx.saturating_add(1)..probe_end)
        .and_then(probe_from_table_ref)
    else {
        return Err(unresolvable_left_columns());
    };
    let item_end = from_idx.saturating_add(1).saturating_add(consumed);
    let from_item = body
        .get(from_idx.saturating_add(1)..item_end)
        .unwrap_or(&[]);
    let (base_where, tail): (&[Token], &[Token]) = match find_where(body) {
        Some((ws, we)) => (
            body.get(ws..we).unwrap_or(&[]),
            body.get(we..).unwrap_or(&[]),
        ),
        None => (&[], body.get(item_end..).unwrap_or(&[])),
    };
    // 参照される内側の列: 投影・tail・取り除いた連言（WHERE より後）の識別子。
    let corr_conj: &[Token] = match find_where(correlated_body) {
        Some((ws, we)) => correlated_body.get(ws..we).unwrap_or(&[]),
        None => &[],
    };
    let referenced = |col: &String| {
        projection
            .iter()
            .chain(tail.iter())
            .chain(corr_conj.iter())
            .any(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case(col)))
    };
    let mut cols: Vec<&String> = inner_cols.iter().filter(|c| referenced(c)).collect();
    if cols.is_empty() {
        if let Some(first) = inner_cols.first() {
            cols.push(first);
        } else {
            return Err(unresolvable_left_columns());
        }
    }
    let mut base: Vec<Token> = vec![Token::Keyword(Keyword::Select)];
    for (n, c) in cols.iter().enumerate() {
        if n > 0 {
            base.push(Token::Punct(','));
        }
        base.push(Token::Ident((*c).clone()));
    }
    base.push(Token::Keyword(Keyword::From));
    base.extend_from_slice(from_item);
    base.extend_from_slice(base_where);
    let mut post: Vec<Token> = body.get(..from_idx).unwrap_or(&[]).to_vec();
    post.push(Token::Keyword(Keyword::From));
    post.push(Token::Ident(base_name.to_string()));
    post.extend_from_slice(tail);
    Ok(ExecutionBodies { base, post })
}

/// LATERAL の本文の `FROM` の別名（`FROM docs x`・`FROM docs AS x`）を落とす。内側の別名による修飾は
/// 束縛・正規化（[`bind_lateral_body`]・`qualified_column`）で解決済みで、CTE 本文のパーサーは本文の
/// `FROM` に別名を受け付けないため、解決のあとで取り除く。別名が無ければ入力をそのまま返す。
pub(crate) fn strip_inner_alias(body: Vec<Token>) -> Vec<Token> {
    let Some(from_idx) = find_top_from(&body) else {
        return body;
    };
    let probe_end = body.len().min(from_idx.saturating_add(5));
    let Some((inner_ref, consumed)) = body
        .get(from_idx.saturating_add(1)..probe_end)
        .and_then(probe_from_table_ref)
    else {
        return body;
    };
    if inner_ref.alias().is_none() {
        return body;
    }
    let item_end = from_idx.saturating_add(1).saturating_add(consumed);
    let alias_at = item_end.saturating_sub(1);
    let has_as = alias_at
        .checked_sub(1)
        .and_then(|p| body.get(p))
        .is_some_and(|t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("AS")));
    let cut_from = if has_as {
        alias_at.saturating_sub(1)
    } else {
        alias_at
    };
    let mut out: Vec<Token> = body.get(..cut_from).unwrap_or(&[]).to_vec();
    out.extend_from_slice(body.get(item_end..).unwrap_or(&[]));
    out
}

/// 取り除く連言の内側の識別子を、内側の列集合に対して検査する（`42703`／`42P01`）。連言に
/// サブクエリがあれば `0A000`（内側の識別子の所属を静的に決められないため）。
fn check_dropped_conjunct(
    toks: &[Token],
    s: usize,
    e: usize,
    inner_ref: &TableRef,
    inner_cols: Option<&[String]>,
) -> Result<(), SqlSurfaceError> {
    let seg = toks.get(s..e).unwrap_or(&[]);
    if seg
        .iter()
        .any(|t| matches!(t, Token::Keyword(Keyword::Select)))
    {
        return Err(SqlSurfaceError::feature_not_supported(
            "a subquery in a LATERAL correlated predicate is not supported",
        ));
    }
    for (off, t) in seg.iter().enumerate() {
        let i = s.saturating_add(off);
        match t {
            Token::QualifiedIdent { qualifier, name } => {
                if is_non_column_position(toks, i) {
                    continue;
                }
                if !inner_ref.matches_qualifier(qualifier) {
                    return Err(SqlSurfaceError::undefined_table(qualifier.clone()));
                }
                if inner_cols.is_some_and(|ic| find_col(ic, name).is_none()) {
                    return Err(SqlSurfaceError::undefined_column(name.clone()));
                }
            }
            Token::Ident(n) => {
                if parse_lateral_ref_placeholder(n).is_some()
                    || is_syntax_word(n)
                    || is_literal_word(n)
                    || is_non_column_position(toks, i)
                {
                    continue;
                }
                if inner_cols.is_some_and(|ic| find_col(ic, n).is_none()) {
                    return Err(SqlSurfaceError::undefined_column(n.clone()));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn cols_of(t: &TableRef) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        Ok(match t.table() {
            "docs" => Some(vec!["id".into(), "lang".into(), "title".into()]),
            "other" => Some(vec!["id".into(), "k".into(), "lang".into()]),
            "opaque" => None,
            _ => None,
        })
    }

    fn bind(sql: &str, left: &[TableRef]) -> Result<BoundBody, SqlSurfaceError> {
        let toks = tokenize(sql).expect("tokenize");
        bind_lateral_body(&toks, left, &mut cols_of, false)
    }

    fn d() -> TableRef {
        TableRef::with_alias("docs", "d")
    }

    #[test]
    fn no_outer_reference_leaves_body_unchanged() {
        let b = bind("SELECT lang FROM docs WHERE title = 'x'", &[d()]).expect("bind");
        assert!(!b.binding.is_correlated());
        assert_eq!(
            b.tokens,
            tokenize("SELECT lang FROM docs WHERE title = 'x'").expect("t")
        );
    }

    #[test]
    fn qualified_outer_reference_is_bound_and_conjunct_dropped() {
        let b = bind(
            "SELECT lang FROM other o WHERE o.k = d.lang AND o.k = 'x'",
            &[d()],
        )
        .expect("bind");
        assert_eq!(
            b.binding.outer_refs,
            vec![LateralOuterRef {
                rel_index: 0,
                column: "lang".into()
            }]
        );
        assert_eq!(
            b.tokens,
            tokenize("SELECT lang FROM other o WHERE o.k = 'x'").expect("t")
        );
    }

    #[test]
    fn unqualified_name_only_in_left_is_outer_and_inner_wins_on_tie() {
        let b = bind("SELECT k FROM other WHERE k = title", &[d()]).expect("bind");
        assert!(b.binding.is_correlated());
        let b = bind("SELECT k FROM other WHERE k = lang", &[d()]).expect("bind");
        assert!(!b.binding.is_correlated(), "inner column shadows outer");
    }

    #[test]
    fn unqualified_name_in_two_left_relations_is_42702() {
        let left = [d(), TableRef::with_alias("other", "e")];
        // `title` は docs だけ、`k` は other だけ。`id` は両方にあり、内側の `third` には無い。
        let toks = tokenize("SELECT a FROM third t WHERE a = id").expect("t");
        let mut cols = |t: &TableRef| -> Result<Option<Vec<String>>, SqlSurfaceError> {
            Ok(match t.table() {
                "third" => Some(vec!["a".into()]),
                _ => cols_of(t)?,
            })
        };
        let err = bind_lateral_body(&toks, &left, &mut cols, false).expect_err("ambiguous");
        assert_eq!(err.wire_code(), "42702");
    }

    #[test]
    fn unknown_inner_columns_skip_unqualified_inference() {
        // 内側の列集合が不明なら非修飾名を外側参照と推定しない（後段の検証に任せる）。
        let b = bind("SELECT x FROM opaque WHERE x = title", &[d()]).expect("bind");
        assert!(!b.binding.is_correlated());
    }

    #[test]
    fn missing_outer_column_is_42703() {
        let err = bind("SELECT lang FROM other o WHERE o.k = d.nope", &[d()])
            .expect_err("missing column");
        assert_eq!(err.wire_code(), "42703");
    }

    #[test]
    fn unknown_left_relation_columns_fail_closed() {
        let err = bind(
            "SELECT lang FROM other o WHERE o.k = z.x",
            &[TableRef::with_alias("opaque", "z")],
        )
        .expect_err("opaque");
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn outer_reference_outside_where_is_0a000() {
        for sql in [
            "SELECT d.lang FROM other o",
            "SELECT k FROM other o ORDER BY d.lang",
            "SELECT k FROM other o WHERE o.k = 'x' ORDER BY title",
            "SELECT k FROM other o WHERE o.k IN (SELECT 1 FROM other p WHERE p.k = d.lang)",
        ] {
            let err = bind(sql, &[d()]).expect_err(sql);
            assert_eq!(err.wire_code(), "0A000", "{sql}");
        }
    }

    #[test]
    fn inner_identifier_in_dropped_conjunct_is_checked() {
        let err = bind("SELECT k FROM other o WHERE nope = d.lang", &[d()]).expect_err("42703");
        assert_eq!(err.wire_code(), "42703");
        let err = bind("SELECT k FROM other o WHERE zz.k = d.lang", &[d()]).expect_err("42P01");
        assert_eq!(err.wire_code(), "42P01");
    }

    #[test]
    fn or_keeps_whole_where_as_one_conjunct() {
        let b = bind(
            "SELECT k FROM other o WHERE o.k = 'a' OR o.k = d.lang",
            &[d()],
        )
        .expect("bind");
        assert!(b.binding.is_correlated());
        assert_eq!(b.tokens, tokenize("SELECT k FROM other o").expect("t"));
    }

    #[test]
    fn inner_alias_is_stripped_after_resolution() {
        for (sql, want) in [
            (
                "SELECT k FROM other o WHERE k = 1",
                "SELECT k FROM other WHERE k = 1",
            ),
            ("SELECT k FROM other AS o", "SELECT k FROM other"),
            ("SELECT k FROM other", "SELECT k FROM other"),
        ] {
            let got = strip_inner_alias(tokenize(sql).expect("t"));
            assert_eq!(got, tokenize(want).expect("t"), "{sql}");
        }
    }

    #[test]
    fn reserved_name_never_appears_in_remaining_body() {
        let b = bind("SELECT k FROM other o WHERE d.title = o.k", &[d()]).expect("bind");
        assert!(!b
            .tokens
            .iter()
            .any(|t| matches!(t, Token::Ident(n) if parse_lateral_ref_placeholder(n).is_some())));
    }
    fn split(sql: &str) -> (Vec<Token>, Vec<Token>) {
        let b = bind(sql, &[d()]).expect("bind");
        let inner_cols: Vec<String> = vec!["id".into(), "k".into(), "lang".into()];
        let norm = |t: Vec<Token>| {
            strip_inner_alias(
                crate::sql::qualified_column::normalize_single_table_qualifiers(&t)
                    .expect("normalize")
                    .into_owned(),
            )
        };
        let tokens = norm(b.tokens);
        let corr = norm(b.correlated_body.expect("correlated"));
        let bodies =
            split_for_execution(&tokens, "?derived0_base", &inner_cols, &corr).expect("split");
        (bodies.base, bodies.post)
    }

    #[test]
    fn correlated_conjuncts_are_kept_with_reserved_names_for_execution() {
        let b = bind(
            "SELECT lang FROM other o WHERE o.k = d.lang AND o.k = 'x' AND o.lang = d.title",
            &[d()],
        )
        .expect("bind");
        let corr = b.correlated_body.expect("correlated body");
        let idents: Vec<&str> = corr
            .iter()
            .filter_map(|t| match t {
                Token::Ident(n) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        assert!(idents.contains(&lateral_ref_placeholder(0, "lang").as_str()));
        assert!(idents.contains(&lateral_ref_placeholder(0, "title").as_str()));
        // 取り除かれた連言だけが残り、非相関の `o.k = 'x'` は含まれない。
        assert!(!corr
            .iter()
            .any(|t| matches!(t, Token::StringLiteral(s) if s == "x")));
        // 外側参照が無い本文には相関本文が無い。
        assert!(bind("SELECT lang FROM other WHERE k = 'x'", &[d()])
            .expect("bind")
            .correlated_body
            .is_none());
    }

    #[test]
    fn or_and_between_stay_one_correlated_conjunct() {
        let b = bind(
            "SELECT lang FROM other o WHERE o.k = 'x' OR o.lang = d.lang",
            &[d()],
        )
        .expect("bind");
        // 深さ 0 の OR があると全体が 1 連言で、非相関の部分も本文から取り除かれる。
        assert_eq!(b.tokens, tokenize("SELECT lang FROM other o").expect("t"));
        let corr = b.correlated_body.expect("correlated");
        assert!(corr
            .iter()
            .any(|t| matches!(t, Token::StringLiteral(s) if s == "x")));
    }

    #[test]
    fn split_for_execution_separates_base_and_post() {
        let (base, post) = split(
            "SELECT lang FROM other o WHERE o.k = d.lang AND o.k = 'x' ORDER BY o.lang LIMIT 1",
        );
        // 基底は参照される列（`k`・`lang`）だけを投影し、非相関の WHERE を残して tail を持たない。
        assert_eq!(
            base,
            tokenize("SELECT k, lang FROM other WHERE k = 'x'").expect("t")
        );
        // 後処理は元の投影・tail を基底の名前の relation へ適用する。
        let mut expected = tokenize("SELECT lang FROM").expect("t");
        expected.push(Token::Ident("?derived0_base".into()));
        expected.extend(tokenize("ORDER BY lang LIMIT 1").expect("t"));
        assert_eq!(post, expected);
    }

    #[test]
    fn split_without_remaining_where_has_no_base_where() {
        let (base, _post) = split("SELECT lang FROM other o WHERE o.k = d.lang");
        assert_eq!(base, tokenize("SELECT k, lang FROM other").expect("t"));
    }

    #[test]
    fn select_star_is_rejected_for_execution() {
        let b = bind("SELECT * FROM other o WHERE o.k = d.lang", &[d()]).expect("bind");
        let inner_cols: Vec<String> = vec!["id".into(), "k".into(), "lang".into()];
        let tokens = strip_inner_alias(b.tokens);
        let corr = strip_inner_alias(b.correlated_body.expect("correlated"));
        let err = split_for_execution(&tokens, "?d_base", &inner_cols, &corr)
            .err()
            .expect("star");
        assert_eq!(err.wire_code(), "0A000");
    }
}
