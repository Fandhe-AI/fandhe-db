//! `core/plpgsql_host.rs` の結合テスト（Issue #2226・SQL-68。ADR D3・D5・D6・D8）。
//!
//! `parse_body` → `validate_embedded` → `run_plpgsql_*` の順で、本物の `EngineCore`（許可リスト解析・
//! 実行本体・RLS）に対して本体の SQL を実行する。同一 Tx でのコミット／ロールバック、`FOUND`／
//! `ROW_COUNT` の実測値、既存上限、テナント境界、インジェクション、台帳・揮発性ゲートを固定する。

use super::plpgsql_host::{PlCallOptions, PlInvocation};
use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::plpgsql::ast::{ParamDecl, ParseOptions, ReturnShape};
use crate::plpgsql::embedded::validate_embedded;
use crate::plpgsql::host::PlScalar;
use crate::plpgsql::interp::{CallOutcome, ReturnSpec};
use crate::plpgsql::parse_body;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::transaction::SessionTransaction;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core(mode: LedgerMode) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("plpgsql-host");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn sql(core: &EngineCore, c: &PolicyContext, s: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(c, &mut session, s)
}

fn count(core: &EngineCore, c: &PolicyContext) -> u64 {
    let SqlOutcome::Query(q) = sql(core, c, "SELECT COUNT(*) FROM docs").expect("count") else {
        panic!("expected query");
    };
    match q.rows[0].cells[0] {
        Cell::Integer(v) => v,
        ref o => panic!("unexpected {o:?}"),
    }
}

fn seed(core: &EngineCore, c: &PolicyContext, ids: &[u64], lang: &str) {
    for id in ids {
        sql(
            core,
            c,
            &format!(
                "INSERT INTO docs (id, embedding, lang, n) VALUES ({id}, '[0.1,0.2]', '{lang}', 0)"
            ),
        )
        .expect("seed");
    }
}

fn int(i: i64) -> PlScalar {
    PlScalar::new(Cell::SignedInteger(i), ColumnType::Integer)
}

fn text(s: &str) -> PlScalar {
    PlScalar::new(Cell::Text(s.to_string()), ColumnType::Text)
}

const WRITE: PlCallOptions = PlCallOptions { may_write: true };

/// 戻り値の整数（`Integer`／`BigInt` のセル）。
fn as_i64(out: &CallOutcome) -> i64 {
    match out {
        CallOutcome::Scalar(PlScalar {
            cell: Cell::SignedInteger(i),
            ..
        }) => *i,
        CallOutcome::Scalar(PlScalar {
            cell: Cell::Integer(u),
            ..
        }) => i64::try_from(*u).expect("fits"),
        other => panic!("expected an integer, got {other:?}"),
    }
}

struct Call<'a> {
    src: &'a str,
    params: Vec<(&'a str, SqlColumnTypeName)>,
    args: Vec<PlScalar>,
    shape: ReturnShape,
    ty: Option<ColumnType>,
    opts: PlCallOptions,
}

fn call<'a>(src: &'a str) -> Call<'a> {
    Call {
        src,
        params: Vec::new(),
        args: Vec::new(),
        shape: ReturnShape::Scalar,
        ty: Some(ColumnType::Integer),
        opts: WRITE,
    }
}

impl<'a> Call<'a> {
    fn param(mut self, name: &'a str, ty: SqlColumnTypeName, arg: PlScalar) -> Self {
        self.params.push((name, ty));
        self.args.push(arg);
        self
    }

    fn set_of(mut self, ty: ColumnType) -> Self {
        self.shape = ReturnShape::SetOf;
        self.ty = Some(ty);
        self
    }

    fn returns(mut self, ty: ColumnType) -> Self {
        self.ty = Some(ty);
        self
    }

    fn read_only(mut self) -> Self {
        self.opts = PlCallOptions { may_write: false };
        self
    }

    fn prepared(&self, core: &EngineCore) -> (crate::plpgsql::Body, ReturnSpec) {
        let opts = ParseOptions::new(
            self.params
                .iter()
                .map(|(n, t)| ParamDecl {
                    name: Some((*n).to_string()),
                    ty: t.clone(),
                })
                .collect(),
            self.shape,
        );
        let mut body = parse_body(self.src, &opts).expect("parse body");
        validate_embedded(&mut body, core).expect("validate body");
        (
            body,
            ReturnSpec {
                shape: self.shape,
                ty: self.ty.clone(),
            },
        )
    }

    /// 開いている Tx（明示・暗黙）へ参加して実行する。
    fn in_txn<'e>(
        &self,
        core: &'e EngineCore,
        c: &PolicyContext,
        s: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
    ) -> Result<CallOutcome, SqlSurfaceError> {
        let (body, ret) = self.prepared(core);
        core.run_plpgsql_in_txn(
            c,
            s,
            txn,
            PlInvocation {
                body: &body,
                args: self.args.clone(),
                ret: &ret,
                opts: self.opts,
            },
        )
    }

    /// autocommit の入口で実行する。
    fn autocommit(
        &self,
        core: &EngineCore,
        c: &PolicyContext,
    ) -> Result<CallOutcome, SqlSurfaceError> {
        let (body, ret) = self.prepared(core);
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.run_plpgsql_autocommit(
            c,
            &mut s,
            &mut txn,
            PlInvocation {
                body: &body,
                args: self.args.clone(),
                ret: &ret,
                opts: self.opts,
            },
        )
    }
}

