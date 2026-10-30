//! PL/pgSQL 本体の SQL を呼び出し元と同じ Tx・同じ RLS で実行する core 側の `PlHost` 実装
//! （Issue #2226・SQL-68。設計は `docs/design/plpgsql-subset.md` D3・D5・D6・D8）。
//!
//! `plpgsql::interp` は制御フローだけを持ち、SQL・式の評価は [`PlHost`] へ委ねる。本モジュールの
//! [`CorePlHost`] がその実体で、変数・`USING` の値を型付きトークンへ置換し（`plpgsql::subst`。SQL
//! テキストへは戻さない）、既存の許可リスト解析（`parse_tokens_gated`）と実行本体
//! （`execute_in_active_txn`）へそのまま渡す。第 2 の SQL 実行器は作らない。この経路により、
//! `mark_written`・未 commit の変更の読み取り・`--max-result-rows`・DML の影響行数上限・RLS-7 の暗黙
//! 述語が呼び出し元の文と同一に効く。テナントの文脈を切り替える API は持たない（`ctx` は借用のまま）。
//!
//! 入口は [`EngineCore::run_plpgsql_in_txn`]（既に開いている Tx に参加する）と
//! [`EngineCore::run_plpgsql_autocommit`]（Tx が無ければ暗黙 Tx を開き、成功で commit・失敗で abort
//! する）。呼び出し元は #2217（`LANGUAGE plpgsql` の結線）・#2218（呼び出し位置）・#2228（`DO`・
//! トリガ関数）が結線するまでテストだけ。
//!
//! 再入の不変条件（ADR D5）: 入口は、外側の実行器が同じ write txn の表ハンドルを開いていない時点
//! からだけ呼ぶこと（同じ表を二重に開くと `TableAlreadyOpen`）。外側の対象行・可視行は呼び出しの前に
//! 確定させる。本体の各文は `check_and_register_statement` を呼ばない（外側の 1 文の一部であり、文数は
//! `--max-plpgsql-statements` が抑える。operation_id も持たず、台帳は外側の文の単位で 1 つ）。
//!
//! fail-closed: 解析・実行のエラーは固定文言のまま返し、明示 Tx は `Failed`（後続は `25P02`）にする。
//! 本体で許す文は DML と `SELECT` 系だけで、DDL・Tx 制御・`SET`・`COPY`・`USING OPERATION_ID`・
//! `PARTITIONED` は `0A000`。`Ledgered` 構成では本体の書き込みを `23502` で拒否する（外側の
//! operation_id を本体の実行器へ引き継ぐ配線は #2218／#2228 で入れる）。

use std::collections::HashSet;

use super::{
    is_partitioned_write, parsed_operation_id, session_time_zone_scope, EngineCore, ParsedSql,
};
use crate::catalog::{ColumnType, TableSchema};
use crate::plpgsql::ast::{Body, DeclType, EmbeddedSql, SqlExpr, VarRole, VarSite};
use crate::plpgsql::host::{
    PlHost, PlScalar, ResolvedType, RowDemand, SqlOutcome as HostOutcome, VarLookup,
};
use crate::plpgsql::interp::{run_function, CallOutcome, ExecBudget, ReturnSpec};
use crate::plpgsql::subst;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{SqlSurfaceError, Statement};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult};
use crate::sql::lexer::{self, Keyword, Token};
use crate::sql::mode::SessionState;
use crate::sql::transaction::{SessionTransaction, TransactionStatus};
use crate::sql::SqlOutcome as CoreOutcome;

/// 呼び出しごとの実行オプション。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlCallOptions {
    /// 本体が書き込んでよいか（揮発性ゲート。`IMMUTABLE`／`STABLE` 関数は偽で、書き込む文は `0A000`）。
    /// #2217 が関数の属性と `Body::may_write` から決める。
    pub(crate) may_write: bool,
}

/// 1 回の呼び出しの入力（本体・引数・戻り値の形・オプション）。入口の引数を束ねる。
pub(crate) struct PlInvocation<'a> {
    pub(crate) body: &'a Body,
    pub(crate) args: Vec<PlScalar>,
    pub(crate) ret: &'a ReturnSpec,
    pub(crate) opts: PlCallOptions,
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

