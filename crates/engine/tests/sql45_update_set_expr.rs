//! `UPDATE ... SET <col> = <式>` の右辺式（算術・列参照・組み込み関数・`$n`）受理
//! （SQL-45・Issue #1544）の結合テスト。
//!
//! 式は行ごとに**更新前の行値**に対して評価され、列型へ変換されて書き込まれる契約を、
//! 単一行形・述語形・分割実行・拡張クエリ（`$n`）・台帳（`operation_id`）・テナント境界の
//! 各経路で実 `Storage` 越しに固定する。`sql45_insert_const_expr.rs` と同じ流儀
//! （`unique_db_path`／`CleanupGuard`）。ポインタ: `docs/design/implementation-status.md`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str, limits: Option<DmlLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(limits) = limits {
        core = core.with_dml_limits(limits);
    }
    (core, CleanupGuard(path))
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("sys"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        other => panic!("expected Update outcome, got {other:?}"),
    }
}

fn rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        SqlOutcome::Returning(r) => r.result.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query or Returning outcome, got {other:?}"),
    }
}

fn cell_of(core: &EngineCore, tenant: &str, col: &str, id: u64) -> Cell {
    let r = rows(
        core,
        tenant,
        &format!("SELECT {col} FROM docs WHERE id = {id} LIMIT 10"),
    );
    assert_eq!(r.len(), 1, "id {id} must be visible to {tenant}");
    r[0][0].clone()
}

