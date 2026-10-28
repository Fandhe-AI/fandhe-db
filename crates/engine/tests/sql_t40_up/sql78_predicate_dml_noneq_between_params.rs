//! 述語形 `UPDATE`／`DELETE` の `WHERE` の非等価比較と `[NOT] BETWEEN` で `$n` を受理する契約
//! （Issue #2431。SELECT 側は Issue #2430 の `sql78_where_noneq_between_params.rs`）の結合テスト。
//!
//! ポインタ: SQL-78（主）・RECOVER-11・WIRE-12、TASK-336。`parse_sql_prepared` → `bind_prepared`
//! → `execute_parsed_in_session` の本番経路を実 `Storage` 越しに通し、同じ値を書いたリテラル形の
//! 解析結果・実行結果・`wire_code` と一致すること、および台帳の内容照合が代入後の文で行われる
//! ことを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn legacy(core: &EngineCore, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx("owner"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn vals(values: &[&str]) -> Vec<Option<Vec<u8>>> {
    values.iter().map(|v| Some(v.as_bytes().to_vec())).collect()
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    legacy(
        &core,
        "CREATE TABLE t (n INTEGER, s TEXT, f BOOLEAN, d DOUBLE PRECISION, dt DATE)",
    );
    for id in 1..=5 {
        legacy(
            &core,
            &format!(
                "INSERT INTO t (id, n, s, f, d, dt) VALUES ({id}, {id}, 's{id}', TRUE, {id}.5, \
                 '2024-01-0{id}') USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, CleanupGuard(path))
}

fn snapshot(core: &EngineCore) -> Vec<Vec<Cell>> {
    match legacy(core, "SELECT n, s, f, d, dt FROM t ORDER BY id LIMIT 100") {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn exec(core: &EngineCore, p: &engine::core::ParsedSql) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_parsed_in_session(&ctx("owner"), &mut session, p)
}

/// 束縛結果がリテラル形の `parse_sql` と一致し、別々の新しい表での実行結果・表の状態も一致する。
fn assert_parity(label: &str, template: &str, values: &[&str], literal: &str) {
    let (core, _g) = setup(&format!("{label}-bind"));
    let prepared = core
        .parse_sql_prepared(template)
        .unwrap_or_else(|e| panic!("{template}: {e}"));
    let bound = core
        .bind_prepared(&prepared, &vals(values))
        .unwrap_or_else(|e| panic!("{template}: {e}"));
    let lit = core.parse_sql(literal).expect("literal parse");
    assert_eq!(bound, lit, "{template}");
    let got = exec(&core, &bound).unwrap_or_else(|e| panic!("{template}: {e}"));
    let after_bound = snapshot(&core);

    let (core2, _g2) = setup(&format!("{label}-lit"));
    let want = exec(&core2, &lit).expect("literal execute");
    assert_eq!(format!("{got:?}"), format!("{want:?}"), "{template}");
    assert_eq!(after_bound, snapshot(&core2), "{template}");
}

#[test]
fn noneq_and_between_bind_matches_literal_form() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("n > $1", &["2"], "n > 2"),
        ("n < $1", &["3"], "n < 3"),
        ("n >= $1", &["3"], "n >= 3"),
        ("n <= $1", &["3"], "n <= 3"),
        ("n <> $1", &["3"], "n <> 3"),
        ("n != $1", &["3"], "n != 3"),
        ("n BETWEEN $1 AND $2", &["2", "4"], "n BETWEEN 2 AND 4"),
        (
            "n NOT BETWEEN $1 AND $2",
            &["2", "4"],
            "n NOT BETWEEN 2 AND 4",
        ),
        ("d > $1", &["2.5"], "d > 2.5"),
        ("s >= $1", &["s3"], "s >= 's3'"),
        ("dt <= $1", &["2024-01-03"], "dt <= '2024-01-03'"),
        (
            "dt BETWEEN $1 AND $2",
            &["2024-01-02", "2024-01-04"],
            "dt BETWEEN '2024-01-02' AND '2024-01-04'",
        ),
        (
            "n > $1 AND (s < $2 OR d BETWEEN $3 AND $4)",
            &["1", "s3", "4.0", "5.0"],
            "n > 1 AND (s < 's3' OR d BETWEEN 4.0 AND 5.0)",
        ),
    ];
    for (i, (pred, values, lit_pred)) in cases.iter().enumerate() {
        assert_parity(
            &format!("sql78-dml-upd-{i}"),
            &format!("UPDATE t SET s = 'x' WHERE {pred} RETURNING id, n"),
            values,
            &format!("UPDATE t SET s = 'x' WHERE {lit_pred} RETURNING id, n"),
        );
        assert_parity(
            &format!("sql78-dml-del-{i}"),
            &format!("DELETE FROM t WHERE {pred} RETURNING id"),
            values,
            &format!("DELETE FROM t WHERE {lit_pred} RETURNING id"),
        );
    }
}

#[test]
fn param_types_follow_compared_column() {
    let (core, _g) = setup("sql78-dml-types");
    for (dml, sel) in [
        (
            "UPDATE t SET s = 'x' WHERE d > $1",
            "SELECT id FROM t WHERE d > $1 LIMIT 5",
        ),
        (
            "DELETE FROM t WHERE dt BETWEEN $1 AND $2",
            "SELECT id FROM t WHERE dt BETWEEN $1 AND $2 LIMIT 5",
        ),
    ] {
        let a = core.parse_sql_prepared(dml).expect(dml);
        let b = core.parse_sql_prepared(sel).expect(sel);
        assert_eq!(
            format!("{:?}", a.param_types()),
            format!("{:?}", b.param_types()),
            "{dml}"
        );
    }
    // SET 側と WHERE 側が混在しても型を取り違えない。
    let p = core
        .parse_sql_prepared("UPDATE t SET s = $1 WHERE n > $2")
        .expect("parse");
    assert_ne!(
        format!("{:?}", p.param_types().first()),
        format!("{:?}", p.param_types().get(1))
    );
}

#[test]
fn param_pair_without_declared_type_is_42p18() {
    let (core, _g) = setup("sql78-dml-pair");
    for sql in [
        "UPDATE t SET s = 'x' WHERE $1 < $2",
        "DELETE FROM t WHERE $1 < $2",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42P18", "{sql}");
    }
}

#[test]
fn out_of_scope_shapes_stay_42601() {
    let (core, _g) = setup("sql78-dml-reject");
    for sql in [
        "UPDATE t SET s = 'x' WHERE $1 < n",
        "DELETE FROM t WHERE n BETWEEN $1 AND 2",
        "DELETE FROM t WHERE n > $1 + 1",
        "UPDATE t SET s = 'x' WHERE n > $1::int",
        "DELETE FROM t WHERE s LIKE $1",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

/// 型の合わない値は `22P02` で拒否し、値は応答に含めず、表は変わらない。
#[test]
fn mismatched_value_is_rejected_without_side_effects() {
    let (core, _g) = setup("sql78-dml-mismatch");
    let before = snapshot(&core);
    for sql in [
        "UPDATE t SET s = 'x' WHERE n > $1",
        "DELETE FROM t WHERE n BETWEEN $1 AND $2",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect(sql);
        let n = usize::from(prepared.param_count());
        let values = vec!["abc-secret"; n];
        let err = core
            .bind_prepared(&prepared, &vals(&values))
            .expect_err(sql);
        assert_eq!(err.wire_code(), "22P02", "{sql}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }
    assert_eq!(before, snapshot(&core));
}

/// 台帳の内容照合は代入後の文で行われる（RECOVER-11）。同じ op-id でリテラル形を再送すると
/// `$n` 形と同じ結果になり、別の値の `$n` 形は内容不一致（22023）になる。表は再送で変わらない。
#[test]
fn ledger_content_check_uses_substituted_statement() {
    let (core, _g) = setup("sql78-dml-ledger");
    let prepared = core
        .parse_sql_prepared("DELETE FROM t WHERE n > $1 USING OPERATION_ID 'op-a'")
        .expect("parse");
    let bound = core.bind_prepared(&prepared, &vals(&["4"])).expect("bind");
    exec(&core, &bound).expect("first");
    let after_first = snapshot(&core);

    let lit = core
        .parse_sql("DELETE FROM t WHERE n > 4 USING OPERATION_ID 'op-a'")
        .expect("lit");
    let lit_res = exec(&core, &lit);
    let other = core.bind_prepared(&prepared, &vals(&["2"])).expect("bind");
    let other_res = exec(&core, &other);
    let same_res = exec(&core, &bound);

    let code = |r: &Result<SqlOutcome, SqlSurfaceError>| r.as_ref().err().map(|e| e.wire_code());
    assert_eq!(
        code(&lit_res),
        code(&same_res),
        "literal and $n forms hash the same"
    );
    assert_eq!(code(&other_res), Some("22023"));
    assert_eq!(after_first, snapshot(&core));
}