/// 本体・動的 SQL で許す文か（書き込みなら `Some(true)`）。許可リスト解析を通った文のうち、DML と
/// `SELECT` 系以外（DDL・Tx 制御・`SET`・`COPY`・カーソル等）は `0A000`。
fn classify(parsed: &ParsedSql) -> Result<bool, SqlSurfaceError> {
    match parsed {
        ParsedSql::Insert(_)
        | ParsedSql::InsertSelect(_)
        | ParsedSql::Update(_)
        | ParsedSql::Delete(_)
        | ParsedSql::DataModifyingCte(_)
        | ParsedSql::ViewInsert(_)
        | ParsedSql::ViewUpdate(_)
        | ParsedSql::ViewDelete(_) => Ok(true),
        ParsedSql::Statement(
            Statement::Select(_)
            | Statement::Aggregate(_)
            | Statement::Scan(_)
            | Statement::Join(_)
            | Statement::BufferedView(_)
            | Statement::ConstSelect(_)
            | Statement::SetOperation(_),
        ) => Ok(false),
        ParsedSql::SessionRouted(routed) => classify(routed.inner()),
        _ => Err(unsupported(
            "this statement is not supported inside a PL/pgSQL body",
        )),
    }
}

/// 結果列メタを（列名, 型）へ。静的な型を持たない列（ベクトル式等）は `0A000`。
fn column_pair(m: &ColumnMeta) -> Result<(String, ColumnType), SqlSurfaceError> {
    match m {
        ColumnMeta::Id => Ok(("id".to_string(), ColumnType::BigInt)),
        ColumnMeta::AliasedId { name } => Ok((name.clone(), ColumnType::BigInt)),
        ColumnMeta::Scalar { name, ty } => Ok((name.clone(), ty.clone())),
        ColumnMeta::Computed { name, ty: Some(ty) } => Ok((name.clone(), ty.clone())),
        ColumnMeta::Computed { ty: None, .. } | ColumnMeta::Record { .. } => Err(unsupported(
            "a result column type is not supported inside a PL/pgSQL body",
        )),
    }
}

fn from_query(
    qr: QueryResult,
    dml: bool,
    affected: Option<u64>,
    demand: RowDemand,
) -> Result<HostOutcome, SqlSurfaceError> {
    let columns = qr
        .columns
        .iter()
        .map(column_pair)
        .collect::<Result<Vec<_>, _>>()?;
    let total = u64::try_from(qr.rows.len()).unwrap_or(u64::MAX);
    let mut rows: Vec<Vec<Cell>> = qr.rows.into_iter().map(|r| r.cells).collect();
    match demand {
        RowDemand::None => rows.clear(),
        RowDemand::AtMostTwo => rows.truncate(2),
        RowDemand::All => {}
    }
    Ok(HostOutcome {
        columns,
        rows,
        row_count: affected.unwrap_or(total),
        dml,
    })
}

/// 実行結果を境界の形へ写す。想定外の variant は `0A000`（ワイルドカードは fail-closed）。
fn map_outcome(out: CoreOutcome, demand: RowDemand) -> Result<HostOutcome, SqlSurfaceError> {
    let dml_only = |n: u64| HostOutcome {
        columns: Vec::new(),
        rows: Vec::new(),
        row_count: n,
        dml: true,
    };
    match out {
        CoreOutcome::Query(qr) => from_query(qr, false, None, demand),
        CoreOutcome::Insert(o) => Ok(dml_only(o.rows_affected)),
        CoreOutcome::Update(o) => Ok(dml_only(o.rows_affected)),
        CoreOutcome::Delete(o) => Ok(dml_only(o.rows_affected)),
        CoreOutcome::Returning(r) => from_query(r.result, true, Some(r.rows_affected), demand),
        _ => Err(unsupported(
            "this statement result is not supported inside a PL/pgSQL body",
        )),
    }
}