fn int_of(core: &EngineCore, tenant: &str, col: &str, id: u64) -> i64 {
    match cell_of(core, tenant, col, id) {
        Cell::SignedInteger(v) => v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

fn text_of(core: &EngineCore, tenant: &str, col: &str, id: u64) -> String {
    match cell_of(core, tenant, col, id) {
        Cell::Text(s) => s,
        other => panic!("expected text cell, got {other:?}"),
    }
}

/// alice に id 1〜3（n=10/20/30・m=1/2/3・t='a'/'b'/'c'）、bob に id 4（n=100）。
fn setup(label: &str, limits: Option<DmlLimits>) -> (EngineCore, CleanupGuard) {
    let (core, g) = new_core(label, limits);
    ddl(
        &core,
        "CREATE TABLE docs (n INTEGER, m INTEGER, t TEXT, k INTEGER NOT NULL DEFAULT 0, \
         flag BOOLEAN)",
    );
    for (id, tenant, n, m, t) in [
        (1, "alice", 10, 1, "a"),
        (2, "alice", 20, 2, "b"),
        (3, "alice", 30, 3, "c"),
        (4, "bob", 100, 4, "d"),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO docs (id, n, m, t) VALUES ({id}, {n}, {m}, '{t}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, g)
}

#[test]
fn single_row_arithmetic_column_reference_and_function() {
    let (core, _g) = setup("sql45-upd-single", None);
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE docs SET n = n + 1 WHERE id = 1 USING OPERATION_ID 'u1'"
        )),
        1
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 11);
    ok(
        &core,
        "alice",
        "UPDATE docs SET m = n WHERE id = 2 USING OPERATION_ID 'u2'",
    );
    assert_eq!(int_of(&core, "alice", "m", 2), 20);
    ok(
        &core,
        "alice",
        "UPDATE docs SET t = upper(t) WHERE id = 3 USING OPERATION_ID 'u3'",
    );
    assert_eq!(text_of(&core, "alice", "t", 3), "C");
    // 単項マイナスと括弧。
    ok(
        &core,
        "alice",
        "UPDATE docs SET n = -(n) * 2 WHERE id = 2 USING OPERATION_ID 'u4'",
    );
    assert_eq!(int_of(&core, "alice", "n", 2), -40);
    // CASE 式（選ばれた分岐だけが評価される）。
    ok(
        &core,
        "alice",
        "UPDATE docs SET n = CASE WHEN n > 15 THEN 1 ELSE 0 END WHERE id = 3 \
         USING OPERATION_ID 'u5'",
    );
    assert_eq!(int_of(&core, "alice", "n", 3), 1);
}

#[test]
fn set_expressions_see_pre_update_values_of_the_same_row() {
    let (core, _g) = setup("sql45-upd-preimage", None);
    // 単一行形: `m = n` は同じ文で更新される前の n（=10）を見る。
    ok(
        &core,
        "alice",
        "UPDATE docs SET n = 99, m = n WHERE id = 1 USING OPERATION_ID 'pre-1'",
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 99);
    assert_eq!(int_of(&core, "alice", "m", 1), 10);
    // 述語形でも同じ。宣言順を入れ替えても結果は同じ。
    ok(
        &core,
        "alice",
        "UPDATE docs SET m = n, n = 7 WHERE n > 15 USING OPERATION_ID 'pre-2'",
    );
    assert_eq!(int_of(&core, "alice", "m", 2), 20);
    assert_eq!(int_of(&core, "alice", "n", 2), 7);
    assert_eq!(int_of(&core, "alice", "m", 3), 30);
    assert_eq!(int_of(&core, "alice", "n", 3), 7);
}

#[test]
fn predicate_form_applies_per_row_and_returning_shows_new_values() {
    let (core, _g) = setup("sql45-upd-predicate", None);
    let r = rows(
        &core,
        "alice",
        "UPDATE docs SET n = n * 2 WHERE m > 1 RETURNING n USING OPERATION_ID 'pr-1'",
    );
    assert_eq!(r.len(), 2);
    let mut got: Vec<i64> = r
        .iter()
        .map(|row| match &row[0] {
            Cell::SignedInteger(v) => *v,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![40, 60]);
    assert_eq!(int_of(&core, "alice", "n", 1), 10);
    // 他テナントの行は更新されない。
    assert_eq!(int_of(&core, "bob", "n", 4), 100);
}

#[test]
fn partitioned_update_applies_each_row_exactly_once_across_chunks() {
    let (core, _g) = setup("sql45-upd-partitioned", None);
    let n = affected(ok(
        &core,
        "alice",
        "UPDATE docs SET n = n + 1 WHERE n > 0 USING OPERATION_ID 'part-1' PARTITIONED CHUNK 1",
    ));
    assert_eq!(n, 3);
    assert_eq!(int_of(&core, "alice", "n", 1), 11);
    assert_eq!(int_of(&core, "alice", "n", 2), 21);
    assert_eq!(int_of(&core, "alice", "n", 3), 31);
    assert_eq!(int_of(&core, "bob", "n", 4), 100);
}

#[test]
fn affected_rows_limit_still_applies_with_zero_side_effects() {
    let (core, _g) = setup(
        "sql45-upd-limit",
        Some(DmlLimits {
            max_affected_rows: std::num::NonZeroUsize::new(2),
            max_insert_rows_per_statement: None,
        }),
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 1 WHERE n > 0 USING OPERATION_ID 'lim-1'"
        ),
        "54000"
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 10);
    assert_eq!(int_of(&core, "alice", "n", 3), 30);
}

#[test]
fn type_and_value_errors_use_postgres_codes() {
    let (core, _g) = setup("sql45-upd-errors", None);
    // 型不一致。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET t = n + 1 WHERE id = 1 USING OPERATION_ID 'e1'"
        ),
        "42804"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = upper(t) WHERE id = 1 USING OPERATION_ID 'e2'"
        ),
        "42804"
    );
    // 整数の桁あふれ・0 除算。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n * 2147483647 WHERE id = 1 USING OPERATION_ID 'e3'"
        ),
        "22003"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n / 0 WHERE id = 1 USING OPERATION_ID 'e4'"
        ),
        "22012"
    );
    // NOT NULL 列へ NULL。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET k = NULLIF(n, n) WHERE id = 1 USING OPERATION_ID 'e5'"
        ),
        "23502"
    );
    // 組み込み以外の関数は構造的に受理しない。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = no_such_fn(n) WHERE id = 1 USING OPERATION_ID 'e6'"
        ),
        "42601"
    );
    // 疑似列・RLS 内部列は式でも SET 対象にできない。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET id = id + 1 WHERE id = 1 USING OPERATION_ID 'e7'"
        ),
        "42601"
    );
    // 失敗した文は行を変更しない。
    assert_eq!(int_of(&core, "alice", "n", 1), 10);
    // 述語形でも同じコード。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n / 0 WHERE m > 0 USING OPERATION_ID 'e8'"
        ),
        "22012"
    );
    assert_eq!(int_of(&core, "alice", "n", 2), 20);
}

