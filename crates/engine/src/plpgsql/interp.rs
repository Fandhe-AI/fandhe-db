//! PL/pgSQL の木構造インタプリタ（Issue #2225・SQL-68。設計は `docs/design/plpgsql-subset.md`
//! D4・D7・D10）。
//!
//! `plpgsql::parse_body` が作った AST（[`Body`]）を辿り、変数環境・制御フロー・代入時の型変換・
//! `INTO`／`STRICT` の行数規則・`FOUND`／`ROW_COUNT`・集合返却の蓄積を担う。式の評価と埋め込み SQL
//! の実行は [`PlHost`] へ委ね（第 2 の SQL 実行器を作らない）、ここは SQL 文字列を組み立てない。
//! 呼び出し元は #2226（core の `PlHost` 実装）・#2228（`DO`・トリガ関数）・#2217（関数定義の結線）
//! で、#2225 の時点ではテスト専用ホストだけ。ストレージ・カタログ・テナントには触れない。
//!
//! 資源上限は [`ExecBudget`] が持つ（文数・ループ反復・呼び出し深さ・集合返却の累積行数と
//! バイト数）。検査はすべて**実行前・追加前**に行い、毎文・毎反復で協調キャンセル
//! （`interrupt::check`＝`57014`）を確認する。超過・エラー時は部分結果を返さず `Err` だけを返す
//! （fail-closed）。インタプリタ自身は commit しないため、確認点が commit の後ろに回ることはない。
//! エラー文言は固定の英語で、変数名・値・本体の断片を含めない。

use crate::catalog::ColumnType;
use crate::plpgsql::ast::{
    AssignTarget, Block, Body, DeclType, DiagItem, EmbeddedKind, EmbeddedSql, IntoClause,
    ReturnShape, SqlExpr, Stmt, VarId, VarKind, VarRef,
};
use crate::plpgsql::host::{
    PlHost, PlRecord, PlScalar, ResolvedType, RowDemand, SqlOutcome, VarLookup,
};
use crate::plpgsql::value::assign_cast;
use crate::plpgsql_limits::PlpgsqlLimits;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::CastType;
use crate::sql::exec::Cell;

/// 集合返却の累積バイト数の上限（`sql::returning::MAX_RETURNING_RESULT_BYTES` と同値の arena 上限）。
const MAX_SET_RESULT_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// 1 回のトップレベル起動ごとに 1 個作り、入れ子の呼び出しへ `&mut` で渡して合算する実行予算。
#[derive(Debug)]
pub(crate) struct ExecBudget {
    limits: PlpgsqlLimits,
    max_set_rows: usize,
    statements: usize,
    iterations: usize,
    depth: usize,
    set_rows: usize,
    set_bytes: usize,
}

impl ExecBudget {
    /// `max_set_rows` は集合返却の累積行数の上限（`--max-result-rows` の値）。
    pub(crate) fn new(limits: PlpgsqlLimits, max_set_rows: usize) -> Self {
        Self {
            limits,
            max_set_rows,
            statements: 0,
            iterations: 0,
            depth: 0,
            set_rows: 0,
            set_bytes: 0,
        }
    }

    /// 文を 1 つ実行する前に呼ぶ。上限ちょうどの文数までは実行でき、次の文で `54000`。
    pub(crate) fn tick_statement(&mut self) -> Result<(), SqlSurfaceError> {
        if self.statements >= self.limits.max_statements() {
            return Err(SqlSurfaceError::payload_too_large(
                "PL/pgSQL statement limit exceeded",
            ));
        }
        self.statements = self.statements.saturating_add(1);
        crate::interrupt::check()
    }

    /// ループの各反復の頭で呼ぶ。
    pub(crate) fn tick_iteration(&mut self) -> Result<(), SqlSurfaceError> {
        if self.iterations >= self.limits.max_loop_iterations() {
            return Err(SqlSurfaceError::payload_too_large(
                "PL/pgSQL loop iteration limit exceeded",
            ));
        }
        self.iterations = self.iterations.saturating_add(1);
        crate::interrupt::check()
    }

    /// 関数に入る時に呼ぶ（超過は `54001`）。出口では必ず [`Self::leave_call`] を呼ぶ。
    pub(crate) fn enter_call(&mut self) -> Result<(), SqlSurfaceError> {
        if self.depth >= self.limits.max_call_depth() {
            return Err(SqlSurfaceError::StatementTooComplex {
                detail: "PL/pgSQL function call depth limit exceeded",
            });
        }
        self.depth = self.depth.saturating_add(1);
        Ok(())
    }

