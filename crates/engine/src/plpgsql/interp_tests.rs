//! `plpgsql::interp` の単体テスト（Issue #2225・SQL-68）。
//!
//! 制御フロー・スコープ・代入時の型変換・`INTO` の行数規則・実行上限・協調キャンセル・新しい
//! SQLSTATE を、テスト専用ホスト [`ScriptedHost`] で確かめる。**ホストは式の意味論を持たない**:
//! 数値・文字列・真偽のリテラルと変数参照を左から右へ `+ - * < > = <= >=` で畳むだけの最小の
//! 評価器で、SQL 実行は文ごとに用意した [`SqlOutcome`] を返すだけ（第 2 の SQL 評価器ではない）。
//! 期待値は PostgreSQL 17 の PL/pgSQL の公開された挙動（制御構文・`STRICT`・`FOUND`・
//! `GET DIAGNOSTICS`）に合わせている。本物の式・SQL の実行と `FOUND`／`ROW_COUNT` の実測値は
//! #2226 の結線後に検証する。

use std::collections::VecDeque;

use super::ast::{Body, DeclType, EmbeddedSql, ParamDecl, ReturnShape, SqlExpr, VarRef};
use super::embedded::{validate_embedded, StructuralChecker};
use super::host::{PlHost, PlScalar, ResolvedType, RowDemand, SqlOutcome, VarLookup};
use super::interp::{run_function, CallOutcome, ExecBudget, ReturnSpec};
use super::{parse_body, ParseOptions};
use crate::catalog::ColumnType;
use crate::interrupt::{InterruptReason, InterruptScope};
use crate::plpgsql_limits::PlpgsqlLimits;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::exec::Cell;
use crate::sql::lexer::Token;

// ---- テスト専用ホスト ----

#[derive(Default)]
struct ScriptedHost {
    sql: VecDeque<SqlOutcome>,
    dynamic: VecDeque<SqlOutcome>,
    /// `rec(...)` の式が再帰で呼ぶ関数（入れ子呼び出し・予算の合算の確認用）。
    recurse: Option<(Body, ReturnSpec)>,
}

fn int(i: i64) -> PlScalar {
    let ty = if i32::try_from(i).is_ok() {
        ColumnType::Integer
    } else {
        ColumnType::BigInt
    };
    PlScalar::new(Cell::SignedInteger(i), ty)
}

fn txt(s: &str) -> PlScalar {
    PlScalar::new(Cell::Text(s.to_string()), ColumnType::Text)
}

fn boolean(b: bool) -> PlScalar {
    PlScalar::new(Cell::Bool(b), ColumnType::Boolean)
}

fn null_int() -> PlScalar {
    PlScalar::null(ColumnType::Integer)
}

fn null_text() -> PlScalar {
    PlScalar::null(ColumnType::Text)
}

fn bad() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "scripted host cannot evaluate this expression".to_string(),
    }
}

impl ScriptedHost {
    fn operand(
        &mut self,
        expr: &SqlExpr,
        i: usize,
        env: &dyn VarLookup,
    ) -> Result<PlScalar, SqlSurfaceError> {
        if let Some(site) = expr.var_refs.iter().find(|s| s.pos == i) {
            return env.scalar(&site.var).cloned();
        }
        match expr.tokens.get(i).ok_or_else(bad)? {
            Token::Number(n) => Ok(int(n.parse::<i64>().map_err(|_| bad())?)),
            Token::StringLiteral(s) => Ok(txt(s)),
            Token::Ident(w) if w.eq_ignore_ascii_case("true") => Ok(boolean(true)),
            Token::Ident(w) if w.eq_ignore_ascii_case("false") => Ok(boolean(false)),
            t if format!("{t:?}").to_ascii_lowercase().contains("null") => Ok(null_int()),
            _ => Err(bad()),
        }
    }