#[test]
fn evaluation_errors_never_reveal_other_tenants_or_missing_rows() {
    let (core, _g) = setup("sql45-upd-tenant", None);
    // bob の行（id 4）と不存在 id に対する、評価すれば 0 除算になる式は、alice からは
    // どちらも `UPDATE 0` で区別できない（評価は所有・可視の行にだけ行う）。
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE docs SET n = n / 0 WHERE id = 4 USING OPERATION_ID 't1'"
        )),
        0
    );
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE docs SET n = n / 0 WHERE id = 999 USING OPERATION_ID 't2'"
        )),
        0
    );
    assert_eq!(int_of(&core, "bob", "n", 4), 100);
    // 一致行 0 件の述語形も同様。
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE docs SET n = n / 0 WHERE n > 1000000 USING OPERATION_ID 't3'"
        )),
        0
    );
}

#[test]
fn ledger_resend_is_idempotent_and_different_expression_is_a_content_mismatch() {
    let (core, _g) = setup("sql45-upd-ledger", None);
    let sql = "UPDATE docs SET n = n + 1 WHERE id = 1 USING OPERATION_ID 'led-1'";
    ok(&core, "alice", sql);
    assert_eq!(int_of(&core, "alice", "n", 1), 11);
    // 同じ文の再送は二重適用せず `23505`（評価値が変わっていても同一内容と判定される）。
    assert_eq!(code(&core, "alice", sql), "23505");
    assert_eq!(int_of(&core, "alice", "n", 1), 11);
    // 式違いを同じ operation_id で送ると内容不一致。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 2 WHERE id = 1 USING OPERATION_ID 'led-1'"
        ),
        "22023"
    );
    // 述語形も同様。
    let psql = "UPDATE docs SET n = n + 1 WHERE m > 0 USING OPERATION_ID 'led-2'";
    ok(&core, "alice", psql);
    assert_eq!(code(&core, "alice", psql), "23505");
    assert_eq!(int_of(&core, "alice", "n", 2), 21);
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n + 5 WHERE m > 0 USING OPERATION_ID 'led-2'"
        ),
        "22023"
    );
    // リテラル `5` と式 `5 + 0` は別内容として扱う。
    ok(
        &core,
        "alice",
        "UPDATE docs SET n = 5 WHERE id = 3 USING OPERATION_ID 'led-3'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = 5 + 0 WHERE id = 3 USING OPERATION_ID 'led-3'"
        ),
        "22023"
    );
}

#[test]
fn check_constraint_is_enforced_on_the_evaluated_value() {
    let (core, _g) = new_core("sql45-upd-check", None);
    ddl(&core, "CREATE TABLE docs (n INTEGER, CHECK (n < 100))");
    ok(
        &core,
        "alice",
        "INSERT INTO docs (id, n) VALUES (1, 50) USING OPERATION_ID 'c-seed'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = n * 3 WHERE id = 1 USING OPERATION_ID 'c-1'"
        ),
        "23514"
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 50);
}

#[test]
fn extended_query_accepts_whole_value_param_only() {
    let (core, _g) = setup("sql45-upd-prepared", None);
    let some = |s: &str| Some(s.as_bytes().to_vec());

    let prepared = core
        .parse_sql_prepared("UPDATE docs SET n = $1 WHERE id = $2 USING OPERATION_ID $3")
        .expect("whole-value $n in SET is accepted");
    assert_eq!(prepared.param_count(), 3);
    // `$1` は対象列 n（INTEGER）の型として推論される。
    assert!(matches!(
        prepared.param_types().first(),
        Some(engine::sql::params::PreparedParamType::Column(_))
    ));
    let parsed = core
        .bind_prepared(&prepared, &[some("77"), some("1"), some("prep-1")])
        .expect("bind");
    let mut session = SessionState::default();
    let outcome = core
        .execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect("execute");
    assert_eq!(affected(outcome), 1);
    assert_eq!(int_of(&core, "alice", "n", 1), 77);
}