    pub(crate) fn leave_call(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// 集合返却に 1 行（概算 `bytes` バイト）を足す前に呼ぶ（追加前検査）。
    pub(crate) fn add_set_row(&mut self, bytes: usize) -> Result<(), SqlSurfaceError> {
        let next_bytes = self.set_bytes.checked_add(bytes);
        match next_bytes {
            Some(n) if self.set_rows < self.max_set_rows && n <= MAX_SET_RESULT_BYTES => {
                self.set_rows = self.set_rows.saturating_add(1);
                self.set_bytes = n;
                Ok(())
            }
            _ => Err(SqlSurfaceError::payload_too_large(
                "PL/pgSQL set-returning result limit exceeded",
            )),
        }
    }
}

/// 呼び出しの戻り値の形と型（`SETOF` で `ty` が `None` なら行型）。
#[derive(Debug, Clone)]
pub(crate) struct ReturnSpec {
    pub(crate) shape: ReturnShape,
    pub(crate) ty: Option<ColumnType>,
}

/// 呼び出しの結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallOutcome {
    Void,
    Scalar(PlScalar),
    Rows(Vec<Vec<PlScalar>>),
}

/// 制御フローの結果。
#[derive(Debug, Clone, PartialEq)]
enum Flow {
    Normal,
    Exit(Option<String>),
    Continue(Option<String>),
    Return,
}

/// ループ 1 周の後始末の判断。
enum LoopCtl {
    Next,
    Break,
    Propagate(Flow),
}

#[derive(Debug, Clone, PartialEq)]
enum SlotTy {
    Unset,
    Scalar(ColumnType),
    /// `None` は動的な `RECORD`（代入時に形が決まる）、`Some` は `%ROWTYPE` の固定行型
    /// （宣言時のフィールド名と型を保ち、代入時にも宣言型を適用する）。
    Record(Option<Vec<(String, ColumnType)>>),
}

#[derive(Debug, Clone, PartialEq)]
enum SlotVal {
    Unset,
    Scalar(PlScalar),
    Record(PlRecord),
}

#[derive(Debug, Clone)]
struct Slot {
    ty: SlotTy,
    val: SlotVal,
    not_null: bool,
}

/// 変数環境（[`VarId`] の添字で引く。`[]` は使わない）。
struct VarEnv {
    slots: Vec<Slot>,
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

fn null_not_allowed() -> SqlSurfaceError {
    SqlSurfaceError::NullValueNotAllowed {
        detail: "null value cannot be assigned to variable declared NOT NULL",
    }
}

impl VarEnv {
    fn slot(&self, id: VarId) -> Result<&Slot, SqlSurfaceError> {
        self.slots
            .get(id.0 as usize)
            .ok_or_else(|| internal("PL/pgSQL variable id out of range"))
    }

    fn slot_mut(&mut self, id: VarId) -> Result<&mut Slot, SqlSurfaceError> {
        self.slots
            .get_mut(id.0 as usize)
            .ok_or_else(|| internal("PL/pgSQL variable id out of range"))
    }
}

impl VarLookup for VarEnv {
    fn scalar(&self, r: &VarRef) -> Result<&PlScalar, SqlSurfaceError> {
        match r {
            VarRef::Var(id) => match &self.slot(*id)?.val {
                SlotVal::Scalar(s) => Ok(s),
                _ => Err(internal("PL/pgSQL variable is not a scalar")),
            },
            VarRef::Field(id, name) => match &self.slot(*id)?.val {
                SlotVal::Record(rec) => {
                    let fields = rec
                        .fields
                        .as_ref()
                        .ok_or(SqlSurfaceError::RecordNotAssigned)?;
                    fields
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v)
                        .ok_or_else(|| SqlSurfaceError::UndefinedColumn {
                            name: "record field".to_string(),
                        })
                }
                _ => Err(internal("PL/pgSQL variable is not a record")),
            },
        }
    }
}

/// 関数 1 回分の実行状態。
struct Interp<'a> {
    body: &'a Body,
    env: VarEnv,
    host: &'a mut dyn PlHost,
    budget: &'a mut ExecBudget,
    ret: &'a ReturnSpec,
    found: Option<VarId>,
    row_count: u64,
    result: Option<PlScalar>,
    rows: Vec<Vec<PlScalar>>,
}

