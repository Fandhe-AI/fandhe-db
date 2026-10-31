//! `LANGUAGE sql` 関数本体（SQL 文の並び）の構文解析（Issue #2218・SQL-67 (5)。設計は
//! `docs/design/plpgsql-subset.md` の「Issue #2218 追記」）。
//!
//! 本体を括弧の深さ 0 の `;` で文に分け、各文の先頭語が DML・`SELECT` 系であることを確かめてから、
//! PL/pgSQL の構文解析器（`parser::parse`）が受理する単純ブロック `BEGIN <文>; ... END` へ組み替えて
//! 解析する（第 2 の埋め込み SQL 解析器・変数スコープ解決を作らない）。結果の [`Body`] は
//! `Stmt::Sql` だけが並び、引数は `VarKind::Param` として `$n`・引数名の両方で参照できる。
//! 先頭語の検査は組み替えの前に済ませるため、本体の文字列が `END` などでブロックを閉じて別の構造に
//! 化けることはない。
//!
//! 呼び出し元は `core::function_define`（定義時検証）と `core::routine_call`（呼び出し時の再解析）。
//! 実行は `interp::run_sql_function`。ストレージ・テナントには触れない純粋な処理で、受信データの
//! 経路なので `unwrap`／`expect`／添字アクセスを使わない。エラー文言は固定の英語で本体の断片を含めない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;

use super::ast::{Body, EmbeddedKind, ParseOptions, ReturnShape, Stmt};
use super::lexer::{scan, TokKind};
use super::parser::{self, OUT_OF_SCOPE_STARTS};
use super::{oos_err, syn_err, too_big_err};

/// 先頭に置けない（`0A000` で拒否する）PL/pgSQL 側の語。`OUT_OF_SCOPE_STARTS` に無い制御語を足す。
const REJECTED_STARTS: &[&str] = &[
    "DO", "BEGIN", "END", "DECLARE", "RETURN", "IF", "LOOP", "WHILE", "FOR", "EXIT", "CONTINUE",
    "EXECUTE", "PERFORM", "GET", "CASE",
];

/// 本体に許す文の先頭語。
const ALLOWED_STARTS: &[&str] = &["SELECT", "WITH", "VALUES", "INSERT", "UPDATE", "DELETE"];

fn bad_shape(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::invalid_function_definition(detail)
}

/// 本体 `src` を文に分けて解析する。本体長は字句化の前に検査する（`54000`）。本体が空なら `42P13`、
/// 許可外の文（トランザクション制御・DDL・`SET` 等）は `0A000`、構文誤りは `42601`。
pub(crate) fn parse_sql_function_body(
    src: &str,
    opts: &ParseOptions,
) -> Result<Body, SqlSurfaceError> {
    if src.len() > opts.max_body_bytes {
        return Err(too_big_err("SQL function body exceeds the size limit"));
    }
    let toks = scan(src)?;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // (先頭トークンの添字, 開始バイト, 終了バイト)
    let mut cur: Option<(usize, usize, usize)> = None;
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match &t.kind {
            TokKind::Punct('(') | TokKind::Punct('[') => depth = depth.saturating_add(1),
            TokKind::Punct(')') | TokKind::Punct(']') => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| syn_err("unbalanced parenthesis in a SQL function body"))?;
            }
            TokKind::Punct(';') if depth == 0 => {
                if let Some((first, start, end)) = cur.take() {
                    check_head(toks.get(first).map(|t| &t.kind))?;
                    ranges.push((start, end));
                }
                continue;
            }
            _ => {}
        }
        cur = Some(match cur {
            Some((first, start, _)) => (first, start, t.end),
            None => (i, t.start, t.end),
        });
    }
    if depth != 0 {
        return Err(syn_err("unbalanced parenthesis in a SQL function body"));
    }
    if let Some((first, start, end)) = cur.take() {
        check_head(toks.get(first).map(|t| &t.kind))?;
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(bad_shape("SQL function body must contain a statement"));
    }
    let mut synth = String::from("BEGIN ");
    for (start, end) in &ranges {
        let text = src
            .get(*start..*end)
            .ok_or_else(|| syn_err("invalid span in a SQL function body"))?;
        synth.push_str(text);
        synth.push_str(";\n");
    }
    synth.push_str("END");
    // 本体長は検査済み。組み替えで増えた分で `54000` にしない。
    let mut inner = opts.clone();
    inner.returns = ReturnShape::Void;
    inner.max_body_bytes = usize::MAX;
    let body = parser::parse(&synth, &inner)?;
    for s in &body.block.stmts {
        match s {
            Stmt::Sql(q) if q.into.is_none() => {}
            Stmt::Sql(_) => {
                return Err(oos_err(
                    "SELECT INTO is not supported in a SQL function body",
                ))
            }
            _ => return Err(syn_err("unsupported statement in a SQL function body")),
        }
    }
    Ok(body)
}

/// 文の先頭トークンを検査する（許可語のみ。制御語・DDL・Tx 制御等は `0A000`、それ以外は `42601`）。
fn check_head(kind: Option<&TokKind>) -> Result<(), SqlSurfaceError> {
    let Some(TokKind::Word(w)) = kind else {
        return Err(syn_err("unsupported statement in a SQL function body"));
    };
    let up = w.to_ascii_uppercase();
    if ALLOWED_STARTS.contains(&up.as_str()) {
        return Ok(());
    }
    if REJECTED_STARTS.contains(&up.as_str()) || OUT_OF_SCOPE_STARTS.contains(&up.as_str()) {
        return Err(oos_err(
            "this statement is not supported in a SQL function body",
        ));
    }
    Err(syn_err("unrecognized statement in a SQL function body"))
}

/// 定義時分類（`validate_embedded`）の後に、最後の文が結果を返す形（`SELECT` 系、または `RETURNING`
/// 付きの DML）かを検査する（そうでなければ `42P13`）。列数・型の照合は呼び出し時の実行器が行う。
pub(crate) fn check_result_shape(body: &Body) -> Result<(), SqlSurfaceError> {
    let Some(Stmt::Sql(last)) = body.block.stmts.last() else {
        return Err(bad_shape("SQL function body must contain a statement"));
    };
    let returns_rows = match last.kind {
        EmbeddedKind::Select | EmbeddedKind::Perform => true,
        EmbeddedKind::Insert | EmbeddedKind::Update | EmbeddedKind::Delete => last
            .tokens
            .iter()
            .any(|t| matches!(t, Token::Ident(w) if w.eq_ignore_ascii_case("returning"))),
    };
    if returns_rows {
        Ok(())
    } else {
        Err(bad_shape(
            "the last statement of a SQL function must return a result",
        ))
    }
}