const INSERT_P: &str =
    "INSERT INTO docs (id, embedding, lang, n) VALUES (p, '[0.1,0.2]', 'body', 7)";

// ---- 受入基準: 呼び出し元と一緒にコミット／ロールバックされる ----

#[test]
fn body_dml_rolls_back_and_commits_with_the_caller_transaction() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut s,
        &mut txn,
        "INSERT INTO docs (id, embedding, lang, n) VALUES (1, '[0.1,0.2]', 'outer', 1)",
    )
    .expect("outer insert");
    let src = format!("BEGIN {INSERT_P}; UPDATE docs SET n = p WHERE id = 1; RETURN 1; END");
    let out = call(&src)
        .param("p", SqlColumnTypeName::Integer, int(2))
        .in_txn(&core, &c, &mut s, &mut txn)
        .expect("body");
    assert_eq!(as_i64(&out), 1);
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &c), 0, "outer and body rows both rolled back");

    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut s,
        &mut txn,
        "INSERT INTO docs (id, embedding, lang, n) VALUES (1, '[0.1,0.2]', 'outer', 1)",
    )
    .expect("outer insert");
    call(&src)
        .param("p", SqlColumnTypeName::Integer, int(2))
        .in_txn(&core, &c, &mut s, &mut txn)
        .expect("body");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    // 別セッションから外側・本体の両方が見える。
    assert_eq!(count(&core, &c), 2);
}

#[test]
fn body_error_fails_the_explicit_transaction() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let src = format!(
        "DECLARE v int; BEGIN {INSERT_P}; SELECT n INTO STRICT v FROM docs WHERE id = 999; RETURN v; END"
    );
    let e = call(&src)
        .param("p", SqlColumnTypeName::Integer, int(5))
        .in_txn(&core, &c, &mut s, &mut txn)
        .expect_err("STRICT without rows");
    assert_eq!(e.wire_code(), "P0002");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT COUNT(*) FROM docs")
        .expect_err("failed transaction");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &c), 0);
}

#[test]
fn autocommit_commits_on_success_and_leaves_nothing_on_error() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let ok = format!("BEGIN {INSERT_P}; RETURN 1; END");
    call(&ok)
        .param("p", SqlColumnTypeName::Integer, int(1))
        .autocommit(&core, &c)
        .expect("autocommit body");
    assert_eq!(count(&core, &c), 1);

    let bad = format!("DECLARE v int; BEGIN {INSERT_P}; SELECT n INTO STRICT v FROM docs WHERE id = 999; RETURN v; END");
    let e = call(&bad)
        .param("p", SqlColumnTypeName::Integer, int(2))
        .autocommit(&core, &c)
        .expect_err("error");
    assert_eq!(e.wire_code(), "P0002");
    assert_eq!(count(&core, &c), 1, "the failed body left no side effect");
    // 暗黙 Tx が残っていない（次の書き込みがゲート待ちにならない）。
    seed(&core, &c, &[50], "after");
    assert_eq!(count(&core, &c), 2);
}

#[test]
fn idle_transaction_is_rejected_by_the_in_txn_entry() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    let e = call("BEGIN RETURN 1; END")
        .in_txn(&core, &c, &mut s, &mut txn)
        .expect_err("idle");
    assert_eq!(e.wire_code(), "XX000");
}

// ---- 同一 Tx の読み取り・FOUND・ROW_COUNT ----

#[test]
fn select_after_insert_sees_uncommitted_row() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let src = format!(
        "DECLARE c bigint; BEGIN {INSERT_P}; SELECT count(*) INTO c FROM docs; RETURN c; END"
    );
    let out = call(&src)
        .param("p", SqlColumnTypeName::Integer, int(1))
        .autocommit(&core, &c)
        .expect("body");
    assert_eq!(as_i64(&out), 1);
}

#[test]
fn row_count_and_found_are_measured() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2, 3], "en");
    let out = call(
        "DECLARE rc int; BEGIN UPDATE docs SET n = 5 WHERE lang = 'en'; GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END",
    )
    .autocommit(&core, &c)
    .expect("update");
    assert_eq!(as_i64(&out), 3);
    let found = call("BEGIN UPDATE docs SET n = 5 WHERE lang = 'en'; RETURN FOUND; END");
    let found = Call {
        ty: Some(ColumnType::Boolean),
        ..found
    };
    let out = found.autocommit(&core, &c).expect("found");
    assert!(matches!(
        out,
        CallOutcome::Scalar(PlScalar {
            cell: Cell::Bool(true),
            ..
        })
    ));
    let none = Call {
        ty: Some(ColumnType::Boolean),
        ..call("BEGIN UPDATE docs SET n = 5 WHERE lang = 'zz'; RETURN FOUND; END")
    };
    let out = none.autocommit(&core, &c).expect("no rows");
    assert!(matches!(
        out,
        CallOutcome::Scalar(PlScalar {
            cell: Cell::Bool(false),
            ..
        })
    ));
}

#[test]
fn select_into_without_rows_clears_found_and_perform_counts_rows() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2], "en");
    let src = "DECLARE v int; rc int; BEGIN \
        SELECT n INTO v FROM docs WHERE id = 999; \
        IF FOUND THEN RETURN -1; END IF; \
        PERFORM 1 FROM docs WHERE lang = 'en'; \
        GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END";
    let out = call(src).autocommit(&core, &c).expect("body");
    assert_eq!(as_i64(&out), 2);
}