/// 関数本体を実行する。トップレベルの起動は呼び出し側が [`ExecBudget`] を作って渡し、入れ子の
/// 呼び出し（ホストの式評価から）は同じ予算を渡して合算する。`Trigger` は `0A000`（#2228）。
pub(crate) fn run_function(
    body: &Body,
    args: Vec<PlScalar>,
    ret: &ReturnSpec,
    host: &mut dyn PlHost,
    budget: &mut ExecBudget,
) -> Result<CallOutcome, SqlSurfaceError> {
    budget.enter_call()?;
    let r = run_inner(body, args, ret, host, budget);
    budget.leave_call();
    r
}

fn run_inner(
    body: &Body,
    args: Vec<PlScalar>,
    ret: &ReturnSpec,
    host: &mut dyn PlHost,
    budget: &mut ExecBudget,
) -> Result<CallOutcome, SqlSurfaceError> {
    if ret.shape == ReturnShape::Trigger {
        return Err(unsupported(
            "trigger functions cannot be called as ordinary functions",
        ));
    }
    let blank = Slot {
        ty: SlotTy::Unset,
        val: SlotVal::Unset,
        not_null: false,
    };
    let mut it = Interp {
        body,
        env: VarEnv {
            slots: vec![blank; body.vars.len()],
        },
        host,
        budget,
        ret,
        found: None,
        row_count: 0,
        result: None,
        rows: Vec::new(),
    };
    it.bind_fixed_vars(args)?;
    let flow = it.exec_block(&body.block)?;
    match flow {
        Flow::Normal | Flow::Return => {}
        Flow::Exit(_) | Flow::Continue(_) => {
            return Err(internal("PL/pgSQL control flow escaped the function body"))
        }
    }
    match ret.shape {
        ReturnShape::Void => Ok(CallOutcome::Void),
        ReturnShape::SetOf => Ok(CallOutcome::Rows(it.rows)),
        ReturnShape::Scalar => match (flow, it.result) {
            (Flow::Return, Some(v)) => Ok(CallOutcome::Scalar(v)),
            (Flow::Return, None) => Err(internal("PL/pgSQL scalar RETURN without a value")),
            _ => Err(SqlSurfaceError::FunctionExecutedNoReturnStatement),
        },
        ReturnShape::Trigger => Err(unsupported(
            "trigger functions cannot be called as ordinary functions",
        )),
    }
}

/// 概算バイト数（集合返却の累積上限の検査に使う）。
fn scalar_bytes(v: &PlScalar) -> usize {
    let payload = match &v.cell {
        Cell::Text(s) | Cell::Json(s) => s.len(),
        Cell::Bytes(b) => b.len(),
        Cell::Vector(x) => x.len().saturating_mul(4),
        Cell::Array(a) => a.approx_heap_bytes().saturating_add(a.dims_header_len()),
        _ => 0,
    };
    payload.saturating_add(32)
}

impl<'a> Interp<'a> {
    // ---- 変数 ----

    /// 引数・`FOUND`・ループ変数の暗黙宣言を初期化する（`DECLARE` 変数はブロックに入る時）。
    fn bind_fixed_vars(&mut self, args: Vec<PlScalar>) -> Result<(), SqlSurfaceError> {
        let body = self.body;
        for (i, decl) in body.vars.iter().enumerate() {
            let id = VarId(u32::try_from(i).map_err(|_| internal("too many PL/pgSQL variables"))?);
            match decl.kind {
                VarKind::Param(n) => {
                    let ty = self.declared_type(&decl.ty)?;
                    let arg = args
                        .get(usize::from(n).saturating_sub(1))
                        .cloned()
                        .ok_or_else(|| internal("PL/pgSQL argument count mismatch"))?;
                    self.set_slot(id, ty, Some(arg), decl.not_null)?;
                }
                VarKind::Found => {
                    self.found = Some(id);
                    let v = PlScalar::new(Cell::Bool(false), ColumnType::Boolean);
                    self.set_slot(id, SlotTy::Scalar(ColumnType::Boolean), Some(v), false)?;
                }
                VarKind::LoopInt => {
                    self.set_slot(id, SlotTy::Scalar(ColumnType::Integer), None, false)?;
                }
                VarKind::LoopRecord => {
                    self.set_slot(id, SlotTy::Record(None), None, false)?;
                }
                VarKind::Local => {}
            }
        }
        Ok(())
    }

    /// 宣言型を実行時の型へ。`Named` は自前で、`%TYPE` 等はホストに解決させる。
    fn declared_type(&mut self, ty: &DeclType) -> Result<SlotTy, SqlSurfaceError> {
        match ty {
            DeclType::Named(n) => {
                let ct = CastType::from_sql_type_name(n)
                    .map_err(|_| unsupported("unsupported variable type in PL/pgSQL"))?;
                ct.result_column_type()
                    .map(SlotTy::Scalar)
                    .ok_or_else(|| unsupported("unsupported variable type in PL/pgSQL"))
            }
            other => Ok(match self.host.resolve_type(other)? {
                ResolvedType::Scalar(t) => SlotTy::Scalar(t),
                ResolvedType::Record(fields) => SlotTy::Record(fields),
            }),
        }
    }