    fn eval_expr(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        budget: &mut ExecBudget,
    ) -> Result<PlScalar, SqlSurfaceError> {
        // `rec(<式>)`: 最初の変数参照の値から 1 を引いた引数で、登録済みの関数を再帰呼び出しする。
        if matches!(expr.tokens.first(), Some(Token::Ident(w)) if w == "rec") {
            let (body, ret) = self.recurse.clone().ok_or_else(bad)?;
            let arg = match expr.var_refs.first() {
                Some(s) => match env.scalar(&s.var)?.cell {
                    Cell::SignedInteger(n) => int(n - 1),
                    _ => return Err(bad()),
                },
                None => int(0),
            };
            return match run_function(&body, vec![arg], &ret, self, budget)? {
                CallOutcome::Scalar(v) => Ok(v),
                _ => Err(bad()),
            };
        }
        let mut acc = self.operand(expr, 0, env)?;
        let mut i = 1;
        while i < expr.tokens.len() {
            let op = expr.tokens.get(i).ok_or_else(bad)?.clone();
            let rhs = self.operand(expr, i + 1, env)?;
            acc = binop(&op, &acc, &rhs)?;
            i += 2;
        }
        Ok(acc)
    }
}

fn binop(op: &Token, a: &PlScalar, b: &PlScalar) -> Result<PlScalar, SqlSurfaceError> {
    let (Cell::SignedInteger(x), Cell::SignedInteger(y)) = (&a.cell, &b.cell) else {
        // どちらかが NULL なら NULL（比較は NULL の真偽）。
        return match (&a.cell, &b.cell) {
            (Cell::Null, _) | (_, Cell::Null) => Ok(match op {
                Token::Punct('+' | '-' | '*') => null_int(),
                _ => PlScalar::null(ColumnType::Boolean),
            }),
            _ => Err(bad()),
        };
    };
    Ok(match op {
        Token::Punct('+') => int(x.checked_add(*y).ok_or_else(bad)?),
        Token::Punct('-') => int(x.checked_sub(*y).ok_or_else(bad)?),
        Token::Punct('*') => int(x.checked_mul(*y).ok_or_else(bad)?),
        Token::Punct('<') => boolean(x < y),
        Token::Punct('>') => boolean(x > y),
        Token::Punct('=') => boolean(x == y),
        Token::Le => boolean(x <= y),
        Token::Ge => boolean(x >= y),
        _ => return Err(bad()),
    })
}

impl PlHost for ScriptedHost {
    fn eval_value(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        budget: &mut ExecBudget,
    ) -> Result<PlScalar, SqlSurfaceError> {
        self.eval_expr(expr, env, budget)
    }

    fn eval_cond(
        &mut self,
        expr: &SqlExpr,
        env: &dyn VarLookup,
        budget: &mut ExecBudget,
    ) -> Result<Option<bool>, SqlSurfaceError> {
        match self.eval_expr(expr, env, budget)?.cell {
            Cell::Bool(b) => Ok(Some(b)),
            Cell::Null => Ok(None),
            _ => Err(bad()),
        }
    }

    fn values_equal(
        &mut self,
        a: &PlScalar,
        b: &PlScalar,
    ) -> Result<Option<bool>, SqlSurfaceError> {
        Ok(match (&a.cell, &b.cell) {
            (Cell::Null, _) | (_, Cell::Null) => None,
            (x, y) => Some(x == y),
        })
    }

    fn exec_sql(
        &mut self,
        _sql: &EmbeddedSql,
        _env: &dyn VarLookup,
        _demand: RowDemand,
        _budget: &mut ExecBudget,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.sql.pop_front().ok_or_else(bad)
    }

    fn exec_dynamic(
        &mut self,
        _query: &str,
        _using: &[PlScalar],
        _demand: RowDemand,
        _budget: &mut ExecBudget,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        self.dynamic.pop_front().ok_or_else(bad)
    }

    fn resolve_type(&mut self, ty: &DeclType) -> Result<ResolvedType, SqlSurfaceError> {
        Ok(match ty {
            DeclType::Record => ResolvedType::Record(None),
            DeclType::PercentRowType { .. } => ResolvedType::Record(Some(Vec::new())),
            _ => ResolvedType::Scalar(ColumnType::Integer),
        })
    }
}

// ---- 補助 ----

fn rows(vals: &[i64]) -> SqlOutcome {
    SqlOutcome {
        columns: vec![("x".to_string(), ColumnType::Integer)],
        rows: vals.iter().map(|v| vec![Cell::SignedInteger(*v)]).collect(),
        row_count: vals.len() as u64,
        dml: false,
    }
}