#[test]
fn delete_returning_into_is_strict_about_multiple_rows() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2], "en");
    let e = call(
        "DECLARE v int; BEGIN DELETE FROM docs WHERE lang = 'en' RETURNING n INTO v; RETURN v; END",
    )
    .autocommit(&core, &c)
    .expect_err("two rows");
    assert_eq!(e.wire_code(), "P0003");
    assert_eq!(count(&core, &c), 2, "rolled back");
    let out = call(
        "DECLARE v int; BEGIN DELETE FROM docs WHERE id = 1 RETURNING n INTO v; RETURN v; END",
    )
    .autocommit(&core, &c)
    .expect("one row");
    assert_eq!(as_i64(&out), 0);
    assert_eq!(count(&core, &c), 1);
}

#[test]
fn for_query_and_return_query_iterate_rows() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2, 3], "en");
    let out = call(
        "DECLARE total int := 0; r record; BEGIN \
         FOR r IN SELECT n FROM docs LIMIT 10 LOOP total := total + 1; END LOOP; RETURN total; END",
    )
    .autocommit(&core, &c)
    .expect("for");
    assert_eq!(as_i64(&out), 3);
    let out = call("BEGIN RETURN QUERY SELECT id FROM docs LIMIT 10; END")
        .set_of(ColumnType::BigInt)
        .autocommit(&core, &c)
        .expect("return query");
    let CallOutcome::Rows(rows) = out else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 3);
}

#[test]
fn select_item_bare_keeps_the_output_column_name() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1], "en");
    let src = "DECLARE tag text := 'zz'; t text; r record; BEGIN \
        FOR r IN SELECT tag FROM docs LIMIT 10 LOOP t := r.tag; END LOOP; \
        RETURN t; END";
    let out = Call {
        ty: Some(ColumnType::Text),
        ..call(src)
    }
    .autocommit(&core, &c)
    .expect("body");
    assert!(
        matches!(out, CallOutcome::Scalar(PlScalar { cell: Cell::Text(ref s), .. }) if s == "zz")
    );
}

// ---- 既存上限 ----

#[test]
fn result_row_limit_applies_inside_the_body() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let core = core.with_result_limits(crate::result_limits::ResultLimits::new(2));
    let c = ctx("t1");
    seed(&core, &c, &[1, 2, 3], "en");
    let e = call("BEGIN RETURN QUERY SELECT id FROM docs LIMIT 10; END")
        .set_of(ColumnType::BigInt)
        .autocommit(&core, &c)
        .expect_err("over the row cap");
    assert!(
        matches!(e.wire_code(), "54000" | "54001" | "22000"),
        "{}",
        e.wire_code()
    );
}

// ---- テナント境界 ----

#[test]
fn body_cannot_read_or_update_another_tenants_rows() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c1 = ctx("t1");
    let c2 = ctx("t2");
    seed(&core, &c2, &[50], "secret");
    let out = call(
        "DECLARE rc int; BEGIN UPDATE docs SET n = 9 WHERE id = 50; GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END",
    )
    .autocommit(&core, &c1)
    .expect("update other tenant");
    assert_eq!(as_i64(&out), 0);
    let out = call("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN c; END")
        .autocommit(&core, &c1)
        .expect("count");
    assert_eq!(as_i64(&out), 0);
    assert_eq!(count(&core, &c2), 1, "the other tenant's row is untouched");
}

#[test]
fn rowtype_of_a_missing_relation_is_indistinguishable_from_an_unknown_schema() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let a = call("DECLARE r nosuch%ROWTYPE; BEGIN RETURN 1; END")
        .autocommit(&core, &c)
        .expect_err("missing table");
    let b = call("DECLARE r other_schema.t%ROWTYPE; BEGIN RETURN 1; END")
        .autocommit(&core, &c)
        .expect_err("unknown schema");
    assert_eq!(a.wire_code(), "42P01");
    assert_eq!(b.wire_code(), "42P01");
    assert_eq!(a.to_string(), b.to_string());
}

#[test]
fn percent_type_resolves_a_column_type() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call("DECLARE x docs.n%TYPE; BEGIN x := 5; RETURN x; END")
        .autocommit(&core, &c)
        .expect("percent type");
    assert_eq!(as_i64(&out), 5);
    let e = call("DECLARE x docs.nosuch%TYPE; BEGIN RETURN 1; END")
        .autocommit(&core, &c)
        .expect_err("missing column");
    assert_eq!(e.wire_code(), "42703");
}

#[test]
fn percent_type_resolves_the_visible_id_pseudo_column() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call("DECLARE x docs.id%TYPE; BEGIN x := 5; RETURN x; END")
        .autocommit(&core, &c)
        .expect("id percent type");
    assert_eq!(as_i64(&out), 5);
}

// ---- インジェクション ----

