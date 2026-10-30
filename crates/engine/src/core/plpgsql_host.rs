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

/// `PlHost` の core 実装。1 回の起動の間だけ生きる（`'e` は `EngineCore` と Tx の寿命）。
struct CorePlHost<'a, 'e> {
    core: &'e EngineCore,
    ctx: &'a PolicyContext,
    session: &'a mut SessionState,
    txn: &'a mut SessionTransaction<'e>,
    opts: PlCallOptions,
    /// `42702` の検査済みの文・式（アドレスをキーにする。本体の AST は起動の間不変で、ループで検査を
    /// 繰り返さない）。値は読み出さないので、アドレスの再利用で誤って参照されることはない。
    conflict_checked: HashSet<usize>,
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
        key: usize,
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
                n.eq_ignore_ascii_case("id")
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
        let key = std::ptr::from_ref(expr) as usize;
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
        let key = std::ptr::from_ref(expr) as usize;
        self.check_conflicts(key, &expr.tokens, &expr.var_refs)?;
        let toks = subst::substitute(&expr.tokens, &expr.var_refs, env, false)?;
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
        let key = std::ptr::from_ref(sql) as usize;
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
                schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(col))
                    .map(|c| ResolvedType::Scalar(c.ty.clone()))
                    .ok_or_else(|| SqlSurfaceError::undefined_column("column"))
            }
            DeclType::PercentRowType { path } => {
                let schema = self.typed_relation_schema(path)?;
                Ok(ResolvedType::Record(Some(
                    schema
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.ty.clone()))
                        .collect(),
                )))
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