fn dml_rows(vals: &[i64]) -> SqlOutcome {
    SqlOutcome {
        dml: true,
        ..rows(vals)
    }
}

fn affected(n: u64) -> SqlOutcome {
    SqlOutcome {
        row_count: n,
        dml: true,
        ..SqlOutcome::default()
    }
}

fn prepare(src: &str, params: &[(&str, SqlColumnTypeName)], shape: ReturnShape) -> Body {
    let decls = params
        .iter()
        .map(|(n, t)| ParamDecl {
            name: Some((*n).to_string()),
            ty: t.clone(),
        })
        .collect();
    let mut b = match parse_body(src, &ParseOptions::new(decls, shape)) {
        Ok(b) => b,
        Err(e) => panic!("parse failed for {src:?}: {} ({e})", e.wire_code()),
    };
    if let Err(e) = validate_embedded(&mut b, &StructuralChecker) {
        panic!("classify failed for {src:?}: {} ({e})", e.wire_code());
    }
    b
}

fn spec(shape: ReturnShape, ty: Option<ColumnType>) -> ReturnSpec {
    ReturnSpec { shape, ty }
}

fn run_with(
    src: &str,
    params: &[(&str, SqlColumnTypeName)],
    args: Vec<PlScalar>,
    ret: ReturnSpec,
    host: &mut ScriptedHost,
    limits: PlpgsqlLimits,
) -> Result<CallOutcome, SqlSurfaceError> {
    let body = prepare(src, params, ret.shape);
    let mut budget = ExecBudget::new(limits, 10_000);
    run_function(&body, args, &ret, host, &mut budget)
}

fn p_int() -> Vec<(&'static str, SqlColumnTypeName)> {
    vec![("a", SqlColumnTypeName::Integer)]
}

/// スカラー（int4）関数を既定の上限で実行する。
fn call(src: &str, args: Vec<PlScalar>) -> Result<CallOutcome, SqlSurfaceError> {
    run_with(
        src,
        &p_int(),
        args,
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    )
}

fn call_host(
    src: &str,
    args: Vec<PlScalar>,
    host: &mut ScriptedHost,
) -> Result<CallOutcome, SqlSurfaceError> {
    run_with(
        src,
        &p_int(),
        args,
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        host,
        PlpgsqlLimits::default(),
    )
}

fn value(r: Result<CallOutcome, SqlSurfaceError>) -> Cell {
    match r {
        Ok(CallOutcome::Scalar(v)) => v.cell,
        other => panic!("expected a scalar result, got {other:?}"),
    }
}

fn ival(r: Result<CallOutcome, SqlSurfaceError>) -> i64 {
    match value(r) {
        Cell::SignedInteger(i) => i,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn err_code(r: Result<CallOutcome, SqlSurfaceError>) -> &'static str {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.wire_code(),
    }
}

fn small_limits(stmts: usize, iters: usize, depth: usize) -> PlpgsqlLimits {
    PlpgsqlLimits::new(stmts, iters, 32, 65_536, depth)
}

// ---- 変数・スコープ ----

#[test]
fn declare_defaults_and_null_initialization() {
    let src = "DECLARE x int := 3; y int; BEGIN RETURN x + 1; END";
    assert_eq!(ival(call(src, vec![int(0)])), 4);
    let src = "DECLARE y int; BEGIN RETURN y; END";
    assert_eq!(value(call(src, vec![int(0)])), Cell::Null);
}

#[test]
fn inner_declaration_shadows_outer() {
    let src = "DECLARE x int := 1; BEGIN DECLARE x int := 2; BEGIN x := x + 10; END; RETURN x; END";
    assert_eq!(ival(call(src, vec![int(0)])), 1);
}

#[test]
fn block_inside_loop_is_reinitialized_each_iteration() {
    let src = "DECLARE s int := 0; BEGIN FOR i IN 1..3 LOOP \
               DECLARE t int := 0; BEGIN t := t + 1; s := s + t; END; END LOOP; RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 3);
}