#[test]
fn text_values_are_stored_as_values_not_sql() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1], "en");
    let evil = "x'); DELETE FROM docs; --";
    let src = "DECLARE t text; BEGIN \
        INSERT INTO docs (id, embedding, lang, n) VALUES (10, '[0.1,0.2]', p, 1); \
        SELECT lang INTO t FROM docs WHERE id = 10; RETURN t; END";
    let out = Call {
        ty: Some(ColumnType::Text),
        ..call(src).param("p", SqlColumnTypeName::Text, text(evil))
    }
    .autocommit(&core, &c)
    .expect("body");
    assert!(
        matches!(out, CallOutcome::Scalar(PlScalar { cell: Cell::Text(ref s), .. }) if s == evil)
    );
    assert_eq!(count(&core, &c), 2, "no table was touched");
}

#[test]
fn execute_using_substitutes_typed_values() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2, 3], "en");
    let src = "DECLARE rc int; BEGIN \
        EXECUTE 'UPDATE docs SET n = $1 WHERE lang = $2' USING 9, 'en'; \
        GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END";
    let out = call(src).autocommit(&core, &c).expect("execute");
    assert_eq!(as_i64(&out), 3);
    let evil = "DECLARE rc int; BEGIN \
        EXECUTE 'UPDATE docs SET lang = $1 WHERE id = 1' USING 'a''; DELETE FROM docs; --'; \
        GET DIAGNOSTICS rc = ROW_COUNT; RETURN rc; END";
    let out = call(evil).autocommit(&core, &c).expect("execute evil");
    assert_eq!(as_i64(&out), 1);
    assert_eq!(count(&core, &c), 3);
}

#[test]
fn execute_parameter_mismatch_is_a_syntax_error() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    for src in [
        "BEGIN EXECUTE 'UPDATE docs SET n = $1 WHERE id = $2' USING 1; RETURN 1; END",
        "BEGIN EXECUTE 'UPDATE docs SET n = 1 WHERE id = 1' USING 1; RETURN 1; END",
    ] {
        let e = call(src).autocommit(&core, &c).expect_err("mismatch");
        assert_eq!(e.wire_code(), "42601", "{src}");
    }
}

#[test]
fn execute_rejects_ddl_ledger_and_partitioned_clauses() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1], "en");
    for stmt in [
        "DROP TABLE docs",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID ''op-1''",
        "DELETE FROM docs WHERE lang = ''en'' PARTITIONED",
        "DELETE FROM docs WHERE lang = ''en'' PARTITIONED USING OPERATION_ID ''op-2''",
        "BEGIN",
        "COMMIT",
    ] {
        let src = format!("BEGIN EXECUTE '{stmt}'; RETURN 1; END");
        let e = call(&src).autocommit(&core, &c).expect_err(stmt);
        assert!(
            matches!(e.wire_code(), "0A000" | "42601" | "23502"),
            "{stmt}: {}",
            e.wire_code()
        );
    }
    assert_eq!(count(&core, &c), 1, "nothing was executed");
}

// ---- 台帳・揮発性ゲート ----

#[test]
fn ledgered_mode_rejects_body_writes_without_side_effects() {
    let (core, _g) = new_core(LedgerMode::Ledgered);
    let c = ctx("t1");
    let src = format!("BEGIN {INSERT_P}; RETURN 1; END");
    let e = call(&src)
        .param("p", SqlColumnTypeName::Integer, int(1))
        .autocommit(&core, &c)
        .expect_err("ledgered");
    assert_eq!(e.wire_code(), "23502");
    assert_eq!(count(&core, &c), 0);
    // 読み取りは台帳の対象外。
    call("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN c; END")
        .autocommit(&core, &c)
        .expect("read");
}

#[test]
fn non_volatile_body_cannot_write() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let src = format!("BEGIN {INSERT_P}; RETURN 1; END");
    let e = call(&src)
        .param("p", SqlColumnTypeName::Integer, int(1))
        .read_only()
        .autocommit(&core, &c)
        .expect_err("read only");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(count(&core, &c), 0);
    call("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN c; END")
        .read_only()
        .autocommit(&core, &c)
        .expect("reads are fine");
}

#[test]
fn body_may_write_is_computed_from_the_ast() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let prepared = |src: &str| call(src).prepared(&core).0;
    assert!(prepared("BEGIN INSERT INTO docs (id, embedding, lang, n) VALUES (1, '[0.1,0.2]', 'a', 1); RETURN 1; END").may_write());
    assert!(prepared("BEGIN IF 1 > 0 THEN EXECUTE 'SELECT 1'; END IF; RETURN 1; END").may_write());
    assert!(
        !prepared("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs; RETURN 1; END")
            .may_write()
    );
}

// ---- 変数名と列名の衝突 ----

#[test]
fn variable_named_like_a_column_is_ambiguous() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1], "en");
    let e = call(
        "DECLARE n int := 0; v int; BEGIN SELECT id INTO v FROM docs WHERE n = n; RETURN 1; END",
    )
    .autocommit(&core, &c)
    .expect_err("conflict");
    assert_eq!(e.wire_code(), "42702");
    // 表を参照しない式では衝突しない。
    call("DECLARE n int := 2; BEGIN RETURN n + 1; END")
        .autocommit(&core, &c)
        .expect("no relation");
}

// ---- 条件式 ----

