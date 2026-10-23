//! `core/dml_subquery_exec.rs` の単体テスト（SQL-53・RLS-10・SQL-29。Issue #1751）。
//!
//! 構文段の受理（#1750）が無い間も検証できるよう、通常の述語形 DML を parse したあとで WHERE の葉を
//! サブクエリの葉へ差し替えて実行する（#1750 のマージ後は SQL 文字列による結合テストを追加する）。
//! 他テナント（bob）の行がどの結果にも影響しないこと（RLS-10）、失敗時に副作用が残らないこと、
//! fail-closed の境界（autocommit の `WriteTarget`・`PARTITIONED`・非セッション入口・台帳）を固定する。

use super::dml_subquery_exec::where_has_subquery;
use super::{EngineCore, ParsedSql, SessionRoutedSql};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    DeleteStatement, ScalarSubqueryOp, SqlSurfaceError, ValidatedUpdateForm, WherePredicate,
};
use crate::sql::exec::Cell;
use crate::sql::lexer::{tokenize, Token};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::tenant::WriteTarget;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("dml-subquery-exec");
    let storage = Storage::open(&path).expect("open storage");
    for name in ["docs", "allowed"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                ],
            ))
            .expect("create table");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

fn seed(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, lang: &str) {
    run(
        core,
        ctx,
        &format!("INSERT INTO {table} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', '{lang}')"),
    )
    .expect("seed");
}

fn langs(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<String> {
    let mut v: Vec<String> = core
        .execute_sql(ctx, &format!("SELECT lang FROM {table} LIMIT 100"))
        .expect("scan")
        .rows
        .iter()
        .map(|r| match &r.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    v.sort();
    v
}

fn inner(sql: &str) -> Vec<Token> {
    tokenize(sql).expect("tokenize inner")
}

fn in_sub(sql: &str) -> WherePredicate {
    WherePredicate::InSubquery {
        column: "lang".to_string(),
        inner_tokens: inner(sql),
        depth: 1,
    }
}

fn exists_sub(sql: &str) -> WherePredicate {
    WherePredicate::Exists {
        inner_tokens: inner(sql),
        depth: 1,
    }
}

/// 述語形 DML を parse し、WHERE を `pred` へ差し替えた `ParsedSql` を返す。
fn with_where(core: &EngineCore, sql: &str, pred: WherePredicate) -> ParsedSql {
    let mut parsed = core.parse_sql(sql).expect("parse outer");
    match &mut parsed {
        ParsedSql::Delete(DeleteStatement::Predicate(v)) => v.where_predicates = vec![pred],
        ParsedSql::Update(ValidatedUpdateForm::Predicate(v)) => v.where_predicates = vec![pred],
        _ => panic!("not a predicate DML"),
    }
    parsed
}

fn exec(
    core: &EngineCore,
    ctx: &PolicyContext,
    parsed: &ParsedSql,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_parsed_in_session(ctx, &mut session, parsed)
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Delete(o) => o.rows_affected,
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("unexpected {other:?}"),
    }
}

const DEL: &str = "DELETE FROM docs WHERE lang = 'x'";
const INNER: &str = "SELECT lang FROM allowed LIMIT 10";

fn setup_two_tenants(core: &EngineCore) -> (PolicyContext, PolicyContext) {
    let (alice, bob) = (ctx("alice"), ctx("bob"));
    seed(core, &alice, "docs", 1, "ja");
    seed(core, &alice, "docs", 2, "en");
    seed(core, &bob, "docs", 3, "ja");
    seed(core, &bob, "docs", 4, "en");
    seed(core, &alice, "allowed", 1, "ja");
    seed(core, &bob, "allowed", 2, "en");
    (alice, bob)
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn in_subquery_ignores_other_tenant_rows_and_commits() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let parsed = with_where(&core, DEL, in_sub(INNER));
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("delete")), 1);
    // bob の allowed('en') は alice の IN に影響しない。bob の docs は不変。
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}

#[test]
fn not_in_and_exists_family_ignore_other_tenant_rows() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    // alice から見た allowed は {ja} のみ。NOT IN は en だけが対象。
    let parsed = with_where(&core, DEL, WherePredicate::Not(Box::new(in_sub(INNER))));
    exec(&core, &alice, &parsed).expect("not in");
    assert_eq!(langs(&core, &alice, "docs"), s(&["ja"]));
    // bob は allowed を 1 行持つ。NOT EXISTS は偽、EXISTS は真。
    let parsed = with_where(&core, DEL, WherePredicate::Not(Box::new(exists_sub(INNER))));
    assert_eq!(affected(exec(&core, &bob, &parsed).expect("not exists")), 0);
    assert_eq!(langs(&core, &bob, "docs").len(), 2);
    let parsed = with_where(&core, DEL, exists_sub(INNER));
    assert_eq!(affected(exec(&core, &bob, &parsed).expect("exists")), 2);
    // alice の行は bob の削除の影響を受けない。
    assert_eq!(langs(&core, &alice, "docs"), s(&["ja"]));
}