/// 1 列 1 行の結果を型付きスカラーにする。
fn single_scalar(qr: &QueryResult) -> Result<PlScalar, SqlSurfaceError> {
    let (Some(meta), [row]) = (qr.columns.first(), qr.rows.as_slice()) else {
        return Err(internal("PL/pgSQL expression did not return one row"));
    };
    let (_, ty) = column_pair(meta)?;
    let cell = match row.cells.as_slice() {
        [c] => c.clone(),
        _ => return Err(internal("PL/pgSQL expression did not return one column")),
    };
    Ok(PlScalar::new(cell, ty))
}

/// 式が変数 1 個だけ（`x := y`・`IF flag`）なら、その変数をそのまま返す（SQL を経由しない。真偽値や
/// 日時などを `SELECT` の項目に置けない形を避け、型も保つ）。
fn direct_var(expr: &SqlExpr, env: &dyn VarLookup) -> Result<Option<PlScalar>, SqlSurfaceError> {
    match (expr.tokens.as_slice(), expr.var_refs.as_slice()) {
        ([_], [site]) if site.pos == 0 => Ok(Some(env.scalar(&site.var)?.clone())),
        _ => Ok(None),
    }
}

/// 式が `NULL`／`true`／`false` の裸のリテラル 1 個だけなら、その値を返す（FROM の無い `SELECT` は
/// これらを列名として扱い `42703` になるため、SQL を経由しない）。`NULL` の型は `Text`（代入先の
/// 宣言型への当てはめはインタプリタが行う）。
fn bare_literal(expr: &SqlExpr) -> Option<PlScalar> {
    if !expr.var_refs.is_empty() {
        return None;
    }
    let [Token::Ident(w)] = expr.tokens.as_slice() else {
        return None;
    };
    if w.eq_ignore_ascii_case("null") {
        Some(PlScalar::null(ColumnType::Text))
    } else if w.eq_ignore_ascii_case("true") {
        Some(PlScalar::new(Cell::Bool(true), ColumnType::Boolean))
    } else if w.eq_ignore_ascii_case("false") {
        Some(PlScalar::new(Cell::Bool(false), ColumnType::Boolean))
    } else {
        None
    }
}

/// `PlHost` の core 実装。1 回の起動の間だけ生きる（`'e` は `EngineCore` と Tx の寿命）。
struct CorePlHost<'a, 'e> {
    core: &'e EngineCore,
    ctx: &'a PolicyContext,
    session: &'a mut SessionState,
    txn: &'a mut SessionTransaction<'e>,
    opts: PlCallOptions,
    /// `42702` の検査済みの文・式（種別（式 0・埋め込み SQL 1）とアドレスをキーにする。本体の AST は起動の間不変で、ループで検査を
    /// 繰り返さない）。値は読み出さないので、アドレスの再利用で誤って参照されることはない。
    conflict_checked: HashSet<(u8, usize)>,
}

/// 変数名と列名が衝突しうる役割（列としても読める位置）。リテラル専用位置は列を受理しない。
fn is_column_capable(role: VarRole) -> bool {
    matches!(
        role,
        VarRole::Value
            | VarRole::WhereHead
            | VarRole::SelectItemBare
            | VarRole::OrderKey
            | VarRole::GroupKey
    )
}

impl<'a, 'e> CorePlHost<'a, 'e> {
    /// 許可リスト解析 → 文種・書き込みのゲート → 既存の実行本体、の順で 1 文を実行する。
    fn run_tokens(&mut self, tokens: Vec<Token>) -> Result<CoreOutcome, SqlSurfaceError> {
        let parsed = {
            // parse 中の定数式畳み込みがセッション TimeZone を読む。
            let _tz = session_time_zone_scope(self.session);
            let resolver = self.core.schema_resolver(self.ctx);
            self.core
                .parse_tokens_gated(tokens, self.session, Some(&resolver))?
        };
        let writes = classify(&parsed)?;
        if parsed_operation_id(&parsed).is_some() || is_partitioned_write(&parsed) {
            return Err(unsupported(
                "USING OPERATION_ID and PARTITIONED are not supported inside a PL/pgSQL body",
            ));
        }
        if writes {
            if !self.opts.may_write {
                return Err(unsupported(
                    "a non-volatile PL/pgSQL function cannot modify data",
                ));
            }
            // 本体の文は operation_id を持たない。台帳必須の構成では書き込みを実行前に拒否する
            // （副作用ゼロ。D8）。
            if self.core.ledger_mode == LedgerMode::Ledgered {
                return Err(SqlSurfaceError::missing_operation_id());
            }
        }
        self.core
            .execute_in_active_txn(self.ctx, self.session, self.txn, &parsed)
    }