#[test]
fn conditions_work_for_single_comparisons_and_reject_compound_forms() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call("BEGIN IF p > 0 THEN RETURN 1; END IF; RETURN 0; END")
        .param("p", SqlColumnTypeName::Integer, int(3))
        .autocommit(&core, &c)
        .expect("if");
    assert_eq!(as_i64(&out), 1);
    let out =
        call("DECLARE i int := 0; BEGIN WHILE i < 3 LOOP i := i + 1; END LOOP; RETURN i; END")
            .autocommit(&core, &c)
            .expect("while");
    assert_eq!(as_i64(&out), 3);
    let out = call(
        "DECLARE i int := 0; BEGIN LOOP i := i + 1; EXIT WHEN i >= 2; END LOOP; RETURN i; END",
    )
    .autocommit(&core, &c)
    .expect("exit when");
    assert_eq!(as_i64(&out), 2);
    let e = call("BEGIN IF p > 0 AND p < 10 THEN RETURN 1; END IF; RETURN 0; END")
        .param("p", SqlColumnTypeName::Integer, int(3))
        .autocommit(&core, &c)
        .expect_err("compound");
    assert_eq!(e.wire_code(), "0A000");
}

// ---- 値の型ごとの置換（NULL・文字列・真偽値） ----

#[test]
fn null_text_and_bool_variables_substitute_in_every_position() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2], "en");
    // NULL との比較は 0 行（`WHERE` の右辺）。
    let out = call(
        "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE lang = p; RETURN c; END",
    )
    .param(
        "p",
        SqlColumnTypeName::Text,
        PlScalar::null(ColumnType::Text),
    )
    .autocommit(&core, &c)
    .expect("null in where");
    assert_eq!(as_i64(&out), 0);
    // 文字列の等価比較（左右どちらの位置でも）。
    let out = call(
        "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE p = lang; RETURN c; END",
    )
    .param("p", SqlColumnTypeName::Text, text("en"))
    .autocommit(&core, &c)
    .expect("text on the left");
    assert_eq!(as_i64(&out), 2);
    // 投影項目の NULL・真偽値・文字列は別名つきの式になる。
    let src = "DECLARE r record; k int := 0; BEGIN \
        FOR r IN SELECT p, flag, q FROM docs LIMIT 10 LOOP k := k + 1; END LOOP; RETURN k; END";
    let out = call(src)
        .param(
            "p",
            SqlColumnTypeName::Text,
            PlScalar::null(ColumnType::Text),
        )
        .param(
            "flag",
            SqlColumnTypeName::Boolean,
            PlScalar::new(Cell::Bool(true), ColumnType::Boolean),
        )
        .param("q", SqlColumnTypeName::Text, text("it's"))
        .autocommit(&core, &c)
        .expect("select items");
    assert_eq!(as_i64(&out), 2);
}

#[test]
fn comparison_with_null_variable_yields_null_not_false() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    // `flag := (a = b)` は a が NULL なら NULL（真でも偽でもない）であり、false に潰れない。
    let src = "DECLARE flag boolean; BEGIN flag := (a = b); RETURN flag; END";
    let out = call(src)
        .returns(ColumnType::Boolean)
        .param(
            "a",
            SqlColumnTypeName::Integer,
            PlScalar::null(ColumnType::Integer),
        )
        .param("b", SqlColumnTypeName::Integer, int(3))
        .autocommit(&core, &c)
        .expect("null comparison");
    assert!(
        matches!(out, CallOutcome::Scalar(ref v) if v.is_null()),
        "{out:?}"
    );
}

#[test]
fn null_absorbing_expressions_with_null_variable_are_not_null() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    // COALESCE は NULL を吸収する（現状の SQL 表層は変数入りの IN 一覧を受理しない）ので、a が NULL でも条件は真になる。
    for cond in ["coalesce(a, 5) = 5", "3 = coalesce(a, 3)"] {
        let src = format!("BEGIN IF {cond} THEN RETURN 1; END IF; RETURN 0; END");
        let out = call(&src)
            .param(
                "a",
                SqlColumnTypeName::Integer,
                PlScalar::null(ColumnType::Integer),
            )
            .autocommit(&core, &c)
            .unwrap_or_else(|e| panic!("{cond}: {e:?}"));
        assert_eq!(as_i64(&out), 1, "{cond}");
    }
}

#[test]
fn expressions_over_variables_are_evaluated_by_the_sql_surface() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call("BEGIN RETURN p * 2 + 1; END")
        .param("p", SqlColumnTypeName::Integer, int(-4))
        .autocommit(&core, &c)
        .expect("arithmetic");
    assert_eq!(as_i64(&out), -7);
    let out = call("BEGIN RETURN p; END")
        .param("p", SqlColumnTypeName::Integer, int(-4))
        .autocommit(&core, &c)
        .expect("direct");
    assert_eq!(as_i64(&out), -4);
    // 単純形 CASE 文の比較は values_equal 経由。
    let out = call(
        "DECLARE a int := 2; BEGIN CASE a WHEN 1 THEN RETURN 10; WHEN 2, 3 THEN RETURN 20; ELSE RETURN 0; END CASE; END",
    )
    .autocommit(&core, &c)
    .expect("simple case");
    assert_eq!(as_i64(&out), 20);
}
// ---- Issue #2226 レビュー対応 ----

fn float(f: f64) -> PlScalar {
    PlScalar::new(Cell::Float(f), ColumnType::Double)
}

fn null_int() -> PlScalar {
    PlScalar::null(ColumnType::Integer)
}