    /// スロットを初期化する。スカラーは `init`（無ければ NULL）を宣言型へ変換して入れる。
    fn set_slot(
        &mut self,
        id: VarId,
        ty: SlotTy,
        init: Option<PlScalar>,
        not_null: bool,
    ) -> Result<(), SqlSurfaceError> {
        let val = match &ty {
            SlotTy::Scalar(t) => {
                let v = match init {
                    Some(v) => assign_cast(v, t)?,
                    None => PlScalar::null(t.clone()),
                };
                SlotVal::Scalar(v)
            }
            SlotTy::Record(layout) => {
                // PostgreSQL と同じく、行／レコード変数の既定値は受け付けない（黙って捨てない）。
                if init.is_some() {
                    return Err(unsupported(
                        "default value for a record variable is not supported",
                    ));
                }
                match layout {
                    // 固定行型は宣言済みの型付き NULL フィールドで初期化する。
                    Some(cols) => SlotVal::Record(PlRecord {
                        fields: Some(
                            cols.iter()
                                .map(|(n, t)| (n.clone(), PlScalar::null(t.clone())))
                                .collect(),
                        ),
                    }),
                    None => SlotVal::Record(PlRecord::default()),
                }
            }
            SlotTy::Unset => SlotVal::Unset,
        };
        *self.env.slot_mut(id)? = Slot { ty, val, not_null };
        Ok(())
    }

    /// `DECLARE` 変数を入るたびに初期化する（既定値の評価は宣言順）。
    fn init_decl(&mut self, id: VarId) -> Result<(), SqlSurfaceError> {
        let body = self.body;
        let decl = body
            .vars
            .get(id.0 as usize)
            .ok_or_else(|| internal("PL/pgSQL variable id out of range"))?;
        let ty = self.declared_type(&decl.ty)?;
        let init = match &decl.default {
            Some(e) => Some(self.eval(e)?),
            None => None,
        };
        let is_scalar = matches!(ty, SlotTy::Scalar(_));
        self.set_slot(id, ty, init, decl.not_null)?;
        if decl.not_null && is_scalar {
            if let SlotVal::Scalar(v) = &self.env.slot(id)?.val {
                if v.is_null() {
                    return Err(null_not_allowed());
                }
            }
        }
        Ok(())
    }

    fn store_scalar(&mut self, id: VarId, v: PlScalar) -> Result<(), SqlSurfaceError> {
        let slot = self.env.slot_mut(id)?;
        let SlotTy::Scalar(ty) = &slot.ty else {
            return Err(unsupported(
                "assignment of a scalar to a record variable is not supported",
            ));
        };
        let v = assign_cast(v, ty)?;
        if slot.not_null && v.is_null() {
            return Err(null_not_allowed());
        }
        slot.val = SlotVal::Scalar(v);
        Ok(())
    }

    fn assign_target(&mut self, t: &AssignTarget, v: PlScalar) -> Result<(), SqlSurfaceError> {
        match t {
            AssignTarget::Var(id) => self.store_scalar(*id, v),
            AssignTarget::Field(id, name) => {
                let slot = self.env.slot_mut(*id)?;
                let SlotVal::Record(rec) = &mut slot.val else {
                    return Err(internal("PL/pgSQL variable is not a record"));
                };
                let fields = rec
                    .fields
                    .as_mut()
                    .ok_or(SqlSurfaceError::RecordNotAssigned)?;
                let f = fields.iter_mut().find(|(n, _)| n == name).ok_or_else(|| {
                    SqlSurfaceError::UndefinedColumn {
                        name: "record field".to_string(),
                    }
                })?;
                f.1 = assign_cast(v, &f.1.ty)?;
                Ok(())
            }
        }
    }

