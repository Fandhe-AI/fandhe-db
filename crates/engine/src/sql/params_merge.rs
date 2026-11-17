//! `MERGE` の `$n`（プレースホルダ）位置規則・型推論の入力・Describe 用ダミーフラグ
//! （SQL-79・Issue #3352。親 #3349、設計記録は `docs/design/sql-merge.md`）。
//!
//! 責務境界: 汎用の位置規則（`sql::params` の `where_region`／`set_region`／`values_region_bounds`）は
//! `UPDATE`・`INSERT` が 1 文に SET／VALUES を 1 つしか持たない前提で作られている。`MERGE` は `WHEN` 句ごとに
//! SET／VALUES を複数持つため、広げると他の位置まで受理してしまう。そこで `looks_like_merge` で分岐する
//! 本モジュールの走査に閉じ、汎用の走査は変えない。`sql::params` の入口（`validate_param_positions`・
//! `infer_param_positions`・`expr_param_segments`）が先頭語 `MERGE` を見て本モジュールへ委ねる。
//!
//! 受理する位置（これ以外の `$n` は `42601`。fail-closed）:
//! - `WHEN ... THEN UPDATE SET <列> = $n` の右辺（括弧で包んだ `($n)` を含む）
//! - `WHEN ... THEN INSERT [(<列>, ...)] VALUES (...)` の要素（単独の `$n`）
//! - 文末の `USING OPERATION_ID $n`
//!
//! 受理しないもの（#3472 の結合 DML と同じ段階的な制限。受け皿は設計記録）: `ON`・`WHEN ... AND` の
//! 条件内の `$n`、式の内側の `$n`（`SET x = $1 + 1`）、元が部分クエリの形の `$n`、`RETURNING` 内の `$n`、
//! `$n::型` のキャスト。トークン列だけを走査し、添字アクセスは `get()` で行う（入力は untrusted）。
//!
//! 型推論: SET の位置は対象の列の型、INSERT の位置は `WHEN` 句ごとの列リスト（省略時は対象表の `*` 展開）の
//! 添字の列の型で引く（`sql::params::ParamPosition::MergeSetColumn`・`MergeInsertValue`）。
//!
//! Describe 用ダミーフラグ: 並びは「`WHEN` 句ごとに、SET の代入ごと（宣言順）または INSERT の値ごと」。
//! 束縛器（`sql::merge_bind`）は構文木から求めた総数と照合し、一致しなければ全て検証側へ倒す。

use std::collections::HashMap;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;
use crate::sql::params::{ParamLiteralKind, ParamPosition, MAX_PARAMS};

fn ident_is(t: Option<&Token>, word: &str) -> bool {
    matches!(t, Some(Token::Ident(w)) if w.eq_ignore_ascii_case(word))
}

/// 括弧の深さを追いながら、深さ 0 の位置の添字を順に返す走査の補助（`(`／`)`／`[`／`]` を数える）。
fn depth_zero_positions(tokens: &[Token], from: usize, to: usize) -> Vec<usize> {
    let mut depth: usize = 0;
    let mut out = Vec::new();
    for i in from..to.min(tokens.len()) {
        match tokens.get(i) {
            Some(Token::Punct('(' | '[')) => depth = depth.saturating_add(1),
            Some(Token::Punct(')' | ']')) => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 {
                    out.push(i);
                }
            }
        }
    }
    out
}

/// `WHEN` 句の開始位置（深さ 0 の `WHEN` の直後が `MATCHED` か `NOT`。`CASE WHEN` と区別する）。
fn clause_starts(tokens: &[Token]) -> Vec<usize> {
    depth_zero_positions(tokens, 0, tokens.len())
        .into_iter()
        .filter(|&i| {
            ident_is(tokens.get(i), "WHEN")
                && (ident_is(tokens.get(i.saturating_add(1)), "MATCHED")
                    || ident_is(tokens.get(i.saturating_add(1)), "NOT"))
        })
        .collect()
}

/// 最後の `WHEN` 句の終端（深さ 0 の `RETURNING`、または `USING OPERATION_ID` の `USING`。無ければ末尾）。
fn tail_start(tokens: &[Token], after: usize) -> usize {
    depth_zero_positions(tokens, after, tokens.len())
        .into_iter()
        .find(|&i| {
            ident_is(tokens.get(i), "RETURNING")
                || (ident_is(tokens.get(i), "USING")
                    && ident_is(tokens.get(i.saturating_add(1)), "OPERATION_ID"))
        })
        .unwrap_or(tokens.len())
}

/// `[from, to)` を深さ 0 のカンマで分けた区間列。
fn split_top_level(tokens: &[Token], from: usize, to: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = from;
    for i in depth_zero_positions(tokens, from, to) {
        if matches!(tokens.get(i), Some(Token::Punct(','))) {
            out.push((start, i));
            start = i.saturating_add(1);
        }
    }
    out.push((start, to));
    out
}