#[test]
fn float_variable_keeps_its_type_in_division() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    // 3.0 が整数リテラル 3 に化けると `p / 2` は整数除算（1）になる。
    let out = call("BEGIN IF p / 2 > 1.4 THEN RETURN 1; END IF; RETURN 0; END")
        .param("p", SqlColumnTypeName::Double, float(3.0))
        .autocommit(&core, &c)
        .expect("float division");
    assert_eq!(as_i64(&out), 1);
}

#[test]
fn comparison_with_null_inside_an_expression_is_null() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let src = "DECLARE flag boolean; BEGIN flag := (a + 1 = 3); RETURN flag; END";
    let out = call(src)
        .returns(ColumnType::Boolean)
        .param("a", SqlColumnTypeName::Integer, null_int())
        .autocommit(&core, &c)
        .expect("null expression comparison");
    assert!(
        matches!(out, CallOutcome::Scalar(ref v) if v.is_null()),
        "{out:?}"
    );
    let out = call(src)
        .returns(ColumnType::Boolean)
        .param("a", SqlColumnTypeName::Integer, int(2))
        .autocommit(&core, &c)
        .expect("non-null expression comparison");
    assert!(matches!(
        out,
        CallOutcome::Scalar(PlScalar {
            cell: Cell::Bool(true),
            ..
        })
    ));
}

#[test]
fn is_null_conditions_are_supported() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    for (cond, arg, want) in [
        ("a IS NULL", null_int(), 1),
        ("a IS NULL", int(1), 0),
        ("a IS NOT NULL", int(1), 1),
        ("a IS NOT NULL", null_int(), 0),
        ("(a IS NULL)", null_int(), 1),
        ("(a IS NULL)", int(1), 0),
        ("((a) IS NOT NULL)", int(1), 1),
        ("((a) IS NOT NULL)", null_int(), 0),
    ] {
        let src = format!("BEGIN IF {cond} THEN RETURN 1; END IF; RETURN 0; END");
        let out = call(&src)
            .param("a", SqlColumnTypeName::Integer, arg)
            .autocommit(&core, &c)
            .unwrap_or_else(|e| panic!("{cond}: {e:?}"));
        assert_eq!(as_i64(&out), want, "{cond}");
    }
}

#[test]
fn bare_null_and_boolean_literals_can_be_assigned_and_returned() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call(
        "DECLARE x int := 5; BEGIN x := NULL; IF x IS NULL THEN RETURN 1; END IF; RETURN 0; END",
    )
    .autocommit(&core, &c)
    .expect("assign null");
    assert_eq!(as_i64(&out), 1);
    let out = call("BEGIN RETURN true; END")
        .returns(ColumnType::Boolean)
        .autocommit(&core, &c)
        .expect("return true");
    assert!(matches!(
        out,
        CallOutcome::Scalar(PlScalar {
            cell: Cell::Bool(true),
            ..
        })
    ));
}

#[test]
fn rowtype_includes_the_visible_id_pseudo_column() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1], "en");
    let out = call(
        "DECLARE r docs%ROWTYPE; BEGIN SELECT * INTO r FROM docs WHERE lang = 'en'; RETURN r.id; END",
    )
    .autocommit(&core, &c)
    .expect("select * into rowtype");
    assert_eq!(as_i64(&out), 1);
}

#[test]
fn repeated_large_text_reference_is_rejected_before_growing_past_the_limit() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let big = "x".repeat(crate::sql::lexer::MAX_INPUT_LEN / 3);
    let e = call(
        "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE lang = p OR lang = p OR lang = p OR lang = p; RETURN c; END",
    )
    .param("p", SqlColumnTypeName::Text, text(&big))
    .autocommit(&core, &c)
    .expect_err("too large");
    assert!(
        matches!(e.wire_code(), "54000" | "0A000" | "42601"),
        "{}",
        e.wire_code()
    );
}

#[test]
fn variable_named_id_conflicts_only_with_tables_that_have_an_id_column() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    core.storage
        .create_table(
            &TableSchema::new(
                "plain",
                vec![ColumnDef::new("v", ColumnType::Integer, true)],
            )
            .with_row_identity(crate::catalog::RowIdentity::Allocated),
        )
        .expect("create table");
    let out = call(
        "DECLARE id int := 1; c bigint; BEGIN SELECT count(*) INTO c FROM plain WHERE v = id; RETURN c; END",
    )
    .autocommit(&core, &c)
    .expect("no id column, no conflict");
    assert_eq!(as_i64(&out), 0);
    seed(&core, &c, &[1], "en");
    let e = call(
        "DECLARE id int := 1; c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE n = id; RETURN c; END",
    )
    .autocommit(&core, &c)
    .expect_err("docs has an id column");
    assert_eq!(e.wire_code(), "42702");
}

// ---- Issue #2226 レビュー対応（数値の宣言型の保持・NULL の型付け）----

fn big(i: i64) -> PlScalar {
    PlScalar::new(Cell::SignedInteger(i), ColumnType::BigInt)
}