    /// `42702`（変数名と列名の衝突。D3）の検出。参照する表（`FROM` 等の関係位置）のスキーマを引き、
    /// 列としても読める位置の未修飾の変数名が、その表の列名（疑似列 `id` を含む）と一致したら拒否する。
    /// 解析器は列の存在を束縛段まで確かめないため、解析の成否では判定できない。検査結果は文ごとに
    /// キャッシュする。CTE・導出表・ビューの列は対象外（既知の差。設計書の追記参照）。
    fn check_conflicts(
        &mut self,
        key: (u8, usize),
        tokens: &[Token],
        sites: &[VarSite],
    ) -> Result<(), SqlSurfaceError> {
        if !self.conflict_checked.insert(key) {
            return Ok(());
        }
        let names: Vec<&str> = sites
            .iter()
            .filter(|s| is_column_capable(s.role))
            .filter_map(|s| match tokens.get(s.pos) {
                Some(Token::Ident(n)) => Some(n.as_str()),
                _ => None,
            })
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let mut work: Vec<Token> = tokens.to_vec();
        {
            let resolver = self.core.schema_resolver(self.ctx);
            // 関係名の解決に失敗する文は、後続の解析が同じ理由で拒否する（ここでは検査を省く）。
            if crate::sql::relation_name::rewrite_relation_names_full(&mut work, Some(&resolver))
                .is_err()
            {
                return Ok(());
            }
        }
        let site_positions: HashSet<usize> = sites.iter().map(|s| s.pos).collect();
        let walk = crate::sql::relation_name::relation_positions(&work);
        for p in walk.positions {
            if site_positions.contains(&p.index) {
                continue;
            }
            let Some(Token::Ident(rel)) = work.get(p.index) else {
                continue;
            };
            let Ok(schema) = self.core.storage.get_table_schema(rel) else {
                continue;
            };
            let clash = names.iter().any(|n| {
                (n.eq_ignore_ascii_case("id") && schema.has_pseudo_id())
                    || schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(n))
            });
            if clash {
                return Err(SqlSurfaceError::ambiguous_column(
                    "(a PL/pgSQL variable conflicts with a column)",
                ));
            }
        }
        Ok(())
    }

    /// `SELECT 1 WHERE <条件>` を実行して行の有無を返す。FROM の無い `SELECT` の `WHERE` は比較 1 個
    /// （または真偽値の式）だけを受理するため、複合条件の構文拒否は `0A000` に写す。
    fn where_probe(&mut self, cond_tokens: Vec<Token>) -> Result<bool, SqlSurfaceError> {
        let mut full = vec![
            Token::Keyword(Keyword::Select),
            Token::Number("1".to_string()),
            Token::Keyword(Keyword::Where),
        ];
        full.extend(cond_tokens);
        match self.run_tokens(full) {
            Ok(CoreOutcome::Query(qr)) => Ok(!qr.rows.is_empty()),
            Ok(_) => Err(internal("PL/pgSQL condition returned an unexpected result")),
            Err(e) if e.wire_code() == "42601" => Err(unsupported(
                "this condition form is not supported in PL/pgSQL (use a single comparison)",
            )),
            Err(e) => Err(e),
        }
    }

    /// オペランド（置換済みのトークン列）の値を、同じ値を表すリテラルのトークン列にする。NULL は `None`。
    /// 単独のリテラルは SQL を経由せずそのまま返し、それ以外は `SELECT <式>` を同じ Tx・RLS で実行して
    /// 1 セルを取る。
    fn operand_value(&mut self, operand: &[Token]) -> Result<Option<Vec<Token>>, SqlSurfaceError> {
        match operand {
            [] => Err(unsupported(
                "this condition form is not supported in PL/pgSQL",
            )),
            [t] if is_word(t, "NULL") => Ok(None),
            [Token::Number(_) | Token::StringLiteral(_)] => Ok(Some(operand.to_vec())),
            [t] if is_word(t, "true") || is_word(t, "false") => Ok(Some(operand.to_vec())),
            _ => {
                let mut full = vec![Token::Keyword(Keyword::Select)];
                full.extend_from_slice(operand);
                match self.run_tokens(full)? {
                    CoreOutcome::Query(qr) => {
                        let v = single_scalar(&qr)?;
                        if v.is_null() {
                            Ok(None)
                        } else {
                            Ok(Some(subst::scalar_tokens(&v, VarRole::Value)?))
                        }
                    }
                    _ => Err(internal("PL/pgSQL condition returned an unexpected result")),
                }
            }
        }
    }

    /// オペランドの値が NULL か（[`Self::operand_value`] の NULL 判定）。
    fn operand_is_null(&mut self, operand: &[Token]) -> Result<bool, SqlSurfaceError> {
        Ok(self.operand_value(operand)?.is_none())
    }

    fn typed_relation_schema(&self, parts: &[String]) -> Result<TableSchema, SqlSurfaceError> {
        // 表が無い場合も他テナントの SCHEMA の表の場合も、区別のない同じ応答にする。
        let not_found = || SqlSurfaceError::undefined_table("relation");
        let rel = match parts {
            [name] => Token::Ident(name.clone()),
            [schema, name] => Token::QualifiedIdent {
                qualifier: schema.clone(),
                name: name.clone(),
            },
            _ => return Err(not_found()),
        };
        let mut toks = vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("x".to_string()),
            Token::Keyword(Keyword::From),
            rel,
        ];
        let resolver = self.core.schema_resolver(self.ctx);
        crate::sql::relation_name::rewrite_relation_names_full(&mut toks, Some(&resolver))
            .map_err(|_| not_found())?;
        let Some(Token::Ident(key)) = toks.get(3) else {
            return Err(not_found());
        };
        self.core
            .storage
            .get_table_schema(key)
            .map_err(|_| not_found())
    }
}