/// Parse → Bind → Execute を通し、実行結果の影響行数を返す。
fn exec_prepared(core: &EngineCore, tenant: &str, sql: &str, values: &[&str]) -> u64 {
    let prepared = core
        .parse_sql_prepared(sql)
        .unwrap_or_else(|e| panic!("{sql} must parse, got {e:?}"));
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| Some(v.as_bytes().to_vec())).collect();
    let parsed = core
        .bind_prepared(&prepared, &binds)
        .unwrap_or_else(|e| panic!("{sql} must bind, got {e:?}"));
    let mut session = SessionState::default();
    affected(
        core.execute_parsed_in_session(&ctx(tenant), &mut session, &parsed)
            .unwrap_or_else(|e| panic!("{sql} must execute, got {e:?}")),
    )
}

fn parse_code(core: &EngineCore, sql: &str) -> String {
    core.parse_sql_prepared(sql)
        .map(|_| panic!("{sql} must fail to parse"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

/// Issue #1546: SET 右辺の式内 `$n` は演算の相手の列の型として公告され、Bind／Execute が通る。
#[test]
fn extended_query_infers_param_types_inside_set_expressions() {
    use engine::catalog::ColumnType;
    use engine::sql::exec::ColumnMeta;
    use engine::sql::params::PreparedParamType;

    let (core, _g) = setup("sql45-upd-expr-infer", None);

    let prepared = core
        .parse_sql_prepared("UPDATE docs SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3")
        .expect("expression $n in SET is accepted");
    assert_eq!(prepared.param_count(), 3);
    assert!(matches!(
        prepared.param_types().first(),
        Some(PreparedParamType::Column(ColumnMeta::Scalar {
            ty: ColumnType::Integer,
            ..
        }))
    ));
    assert_eq!(
        exec_prepared(
            &core,
            "alice",
            "UPDATE docs SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3",
            &["5", "1", "e-1"],
        ),
        1
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 15);
    // 負値（`n + -3`）も通る。
    assert_eq!(
        exec_prepared(
            &core,
            "alice",
            "UPDATE docs SET n = n + $1 WHERE id = $2 USING OPERATION_ID $3",
            &["-3", "1", "e-2"],
        ),
        1
    );
    assert_eq!(int_of(&core, "alice", "n", 1), 12);

    // 関数引数（テキスト）・CASE の兄弟枝・演算の左右どちらでも型が決まる。
    let prepared = core
        .parse_sql_prepared("UPDATE docs SET t = upper($1) WHERE id = $2 USING OPERATION_ID $3")
        .expect("upper($1)");
    assert_eq!(
        prepared.param_types().first(),
        Some(&PreparedParamType::Text)
    );
    assert_eq!(
        exec_prepared(
            &core,
            "alice",
            "UPDATE docs SET t = upper($1) WHERE id = $2 USING OPERATION_ID $3",
            &["zz", "2", "e-3"],
        ),
        1
    );
    assert_eq!(text_of(&core, "alice", "t", 2), "ZZ");
    assert_eq!(
        exec_prepared(
            &core,
            "alice",
            "UPDATE docs SET n = CASE WHEN m = $1 THEN $2 ELSE n END WHERE id = $3 \
             USING OPERATION_ID $4",
            &["3", "99", "3", "e-4"],
        ),
        1
    );
    assert_eq!(int_of(&core, "alice", "n", 3), 99);
    assert_eq!(
        exec_prepared(
            &core,
            "alice",
            "UPDATE docs SET n = $1 * 2 WHERE id = $2 USING OPERATION_ID $3",
            &["7", "2", "e-5"],
        ),
        1
    );
    assert_eq!(int_of(&core, "alice", "n", 2), 14);

    // 不正な値は型検査で `22P02`（式経路でも同じ）。
    let prepared = core
        .parse_sql_prepared("UPDATE docs SET n = n + $1 WHERE id = 1 USING OPERATION_ID 'x'")
        .expect("parse");
    let err = core
        .bind_prepared(&prepared, &[Some(b"abc".to_vec())])
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

/// Issue #1546: 型を決められない `$n` は Parse で `42P18`。構文エラーは先に報告され、
/// 表・列の有無で応答コードは変わらない。
#[test]
fn extended_query_rejects_indeterminate_set_params_with_42p18() {
    let (core, _g) = setup("sql45-upd-expr-42p18", None);
    for sql in [
        "UPDATE docs SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'x'",
        "UPDATE docs SET n = COALESCE($1, $2) WHERE id = 1 USING OPERATION_ID 'x'",
        "UPDATE docs SET n = abs($1) WHERE id = 1 USING OPERATION_ID 'x'",
        // 存在しない表・列でも判定（構文の形だけ）は変わらない。
        "UPDATE no_such SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'x'",
    ] {
        assert_eq!(parse_code(&core, sql), "42P18", "{sql}");
    }
    // 相手が列参照なら、表・列が存在しなくても `42P18` にはならない（Text に倒れ、
    // 既存の構造検証・束縛段のエラー〔`42P01` 等〕がそのまま返る）。
    assert_eq!(
        parse_code(
            &core,
            "UPDATE no_such SET n = n + $1 WHERE id = 1 USING OPERATION_ID 'x'"
        ),
        "42P01"
    );
    assert!(core
        .parse_sql_prepared("UPDATE docs SET n = no_col + $1 WHERE id = 1 USING OPERATION_ID 'x'")
        .is_ok());
    // 構文エラーは `42P18` より先に返る。
    assert_eq!(
        parse_code(
            &core,
            "UPDATE docs SET n = $1 + $2 + + WHERE id = 1 USING OPERATION_ID 'x'"
        ),
        "42601"
    );
    // 式の中で扱えない型（BOOLEAN）は fail-closed の `0A000`。
    assert_eq!(
        parse_code(
            &core,
            "UPDATE docs SET n = CASE WHEN flag = $1 THEN 1 ELSE 0 END WHERE id = 1 \
             USING OPERATION_ID 'x'"
        ),
        "0A000"
    );
    // ON CONFLICT ... DO UPDATE SET の式内 `$n` は別担当（`42601` のまま）。
    assert_eq!(
        parse_code(
            &core,
            "INSERT INTO docs (id, n) VALUES (9, 1) ON CONFLICT (id) DO UPDATE SET n = n + $1 \
             USING OPERATION_ID 'x'"
        ),
        "42601"
    );
}

/// Issue #1546: 式経路でも `operation_id` の内容照合は置換後の文で行われる。
#[test]
fn extended_query_expression_resend_is_matched_on_the_substituted_statement() {
    let (core, _g) = setup("sql45-upd-expr-ledger", None);
    let sql = "UPDATE docs SET n = n + $1 WHERE id = 1 USING OPERATION_ID 'resend-1'";
    assert_eq!(exec_prepared(&core, "alice", sql, &["5"]), 1);
    assert_eq!(int_of(&core, "alice", "n", 1), 15);

    let run_bound = |value: &str| {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        let parsed = core
            .bind_prepared(&prepared, &[Some(value.as_bytes().to_vec())])
            .expect("bind");
        let mut session = SessionState::default();
        core.execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
            .map(|_| ())
            .unwrap_err()
            .wire_code()
            .to_string()
    };
    // 同じ値の再送は内容一致、異なる値は内容不一致。
    assert_eq!(run_bound("5"), "23505");
    assert_eq!(run_bound("6"), "22023");
    assert_eq!(int_of(&core, "alice", "n", 1), 15);
}