#[test]
fn bigint_variable_keeps_its_type_at_the_int4_boundary() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    // 2147483647 が裸のリテラルだと int4 同士の演算になり範囲エラーになる。
    let out = call("BEGIN RETURN p + 1; END")
        .returns(ColumnType::BigInt)
        .param("p", SqlColumnTypeName::BigInt, big(2_147_483_647))
        .autocommit(&core, &c)
        .expect("bigint arithmetic");
    assert_eq!(as_i64(&out), 2_147_483_648);
    // 静的 SQL の投影（SELECT p INTO v）でも bigint のまま。
    let out = call("DECLARE v bigint; BEGIN SELECT p + 1 INTO v; RETURN v; END")
        .returns(ColumnType::BigInt)
        .param("p", SqlColumnTypeName::BigInt, big(4_000_000_000))
        .autocommit(&core, &c)
        .expect("bigint select into");
    assert_eq!(as_i64(&out), 4_000_000_001);
    // smallint は smallint のまま（演算結果の型は SQL 側が決める）。
    let out = call("BEGIN RETURN p + 1; END")
        .param(
            "p",
            SqlColumnTypeName::SmallInt,
            PlScalar::new(Cell::SignedInteger(7), ColumnType::SmallInt),
        )
        .autocommit(&core, &c)
        .expect("smallint");
    assert_eq!(as_i64(&out), 8);
}

#[test]
fn float_variable_select_into_keeps_the_float_type() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let out = call("DECLARE v double precision; BEGIN SELECT p INTO v; RETURN v; END")
        .returns(ColumnType::Double)
        .param("p", SqlColumnTypeName::Double, float(2.5))
        .autocommit(&core, &c)
        .expect("float select into");
    assert!(matches!(
        out,
        CallOutcome::Scalar(PlScalar { cell: Cell::Float(f), .. }) if (f - 2.5).abs() < f64::EPSILON
    ));
}

#[test]
fn execute_with_numbers_in_const_and_literal_positions() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2, 3], "en");
    // INSERT .. VALUES は定数式（CAST 不可）、IN リストはリテラル専用位置。
    let out = call(
        "DECLARE c bigint; BEGIN \
         EXECUTE 'INSERT INTO docs (id, embedding, lang, n) VALUES ($1, ''[0.1,0.2]'', ''x'', $2)' USING 9, 4; \
         EXECUTE 'UPDATE docs SET n = $1 WHERE id IN ($2, $3)' USING 5, 1, 2; \
         SELECT count(*) INTO c FROM docs WHERE n BETWEEN 4 AND 5; RETURN c; END",
    )
    .returns(ColumnType::BigInt)
    .autocommit(&core, &c)
    .expect("execute params");
    assert_eq!(as_i64(&out), 3);
}

#[test]
fn null_order_key_is_typed_not_a_column_name() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2], "en");
    // 裸の NULL は列名として解釈され 42703 になる。型付き NULL は定数キーとして受理される。
    call(
        "DECLARE c bigint; BEGIN EXECUTE 'SELECT id FROM docs ORDER BY $1' USING p; RETURN 1; END",
    )
    .param("p", SqlColumnTypeName::Integer, null_int())
    .autocommit(&core, &c)
    .expect("typed null order key");
}

fn numeric0(n: i128) -> PlScalar {
    PlScalar::new(
        Cell::Numeric(crate::numeric::Decimal::from_parts(n, 0).expect("decimal")),
        ColumnType::Numeric {
            precision: 10,
            scale: 0,
        },
    )
}

const NUMERIC0: SqlColumnTypeName = SqlColumnTypeName::Numeric {
    precision: 10,
    scale: 0,
};

/// `docs.n` の値（id 指定）。
fn n_of(core: &EngineCore, c: &PolicyContext, id: u64) -> Option<i64> {
    let out = call(&format!(
        "DECLARE v int; BEGIN SELECT n INTO v FROM docs WHERE id = {id}; RETURN v; END"
    ))
    .autocommit(core, c)
    .expect("read n");
    match out {
        CallOutcome::Scalar(v) if v.is_null() => None,
        other => Some(as_i64(&other)),
    }
}

const VALUES_HEAD: &str = "INSERT INTO docs (id, embedding, lang, n) VALUES";

/// `VALUES` の要素式 `expr`（変数 `p` を含む）を持つ本体を実行する（id 固定）。
fn insert_with(
    core: &EngineCore,
    c: &PolicyContext,
    id: u64,
    expr: &str,
    ty: SqlColumnTypeName,
    arg: PlScalar,
) -> Result<CallOutcome, SqlSurfaceError> {
    let src = format!("BEGIN {VALUES_HEAD} ({id}, '[0.1,0.2]', 'a', {expr}); RETURN 1; END");
    call(&src).param("p", ty, arg).autocommit(core, c)
}

#[test]
fn values_elements_keep_bigint_semantics() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    // 宣言型（bigint）を保つため、`2147483647 + 1` が int4 の算術として範囲エラーにならない。
    insert_with(
        &core,
        &c,
        1,
        "p + 1 - 5",
        SqlColumnTypeName::BigInt,
        big(2_147_483_647),
    )
    .expect("bigint boundary");
    assert_eq!(n_of(&core, &c, 1), Some(2_147_483_643));
    // 単独・負値。
    insert_with(
        &core,
        &c,
        2,
        "p",
        SqlColumnTypeName::BigInt,
        big(-2_147_483_648),
    )
    .expect("bigint lone");
    assert_eq!(n_of(&core, &c, 2), Some(-2_147_483_648));
    // integer の変数は int4 の算術（範囲エラーは PostgreSQL と同じ 22003）。
    let e = insert_with(
        &core,
        &c,
        3,
        "p + 1",
        SqlColumnTypeName::Integer,
        int(2_147_483_647),
    )
    .expect_err("int4 overflow");
    assert_eq!(e.wire_code(), "22003");
}

