//! `SELECT` の行ロック句（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、
//! `OF <名前>[, ...]`、`NOWAIT`／`SKIP LOCKED`）の構文解釈と、解析結果を実行計画へ渡す表現
//! （SQL-38 のポインタ。Issue #1957。設計記録は `docs/design/explicit-transaction.md`）。
//!
//! 役割分担:
//!
//! - 本モジュールは構文の解釈と、ほかの句との併用検査までを担う。ロックの取得（writer の取得・
//!   `NOWAIT`・保持期間）は #2230、`SKIP LOCKED` の実行は #2231、`LOCK TABLE` は #1958 の担当で、
//!   本モジュールは一切触れない。受理した形は `core::ParsedSql::LockingSelect` まで届くが、実行入口は
//!   #2230 まで `0A000` で拒否する（ロックを取らずに素の SELECT として実行する fail-open を作らない）。
//! - 呼び出し元は `core::EngineCore::parse_tokens_in`（行数制限句の正規化の後・名前解決の前に
//!   [`extract_row_locking`] を 1 回だけ呼び、検証済みの文を得た後に [`check_row_locking`] を呼ぶ）。
//!   `fetch_clause::normalize_statement_tokens` の中には置かない（PL/pgSQL・ビュー本文・
//!   `INSERT ... SELECT` の内側へ黙って効いてしまうため）。
//!
//! 認識の範囲（満たさなければトークンに触れず、既存の経路の `42601` に任せる）:
//! 文頭が `SELECT` または `RECURSIVE` でない `WITH`、括弧の深さ 0 の `FOR`、直前がオペランド終端
//! （`WHERE for = 2` のように `for` を列名に使う文を壊さない）、直後が強度語。括弧の内側（サブクエリ・
//! CTE 本文）の行ロック句は認識しない。
//!
//! untrusted な入力を扱うため、添字アクセス・`unwrap`・`expect` を使わず `get` で走査する。確保量は
//! 入力トークン数で上限が決まる。

use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::relation::TableRef;

/// ロックの強度（強い順に `Update` > `NoKeyUpdate` > `Share` > `KeyShare`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    /// `FOR UPDATE`。
    Update,
    /// `FOR NO KEY UPDATE`。
    NoKeyUpdate,
    /// `FOR SHARE`。
    Share,
    /// `FOR KEY SHARE`。
    KeyShare,
}

/// 競合したときの待機方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitPolicy {
    /// 指定なし（待機する）。
    Block,
    /// `NOWAIT`。
    NoWait,
    /// `SKIP LOCKED`。
    SkipLocked,
}

/// 行ロック句 1 個。`FOR <強度> [OF <名前>, ...] [NOWAIT | SKIP LOCKED]`。
#[derive(Debug, Clone, PartialEq)]
pub struct RowLockClause {
    pub(crate) strength: LockStrength,
    /// `OF` で指定された名前（FROM の参照名。空なら FROM の全表）。
    pub(crate) of: Vec<String>,
    pub(crate) wait: LockWaitPolicy,
}

impl RowLockClause {
    /// ロックの強度。
    pub fn strength(&self) -> LockStrength {
        self.strength
    }

    /// `OF` で指定された名前（空なら FROM の全表が対象）。
    pub fn of(&self) -> &[String] {
        &self.of
    }

    /// 待機方針。
    pub fn wait(&self) -> LockWaitPolicy {
        self.wait
    }
}

/// 1 つの SELECT に付いた行ロック句の並び（1 個以上）。複数の句が同じ表に及ぶときの強度・待機方針の
/// 合成は本 Issue では決めず、#2230 が担う。
#[derive(Debug, Clone, PartialEq)]
pub struct RowLocking {
    pub(crate) clauses: Vec<RowLockClause>,
}

impl RowLocking {
    /// 行ロック句の並び（書かれた順。1 個以上）。
    pub fn clauses(&self) -> &[RowLockClause] {
        &self.clauses
    }
}

/// 行ロック句つきの SELECT（行ロック句を除いた文の検証結果と、解析した句）。
/// 実行入口は #2230 まで `0A000` で拒否する。
#[derive(Debug, Clone, PartialEq)]
pub struct LockingSelect {
    pub(crate) statement: Statement,
    pub(crate) locking: RowLocking,
}