    /// 問い合わせの 1 行（無ければ NULL 行）を `INTO`／`FOR` の対象へ入れる。
    fn assign_row(
        &mut self,
        targets: &[AssignTarget],
        columns: &[(String, ColumnType)],
        row: Option<&Vec<Cell>>,
    ) -> Result<(), SqlSurfaceError> {
        if let [AssignTarget::Var(id)] = targets {
            if let SlotTy::Record(layout) = &self.env.slot(*id)?.ty {
                let layout = layout.clone();
                let mut fields = Vec::with_capacity(columns.len());
                for (i, (name, ty)) in columns.iter().enumerate() {
                    let cell = row.and_then(|r| r.get(i)).cloned().unwrap_or(Cell::Null);
                    let v = PlScalar::new(cell, ty.clone());
                    match &layout {
                        // 固定行型は位置対応で宣言型へ代入変換し、宣言のフィールド名を保つ。
                        Some(cols) => {
                            let (dn, dt) = cols.get(i).ok_or_else(|| {
                                SqlSurfaceError::datatype_mismatch(
                                    "number of returned columns does not match the row type",
                                )
                            })?;
                            fields.push((dn.clone(), assign_cast(v, dt)?));
                        }
                        None => fields.push((name.clone(), v)),
                    }
                }
                if layout.as_ref().is_some_and(|c| c.len() != columns.len()) {
                    return Err(SqlSurfaceError::datatype_mismatch(
                        "number of returned columns does not match the row type",
                    ));
                }
                self.env.slot_mut(*id)?.val = SlotVal::Record(PlRecord {
                    fields: Some(fields),
                });
                return Ok(());
            }
        }
        for (i, t) in targets.iter().enumerate() {
            let v = match (columns.get(i), row.and_then(|r| r.get(i))) {
                (Some((_, ty)), Some(cell)) => PlScalar::new(cell.clone(), ty.clone()),
                (Some((_, ty)), None) => PlScalar::null(ty.clone()),
                // 列が足りない対象は NULL（型は対象の宣言型へ代入時に付く）。
                (None, _) => PlScalar::null(ColumnType::Text),
            };
            self.assign_target(t, v)?;
        }
        Ok(())
    }

    fn set_found(&mut self, b: bool) -> Result<(), SqlSurfaceError> {
        let Some(id) = self.found else {
            return Ok(());
        };
        self.store_scalar(id, PlScalar::new(Cell::Bool(b), ColumnType::Boolean))
    }

    // ---- ホストへの委譲 ----

    fn eval(&mut self, e: &SqlExpr) -> Result<PlScalar, SqlSurfaceError> {
        self.host.eval_value(e, &self.env, &mut *self.budget)
    }

    fn cond(&mut self, e: &SqlExpr) -> Result<Option<bool>, SqlSurfaceError> {
        self.host.eval_cond(e, &self.env, &mut *self.budget)
    }

    fn run_sql(
        &mut self,
        sql: &EmbeddedSql,
        demand: RowDemand,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.host
            .exec_sql(sql, &self.env, demand, &mut *self.budget)
    }

    // ---- 文 ----