/// 全体を囲む対応した外側の括弧を（入れ子の分まで）外した部分列。
fn strip_outer_parens(mut toks: &[Token]) -> &[Token] {
    loop {
        let (Some(Token::Punct('(')), Some(Token::Punct(')'))) = (toks.first(), toks.last()) else {
            return toks;
        };
        let mut depth = 0usize;
        for (i, t) in toks.iter().enumerate() {
            match t {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth -= 1;
                    // 先頭の括弧が末尾より前で閉じるなら、全体を囲む括弧ではない（`(a) = (b)`）。
                    if depth == 0 && i + 1 != toks.len() {
                        return toks;
                    }
                }
                _ => {}
            }
        }
        match toks.get(1..toks.len() - 1) {
            Some(inner) => toks = inner,
            None => return toks,
        }
    }
}

/// 語（`IS`・`NOT` 等）の一致（大文字小文字を区別しない）。
fn is_word(t: &Token, w: &str) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(w))
}

/// 論理結合・述語など、オペランド単位の NULL 判定では扱わない語を含むか。
fn has_compound_word(toks: &[Token]) -> bool {
    toks.iter().any(|t| {
        matches!(t, Token::Keyword(Keyword::And))
            || ["OR", "NOT", "IS", "BETWEEN", "IN", "LIKE", "ILIKE", "CASE"]
                .iter()
                .any(|w| is_word(t, w))
    })
}