impl LockingSelect {
    /// 行ロック句を除いた SELECT の検証結果。
    pub fn statement(&self) -> &Statement {
        &self.statement
    }

    /// 解析した行ロック句。
    pub fn locking(&self) -> &RowLocking {
        &self.locking
    }
}

/// [`extract_row_locking`] が取り出した構文上の情報（検証の後に [`check_row_locking`] が使う）。
#[derive(Debug, Clone)]
pub(crate) struct RowLockingSyntax {
    clauses: Vec<RowLockClause>,
    /// 最上位の FROM の先頭の参照 `(表名, 別名)`（単一表の SELECT の `OF` 照合に使う）。
    first_from: Option<TableRef>,
    /// `first_from` がスキーマ修飾付き（`s.t`）か。修飾付きは CTE 名と衝突しない（実表）。
    first_from_qualified: bool,
    /// 先頭の `WITH` が定義した CTE 名。
    cte_names: Vec<String>,
    /// `DISTINCT`／`GROUP BY`／`HAVING`／`WINDOW`／集合演算が最上位にある。
    combo_violation: bool,
}

fn is_word(token: Option<&Token>, word: &str) -> bool {
    matches!(token, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(word))
}

fn syntax(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// `FOR` の位置 `i` から行ロック句の強度語が始まるか（誤認を避けるため強度語の先頭だけ見る）。
fn starts_strength(tokens: &[Token], i: usize) -> bool {
    is_word(tokens.get(i), "UPDATE")
        || is_word(tokens.get(i), "SHARE")
        || is_word(tokens.get(i), "NO")
        || is_word(tokens.get(i), "KEY")
}

/// 行ロック句 1 個を `i`（`FOR` の位置）から解析し、`(句, 次の位置)` を返す。構文誤りは `42601`。
fn parse_clause(tokens: &[Token], mut i: usize) -> Result<(RowLockClause, usize), SqlSurfaceError> {
    i += 1; // FOR
    let strength = if is_word(tokens.get(i), "UPDATE") {
        i += 1;
        LockStrength::Update
    } else if is_word(tokens.get(i), "SHARE") {
        i += 1;
        LockStrength::Share
    } else if is_word(tokens.get(i), "NO") {
        if is_word(tokens.get(i + 1), "KEY") && is_word(tokens.get(i + 2), "UPDATE") {
            i += 3;
            LockStrength::NoKeyUpdate
        } else {
            return Err(syntax("invalid row-level locking clause"));
        }
    } else if is_word(tokens.get(i), "KEY") && is_word(tokens.get(i + 1), "SHARE") {
        i += 2;
        LockStrength::KeyShare
    } else {
        return Err(syntax("invalid row-level locking clause"));
    };

    let mut of: Vec<String> = Vec::new();
    if is_word(tokens.get(i), "OF") {
        i += 1;
        loop {
            match tokens.get(i) {
                // `OF` / `,` の直後は名前が必須の位置なので、待機方針語と同じ綴りでも名前として受理する
                // （`AS "nowait"` への `OF "nowait"`）。字句段は引用有無を保持しないため綴りでは区別できない。
                // 引用付きの値関数名（`"current_date"` = QuotedNull）・真偽語（QuotedBoolWord）も FROM の
                // 識別子解析と同様に受理する。
                Some(
                    Token::Ident(name) | Token::QuotedNull(name) | Token::QuotedBoolWord(name),
                ) => {
                    of.push(name.clone());
                    i += 1;
                }
                _ => return Err(syntax("invalid OF list in row-level locking clause")),
            }
            if matches!(tokens.get(i), Some(Token::Punct(','))) {
                i += 1;
            } else {
                break;
            }
        }
    }

    let mut wait = LockWaitPolicy::Block;
    if is_word(tokens.get(i), "NOWAIT") {
        wait = LockWaitPolicy::NoWait;
        i += 1;
    } else if is_word(tokens.get(i), "SKIP") {
        if is_word(tokens.get(i + 1), "LOCKED") {
            wait = LockWaitPolicy::SkipLocked;
            i += 2;
        } else {
            return Err(syntax("invalid wait policy in row-level locking clause"));
        }
    }
    if is_word(tokens.get(i), "NOWAIT") || is_word(tokens.get(i), "SKIP") {
        return Err(syntax(
            "conflicting wait policies in row-level locking clause",
        ));
    }
    Ok((RowLockClause { strength, of, wait }, i))
}

/// 行ロック句の後ろに置いてよいトークン（`LIMIT`／`OFFSET` と正規化後の `FETCH` 形）か。
fn is_tail_token(token: &Token) -> bool {
    match token {
        Token::Keyword(Keyword::Limit) | Token::Number(_) | Token::Param(_) => true,
        Token::Ident(s) => ["OFFSET", "ALL", "FETCH", "FIRST", "ROWS", "WITH", "TIES"]
            .iter()
            .any(|w| s.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

fn is_limit_or_offset(tokens: &[Token], i: usize) -> bool {
    matches!(tokens.get(i), Some(Token::Keyword(Keyword::Limit)))
        || (is_word(tokens.get(i), "OFFSET")
            && crate::sql::fetch_clause::follows_operand(
                i.checked_sub(1).and_then(|p| tokens.get(p)),
            ))
}

/// `tokens` から最上位の行ロック句を取り除き、構文上の情報を返す（モジュール冒頭参照）。
///
/// 認識条件を満たさない文はトークンに触れず `None`。句と認識したうえでの構文誤り（`OF` の後が空・
/// `NOWAIT` と `SKIP LOCKED` の併記など）は `42601`。冪等（句を取り除いた列に再適用しても `None`）。
pub(crate) fn extract_row_locking(
    tokens: Vec<Token>,
) -> Result<(Vec<Token>, Option<RowLockingSyntax>), SqlSurfaceError> {
    let head_ok = match tokens.first() {
        Some(Token::Keyword(Keyword::Select)) => true,
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("WITH") => {
            !is_word(tokens.get(1), "RECURSIVE")
        }
        _ => false,
    };
    if !head_ok {
        return Ok((tokens, None));
    }

    // 最上位（括弧の深さ 0）で最初の行ロック句の開始位置を探す。
    let mut depth: usize = 0;
    let mut start: Option<usize> = None;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Punct(';') if depth == 0 => break,
            Token::Ident(w)
                if depth == 0
                    && w.eq_ignore_ascii_case("FOR")
                    && crate::sql::fetch_clause::follows_operand(
                        i.checked_sub(1).and_then(|p| tokens.get(p)),
                    )
                    && starts_strength(&tokens, i + 1) =>
            {
                start = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(start) = start else {
        return Ok((tokens, None));
    };

    // 句を 1 個以上続けて解析する。
    let mut clauses: Vec<RowLockClause> = Vec::new();
    let mut i = start;
    while is_word(tokens.get(i), "FOR") {
        let (clause, next) = parse_clause(&tokens, i)?;
        clauses.push(clause);
        i = next;
    }
    let end = i;

    // 句の後ろは文末（または最上位の `;`）か `LIMIT`／`OFFSET`／`FETCH` の正規形だけを許す。
    let mut tail_has_limit = false;
    let mut j = end;
    while let Some(tok) = tokens.get(j) {
        if matches!(tok, Token::Punct(';')) {
            break;
        }
        if !is_tail_token(tok) {
            return Ok((tokens, None));
        }
        if is_limit_or_offset(&tokens, j) {
            tail_has_limit = true;
        }
        j += 1;
    }
    // 句の前後の両方に `LIMIT`／`OFFSET` がある形は PostgreSQL も受理しない。
    let mut depth: usize = 0;
    let mut pre_has_limit = false;
    for (k, tok) in tokens.iter().enumerate().take(start) {
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 && is_limit_or_offset(&tokens, k) => pre_has_limit = true,
            _ => {}
        }
    }
    if pre_has_limit && tail_has_limit {
        return Ok((tokens, None));
    }

    // 最上位の併用違反の印・FROM の先頭の参照・CTE 名を句の前の部分から集める。
    let pre = tokens.get(..start).unwrap_or(&[]);
    let mut combo_violation = false;
    let mut first_from: Option<TableRef> = None;
    let mut first_from_qualified = false;
    let mut cte_names: Vec<String> = Vec::new();
    let mut depth: usize = 0;
    for (k, tok) in pre.iter().enumerate() {
        match tok {
            Token::Punct('(') => {
                depth = depth.saturating_add(1);
                continue;
            }
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ => {}
        }
        if depth != 0 {
            continue;
        }
        let prev = k.checked_sub(1).and_then(|p| pre.get(p));
        match tok {
            Token::Keyword(Keyword::From) if first_from.is_none() => {
                first_from = parse_first_from(pre, k + 1);
                first_from_qualified = matches!(pre.get(k + 1), Some(Token::QualifiedIdent { .. }));
            }
            Token::Ident(w) => {
                let upper = w.to_ascii_uppercase();
                match upper.as_str() {
                    // 表名・列名・別名としての `distinct`／`having`（引用付きを含む）を誤認しないよう、
                    // `DISTINCT` は直前が `SELECT`、`HAVING` はオペランド終端の直後だけを句とみなす。
                    "DISTINCT" if matches!(prev, Some(Token::Keyword(Keyword::Select))) => {
                        combo_violation = true
                    }
                    "HAVING"
                        if crate::sql::fetch_clause::follows_operand(prev)
                            && !prev_is_clause_word(prev) =>
                    {
                        combo_violation = true
                    }
                    "GROUP" if matches!(pre.get(k + 1), Some(Token::Keyword(Keyword::By))) => {
                        combo_violation = true
                    }
                    // 列別名（`AS "union"`）を集合演算と誤認しないよう、直前が `AS` でなく、
                    // 後続が SELECT の枝（`SELECT`／`ALL`／`DISTINCT`／`VALUES`／`TABLE`／`(`）のときだけ印を立てる。
                    "UNION" | "INTERSECT" | "EXCEPT"
                        if crate::sql::fetch_clause::follows_operand(prev)
                            && !is_word(prev, "AS")
                            && set_op_branch_follows(pre.get(k + 1)) =>
                    {
                        combo_violation = true
                    }
                    "WINDOW"
                        if crate::sql::fetch_clause::follows_operand(prev)
                            && matches!(pre.get(k + 1), Some(Token::Ident(_)))
                            && is_word(pre.get(k + 2), "AS") =>
                    {
                        combo_violation = true
                    }
                    "AS" => {
                        // `<名前> [(<列>...)] AS [NOT] [MATERIALIZED] (` は CTE の定義。
                        let mut n = k + 1;
                        if is_word(pre.get(n), "NOT") {
                            n += 1;
                        }
                        if is_word(pre.get(n), "MATERIALIZED") {
                            n += 1;
                        }
                        if matches!(pre.get(n), Some(Token::Punct('('))) {
                            // 列リスト付き（`c(a, b) AS (`）は直前が `)` なので対応する `(` の前の名前を取る。
                            let name_tok = if matches!(prev, Some(Token::Punct(')'))) {
                                cte_name_before_column_list(pre, k)
                            } else {
                                prev
                            };
                            if let Some(Token::Ident(name)) = name_tok {
                                cte_names.push(name.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // 先頭が WITH でない文では CTE 名を持たない。
    if !is_word(tokens.first(), "WITH") {
        cte_names.clear();
    }

    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    out.extend(tokens.iter().take(start).cloned());
    out.extend(tokens.iter().skip(end).cloned());
    Ok((
        out,
        Some(RowLockingSyntax {
            clauses,
            first_from,
            first_from_qualified,
            cte_names,
            combo_violation,
        }),
    ))
}

/// 集合演算語の直後が SELECT の枝の先頭か。
fn set_op_branch_follows(next: Option<&Token>) -> bool {
    match next {
        Some(Token::Keyword(Keyword::Select)) | Some(Token::Punct('(')) => true,
        Some(Token::Ident(w)) => matches!(
            w.to_ascii_uppercase().as_str(),
            "ALL" | "DISTINCT" | "VALUES" | "TABLE"
        ),
        _ => false,
    }
}

/// 直前の識別子がキーワード相当の語（`JOIN`・`OUTER`・`ON` 等）で、次の語が名前の位置にある。
fn prev_is_clause_word(prev: Option<&Token>) -> bool {
    const WORDS: &[&str] = &[
        "JOIN", "OUTER", "ON", "USING", "AS", "BY", "WHERE", "AND", "OR", "NOT", "FROM",
    ];
    matches!(prev, Some(Token::Ident(w)) if WORDS.iter().any(|x| w.eq_ignore_ascii_case(x)))
}

/// `pre[as_idx]`（`AS`）の直前が `)` のとき、対応する `(` の直前の名前トークンを返す。
fn cte_name_before_column_list(pre: &[Token], as_idx: usize) -> Option<&Token> {
    let mut depth: usize = 0;
    let mut k = as_idx;
    while k > 0 {
        k -= 1;
        match pre.get(k)? {
            Token::Punct(')') => depth = depth.saturating_add(1),
            Token::Punct('(') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return k.checked_sub(1).and_then(|p| pre.get(p));
                }
            }
            _ => {}
        }
    }
    None
}

/// `pre[i..]`（`FROM` の直後）から先頭の参照を読む。別名の規則は単一表 FROM の構文解析と
/// 同じ実装（`allowlist::probe_from_table_ref`）に一本化し、ここでは第 2 の規則を持たない。
/// 修飾名（`s.t`）は名前部だけを表名として渡す（拡張クエリでは内部キー `#<id>.<name>` の
/// 場合があり、公開名は [`TableRef::exposed_name`] が名前部へ正規化する）。
fn parse_first_from(pre: &[Token], i: usize) -> Option<TableRef> {
    let mut rest: Vec<Token> = pre.get(i..)?.to_vec();
    if let Some(Token::QualifiedIdent { name, .. }) = rest.first() {
        let name = name.clone();
        *rest.first_mut()? = Token::Ident(name);
    }
    crate::sql::allowlist::probe_from_table_ref(&rest).map(|(r, _)| r)
}

fn combo_error() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "row-level locking clauses cannot be combined with DISTINCT, GROUP BY, HAVING, aggregates, window functions, set operations, or the nullable side of an outer join",
    )
}

/// 検証済みの文 `stmt` と構文情報から、併用違反を検査して [`RowLocking`] を作る（モジュール冒頭参照）。
///
/// 判定順は PostgreSQL に合わせる: 構文（抽出の段）→ FROM の解決（検証の段）→ 構文上の併用違反・
/// 文の形（`0A000`）→ `OF` の解決（`42P01`）→ 外部結合で NULL を補う側（`0A000`）。
/// `OF` の照合先は文の FROM の参照名だけで、カタログは引かない。今回の対象外の文の形
/// （ビュー・再帰 CTE・関数スキャン・定数 SELECT・`EXPLAIN` など）は `0A000`（fail-closed）。
pub(crate) fn check_row_locking(
    stmt: &Statement,
    syntax: &RowLockingSyntax,
) -> Result<RowLocking, SqlSurfaceError> {
    if syntax.combo_violation {
        return Err(combo_error());
    }
    let of_all: Vec<&str> = syntax
        .clauses
        .iter()
        .flat_map(|c| c.of.iter().map(String::as_str))
        .collect();
    let any_unscoped = syntax.clauses.iter().any(|c| c.of.is_empty());

    match stmt {
        Statement::Select(_) => {}
        Statement::Scan(scan) => {
            if !scan.window_items.is_empty() {
                return Err(combo_error());
            }
        }
        Statement::Join(join) => {
            if join.aggregate.is_some() {
                return Err(combo_error());
            }
        }
        _ => return Err(combo_error()),
    }

    if let Statement::Join(join) = stmt {
        let exposed: Vec<&str> = join.relations.iter().map(|r| r.exposed_name()).collect();
        let is_cte = |idx: usize| {
            join.cte_inputs
                .as_ref()
                .and_then(|c| c.relation_slot.get(idx))
                .is_some_and(|s| s.is_some())
        };
        // `OF` の名前は FROM の参照名と一致しなければ 42P01（カタログは引かない）。
        for name in &of_all {
            if !exposed.iter().any(|e| e == name) {
                return Err(SqlSurfaceError::undefined_table(*name));
            }
        }
        // 各句の対象（`OF` があればその表、無ければ FROM の全表）を検査する。
        for clause in &syntax.clauses {
            for (idx, e) in exposed.iter().enumerate() {
                if !clause.of.is_empty() && !clause.of.iter().any(|n| n == e) {
                    continue;
                }
                if is_cte(idx) {
                    return Err(combo_error());
                }
                if is_nullable_side(&join.steps, idx) {
                    return Err(combo_error());
                }
            }
        }
    } else {
        // 単一表の SELECT。参照名は FROM の先頭の参照（別名があれば別名）。
        let Some(first) = &syntax.first_from else {
            return Err(combo_error());
        };
        let exposed = first.exposed_name();
        if !syntax.first_from_qualified && syntax.cte_names.iter().any(|c| c == first.table()) {
            return Err(combo_error());
        }
        for name in &of_all {
            if *name != exposed {
                return Err(SqlSurfaceError::undefined_table(*name));
            }
        }
        let _ = any_unscoped;
    }

    Ok(RowLocking {
        clauses: syntax.clauses.clone(),
    })
}

/// 左深の結合連鎖で、関係 `idx` が外部結合により NULL を補われうるか。
/// `Left` は右辺（段 k が加える関係 k+1）、`Right` はそれまでの左辺すべて、`Full` は両辺。
fn is_nullable_side(steps: &[crate::sql::allowlist::JoinStep], idx: usize) -> bool {
    use crate::sql::allowlist::JoinKind;
    steps.iter().enumerate().any(|(k, step)| {
        let right = k + 1;
        match step.kind {
            JoinKind::Inner => false,
            JoinKind::Left => idx == right,
            JoinKind::Right => idx <= k,
            JoinKind::Full => idx <= right,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize_with_params(sql).expect("tokenize")
    }

    fn extract(sql: &str) -> Result<(Vec<Token>, Option<RowLockingSyntax>), SqlSurfaceError> {
        extract_row_locking(toks(sql))
    }

    fn clauses(sql: &str) -> Vec<RowLockClause> {
        extract(sql)
            .expect("extract")
            .1
            .expect("recognized")
            .clauses
    }

    fn untouched(sql: &str) {
        let (out, syn) = extract(sql).expect("no error");
        assert!(syn.is_none(), "{sql}");
        assert_eq!(out, toks(sql), "{sql}");
    }

    fn code(sql: &str) -> &'static str {
        extract(sql).expect_err("error").wire_code()
    }

    #[test]
    fn strength_of_wait_cross_product() {
        let strengths = [
            ("FOR UPDATE", LockStrength::Update),
            ("FOR NO KEY UPDATE", LockStrength::NoKeyUpdate),
            ("FOR SHARE", LockStrength::Share),
            ("FOR KEY SHARE", LockStrength::KeyShare),
        ];
        let ofs: [(&str, Vec<&str>); 3] = [
            ("", vec![]),
            (" OF a", vec!["a"]),
            (" OF a, b", vec!["a", "b"]),
        ];
        let waits = [
            ("", LockWaitPolicy::Block),
            (" NOWAIT", LockWaitPolicy::NoWait),
            (" SKIP LOCKED", LockWaitPolicy::SkipLocked),
        ];
        for (s_sql, s) in strengths {
            for (o_sql, o) in &ofs {
                for (w_sql, w) in waits {
                    let sql = format!("SELECT id FROM a WHERE id = 1 {s_sql}{o_sql}{w_sql}");
                    let got = clauses(&sql);
                    assert_eq!(
                        got,
                        vec![RowLockClause {
                            strength: s,
                            of: o.iter().map(|x| x.to_string()).collect(),
                            wait: w,
                        }],
                        "{sql}"
                    );
                }
            }
        }
    }

    #[test]
    fn case_and_whitespace_are_insensitive() {
        let got = clauses("select id from a for\n  no   key\tupdate of a skip\nlocked");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].strength, LockStrength::NoKeyUpdate);
        assert_eq!(got[0].wait, LockWaitPolicy::SkipLocked);
    }

    #[test]
    fn clause_is_removed_from_tokens() {
        let (out, syn) = extract("SELECT id FROM a WHERE id = 1 FOR UPDATE").unwrap();
        assert!(syn.is_some());
        assert_eq!(out, toks("SELECT id FROM a WHERE id = 1"));
    }

    #[test]
    fn for_as_identifier_is_not_a_clause() {
        untouched("SELECT id FROM t WHERE for = 2");
        untouched("SELECT share FROM t");
        untouched("SELECT id FROM t WHERE x = 1 AND for = 2");
    }

    #[test]
    fn tail_orderings() {
        let (out, syn) = extract("SELECT id FROM a LIMIT 3 FOR UPDATE").unwrap();
        assert!(syn.is_some());
        assert_eq!(out, toks("SELECT id FROM a LIMIT 3"));
        let (out, syn) = extract("SELECT id FROM a FOR UPDATE LIMIT 3").unwrap();
        assert!(syn.is_some());
        assert_eq!(out, toks("SELECT id FROM a LIMIT 3"));
        let (out, syn) = extract("SELECT id FROM a ORDER BY id FOR UPDATE OFFSET 2").unwrap();
        assert!(syn.is_some());
        assert_eq!(out, toks("SELECT id FROM a ORDER BY id OFFSET 2"));
        // 正規化後の FETCH 形は LIMIT として届く。
        assert!(extract("SELECT id FROM a ORDER BY id FOR UPDATE LIMIT 1")
            .unwrap()
            .1
            .is_some());
    }

    #[test]
    fn misplaced_tails_are_untouched() {
        untouched("SELECT id FROM a LIMIT 1 FOR UPDATE OFFSET 2");
        untouched("SELECT id FROM a FOR UPDATE ORDER BY id");
        untouched("SELECT id FROM a FOR UPDATE WHERE id = 1");
        untouched("SELECT RANK() OVER w FROM a FOR UPDATE WINDOW w AS (ORDER BY id)");
    }

    #[test]
    fn of_accepts_quoted_names_spelled_like_keywords() {
        for name in ["nowait", "skip", "current_date", "true"] {
            let sql = format!("SELECT id FROM a AS \"{name}\" FOR UPDATE OF \"{name}\" NOWAIT");
            let cs = clauses(&sql);
            assert_eq!(cs.len(), 1, "{sql}");
            assert_eq!(cs[0].of(), [name.to_string()], "{sql}");
            assert_eq!(cs[0].wait(), LockWaitPolicy::NoWait, "{sql}");
        }
    }

    #[test]
    fn multiple_clauses() {
        let got =
            clauses("SELECT id FROM a JOIN b ON a.id = b.id FOR UPDATE OF a FOR SHARE OF b NOWAIT");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].strength, LockStrength::Update);
        assert_eq!(got[1].strength, LockStrength::Share);
        assert_eq!(got[1].wait, LockWaitPolicy::NoWait);
    }

    #[test]
    fn syntax_errors_are_42601() {
        for sql in [
            "SELECT id FROM a FOR NO KEY",
            "SELECT id FROM a FOR UPDATE OF",
            "SELECT id FROM a FOR UPDATE OF a,",
            "SELECT id FROM a FOR UPDATE NOWAIT SKIP LOCKED",
            "SELECT id FROM a FOR UPDATE SKIP",
            "SELECT id FROM a FOR UPDATE SKIP LOCKED NOWAIT",
            "SELECT id FROM a FOR UPDATE OF s.t",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
        // 認識しない形は触れない（既存の経路が 42601 にする）。
        untouched("SELECT id FROM a FOR");
        untouched("SELECT id FROM a FOR READ ONLY");
    }

    #[test]
    fn nested_and_other_heads_are_untouched() {
        untouched("SELECT id FROM (SELECT id FROM a FOR UPDATE) AS s");
        untouched("SELECT substring(x FROM 1 FOR 2) FROM a");
        untouched("DECLARE c CURSOR FOR SELECT id FROM a FOR UPDATE");
        untouched("EXPLAIN SELECT id FROM a FOR UPDATE");
        untouched("INSERT INTO t SELECT id FROM a FOR UPDATE");
        untouched("CREATE VIEW v AS SELECT id FROM a FOR UPDATE");
        untouched("WITH RECURSIVE c AS (SELECT 1) SELECT id FROM a FOR UPDATE");
    }

    #[test]
    fn with_head_is_recognized() {
        let syn = extract("WITH c AS (SELECT id FROM a) SELECT id FROM c FOR UPDATE")
            .unwrap()
            .1
            .unwrap();
        assert_eq!(syn.cte_names, vec!["c".to_string()]);
    }

    #[test]
    fn column_list_cte_name_and_quoted_clause_words() {
        let (_, syn) = extract("WITH c(a, b) AS (SELECT 1) SELECT a FROM c FOR UPDATE").unwrap();
        assert_eq!(syn.unwrap().cte_names, vec!["c".to_string()]);
        let (_, syn) = extract("SELECT id FROM \"having\" LIMIT 5 FOR UPDATE").unwrap();
        assert!(!syn.unwrap().combo_violation);
        let (_, syn) = extract("SELECT id FROM a JOIN \"having\" ON a.x = 1 FOR UPDATE").unwrap();
        assert!(!syn.unwrap().combo_violation);
    }

    #[test]
    fn only_first_statement_is_processed() {
        let (out, syn) = extract("SELECT id FROM a FOR UPDATE; SELECT id FROM b").unwrap();
        assert!(syn.is_some());
        assert_eq!(out, toks("SELECT id FROM a; SELECT id FROM b"));
    }

    #[test]
    fn extraction_is_idempotent() {
        let (out, _) = extract("SELECT id FROM a FOR UPDATE OF a NOWAIT").unwrap();
        let (again, syn) = extract_row_locking(out.clone()).unwrap();
        assert!(syn.is_none());
        assert_eq!(again, out);
    }

    #[test]
    fn qualified_from_is_flagged_so_it_never_matches_a_cte_name() {
        let syn = extract("WITH t AS (SELECT 1) SELECT id FROM public.t FOR UPDATE")
            .unwrap()
            .1
            .unwrap();
        assert!(syn.first_from_qualified);
        let syn = extract("WITH t AS (SELECT 1) SELECT id FROM t FOR UPDATE")
            .unwrap()
            .1
            .unwrap();
        assert!(!syn.first_from_qualified);
    }

    #[test]
    fn combo_marks() {
        for sql in [
            "SELECT DISTINCT id FROM a FOR UPDATE",
            "SELECT id FROM a GROUP BY id FOR UPDATE",
            "SELECT id FROM a GROUP BY id HAVING id > 1 FOR UPDATE",
            "SELECT id FROM a UNION SELECT id FROM b FOR UPDATE",
            "SELECT id FROM a UNION ALL SELECT id FROM b FOR UPDATE",
        ] {
            assert!(extract(sql).unwrap().1.unwrap().combo_violation, "{sql}");
        }
        for sql in [
            "SELECT id AS \"union\" FROM a LIMIT 5 FOR UPDATE",
            "SELECT id AS \"except\" FROM a FOR UPDATE",
        ] {
            assert!(!extract(sql).unwrap().1.unwrap().combo_violation, "{sql}");
        }
        assert!(
            !extract("SELECT id FROM a FOR UPDATE")
                .unwrap()
                .1
                .unwrap()
                .combo_violation
        );
    }

    #[test]
    fn first_from_reads_alias() {
        let from = |sql: &str| extract(sql).unwrap().1.unwrap().first_from.unwrap();
        let r = from("SELECT x.id FROM public.a AS x FOR UPDATE OF x");
        assert_eq!((r.table(), r.alias()), ("a", Some("x")));
        let r = from("SELECT id FROM a x WHERE id = 1 FOR UPDATE");
        assert_eq!((r.table(), r.alias()), ("a", Some("x")));
        let r = from("SELECT id FROM a FOR UPDATE");
        assert_eq!((r.table(), r.alias()), ("a", None));
        // 暗黙の別名が句の語と同名でも、単一表 FROM の規則どおり別名として読む。
        let r = from("SELECT id FROM a for FOR UPDATE");
        assert_eq!((r.table(), r.alias()), ("a", Some("for")));
        let r = from("SELECT id FROM a window FOR UPDATE");
        assert_eq!((r.table(), r.alias()), ("a", Some("window")));
    }
}