#[test]
fn false_exists_matches_nothing() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let parsed = with_where(
        &core,
        DEL,
        exists_sub("SELECT lang FROM allowed WHERE lang = 'zz' LIMIT 10"),
    );
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("exists")), 0);
    assert_eq!(langs(&core, &alice, "docs").len(), 2);
}

#[test]
fn scalar_compare_sees_only_own_rows_and_rejects_multi_row_atomically() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let scalar = || WherePredicate::ScalarSubqueryCompare {
        column: "lang".to_string(),
        op: ScalarSubqueryOp::Eq,
        inner_tokens: inner(INNER),
        depth: 1,
    };
    // alice から見た allowed は 1 行（bob の行は 21000 の件数に数えない）。
    let parsed = with_where(&core, DEL, scalar());
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("scalar")), 1);
    // 2 行以上は 21000。副作用なし。
    seed(&core, &alice, "allowed", 9, "en");
    let parsed = with_where(&core, DEL, scalar());
    let err = exec(&core, &alice, &parsed).expect_err("multi-row scalar");
    assert_eq!(err.wire_code(), "21000");
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
}

#[test]
fn self_referencing_subquery_reads_statement_start_snapshot() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    // docs 自身を内側にする。全行（ja・en）が内側に見え、全件が対象になる。
    let parsed = with_where(&core, DEL, in_sub("SELECT lang FROM docs LIMIT 10"));
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("self ref")), 2);
    assert!(langs(&core, &alice, "docs").is_empty());
}

#[test]
fn update_with_subquery_changes_only_own_matching_rows() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let parsed = with_where(
        &core,
        "UPDATE docs SET lang = 'fr' WHERE lang = 'x'",
        in_sub(INNER),
    );
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("update")), 1);
    assert_eq!(langs(&core, &alice, "docs"), s(&["en", "fr"]));
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}

#[test]
fn returning_matches_literal_form() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let literal = run(
        &core,
        &alice,
        "DELETE FROM docs WHERE lang = 'ja' RETURNING id, lang",
    )
    .expect("literal");
    // 同じ状態を作り直して、サブクエリ形と比較する。
    seed(&core, &alice, "docs", 1, "ja");
    let parsed = with_where(
        &core,
        "DELETE FROM docs WHERE lang = 'x' RETURNING id, lang",
        in_sub(INNER),
    );
    let sub = exec(&core, &alice, &parsed).expect("subquery");
    match (literal, sub) {
        (SqlOutcome::Returning(a), SqlOutcome::Returning(b)) => {
            assert_eq!(a.rows_affected, b.rows_affected);
            assert_eq!(a.result.columns.len(), b.result.columns.len());
            assert_eq!(a.result.rows.len(), b.result.rows.len());
            for (x, y) in a.result.rows.iter().zip(b.result.rows.iter()) {
                assert_eq!(x.id, y.id);
                assert_eq!(x.cells, y.cells);
            }
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// SQL-53 (5)・RECOVER-11: `USING OPERATION_ID` 付きも受理し、同一文の再送は `23505`（副作用なし）。
#[test]
fn operation_id_with_subquery_is_accepted_and_resend_is_23505() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let parsed = with_where(
        &core,
        "DELETE FROM docs WHERE lang = 'x' USING OPERATION_ID 'op-sub-1'",
        in_sub(INNER),
    );
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("first")), 1);
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
    let err = exec(&core, &alice, &parsed).expect_err("resend");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}

/// 同じ `operation_id` で内側だけ違う文は `22023`（副作用なし）。
#[test]
fn operation_id_reuse_with_different_subquery_is_22023() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let sql = "DELETE FROM docs WHERE lang = 'x' USING OPERATION_ID 'op-sub-2'";
    let first = with_where(&core, sql, in_sub(INNER));
    exec(&core, &alice, &first).expect("first");
    let other = with_where(
        &core,
        sql,
        in_sub("SELECT lang FROM allowed WHERE lang = 'en' LIMIT 10"),
    );
    let err = exec(&core, &alice, &other).expect_err("different statement");
    assert_eq!(err.wire_code(), "22023");
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
}

/// 事前照合は内側の評価より前: 再送時に内側が 21000 になる状態でも `23505`。
#[test]
fn ledger_precheck_runs_before_inner_subquery_evaluation() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let scalar = || WherePredicate::ScalarSubqueryCompare {
        column: "lang".to_string(),
        op: ScalarSubqueryOp::Eq,
        inner_tokens: inner(INNER),
        depth: 1,
    };
    let sql = "DELETE FROM docs WHERE lang = 'x' USING OPERATION_ID 'op-sub-3'";
    let parsed = with_where(&core, sql, scalar());
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("first")), 1);
    seed(&core, &alice, "allowed", 9, "en");
    let err = exec(&core, &alice, &parsed).expect_err("resend");
    assert_eq!(err.wire_code(), "23505");
}

/// 内側の結果が 0 行に変わっても再送は `23505`。
#[test]
fn resend_after_inner_result_changes_to_zero_rows_is_23505() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let sql = "DELETE FROM docs WHERE lang = 'x' USING OPERATION_ID 'op-sub-4'";
    let parsed = with_where(&core, sql, exists_sub(INNER));
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("first")), 2);
    run(&core, &alice, "DELETE FROM allowed WHERE lang = 'ja'").expect("empty inner");
    let err = exec(&core, &alice, &parsed).expect_err("resend");
    assert_eq!(err.wire_code(), "23505");
}