/// 深さ 0 の比較演算子の連なり（`=`・`<`・`>`・`<=`・`>=`・`<>`）が 1 個だけなら、その前後のオペランド。
fn split_single_comparison(toks: &[Token]) -> Option<(&[Token], &[Token], &[Token])> {
    let is_cmp = |t: &Token| matches!(t, Token::Punct('=' | '<' | '>') | Token::Le | Token::Ge);
    let mut depth = 0usize;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.checked_sub(1)?,
            t if depth == 0 && is_cmp(t) => match runs.last_mut() {
                Some(last) if last.1.checked_add(1) == Some(i) => last.1 = i,
                _ => runs.push((i, i)),
            },
            _ => {}
        }
    }
    let [(start, end)] = runs.as_slice() else {
        return None;
    };
    Some((
        toks.get(..*start)?,
        toks.get(*start..=*end)?,
        toks.get(end.checked_add(1)?..)?,
    ))
}

/// 末尾が `IS [NOT] NULL` で、その前が単一のオペランド（論理結合なし）なら、(オペランド, 否定か)。
fn split_is_null(toks: &[Token]) -> Option<(&[Token], bool)> {
    let (last, rest) = toks.split_last()?;
    if !is_word(last, "NULL") {
        return None;
    }
    let (rest, negated) = match rest.split_last() {
        Some((t, r)) if is_word(t, "NOT") => (r, true),
        _ => (rest, false),
    };
    let (is, operand) = rest.split_last()?;
    if !is_word(is, "IS") || operand.is_empty() || has_compound_word(operand) {
        return None;
    }
    split_single_comparison(operand)
        .is_none()
        .then_some((operand, negated))
}

impl PlHost for CorePlHost<'_, '_> {
    fn eval_value(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        _budget: &mut ExecBudget,
    ) -> Result<PlScalar, SqlSurfaceError> {
        if let Some(v) = direct_var(expr, env)? {
            return Ok(v);
        }
        if let Some(v) = bare_literal(expr) {
            return Ok(v);
        }
        let key = (0u8, std::ptr::from_ref(expr) as usize);
        self.check_conflicts(key, &expr.tokens, &expr.var_refs)?;
        if expr.cond {
            let c = self.eval_cond(expr, env, _budget)?;
            return Ok(match c {
                Some(b) => PlScalar::new(Cell::Bool(b), ColumnType::Boolean),
                None => PlScalar::null(ColumnType::Boolean),
            });
        }
        let toks = subst::substitute(&expr.tokens, &expr.var_refs, env, true)?;
        let mut full = vec![Token::Keyword(Keyword::Select)];
        full.extend(toks);
        match self.run_tokens(full) {
            Ok(CoreOutcome::Query(qr)) => single_scalar(&qr),
            Ok(_) => Err(internal(
                "PL/pgSQL expression returned an unexpected result",
            )),
            // 定義時分類が `WHERE` 形で受理した式（比較・論理式）は、条件として評価する。
            Err(e) if e.wire_code() == "42601" => {
                let c = self.eval_cond(expr, env, _budget)?;
                Ok(match c {
                    Some(b) => PlScalar::new(Cell::Bool(b), ColumnType::Boolean),
                    None => PlScalar::null(ColumnType::Boolean),
                })
            }
            Err(e) => Err(e),
        }
    }