#[test]
fn values_elements_accept_null_and_float() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    insert_with(&core, &c, 1, "p", SqlColumnTypeName::Integer, null_int()).expect("null");
    assert_eq!(n_of(&core, &c, 1), None);
    insert_with(&core, &c, 2, "p", SqlColumnTypeName::Double, float(4.0)).expect("float");
    assert_eq!(n_of(&core, &c, 2), Some(4));
    insert_with(&core, &c, 3, "p * 2", SqlColumnTypeName::Double, float(2.5)).expect("float expr");
    assert_eq!(n_of(&core, &c, 3), Some(5));
}

#[test]
fn values_elements_numeric_is_lone_only() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    insert_with(&core, &c, 1, "p", NUMERIC0, numeric0(3)).expect("numeric lone");
    assert_eq!(n_of(&core, &c, 1), Some(3));
    // 静的な `-p` は定義時の検証が拒否する（定数式に変数への単項マイナスは無い。素の SQL の `-col` と同じ）。
    // 符号付きの単独要素は EXECUTE の `-$1` で確かめる（`execute_using_values_follow_the_same_substitution`）。
    // 式の中の numeric は型を保てないため拒否する（整数除算への化けを残さない）。
    for expr in ["p / 2", "p * 2"] {
        let e = insert_with(&core, &c, 9, expr, NUMERIC0, numeric0(3)).expect_err(expr);
        assert_eq!(e.wire_code(), "0A000", "{expr}");
    }
}

#[test]
fn plain_sql_values_behavior_is_unchanged() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let ins = |e: &str| {
        sql(
            &core,
            &c,
            &format!("{VALUES_HEAD} (50, '[0.1,0.2]', 'x', {e})"),
        )
    };
    // 利用者が書く SQL: int4 の算術は 22003、CAST は定数式で 0A000 のまま。
    assert_eq!(
        ins("2147483647 + 1").expect_err("overflow").wire_code(),
        "22003"
    );
    assert_eq!(
        ins("CAST(1 AS bigint)").expect_err("cast").wire_code(),
        "0A000"
    );
}

#[test]
fn in_and_between_accept_numeric_integer_form() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    seed(&core, &c, &[1, 2], "en");
    sql(&core, &c, "UPDATE docs SET n = 3 WHERE id = 1").expect("n1");
    sql(&core, &c, "UPDATE docs SET n = 8 WHERE id = 2").expect("n2");
    let out = call("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE n IN (p, 99); RETURN c; END")
        .returns(ColumnType::BigInt)
        .param("p", NUMERIC0, numeric0(3))
        .autocommit(&core, &c)
        .expect("in");
    assert_eq!(as_i64(&out), 1);
    let out = call("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM docs WHERE n BETWEEN p AND 9; RETURN c; END")
        .returns(ColumnType::BigInt)
        .param("p", NUMERIC0, numeric0(3))
        .autocommit(&core, &c)
        .expect("between");
    assert_eq!(as_i64(&out), 2);
}

#[test]
fn execute_using_values_follow_the_same_substitution() {
    let (core, _g) = new_core(LedgerMode::Optional);
    let c = ctx("t1");
    let exec = |id: u64, expr: &str, ty: SqlColumnTypeName, arg: PlScalar| {
        let src = format!(
            "BEGIN EXECUTE '{VALUES_HEAD} ({id}, ''[0.1,0.2]'', ''a'', {expr})' USING p; RETURN 1; END"
        );
        call(&src).param("p", ty, arg).autocommit(&core, &c)
    };
    exec(
        1,
        "$1 + 1 - 5",
        SqlColumnTypeName::BigInt,
        big(2_147_483_647),
    )
    .expect("bigint");
    assert_eq!(n_of(&core, &c, 1), Some(2_147_483_643));
    exec(2, "$1", SqlColumnTypeName::Integer, null_int()).expect("null");
    assert_eq!(n_of(&core, &c, 2), None);
    exec(3, "$1", SqlColumnTypeName::Double, float(4.0)).expect("float");
    assert_eq!(n_of(&core, &c, 3), Some(4));
    exec(4, "-$1", NUMERIC0, numeric0(3)).expect("numeric signed lone");
    assert_eq!(n_of(&core, &c, 4), Some(-3));
    let e = exec(9, "$1 / 2", NUMERIC0, numeric0(3)).expect_err("numeric expr");
    assert_eq!(e.wire_code(), "0A000");
    // IN・BETWEEN（リテラル専用位置）の numeric 整数形。
    let out = call(
        "DECLARE c bigint; BEGIN \
         EXECUTE 'UPDATE docs SET lang = ''z'' WHERE n IN ($1, 99)' USING q; \
         EXECUTE 'UPDATE docs SET lang = ''y'' WHERE n BETWEEN $1 AND 9' USING q; \
         SELECT count(*) INTO c FROM docs WHERE lang = 'y'; RETURN c; END",
    )
    .returns(ColumnType::BigInt)
    .param("q", NUMERIC0, numeric0(3))
    .autocommit(&core, &c)
    .expect("literal slots");
    assert_eq!(as_i64(&out), 1);
}