/// UPDATE でも再送 `23505`・別文 `22023`。
#[test]
fn update_with_subquery_operation_id_resend_and_reuse() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let sql = "UPDATE docs SET lang = 'fr' WHERE lang = 'x' USING OPERATION_ID 'op-sub-5'";
    let parsed = with_where(&core, sql, in_sub(INNER));
    assert_eq!(affected(exec(&core, &alice, &parsed).expect("first")), 1);
    assert_eq!(langs(&core, &alice, "docs"), s(&["en", "fr"]));
    assert_eq!(
        exec(&core, &alice, &parsed)
            .expect_err("resend")
            .wire_code(),
        "23505"
    );
    let other = with_where(&core, sql, exists_sub(INNER));
    assert_eq!(
        exec(&core, &alice, &other).expect_err("reuse").wire_code(),
        "22023"
    );
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}

#[test]
fn autocommit_target_with_subquery_fails_closed() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let parsed = with_where(&core, DEL, in_sub(INNER));
    let ParsedSql::Delete(DeleteStatement::Predicate(v)) = &parsed else {
        panic!("predicate delete");
    };
    let session = SessionState::default();
    let err = core
        .execute_predicate_delete_form(WriteTarget::Autocommit(&core.storage), &alice, &session, v)
        .expect_err("autocommit target");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(langs(&core, &alice, "docs").len(), 2);
}

#[test]
fn partitioned_with_subquery_is_rejected() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let parsed = with_where(
        &core,
        "DELETE FROM docs WHERE lang = 'x' USING OPERATION_ID 'p-1' PARTITIONED CHUNK 1",
        in_sub(INNER),
    );
    let err = exec(&core, &alice, &parsed).expect_err("partitioned");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(langs(&core, &alice, "docs").len(), 2);
}

#[test]
fn non_session_entry_still_rejects_subquery() {
    let (core, _g) = new_core();
    let (alice, _bob) = setup_two_tenants(&core);
    let err = core
        .execute_delete_sql(
            &alice,
            "DELETE FROM docs WHERE lang IN (SELECT lang FROM allowed LIMIT 10)",
        )
        .expect_err("non-session");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn detects_subquery_leaves_recursively() {
    let plain = WherePredicate::Equality {
        column: "a".to_string(),
        value: "x".to_string(),
    };
    assert!(!where_has_subquery(std::slice::from_ref(&plain)));
    assert!(where_has_subquery(&[in_sub(INNER)]));
    assert!(where_has_subquery(&[WherePredicate::Not(Box::new(
        in_sub(INNER)
    ))]));
    assert!(where_has_subquery(&[WherePredicate::Or(vec![
        vec![plain.clone()],
        vec![in_sub(INNER)],
    ])]));
    assert!(!where_has_subquery(&[WherePredicate::Or(vec![
        vec![plain.clone()],
        vec![plain],
    ])]));
}

/// 振り分け済み（永続側）のサブクエリつき DML を、session の現在の一時カタログ・path 世代つきで包む。
fn routed_persistent(session: &SessionState, parsed: ParsedSql) -> ParsedSql {
    ParsedSql::SessionRouted(Box::new(
        SessionRoutedSql::persistent_guarded(parsed, session.temp_tables())
            .stamped_with_path(session),
    ))
}

/// 一時表を持つセッションでも、永続表へ振り分け済みのサブクエリつき DML が受理される
/// （粗い一時表ゲートを再適用して `0A000` にしない。Issue #1751 の review 指摘）。
#[test]
fn persistent_subquery_dml_is_accepted_in_session_with_temp_table() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TEMP TABLE scratch (n INT)")
        .expect("create temp table");
    let routed = routed_persistent(&session, with_where(&core, DEL, in_sub(INNER)));
    let out = core
        .execute_parsed_in_session(&alice, &mut session, &routed)
        .expect("routed persistent DML with subquery");
    assert_eq!(affected(out), 1);
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}

/// `public` が見えない `search_path` で、解決済みの永続表のサブクエリつき DML が受理される
/// （粗いゲートの `search_path` 検査を再適用しない）。
#[test]
fn routed_subquery_dml_is_accepted_when_search_path_hides_public() {
    let (core, _g) = new_core();
    let (alice, bob) = setup_two_tenants(&core);
    let mut session = SessionState::default();
    core.execute_sql_in_session(&alice, &mut session, "SET search_path = tenant_b")
        .expect("set search_path");
    let routed = routed_persistent(&session, with_where(&core, DEL, in_sub(INNER)));
    let out = core
        .execute_parsed_in_session(&alice, &mut session, &routed)
        .expect("resolved DML with subquery");
    assert_eq!(affected(out), 1);
    assert_eq!(langs(&core, &alice, "docs"), s(&["en"]));
    assert_eq!(langs(&core, &bob, "docs"), s(&["en", "ja"]));
}