    fn exec_stmts(&mut self, stmts: &[Stmt]) -> Result<Flow, SqlSurfaceError> {
        for s in stmts {
            let f = self.exec_stmt(s)?;
            if f != Flow::Normal {
                return Ok(f);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_block(&mut self, b: &Block) -> Result<Flow, SqlSurfaceError> {
        for id in &b.decls {
            self.init_decl(*id)?;
        }
        let f = self.exec_stmts(&b.stmts)?;
        Ok(match f {
            Flow::Exit(Some(l)) if b.label.as_ref() == Some(&l) => Flow::Normal,
            other => other,
        })
    }

    fn exec_stmt(&mut self, s: &Stmt) -> Result<Flow, SqlSurfaceError> {
        self.budget.tick_statement()?;
        match s {
            Stmt::Null => Ok(Flow::Normal),
            Stmt::Block(b) => self.exec_block(b),
            Stmt::Assign { target, expr } => self.exec_assign(target, expr),
            Stmt::If { branches, else_ } => self.exec_if(branches, else_.as_deref()),
            Stmt::Case {
                selector,
                whens,
                else_,
            } => self.exec_case(selector.as_ref(), whens, else_.as_deref()),
            Stmt::Loop { label, body } => self.exec_loop(label, body),
            Stmt::While { label, cond, body } => self.exec_while(label, cond, body),
            Stmt::ForInt {
                label,
                var,
                reverse,
                lower,
                upper,
                step,
                body,
            } => self.exec_for_int(label, *var, *reverse, lower, upper, step.as_ref(), body),
            Stmt::ForQuery {
                label,
                targets,
                query,
                body,
            } => self.exec_for_query(label, targets, query, body),
            Stmt::Exit { label, when } => self.exec_jump(label, when.as_ref(), true),
            Stmt::Continue { label, when } => self.exec_jump(label, when.as_ref(), false),
            Stmt::Return(e) => self.exec_return(e.as_ref()),
            Stmt::ReturnNext(e) => self.exec_return_next(e.as_ref()),
            Stmt::ReturnQuery(q) => self.exec_return_query(q),
            Stmt::Perform(q) => self.exec_perform(q),
            Stmt::Sql(q) => self.exec_sql_stmt(q),
            Stmt::Execute { query, into, using } => self.exec_execute(query, into.as_ref(), using),
            Stmt::GetDiagnostics { items } => self.exec_diag(items),
        }
    }

    fn exec_assign(
        &mut self,
        target: &AssignTarget,
        expr: &SqlExpr,
    ) -> Result<Flow, SqlSurfaceError> {
        let v = self.eval(expr)?;
        self.assign_target(target, v)?;
        Ok(Flow::Normal)
    }

    fn exec_if(
        &mut self,
        branches: &[(SqlExpr, Vec<Stmt>)],
        else_: Option<&[Stmt]>,
    ) -> Result<Flow, SqlSurfaceError> {
        for (c, stmts) in branches {
            if self.cond(c)? == Some(true) {
                return self.exec_stmts(stmts);
            }
        }
        match else_ {
            Some(stmts) => self.exec_stmts(stmts),
            None => Ok(Flow::Normal),
        }
    }

    fn exec_case(
        &mut self,
        selector: Option<&SqlExpr>,
        whens: &[(Vec<SqlExpr>, Vec<Stmt>)],
        else_: Option<&[Stmt]>,
    ) -> Result<Flow, SqlSurfaceError> {
        // 単純形はセレクタを 1 回だけ評価する。
        let sel = match selector {
            Some(e) => Some(self.eval(e)?),
            None => None,
        };
        for (exprs, stmts) in whens {
            for e in exprs {
                let hit = match &sel {
                    Some(s) => {
                        let v = self.eval(e)?;
                        self.host.values_equal(s, &v)? == Some(true)
                    }
                    None => self.cond(e)? == Some(true),
                };
                if hit {
                    return self.exec_stmts(stmts);
                }
            }
        }
        match else_ {
            Some(stmts) => self.exec_stmts(stmts),
            None => Err(SqlSurfaceError::CaseNotFound),
        }
    }

    /// ループ本体 1 周の結果を、ループのラベルに照らして次の動作へ変える。
    fn loop_ctl(label: &Option<String>, f: Flow) -> LoopCtl {
        match f {
            Flow::Normal | Flow::Continue(None) => LoopCtl::Next,
            Flow::Exit(None) => LoopCtl::Break,
            Flow::Exit(Some(l)) if label.as_ref() == Some(&l) => LoopCtl::Break,
            Flow::Continue(Some(l)) if label.as_ref() == Some(&l) => LoopCtl::Next,
            other => LoopCtl::Propagate(other),
        }
    }

    fn exec_loop(
        &mut self,
        label: &Option<String>,
        body: &[Stmt],
    ) -> Result<Flow, SqlSurfaceError> {
        loop {
            self.budget.tick_iteration()?;
            let f = self.exec_stmts(body)?;
            match Self::loop_ctl(label, f) {
                LoopCtl::Next => {}
                LoopCtl::Break => return Ok(Flow::Normal),
                LoopCtl::Propagate(f) => return Ok(f),
            }
        }
    }

    fn exec_while(
        &mut self,
        label: &Option<String>,
        cond: &SqlExpr,
        body: &[Stmt],
    ) -> Result<Flow, SqlSurfaceError> {
        // 条件が NULL（または偽）ならループを終える。
        while self.cond(cond)? == Some(true) {
            self.budget.tick_iteration()?;
            let f = self.exec_stmts(body)?;
            match Self::loop_ctl(label, f) {
                LoopCtl::Next => {}
                LoopCtl::Break => return Ok(Flow::Normal),
                LoopCtl::Propagate(f) => return Ok(f),
            }
        }
        Ok(Flow::Normal)
    }

    /// 整数 `FOR` の境界・刻みを int4 として 1 回だけ評価する（NULL は `22004`）。
    fn eval_int4(&mut self, e: &SqlExpr) -> Result<i64, SqlSurfaceError> {
        let v = assign_cast(self.eval(e)?, &ColumnType::Integer)?;
        match v.cell {
            Cell::SignedInteger(i) => Ok(i),
            Cell::Null => Err(SqlSurfaceError::NullValueNotAllowed {
                detail: "upper, lower and BY bounds of a FOR loop cannot be null",
            }),
            _ => Err(internal("PL/pgSQL integer loop bound is not an integer")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for_int(
        &mut self,
        label: &Option<String>,
        var: VarId,
        reverse: bool,
        lower: &SqlExpr,
        upper: &SqlExpr,
        step: Option<&SqlExpr>,
        body: &[Stmt],
    ) -> Result<Flow, SqlSurfaceError> {
        let lo = self.eval_int4(lower)?;
        let hi = self.eval_int4(upper)?;
        let by = match step {
            Some(e) => self.eval_int4(e)?,
            None => 1,
        };
        if by <= 0 {
            return Err(SqlSurfaceError::InvalidParameterValue {
                detail: "BY value of FOR loop must be greater than zero",
            });
        }
        // `REVERSE` は書いた順（大..小）に評価し、先頭の式から下端へ向かう（PostgreSQL と同じ。
        // 小..大の順に書くと 0 回）。
        let mut cur = lo;
        let mut iterated = false;
        loop {
            let in_range = if reverse { cur >= hi } else { cur <= hi };
            if !in_range {
                break;
            }
            self.budget.tick_iteration()?;
            iterated = true;
            self.store_scalar(
                var,
                PlScalar::new(Cell::SignedInteger(cur), ColumnType::Integer),
            )?;
            let f = self.exec_stmts(body)?;
            match Self::loop_ctl(label, f) {
                LoopCtl::Next => {}
                LoopCtl::Break => {
                    self.set_found(iterated)?;
                    return Ok(Flow::Normal);
                }
                LoopCtl::Propagate(f) => return Ok(f),
            }
            // int4 の範囲を溢れる次の値で終える（オーバーフローさせない）。
            let next = if reverse {
                cur.checked_sub(by)
            } else {
                cur.checked_add(by)
            };
            match next {
                Some(n) if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&n) => cur = n,
                _ => break,
            }
        }
        self.set_found(iterated)?;
        Ok(Flow::Normal)
    }

    fn exec_for_query(
        &mut self,
        label: &Option<String>,
        targets: &[AssignTarget],
        query: &EmbeddedSql,
        body: &[Stmt],
    ) -> Result<Flow, SqlSurfaceError> {
        // 行は INSENSITIVE に実体化してから回す（ループ本体の書き込みを見ない）。
        let out = self.run_sql(query, RowDemand::All)?;
        let mut iterated = false;
        for row in &out.rows {
            self.budget.tick_iteration()?;
            iterated = true;
            self.assign_row(targets, &out.columns, Some(row))?;
            let f = self.exec_stmts(body)?;
            match Self::loop_ctl(label, f) {
                LoopCtl::Next => {}
                LoopCtl::Break => {
                    self.set_found(iterated)?;
                    return Ok(Flow::Normal);
                }
                LoopCtl::Propagate(f) => return Ok(f),
            }
        }
        self.set_found(iterated)?;
        Ok(Flow::Normal)
    }

    fn exec_jump(
        &mut self,
        label: &Option<String>,
        when: Option<&SqlExpr>,
        exit: bool,
    ) -> Result<Flow, SqlSurfaceError> {
        if let Some(w) = when {
            if self.cond(w)? != Some(true) {
                return Ok(Flow::Normal);
            }
        }
        Ok(if exit {
            Flow::Exit(label.clone())
        } else {
            Flow::Continue(label.clone())
        })
    }

    fn exec_return(&mut self, e: Option<&SqlExpr>) -> Result<Flow, SqlSurfaceError> {
        match self.ret.shape {
            ReturnShape::Scalar => {
                let e = e.ok_or_else(|| internal("PL/pgSQL scalar RETURN without a value"))?;
                let v = self.eval(e)?;
                let v = match &self.ret.ty {
                    Some(t) => assign_cast(v, t)?,
                    None => v,
                };
                self.result = Some(v);
            }
            ReturnShape::Void | ReturnShape::SetOf | ReturnShape::Trigger => {}
        }
        Ok(Flow::Return)
    }

    fn push_row(&mut self, row: Vec<PlScalar>) -> Result<(), SqlSurfaceError> {
        let bytes = row.iter().map(scalar_bytes).sum::<usize>();
        self.budget.add_set_row(bytes)?;
        self.rows.push(row);
        Ok(())
    }

    fn exec_return_next(&mut self, e: Option<&SqlExpr>) -> Result<Flow, SqlSurfaceError> {
        let e = e.ok_or_else(|| internal("PL/pgSQL RETURN NEXT without a value"))?;
        let v = self.eval(e)?;
        let v = match &self.ret.ty {
            Some(t) => assign_cast(v, t)?,
            None => v,
        };
        self.push_row(vec![v])?;
        Ok(Flow::Normal)
    }

    fn exec_return_query(&mut self, q: &EmbeddedSql) -> Result<Flow, SqlSurfaceError> {
        let out = self.run_sql(q, RowDemand::All)?;
        for row in &out.rows {
            let mut vals = Vec::with_capacity(row.len());
            for (i, cell) in row.iter().enumerate() {
                let ty = out
                    .columns
                    .get(i)
                    .map(|c| c.1.clone())
                    .ok_or_else(|| internal("PL/pgSQL query row wider than its columns"))?;
                let mut v = PlScalar::new(cell.clone(), ty);
                if let (1, Some(t)) = (row.len(), &self.ret.ty) {
                    v = assign_cast(v, t)?;
                }
                vals.push(v);
            }
            self.push_row(vals)?;
        }
        self.row_count = u64::try_from(out.rows.len()).unwrap_or(u64::MAX);
        self.set_found(!out.rows.is_empty())?;
        Ok(Flow::Normal)
    }

    fn exec_perform(&mut self, q: &EmbeddedSql) -> Result<Flow, SqlSurfaceError> {
        let out = self.run_sql(q, RowDemand::None)?;
        self.row_count = out.row_count;
        self.set_found(out.row_count > 0)?;
        Ok(Flow::Normal)
    }

    fn exec_sql_stmt(&mut self, q: &EmbeddedSql) -> Result<Flow, SqlSurfaceError> {
        let Some(into) = &q.into else {
            let out = self.run_sql(q, RowDemand::None)?;
            self.row_count = out.row_count;
            self.set_found(out.row_count > 0)?;
            return Ok(Flow::Normal);
        };
        let out = self.run_sql(q, RowDemand::AtMostTwo)?;
        let dml = q.writes || out.dml || q.kind != EmbeddedKind::Select;
        self.apply_into(into, &out, dml, true)?;
        Ok(Flow::Normal)
    }

    /// `INTO` の行数規則。`STRICT`（および DML の `RETURNING INTO`）は 0 行で `P0002`、2 行以上で
    /// `P0003`。それ以外は 0 行なら NULL、2 行以上なら先頭行。`EXECUTE` は FOUND を変えない。
    fn apply_into(
        &mut self,
        into: &IntoClause,
        out: &SqlOutcome,
        dml: bool,
        update_found: bool,
    ) -> Result<(), SqlSurfaceError> {
        let n = out.rows.len();
        if n == 0 && into.strict {
            return Err(SqlSurfaceError::NoDataFound);
        }
        if n >= 2 && (into.strict || dml) {
            return Err(SqlSurfaceError::TooManyRows);
        }
        let affected = if dml {
            out.row_count
        } else {
            u64::try_from(n.min(1)).unwrap_or(1)
        };
        self.row_count = affected;
        if update_found {
            self.set_found(affected > 0)?;
        }
        self.assign_row(&into.targets, &out.columns, out.rows.first())
    }

    fn exec_execute(
        &mut self,
        query: &SqlExpr,
        into: Option<&IntoClause>,
        using: &[SqlExpr],
    ) -> Result<Flow, SqlSurfaceError> {
        let q = assign_cast(self.eval(query)?, &ColumnType::Text)?;
        let Cell::Text(text) = q.cell else {
            return Err(SqlSurfaceError::NullValueNotAllowed {
                detail: "query string argument of EXECUTE is null",
            });
        };
        let mut args = Vec::with_capacity(using.len());
        for u in using {
            args.push(self.eval(u)?);
        }
        let demand = if into.is_some() {
            RowDemand::AtMostTwo
        } else {
            RowDemand::None
        };
        let out = self
            .host
            .exec_dynamic(&text, &args, demand, &mut *self.budget)?;
        match into {
            Some(i) => self.apply_into(i, &out, out.dml, false)?,
            None => self.row_count = out.row_count,
        }
        Ok(Flow::Normal)
    }

    fn exec_diag(&mut self, items: &[(AssignTarget, DiagItem)]) -> Result<Flow, SqlSurfaceError> {
        for (t, item) in items {
            match item {
                DiagItem::RowCount => {
                    let n = i64::try_from(self.row_count).unwrap_or(i64::MAX);
                    let v = PlScalar::new(Cell::SignedInteger(n), ColumnType::BigInt);
                    self.assign_target(t, v)?;
                }
            }
        }
        Ok(Flow::Normal)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! テストが `Interp` の内部状態を使わずに済むよう、実行予算の読み出しだけ公開する。
    use super::ExecBudget;

    impl ExecBudget {
        pub(crate) fn statements_run(&self) -> usize {
            self.statements
        }
    }
}