#[test]
fn parameter_is_converted_to_its_declared_type() {
    let r = run_with(
        "BEGIN RETURN a + 1; END",
        &[("a", SqlColumnTypeName::Integer)],
        vec![txt("12")],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert_eq!(ival(r), 13);
}

#[test]
fn not_null_variable_rejects_null_on_init_and_assignment() {
    let init = "DECLARE x int NOT NULL := a; BEGIN RETURN x; END";
    assert_eq!(err_code(call(init, vec![null_int()])), "22004");
    let assign = "DECLARE x int NOT NULL := 1; BEGIN x := a; RETURN x; END";
    assert_eq!(err_code(call(assign, vec![null_int()])), "22004");
    assert_eq!(ival(call(assign, vec![int(5)])), 5);
}

#[test]
fn unassigned_record_field_reference_is_55000() {
    let src = "DECLARE r RECORD; BEGIN RETURN r.f; END";
    assert_eq!(err_code(call(src, vec![int(0)])), "55000");
}

#[test]
fn assignment_converts_to_declared_type() {
    let src = "DECLARE t text; BEGIN t := a + 1; RETURN a; END";
    assert_eq!(ival(call(src, vec![int(1)])), 1);
    // text 変数への代入は文字列になり、int 変数への不正な文字列は 22P02 になる。
    let src = "DECLARE n int; BEGIN n := 'abc'; RETURN n; END";
    assert_eq!(err_code(call(src, vec![int(1)])), "22P02");
}

// ---- 分岐 ----

#[test]
fn if_treats_null_condition_as_false() {
    let src = "BEGIN IF a > 5 THEN RETURN 1; ELSIF a = 5 THEN RETURN 2; ELSE RETURN 3; END IF; END";
    assert_eq!(ival(call(src, vec![int(9)])), 1);
    assert_eq!(ival(call(src, vec![int(5)])), 2);
    assert_eq!(ival(call(src, vec![null_int()])), 3);
}

#[test]
fn simple_and_searched_case() {
    let simple = "BEGIN CASE a WHEN 1 THEN RETURN 10; WHEN 2, 3 THEN RETURN 20; \
                  ELSE RETURN 30; END CASE; END";
    assert_eq!(ival(call(simple, vec![int(1)])), 10);
    assert_eq!(ival(call(simple, vec![int(3)])), 20);
    assert_eq!(ival(call(simple, vec![int(9)])), 30);
    assert_eq!(ival(call(simple, vec![null_int()])), 30);
    let searched = "BEGIN CASE WHEN a < 0 THEN RETURN -1; WHEN a = 0 THEN RETURN 0; \
                    ELSE RETURN 1; END CASE; END";
    assert_eq!(ival(call(searched, vec![int(0)])), 0);
    assert_eq!(ival(call(searched, vec![int(4)])), 1);
}

#[test]
fn case_without_else_and_no_match_is_20000() {
    let src = "BEGIN CASE a WHEN 1 THEN RETURN 10; END CASE; RETURN 0; END";
    assert_eq!(ival(call(src, vec![int(1)])), 10);
    assert_eq!(err_code(call(src, vec![int(2)])), "20000");
}

// ---- ループ ----

#[test]
fn loop_exit_when_and_while() {
    let src =
        "DECLARE s int := 0; BEGIN LOOP s := s + 1; EXIT WHEN s >= 5; END LOOP; RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 5);
    let src = "DECLARE s int := 0; BEGIN WHILE s < 4 LOOP s := s + 1; END LOOP; RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 4);
    // 条件が NULL ならループを終える。
    let src = "DECLARE s int := 7; BEGIN WHILE a > 0 LOOP s := s + 1; END LOOP; RETURN s; END";
    assert_eq!(ival(call(src, vec![null_int()])), 7);
}

#[test]
fn labeled_continue_and_exit_target_the_outer_loop() {
    let cont = "DECLARE s int := 0; BEGIN <<outer>> FOR i IN 1..3 LOOP FOR j IN 1..3 LOOP \
                CONTINUE outer WHEN j = 2; s := s + 1; END LOOP; END LOOP; RETURN s; END";
    assert_eq!(ival(call(cont, vec![int(0)])), 3);
    let exit = "DECLARE s int := 0; BEGIN <<outer>> FOR i IN 1..3 LOOP FOR j IN 1..3 LOOP \
                EXIT outer WHEN i = 2; s := s + 1; END LOOP; END LOOP; RETURN s; END";
    assert_eq!(ival(call(exit, vec![int(0)])), 3);
}

#[test]
fn exit_can_leave_a_labeled_block() {
    let src = "DECLARE s int := 0; BEGIN <<b>> BEGIN s := 1; EXIT b; s := 2; END b; RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 1);
}

#[test]
fn integer_for_forward_reverse_by_and_empty() {
    let fwd = "DECLARE s int := 0; BEGIN FOR i IN 1..4 LOOP s := s + i; END LOOP; RETURN s; END";
    assert_eq!(ival(call(fwd, vec![int(0)])), 10);
    let rev = "DECLARE s int := 0; BEGIN FOR i IN REVERSE 10..1 BY 3 LOOP s := s + i; END LOOP; \
               RETURN s; END";
    assert_eq!(ival(call(rev, vec![int(0)])), 22);
    let fwd_by = "DECLARE s int := 0; BEGIN FOR i IN 1..10 BY 4 LOOP s := s + i; END LOOP; \
                  RETURN s; END";
    assert_eq!(ival(call(fwd_by, vec![int(0)])), 15);
}

#[test]
fn integer_for_sets_found_and_validates_bounds() {
    let found = |lo: i64, hi: i64| {
        let src = format!("BEGIN FOR i IN {lo}..{hi} LOOP NULL; END LOOP; RETURN FOUND; END");
        run_with(
            &src,
            &p_int(),
            vec![int(0)],
            spec(ReturnShape::Scalar, Some(ColumnType::Boolean)),
            &mut ScriptedHost::default(),
            PlpgsqlLimits::default(),
        )
    };
    assert_eq!(value(found(1, 2)), Cell::Bool(true));
    assert_eq!(value(found(5, 1)), Cell::Bool(false));
    let null_bound = "BEGIN FOR i IN 1..a LOOP NULL; END LOOP; RETURN 0; END";
    assert_eq!(err_code(call(null_bound, vec![null_int()])), "22004");
    let by_zero = "BEGIN FOR i IN 1..3 BY a LOOP NULL; END LOOP; RETURN 0; END";
    assert_eq!(err_code(call(by_zero, vec![int(0)])), "22023");
    assert_eq!(err_code(call(by_zero, vec![int(-1)])), "22023");
}

#[test]
fn integer_for_does_not_overflow_near_int4_limits() {
    let src =
        "DECLARE s int := 0; BEGIN FOR i IN 2147483646..2147483647 LOOP s := s + 1; END LOOP; \
               RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 2);
    // REVERSE は書いた順（大..小）で、下端を越える次の値で終わる。
    let src = "DECLARE s int := 0; BEGIN FOR i IN REVERSE 0-2147483647..0-2147483648 BY 1 LOOP \
               s := s + 1; END LOOP; RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 2);
    // 小..大の順に書いた REVERSE は 0 回（PostgreSQL と同じ）。
    let src = "DECLARE s int := 0; BEGIN FOR i IN REVERSE 1..10 LOOP s := s + 1; END LOOP; \
               RETURN s; END";
    assert_eq!(ival(call(src, vec![int(0)])), 0);
}

#[test]
fn query_for_iterates_rows_and_sets_found() {
    let src = "DECLARE s int := 0; BEGIN FOR r IN SELECT x FROM t LOOP s := s + r.x; END LOOP; \
               RETURN s; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[1, 2, 3]));
    assert_eq!(ival(call_host(src, vec![int(0)], &mut host)), 6);
    let found = "BEGIN FOR r IN SELECT x FROM t LOOP NULL; END LOOP; RETURN FOUND; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[]));
    let r = run_with(
        found,
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Scalar, Some(ColumnType::Boolean)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(value(r), Cell::Bool(false));
}

// ---- INTO・FOUND・ROW_COUNT ----

#[test]
fn select_into_strict_row_count_rules() {
    let strict = "DECLARE v int; BEGIN SELECT x INTO STRICT v FROM t; RETURN v; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[]));
    assert_eq!(
        err_code(call_host(strict, vec![int(0)], &mut host)),
        "P0002"
    );
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[1, 2]));
    assert_eq!(
        err_code(call_host(strict, vec![int(0)], &mut host)),
        "P0003"
    );
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[7]));
    assert_eq!(ival(call_host(strict, vec![int(0)], &mut host)), 7);
}