    fn eval_cond(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        _budget: &mut ExecBudget,
    ) -> Result<Option<bool>, SqlSurfaceError> {
        if let Some(v) = direct_var(expr, env)? {
            return match v.cell {
                Cell::Bool(b) => Ok(Some(b)),
                Cell::Null => Ok(None),
                _ => Err(SqlSurfaceError::datatype_mismatch(
                    "argument of a condition must be type boolean",
                )),
            };
        }
        let key = (0u8, std::ptr::from_ref(expr) as usize);
        self.check_conflicts(key, &expr.tokens, &expr.var_refs)?;
        let toks = subst::substitute(&expr.tokens, &expr.var_refs, env, false)?;
        // `<式> IS [NOT] NULL` は `FROM` の無い `SELECT .. WHERE` が構文として受理しないため、式の
        // 値を直接求めて判定する（三値論理に乗らず、常に真偽値）。
        if let Some((operand, negated)) = split_is_null(strip_outer_parens(&toks)) {
            let is_null = self.operand_is_null(operand)?;
            return Ok(Some(is_null != negated));
        }
        // 単純比較は、いずれかのオペランドが NULL なら結果は NULL（`where_probe` の「行なし」＝偽に
        // 潰さない）。`a + 1 = 3` のような演算・括弧を含むオペランドも、値を求めて判定する。
        let inner = strip_outer_parens(&toks);
        if !has_compound_word(inner) {
            if let Some((l, op, r)) = split_single_comparison(inner) {
                // 評価できないオペランドはここでは無視し、後続の条件評価が同じエラーを返す。
                if let (Ok(lv), Ok(rv)) = (self.operand_value(l), self.operand_value(r)) {
                    let (Some(mut lt), Some(rt)) = (lv, rv) else {
                        return Ok(None);
                    };
                    // 演算を含むオペランドは評価済みの値のリテラルに置き換えて比較する（`FROM` の無い
                    // `WHERE` は演算を含む式を受理しない）。
                    lt.extend_from_slice(op);
                    lt.extend(rt);
                    return Ok(Some(self.where_probe(lt)?));
                }
            }
        }
        Ok(Some(self.where_probe(toks)?))
    }

    fn values_equal(
        &mut self,
        a: &PlScalar,
        b: &PlScalar,
    ) -> Result<Option<bool>, SqlSurfaceError> {
        if a.is_null() || b.is_null() {
            return Ok(None);
        }
        if let (Cell::Bool(x), Cell::Bool(y)) = (&a.cell, &b.cell) {
            return Ok(Some(x == y));
        }
        let mut toks = subst::scalar_tokens(a, VarRole::Value)?;
        toks.push(Token::Punct('='));
        toks.extend(subst::scalar_tokens(b, VarRole::Value)?);
        Ok(Some(self.where_probe(toks)?))
    }

    fn exec_sql(
        &mut self,
        sql: &EmbeddedSql,
        env: &dyn VarLookup,
        demand: RowDemand,
        _budget: &mut ExecBudget,
    ) -> Result<HostOutcome, SqlSurfaceError> {
        let key = (1u8, std::ptr::from_ref(sql) as usize);
        self.check_conflicts(key, &sql.tokens, &sql.var_refs)?;
        let toks = subst::substitute(&sql.tokens, &sql.var_refs, env, true)?;
        let out = self.run_tokens(toks)?;
        map_outcome(out, demand)
    }

    fn exec_dynamic(
        &mut self,
        query: &str,
        using: &[PlScalar],
        demand: RowDemand,
        _budget: &mut ExecBudget,
    ) -> Result<HostOutcome, SqlSurfaceError> {
        if query.len() > lexer::MAX_INPUT_LEN {
            return Err(SqlSurfaceError::payload_too_large(
                "EXECUTE string is too large",
            ));
        }
        let toks = lexer::tokenize_with_params(query)
            .map_err(|_| SqlSurfaceError::unsupported("syntax error in the EXECUTE string"))?;
        let toks = subst::substitute_params(&toks, using)?;
        let out = self.run_tokens(toks)?;
        map_outcome(out, demand)
    }