/// 区間の外側の括弧を剥がした区間（`($n)` → `$n`。何重でも）。
fn strip_parens(tokens: &[Token], mut from: usize, mut to: usize) -> (usize, usize) {
    while to > from.saturating_add(1)
        && matches!(tokens.get(from), Some(Token::Punct('(')))
        && matches!(tokens.get(to.saturating_sub(1)), Some(Token::Punct(')')))
    {
        // 先頭の `(` の対応が末尾の `)` であること（`(a) + (b)` を剥がさない）。
        let mut depth: usize = 0;
        let mut closes_at_end = false;
        for i in from..to {
            match tokens.get(i) {
                Some(Token::Punct('(')) => depth = depth.saturating_add(1),
                Some(Token::Punct(')')) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        closes_at_end = i == to.saturating_sub(1);
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            break;
        }
        from = from.saturating_add(1);
        to = to.saturating_sub(1);
    }
    (from, to)
}

/// 1 つの `WHEN` 句の `THEN` 以降の構造（`$n` の位置と代入／値の並び）。
enum Action {
    /// `UPDATE SET`: 代入ごとの (左辺の列名, 右辺の区間)。行値代入は列名なし。
    Update(Vec<(Option<String>, (usize, usize), usize)>),
    /// `INSERT`: (列リスト, 値の区間)。
    Insert(Option<Vec<String>>, Vec<(usize, usize)>),
    /// 値を持たない操作（`DELETE`・`DO NOTHING`・`INSERT DEFAULT VALUES`）。
    Other,
}

/// 句の操作（`THEN` の後ろ）を読む。崩れた形は `Other`（`$n` が含まれれば呼び出し側が拒否する）。
fn parse_action(tokens: &[Token], then_idx: usize, end: usize) -> Action {
    let first = then_idx.saturating_add(1);
    if ident_is(tokens.get(first), "UPDATE") && ident_is(tokens.get(first.saturating_add(1)), "SET")
    {
        let region_start = first.saturating_add(2);
        let mut items = Vec::new();
        for (s, e) in split_top_level(tokens, region_start, end) {
            // 行値代入 `(a, b) = (...)` は代入数を左辺の列数として数える（`$n` は拒否する）。
            if matches!(tokens.get(s), Some(Token::Punct('('))) {
                let cols = split_top_level(tokens, s.saturating_add(1), e).len().max(1);
                items.push((None, (s, e), cols));
                continue;
            }
            let col = match tokens.get(s) {
                Some(Token::Ident(c) | Token::QuotedNull(c))
                    if matches!(tokens.get(s.saturating_add(1)), Some(Token::Punct('='))) =>
                {
                    Some(c.clone())
                }
                _ => None,
            };
            items.push((col, (s.saturating_add(2).min(e), e), 1));
        }
        return Action::Update(items);
    }
    if ident_is(tokens.get(first), "INSERT") {
        let mut p = first.saturating_add(1);
        let mut columns: Option<Vec<String>> = None;
        if matches!(tokens.get(p), Some(Token::Punct('('))) {
            let mut cols = Vec::new();
            p = p.saturating_add(1);
            while let Some(t) = tokens.get(p) {
                match t {
                    Token::Ident(c) | Token::QuotedNull(c) => cols.push(c.clone()),
                    Token::Punct(',') => {}
                    _ => break,
                }
                p = p.saturating_add(1);
            }
            if matches!(tokens.get(p), Some(Token::Punct(')'))) {
                p = p.saturating_add(1);
            }
            columns = Some(cols);
        }
        if ident_is(tokens.get(p), "VALUES")
            && matches!(tokens.get(p.saturating_add(1)), Some(Token::Punct('(')))
        {
            let inner_start = p.saturating_add(2);
            // 値リストの閉じ括弧（対応する `)`）を探す。
            let mut depth: usize = 1;
            let mut close = None;
            for i in inner_start..end.min(tokens.len()) {
                match tokens.get(i) {
                    Some(Token::Punct('(')) => depth = depth.saturating_add(1),
                    Some(Token::Punct(')')) => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = close {
                return Action::Insert(columns, split_top_level(tokens, inner_start, close));
            }
        }
    }
    Action::Other
}

/// 解析した 1 句。
struct Clause {
    action: Action,
}

/// 句の並びと文末の位置を求める。
fn parse_clauses(tokens: &[Token]) -> (Vec<Clause>, usize, usize) {
    let starts = clause_starts(tokens);
    let first_start = starts.first().copied().unwrap_or(tokens.len());
    let tail = tail_start(tokens, first_start);
    let mut clauses = Vec::with_capacity(starts.len().min(64));
    for (k, &s) in starts.iter().enumerate() {
        let end = starts
            .get(k.saturating_add(1))
            .copied()
            .unwrap_or(tail)
            .min(tail);
        let then_idx = depth_zero_positions(tokens, s, end)
            .into_iter()
            .find(|&i| ident_is(tokens.get(i), "THEN"))
            .unwrap_or(end);
        clauses.push(Clause {
            action: parse_action(tokens, then_idx, end),
        });
    }
    (clauses, first_start, tail)
}

/// 区間が単独の `$n`（括弧で包んでもよい）なら、その番号と位置。
fn lone_param(tokens: &[Token], from: usize, to: usize) -> Option<(usize, u16)> {
    let (s, e) = strip_parens(tokens, from, to);
    match (e.checked_sub(s), tokens.get(s)) {
        (Some(1), Some(Token::Param(n))) => Some((s, *n)),
        _ => None,
    }
}

/// 許可位置にある `$n` の分類（トークン添字 → 位置）。
pub(crate) fn param_positions(tokens: &[Token]) -> HashMap<usize, ParamPosition> {
    let mut out = HashMap::new();
    let (clauses, _, _) = parse_clauses(tokens);
    for c in &clauses {
        match &c.action {
            Action::Update(items) => {
                for (col, (s, e), _) in items {
                    if let (Some(col), Some((idx, _))) = (col, lone_param(tokens, *s, *e)) {
                        out.insert(idx, ParamPosition::MergeSetColumn(col.clone()));
                    }
                }
            }
            Action::Insert(cols, values) => {
                for (k, (s, e)) in values.iter().enumerate() {
                    if let Some((idx, _)) = lone_param(tokens, *s, *e) {
                        out.insert(
                            idx,
                            ParamPosition::MergeInsertValue {
                                columns: cols.clone(),
                                index: k,
                            },
                        );
                    }
                }
            }
            Action::Other => {}
        }
    }
    out
}

/// `USING OPERATION_ID $n` の `$n` の位置。
fn operation_id_param(tokens: &[Token], i: usize) -> bool {
    i >= 2
        && ident_is(i.checked_sub(1).and_then(|j| tokens.get(j)), "OPERATION_ID")
        && ident_is(i.checked_sub(2).and_then(|j| tokens.get(j)), "USING")
}

/// `MERGE` の `$n` が全て許可位置にあることを検証し、最大の番号を返す（`$n` が無ければ 0）。
/// 許可位置以外は `42601`、番号が [`MAX_PARAMS`] を超えれば `54000`。
pub(crate) fn validate(tokens: &[Token]) -> Result<u16, SqlSurfaceError> {
    let allowed = param_positions(tokens);
    let mut max_index: u16 = 0;
    for (i, t) in tokens.iter().enumerate() {
        let Token::Param(n) = t else { continue };
        max_index = max_index.max(*n);
        if allowed.contains_key(&i) || operation_id_param(tokens, i) {
            // `$n::型` のキャストは受理しない。
            if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct(':'))) {
                return Err(SqlSurfaceError::unsupported(format!(
                    "parameter placeholder ${n} followed by '::' is not supported at this position"
                )));
            }
            continue;
        }
        return Err(SqlSurfaceError::unsupported(format!(
            "parameter placeholder ${n} is not supported at this position"
        )));
    }
    if max_index > MAX_PARAMS {
        return Err(SqlSurfaceError::payload_too_large(format!(
            "parameter number ${max_index} exceeds the allowed maximum (${MAX_PARAMS})"
        )));
    }
    Ok(max_index)
}

/// 対象表名（`MERGE INTO <表>`）。型推論が対象の列の型を引く先。
pub(crate) fn merge_target_table(tokens: &[Token]) -> Option<String> {
    if !ident_is(tokens.get(1), "INTO") {
        return None;
    }
    match tokens.get(2) {
        Some(Token::Ident(name) | Token::QuotedNull(name)) => Some(name.clone()),
        _ => None,
    }
}

/// Describe 用ダミーフラグ（並びは文書冒頭のとおり）。`$n` 由来の文字列ダミーの単独値だけが真。
pub(crate) fn dummy_flags(tokens: &[Token], kinds: &[ParamLiteralKind]) -> Vec<bool> {
    let is_string_param = |s: usize, e: usize| -> bool {
        matches!(
            lone_param(tokens, s, e),
            Some((_, n)) if usize::from(n)
                .checked_sub(1)
                .and_then(|k| kinds.get(k))
                .copied()
                .unwrap_or(ParamLiteralKind::String)
                == ParamLiteralKind::String
        )
    };
    let (clauses, _, _) = parse_clauses(tokens);
    let mut flags = Vec::new();
    for c in &clauses {
        match &c.action {
            Action::Update(items) => {
                for (col, (s, e), width) in items {
                    if *width > 1 || col.is_none() {
                        flags.extend(std::iter::repeat_n(false, (*width).max(1)));
                    } else {
                        flags.push(is_string_param(*s, *e));
                    }
                }
            }
            Action::Insert(_, values) => {
                for (s, e) in values {
                    flags.push(is_string_param(*s, *e));
                }
            }
            Action::Other => {}
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params as tokenize;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql).expect("lex")
    }

    fn check(sql: &str) -> Result<u16, &'static str> {
        validate(&toks(sql)).map_err(|e| e.wire_code())
    }

    const HEAD: &str = "MERGE INTO t USING s ON t.k = s.k";

    #[test]
    fn accepts_value_positions_only() {
        for (sql, max) in [
            (format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = $1"), 1),
            (
                format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = ($2), k = $1"),
                2,
            ),
            (
                format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($1, $2)"),
                2,
            ),
            (
                format!("{HEAD} WHEN NOT MATCHED THEN INSERT VALUES ($1, s.k, ($2))"),
                2,
            ),
            (
                format!("{HEAD} WHEN MATCHED THEN DELETE USING OPERATION_ID $1"),
                1,
            ),
            (
                format!(
                    "{HEAD} WHEN MATCHED THEN UPDATE SET x = $1 \
                     WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($2, $3) USING OPERATION_ID $4"
                ),
                4,
            ),
            (format!("{HEAD} WHEN MATCHED THEN DELETE"), 0),
        ] {
            assert_eq!(check(&sql), Ok(max), "{sql}");
        }
    }

    #[test]
    fn rejects_every_other_position() {
        for sql in [
            // ON・WHEN の AND の条件。
            "MERGE INTO t USING s ON t.k = $1 WHEN MATCHED THEN DELETE".to_string(),
            format!("{HEAD} WHEN MATCHED AND s.v = $1 THEN DELETE"),
            // 式の内側・キャスト・比較。
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = $1 + 1"),
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = $1::int"),
            format!("{HEAD} WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($1 || 'a', 1)"),
            // 行値代入。
            format!("{HEAD} WHEN MATCHED THEN UPDATE SET (x, k) = ($1, $2)"),
            // RETURNING。
            format!("{HEAD} WHEN MATCHED THEN DELETE RETURNING $1"),
            // 部分クエリの元とその内側。
            "MERGE INTO t USING (SELECT k FROM s WHERE k = $1) AS q ON t.k = q.k WHEN MATCHED THEN DELETE"
                .to_string(),
            "MERGE INTO t USING (SELECT k FROM s) AS q ON t.k = q.k WHEN MATCHED THEN UPDATE SET x = $1 + 1"
                .to_string(),
        ] {
            assert_eq!(check(&sql), Err("42601"), "{sql}");
        }
    }

    #[test]
    fn param_number_over_the_limit_is_54000() {
        let sql = format!("{HEAD} WHEN MATCHED THEN UPDATE SET x = $65");
        assert_eq!(check(&sql), Err("54000"));
    }

    #[test]
    fn case_when_is_not_a_clause_boundary() {
        let sql = format!(
            "{HEAD} WHEN MATCHED THEN UPDATE SET x = CASE WHEN s.v > 1 THEN 1 ELSE 2 END, k = $1"
        );
        assert_eq!(check(&sql), Ok(1));
    }

    #[test]
    fn positions_name_the_target_column_and_the_insert_ordinal() {
        let sql = format!(
            "{HEAD} WHEN MATCHED THEN UPDATE SET x = $1 \
             WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($2, $3)"
        );
        let t = toks(&sql);
        let pos = param_positions(&t);
        let by_n = |n: u16| {
            let i = t
                .iter()
                .position(|x| matches!(x, Token::Param(m) if *m == n))
                .expect("param");
            pos.get(&i).cloned()
        };
        assert_eq!(
            by_n(1),
            Some(ParamPosition::MergeSetColumn("x".to_string()))
        );
        assert_eq!(
            by_n(3),
            Some(ParamPosition::MergeInsertValue {
                columns: Some(vec!["k".to_string(), "x".to_string()]),
                index: 1
            })
        );
        assert_eq!(merge_target_table(&t).as_deref(), Some("t"));
    }

    #[test]
    fn dummy_flags_follow_clause_then_item_order() {
        let sql = format!(
            "{HEAD} WHEN MATCHED THEN UPDATE SET x = $1, k = 'a' \
             WHEN NOT MATCHED THEN INSERT (k, x) VALUES ($2, 5)"
        );
        let t = toks(&sql);
        let kinds = [ParamLiteralKind::String, ParamLiteralKind::String];
        assert_eq!(dummy_flags(&t, &kinds), vec![true, false, true, false]);
        let kinds = [ParamLiteralKind::Integer, ParamLiteralKind::String];
        assert_eq!(dummy_flags(&t, &kinds), vec![false, false, true, false]);
    }
}