#[test]
fn select_into_without_strict_takes_first_row_or_null() {
    let plain = "DECLARE v int := 99; BEGIN SELECT x INTO v FROM t; RETURN v; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[]));
    assert_eq!(value(call_host(plain, vec![int(0)], &mut host)), Cell::Null);
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[4, 5]));
    assert_eq!(ival(call_host(plain, vec![int(0)], &mut host)), 4);
}

#[test]
fn dml_returning_into_with_many_rows_is_p0003() {
    let src = "DECLARE v int; BEGIN INSERT INTO t(x) VALUES (1) RETURNING x INTO v; RETURN v; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(dml_rows(&[1, 2]));
    assert_eq!(err_code(call_host(src, vec![int(0)], &mut host)), "P0003");
    let mut host = ScriptedHost::default();
    host.sql.push_back(dml_rows(&[1]));
    assert_eq!(ival(call_host(src, vec![int(0)], &mut host)), 1);
}

#[test]
fn found_and_get_diagnostics_follow_the_last_statement() {
    let src =
        "DECLARE n int; BEGIN UPDATE t SET x = 1; GET DIAGNOSTICS n = ROW_COUNT; RETURN n; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(affected(3));
    assert_eq!(ival(call_host(src, vec![int(0)], &mut host)), 3);
    let src = "BEGIN PERFORM x FROM t; RETURN FOUND; END";
    let ret = || spec(ReturnShape::Scalar, Some(ColumnType::Boolean));
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[1]));
    let r = run_with(
        src,
        &p_int(),
        vec![int(0)],
        ret(),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(value(r), Cell::Bool(true));
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[]));
    let r = run_with(
        src,
        &p_int(),
        vec![int(0)],
        ret(),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(value(r), Cell::Bool(false));
}