    fn resolve_type(&mut self, ty: &DeclType) -> Result<ResolvedType, SqlSurfaceError> {
        match ty {
            DeclType::Named(_) => Err(internal(
                "named PL/pgSQL type is resolved by the interpreter",
            )),
            DeclType::Record => Ok(ResolvedType::Record(None)),
            DeclType::PercentType { path } => {
                let Some((col, rel)) = path.split_last() else {
                    return Err(unsupported("unsupported %TYPE reference in PL/pgSQL"));
                };
                // `x%TYPE`（変数の型）は初版の対象外。`表.列%TYPE`・`スキーマ.表.列%TYPE` のみ。
                if rel.is_empty() || rel.len() > 2 {
                    return Err(unsupported("unsupported %TYPE reference in PL/pgSQL"));
                }
                let schema = self.typed_relation_schema(rel)?;
                // 可視の `id` 疑似列は `schema.columns` に無いが `SELECT id` では BigInt として読める。
                if schema.has_pseudo_id() && col.eq_ignore_ascii_case("id") {
                    return Ok(ResolvedType::Scalar(ColumnType::BigInt));
                }
                schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(col))
                    .map(|c| ResolvedType::Scalar(c.ty.clone()))
                    .ok_or_else(|| SqlSurfaceError::undefined_column("column"))
            }
            DeclType::PercentRowType { path } => {
                let schema = self.typed_relation_schema(path)?;
                // `SELECT *` は可視の `id` 疑似列を先頭に返す（旧式・移行済みの表）ため、行型にも含める。
                let mut fields: Vec<(String, ColumnType)> = Vec::new();
                if schema.has_pseudo_id() {
                    fields.push(("id".to_string(), ColumnType::BigInt));
                }
                fields.extend(
                    schema
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.ty.clone())),
                );
                Ok(ResolvedType::Record(Some(fields)))
            }
        }
    }
}

impl EngineCore {
    /// 既に開いている Tx（明示・暗黙）の上で PL/pgSQL の本体を実行する（Issue #2226・ADR D5）。
    /// `txn` が `Idle` なら `Internal`（呼び出し側の不変条件違反。fail-closed）、`Failed` なら `25P02`。
    /// 本体のエラーでは Tx を `Failed` にする（暗黙 Tx は破棄）。エラーは出口で内部キー
    /// （`#<id>.<name>`）を利用者向けの名前へ戻す。再入の不変条件は本モジュールの doc を参照。
    pub(crate) fn run_plpgsql_in_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        call: PlInvocation<'_>,
    ) -> Result<CallOutcome, SqlSurfaceError> {
        Self::check_resolution_tenant(session, ctx)?;
        match txn.status() {
            TransactionStatus::Idle => {
                return Err(internal("PL/pgSQL body requires an active transaction"))
            }
            TransactionStatus::Failed => return Err(txn.take_failed_error()),
            TransactionStatus::InTransaction => {}
        }
        let r = self.run_plpgsql_active(ctx, session, txn, call);
        if r.is_err() {
            txn.fail();
        }
        r.map_err(|e| self.scrub_error(ctx, e))
    }

    /// `txn` が `Idle` なら暗黙 Tx を開いて本体を実行し、成功で commit・失敗で abort する（すべての
    /// 経路で後始末する）。開いている Tx があればそこへ参加する（[`Self::run_plpgsql_in_txn`]）。
    pub(crate) fn run_plpgsql_autocommit<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        call: PlInvocation<'_>,
    ) -> Result<CallOutcome, SqlSurfaceError> {
        if txn.status() != TransactionStatus::Idle {
            return self.run_plpgsql_in_txn(ctx, session, txn, call);
        }
        Self::check_resolution_tenant(session, ctx)?;
        txn.begin_implicit(&self.storage, session)
            .map_err(|e| self.scrub_error(ctx, e))?;
        match self.run_plpgsql_active(ctx, session, txn, call) {
            Ok(out) => {
                txn.commit_implicit(session)
                    .map_err(|e| self.scrub_error(ctx, e))?;
                Ok(out)
            }
            Err(e) => {
                txn.abort_implicit();
                Err(self.scrub_error(ctx, e))
            }
        }
    }

    /// Active な Tx の上で本体を実行する（状態遷移は呼び出し側が行う）。
    fn run_plpgsql_active<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        call: PlInvocation<'_>,
    ) -> Result<CallOutcome, SqlSurfaceError> {
        let PlInvocation {
            body,
            args,
            ret,
            opts,
        } = call;
        let mut budget = ExecBudget::new(self.plpgsql_limits, self.result_limits.max_result_rows());
        let mut host = CorePlHost {
            core: self,
            ctx,
            session,
            txn,
            opts,
            conflict_checked: HashSet::new(),
        };
        run_function(body, args, ret, &mut host, &mut budget)
    }
}