#[test]
fn execute_updates_row_count_but_not_found() {
    let params = [("q", SqlColumnTypeName::Text)];
    let src = "DECLARE n int; BEGIN FOR i IN 1..1 LOOP NULL; END LOOP; EXECUTE q; \
               GET DIAGNOSTICS n = ROW_COUNT; RETURN n; END";
    let mut host = ScriptedHost::default();
    host.dynamic.push_back(affected(0));
    let r = run_with(
        src,
        &params,
        vec![txt("UPDATE t SET x = 1")],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(ival(r), 0);
    let found = "BEGIN FOR i IN 1..1 LOOP NULL; END LOOP; EXECUTE q; RETURN FOUND; END";
    let mut host = ScriptedHost::default();
    host.dynamic.push_back(affected(0));
    let r = run_with(
        found,
        &params,
        vec![txt("UPDATE t SET x = 1")],
        spec(ReturnShape::Scalar, Some(ColumnType::Boolean)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    // EXECUTE は FOUND を変えない（直前の FOR が真にした値のまま）。
    assert_eq!(value(r), Cell::Bool(true));
}

#[test]
fn execute_with_null_query_is_22004() {
    let params = [("q", SqlColumnTypeName::Text)];
    let r = run_with(
        "BEGIN EXECUTE q; RETURN 0; END",
        &params,
        vec![null_text()],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert_eq!(err_code(r), "22004");
}

// ---- RETURN の形 ----

#[test]
fn scalar_return_is_assignment_cast_to_the_return_type() {
    let r = run_with(
        "BEGIN RETURN a + 1; END",
        &p_int(),
        vec![int(41)],
        spec(ReturnShape::Scalar, Some(ColumnType::Text)),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert_eq!(value(r), Cell::Text("42".to_string()));
}

#[test]
fn void_function_returns_nothing() {
    let r = run_with(
        "BEGIN RETURN; END",
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Void, None),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert!(matches!(r, Ok(CallOutcome::Void)));
    let r = run_with(
        "BEGIN NULL; END",
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Void, None),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert!(matches!(r, Ok(CallOutcome::Void)));
}

#[test]
fn scalar_function_reaching_the_end_is_2f005() {
    assert_eq!(err_code(call("BEGIN NULL; END", vec![int(0)])), "2F005");
}

#[test]
fn trigger_function_cannot_be_called_as_an_ordinary_function() {
    let r = run_with(
        "BEGIN RETURN NULL; END",
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Trigger, None),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert_eq!(err_code(r), "0A000");
}

#[test]
fn setof_accumulates_return_next_and_return_query() {
    let src = "BEGIN RETURN NEXT 1; RETURN NEXT a; RETURN QUERY SELECT x FROM t; RETURN; END";
    let mut host = ScriptedHost::default();
    host.sql.push_back(rows(&[10, 11]));
    let r = run_with(
        src,
        &p_int(),
        vec![int(2)],
        spec(ReturnShape::SetOf, Some(ColumnType::Integer)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    let Ok(CallOutcome::Rows(rows)) = r else {
        panic!("expected rows, got {r:?}");
    };
    let got: Vec<Cell> = rows.iter().map(|r| r[0].cell.clone()).collect();
    assert_eq!(
        got,
        vec![
            Cell::SignedInteger(1),
            Cell::SignedInteger(2),
            Cell::SignedInteger(10),
            Cell::SignedInteger(11)
        ]
    );
    // 終わりに到達しても SETOF は 2F005 にならない。
    let r = run_with(
        "BEGIN RETURN NEXT 1; END",
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::SetOf, Some(ColumnType::Integer)),
        &mut ScriptedHost::default(),
        PlpgsqlLimits::default(),
    );
    assert!(matches!(r, Ok(CallOutcome::Rows(v)) if v.len() == 1));
}

// ---- 実行上限・協調キャンセル ----

fn run_small(src: &str, limits: PlpgsqlLimits) -> Result<CallOutcome, SqlSurfaceError> {
    run_with(
        src,
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut ScriptedHost::default(),
        limits,
    )
}

#[test]
fn infinite_loops_stop_at_the_limit_with_54000() {
    for src in [
        "BEGIN LOOP END LOOP; END",
        "BEGIN WHILE true LOOP NULL; END LOOP; END",
        "BEGIN FOR i IN 1..2147483647 LOOP NULL; END LOOP; RETURN 0; END",
    ] {
        assert_eq!(
            err_code(run_small(src, PlpgsqlLimits::default())),
            "54000",
            "{src}"
        );
        assert_eq!(
            err_code(run_small(src, small_limits(50, 20, 32))),
            "54000",
            "{src}"
        );
    }
}

#[test]
fn statement_limit_boundary_is_exact() {
    let src = "BEGIN NULL; NULL; RETURN 1; END";
    // 実行する文は 3 つ（NULL・NULL・RETURN）。
    assert_eq!(ival(run_small(src, small_limits(3, 100, 32))), 1);
    assert_eq!(err_code(run_small(src, small_limits(2, 100, 32))), "54000");
}

#[test]
fn loop_iteration_limit_boundary_is_exact() {
    let src = "BEGIN FOR i IN 1..5 LOOP NULL; END LOOP; RETURN 1; END";
    assert_eq!(ival(run_small(src, small_limits(1000, 5, 32))), 1);
    assert_eq!(err_code(run_small(src, small_limits(1000, 4, 32))), "54000");
}

fn recursive_host(depth_body: &str) -> ScriptedHost {
    let ret = spec(ReturnShape::Scalar, Some(ColumnType::Integer));
    ScriptedHost {
        recurse: Some((prepare(depth_body, &p_int(), ReturnShape::Scalar), ret)),
        ..ScriptedHost::default()
    }
}

#[test]
fn call_depth_limit_is_54001_and_statements_accumulate_across_calls() {
    let body = "BEGIN NULL; IF a > 0 THEN RETURN rec(a - 1); END IF; RETURN 0; END";
    // 深さ 4 の呼び出し（a=3 → 2 → 1 → 0）は上限 4 で通り、3 で 54001。
    let mut host = recursive_host(body);
    let ok = run_with(
        body,
        &p_int(),
        vec![int(3)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        small_limits(1000, 1000, 4),
    );
    assert_eq!(ival(ok), 0);
    let mut host = recursive_host(body);
    let too_deep = run_with(
        body,
        &p_int(),
        vec![int(3)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        small_limits(1000, 1000, 3),
    );
    assert_eq!(err_code(too_deep), "54001");
    // 文数は呼び出しをまたいで合算する（1 回あたり NULL・IF・RETURN の 3 文）。
    let mut host = recursive_host(body);
    let many = run_with(
        body,
        &p_int(),
        vec![int(3)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        small_limits(8, 1000, 32),
    );
    assert_eq!(err_code(many), "54000");
}

#[test]
fn unbounded_recursion_stops_at_the_default_depth() {
    let body = "BEGIN RETURN rec(a); END";
    let mut host = recursive_host(body);
    let r = run_with(
        body,
        &p_int(),
        vec![int(0)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(err_code(r), "54001");
}

#[test]
fn set_result_row_limit_is_checked_before_adding() {
    let src = "BEGIN RETURN NEXT 1; RETURN NEXT 2; RETURN NEXT 3; RETURN; END";
    let body = prepare(src, &p_int(), ReturnShape::SetOf);
    let ret = spec(ReturnShape::SetOf, Some(ColumnType::Integer));
    // 上限ちょうど（3 行）は成功し、2 行の上限では 54000（部分結果は返さない）。
    let mut budget = ExecBudget::new(PlpgsqlLimits::default(), 3);
    let ok = run_function(
        &body,
        vec![int(0)],
        &ret,
        &mut ScriptedHost::default(),
        &mut budget,
    );
    assert!(matches!(ok, Ok(CallOutcome::Rows(v)) if v.len() == 3));
    let mut budget = ExecBudget::new(PlpgsqlLimits::default(), 2);
    let r = run_function(
        &body,
        vec![int(0)],
        &ret,
        &mut ScriptedHost::default(),
        &mut budget,
    );
    assert_eq!(err_code(r), "54000");
}

#[test]
fn cooperative_cancel_stops_an_infinite_loop_with_57014() {
    let _guard = InterruptScope::install(Box::new(|| Some(InterruptReason::UserCancel)));
    let r = run_small("BEGIN LOOP END LOOP; END", PlpgsqlLimits::default());
    assert_eq!(err_code(r), "57014");
    let r = run_small("BEGIN NULL; RETURN 1; END", PlpgsqlLimits::default());
    assert_eq!(err_code(r), "57014");
}

#[test]
fn deep_nesting_with_deep_calls_fits_the_stack() {
    // 構文段の入れ子 32 × 呼び出し深さ 32 の最悪ケースの下限を、スタックを食う形で確かめる。
    let mut body = String::from("BEGIN ");
    for _ in 0..30 {
        body.push_str("IF a >= 0 THEN ");
    }
    body.push_str("IF a > 0 THEN RETURN rec(a - 1); END IF; ");
    for _ in 0..30 {
        body.push_str("END IF; ");
    }
    body.push_str("RETURN 0; END");
    let mut host = recursive_host(&body);
    let r = run_with(
        &body,
        &p_int(),
        vec![int(30)],
        spec(ReturnShape::Scalar, Some(ColumnType::Integer)),
        &mut host,
        PlpgsqlLimits::default(),
    );
    assert_eq!(ival(r), 0);
}

// ---- 文言の非漏えい ----

#[test]
fn error_messages_do_not_leak_names_or_values() {
    let r = call(
        "DECLARE secretvar int NOT NULL := a; BEGIN RETURN secretvar; END",
        vec![null_int()],
    );
    let msg = r.expect_err("must fail").to_string();
    assert!(!msg.contains("secretvar"), "{msg}");
    let r = call(
        "DECLARE n int; BEGIN n := 'topsecret'; RETURN n; END",
        vec![int(0)],
    );
    let msg = r.expect_err("must fail").to_string();
    assert!(!msg.contains("topsecret"), "{msg}");
    for e in [
        SqlSurfaceError::NoDataFound,
        SqlSurfaceError::TooManyRows,
        SqlSurfaceError::CaseNotFound,
        SqlSurfaceError::FunctionExecutedNoReturnStatement,
        SqlSurfaceError::RecordNotAssigned,
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[allow(dead_code)]
fn _var_ref_is_used(_: &VarRef) {}
