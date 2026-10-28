//! `UPDATE`／`DELETE` の `$n` プレースホルダ（拡張クエリの Parse／Bind。Issue #1748。
//! ポインタ: SQL-50 (4)・SQL-45 (6)・WIRE-12・WIRE-24・RLS-9・RLS-10）の engine 層結合テスト。
//!
//! 固定する内容: (1) `WHERE <列> = $n`（SELECT と同じ位置規則）と `SET <列> = $n`／式内 `$n` の
//! 束縛結果が同じ値を書いたリテラル文の `parse_sql` 結果・実行結果と一致すること、
//! (2) `param_types()` の型、(3) 型の合わない値の fail-closed な拒否（値は応答に含めず、
//! 表は変わらない）、(4) SELECT と同じ規則で `42601` になる形（非等価・BETWEEN は #2431 で受理済み）、
//! (5) NULL の Bind がリテラル `= NULL` と同値、(6) テナント境界と明示トランザクション。
//! wire 層（text／binary の Bind）は `crates/wire-server/tests/wire_sql50_update_delete_param.rs`。

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

fn legacy_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
}

fn legacy(core: &EngineCore, sql: &str) -> SqlOutcome {
    legacy_as(core, "owner", sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn vals(values: &[&str]) -> Vec<Option<Vec<u8>>> {
    values.iter().map(|v| Some(v.as_bytes().to_vec())).collect()
}

/// 整数・BIGINT・TEXT・BOOLEAN・DOUBLE・UUID・DATE の列を持つ表 `t`（owner が id 1〜3 を所有）。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    legacy(
        &core,
        "CREATE TABLE t (n INTEGER, b BIGINT, s TEXT, f BOOLEAN, d DOUBLE PRECISION, \
         u UUID, dt DATE)",
    );
    for id in 1..=3 {
        legacy(
            &core,
            &format!(
                "INSERT INTO t (id, n, b, s, f, d, u, dt) VALUES ({id}, {id}, {id}00, 's{id}', \
                 TRUE, {id}.5, '00000000-0000-0000-0000-00000000000{id}', '2024-01-0{id}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    (core, CleanupGuard(path))
}

fn snapshot(core: &EngineCore) -> Vec<Vec<Cell>> {
    match legacy(
        core,
        "SELECT n, b, s, f, d, u, dt FROM t ORDER BY id LIMIT 100",
    ) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// Bind 結果が同じ値を書いたリテラル文の `parse_sql` と一致し、実行結果（影響行数・
/// RETURNING・更新後の表）も同じになること。リテラル版と束縛版を別々の新しい表で実行して比べる。
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
    let mut session = SessionState::default();
    let got = core
        .execute_parsed_in_session(&ctx("owner"), &mut session, &bound)
        .unwrap_or_else(|e| panic!("{template}: {e}"));
    let after_bound = snapshot(&core);

    let (core2, _g2) = setup(&format!("{label}-lit"));
    let mut session2 = SessionState::default();
    let want = core2
        .execute_parsed_in_session(&ctx("owner"), &mut session2, &lit)
        .expect("literal execute");
    assert_eq!(format!("{got:?}"), format!("{want:?}"), "{template}");
    assert_eq!(after_bound, snapshot(&core2), "{template}");
}

#[test]
fn update_and_delete_bind_matches_literal_form() {
    let cases: &[(&str, &[&str], &str)] = &[
        (
            "UPDATE t SET s = $1 WHERE n = $2",
            &["x", "2"],
            "UPDATE t SET s = 'x' WHERE n = 2",
        ),
        (
            "UPDATE t SET n = n + $1 WHERE s = $2 AND id = $3 RETURNING n",
            &["5", "s1", "1"],
            "UPDATE t SET n = n + 5 WHERE s = 's1' AND id = 1 RETURNING n",
        ),
        (
            "UPDATE t SET n = $1 WHERE n = $1",
            &["2"],
            "UPDATE t SET n = 2 WHERE n = 2",
        ),
        (
            "UPDATE t SET s = DEFAULT WHERE id = $1",
            &["1"],
            "UPDATE t SET s = DEFAULT WHERE id = 1",
        ),
        ("UPDATE t SET s = $1", &["z"], "UPDATE t SET s = 'z'"),
        (
            "UPDATE t SET s = $1 WHERE TRUE",
            &["z"],
            "UPDATE t SET s = 'z' WHERE TRUE",
        ),
        (
            "UPDATE t SET s = $1 WHERE b = $2",
            &["q", "200"],
            "UPDATE t SET s = 'q' WHERE b = 200",
        ),
        (
            "UPDATE t SET s = $1 WHERE f = $2",
            &["q", "true"],
            "UPDATE t SET s = 'q' WHERE f = TRUE",
        ),
        (
            "UPDATE t SET s = $1 WHERE u = $2",
            &["q", "00000000-0000-0000-0000-000000000002"],
            "UPDATE t SET s = 'q' WHERE u = '00000000-0000-0000-0000-000000000002'",
        ),
        (
            "DELETE FROM t WHERE id = $1 AND s = $2",
            &["1", "s1"],
            "DELETE FROM t WHERE id = 1 AND s = 's1'",
        ),
        (
            "DELETE FROM t WHERE id = $1 RETURNING id, s",
            &["2"],
            "DELETE FROM t WHERE id = 2 RETURNING id, s",
        ),
        (
            "DELETE FROM t WHERE n = $1 USING OPERATION_ID $2",
            &["3", "del-3"],
            "DELETE FROM t WHERE n = 3 USING OPERATION_ID 'del-3'",
        ),
        (
            "DELETE FROM t WHERE b = $1",
            &["300"],
            "DELETE FROM t WHERE b = 300",
        ),
        (
            "DELETE FROM t WHERE f = $1",
            &["t"],
            "DELETE FROM t WHERE f = TRUE",
        ),
    ];
    for (i, (template, values, literal)) in cases.iter().enumerate() {
        assert_parity(&format!("sql50-udp-parity-{i}"), template, values, literal);
    }
}

#[test]
fn param_types_follow_column_types() {
    let (core, _g) = setup("sql50-udp-types");
    for (sql, n) in [
        ("UPDATE t SET s = $1 WHERE n = $2", 2usize),
        ("UPDATE t SET n = n + $1 WHERE s = $2 AND id = $3", 3),
        ("DELETE FROM t WHERE n = $1 AND u = $2", 2),
        ("DELETE FROM t WHERE n = $1 USING OPERATION_ID $2", 2),
    ] {
        let p = core.parse_sql_prepared(sql).expect(sql);
        assert_eq!(usize::from(p.param_count()), n, "{sql}");
        assert_eq!(p.param_types().len(), n, "{sql}");
    }
    // WHERE の `$n` は SELECT と同じ列の型（Text にはならない）。
    let upd = core
        .parse_sql_prepared("UPDATE t SET s = $1 WHERE n = $2")
        .expect("parse");
    let sel = core
        .parse_sql_prepared("SELECT id FROM t WHERE n = $1 LIMIT 5")
        .expect("parse");
    assert_eq!(
        format!("{:?}", upd.param_types().get(1)),
        format!("{:?}", sel.param_types().first())
    );
    assert_ne!(
        format!("{:?}", upd.param_types().get(1)),
        format!("{:?}", upd.param_types().first())
    );
}

/// 型の合わない値は `22P02` で拒否し、値は応答に含めず、表は変わらない。
#[test]
fn mismatched_values_are_rejected_fail_closed_without_side_effects() {
    let (core, _g) = setup("sql50-udp-mismatch");
    let before = snapshot(&core);
    let cases: &[(&str, &str)] = &[
        ("UPDATE t SET s = 'x' WHERE n = $1", "abc-secret"),
        ("UPDATE t SET s = 'x' WHERE n = $1", "1.5"),
        ("UPDATE t SET s = 'x' WHERE n = $1", ""),
        ("UPDATE t SET s = 'x' WHERE f = $1", "maybe-secret"),
        ("DELETE FROM t WHERE n = $1", "abc-secret"),
        ("DELETE FROM t WHERE b = $1", "1.5"),
        ("DELETE FROM t WHERE f = $1", "maybe-secret"),
        ("DELETE FROM t WHERE id = $1", "abc-secret"),
    ];
    for (sql, v) in cases {
        let prepared = core.parse_sql_prepared(sql).expect(sql);
        let err = core.bind_prepared(&prepared, &vals(&[v])).expect_err(sql);
        assert_eq!(err.wire_code(), "22P02", "{sql} <- {v:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }
    assert_eq!(before, snapshot(&core));
}

/// SELECT の WHERE と同じ規則: 許可位置以外の `$n` は UPDATE／DELETE でも `42601`。
/// 非等価・BETWEEN の `$n` は #2430 で SELECT が受理側へ移ったため、別テスト
/// `noneq_and_between_positions_match_select` で SELECT との一致を固定する。
#[test]
fn unsupported_positions_match_select_rule() {
    let (core, _g) = setup("sql50-udp-rules");
    for pred in [
        "t.n = $1",
        "n IN ($1)",
        "$1 = n",
        "s LIKE $1",
        "n = $1::int",
    ] {
        let sel = core
            .parse_sql_prepared(&format!("SELECT id FROM t WHERE {pred} LIMIT 5"))
            .err()
            .map(|e| e.wire_code().to_string());
        for sql in [
            format!("UPDATE t SET s = 'x' WHERE {pred}"),
            format!("DELETE FROM t WHERE {pred}"),
        ] {
            let got = core
                .parse_sql_prepared(&sql)
                .err()
                .map(|e| e.wire_code().to_string());
            assert_eq!(got, sel, "{sql}");
            assert_eq!(got.as_deref(), Some("42601"), "{sql}");
        }
    }
}

/// 非等価・BETWEEN の `$n` は SELECT と同じく UPDATE／DELETE でも受理（#2430・#2431）。
#[test]
fn noneq_and_between_positions_match_select() {
    let (core, _g) = setup("sql50-udp-noneq");
    for pred in ["n <> $1", "n < $1", "n BETWEEN $1 AND $2"] {
        let sel = core
            .parse_sql_prepared(&format!("SELECT id FROM t WHERE {pred} LIMIT 5"))
            .err()
            .map(|e| e.wire_code().to_string());
        assert_eq!(sel, None, "SELECT {pred}");
        for sql in [
            format!("UPDATE t SET s = 'x' WHERE {pred}"),
            format!("DELETE FROM t WHERE {pred}"),
        ] {
            let got = core
                .parse_sql_prepared(&sql)
                .err()
                .map(|e| e.wire_code().to_string());
            assert_eq!(got, sel, "{sql}");
        }
    }
    // 範囲外の形（逆向き）は引き続き fail-closed。
    for sql in [
        "UPDATE t SET s = 'x' WHERE $1 < n",
        "DELETE FROM t WHERE $1 < n",
    ] {
        let got = core
            .parse_sql_prepared(sql)
            .err()
            .map(|e| e.wire_code().to_string());
        assert_eq!(got.as_deref(), Some("42601"), "{sql}");
    }
}

/// NULL の Bind はリテラル `= NULL` と同じ `wire_code`（#2439 の同値性契約の UPDATE／DELETE 版）。
#[test]
fn null_bind_matches_literal_null() {
    let (core, _g) = setup("sql50-udp-null");
    for (tmpl, lit) in [
        (
            "UPDATE t SET s = 'x' WHERE n = $1",
            "UPDATE t SET s = 'x' WHERE n = NULL",
        ),
        ("DELETE FROM t WHERE n = $1", "DELETE FROM t WHERE n = NULL"),
    ] {
        let prepared = core.parse_sql_prepared(tmpl).expect(tmpl);
        let bound = core.bind_prepared(&prepared, &[None]);
        let literal = core.parse_sql(lit);
        assert_eq!(
            bound.as_ref().err().map(|e| e.wire_code().to_string()),
            literal.as_ref().err().map(|e| e.wire_code().to_string()),
            "{tmpl}"
        );
    }
}

/// インジェクション耐性: 値は 1 個のリテラルとしてのみ扱われ、文の形が変わらない。
#[test]
fn injection_like_values_stay_single_literals() {
    let (core, _g) = setup("sql50-udp-inject");
    let before = snapshot(&core);
    for v in ["s1' OR '1'='1", "'); DELETE FROM t; --", "a\nb"] {
        for sql in [
            "UPDATE t SET n = 99 WHERE s = $1",
            "DELETE FROM t WHERE s = $1",
        ] {
            let prepared = core.parse_sql_prepared(sql).expect(sql);
            let bound = core.bind_prepared(&prepared, &vals(&[v])).expect(sql);
            let mut session = SessionState::default();
            let out = core
                .execute_parsed_in_session(&ctx("owner"), &mut session, &bound)
                .expect(sql);
            let n = match out {
                SqlOutcome::Update(o) => o.rows_affected,
                SqlOutcome::Delete(o) => o.rows_affected,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(n, 0, "{sql} <- {v:?}");
        }
    }
    assert_eq!(before, snapshot(&core));
}

/// テナント境界: 他テナントの行の id を `$n` で指定した結果は存在しない id と同じ。
#[test]
fn other_tenant_rows_look_like_missing_rows() {
    let (core, _g) = setup("sql50-udp-rls");
    let run_as_b = |sql: &str, id: &str| -> String {
        let prepared = core.parse_sql_prepared(sql).expect(sql);
        let bound = core.bind_prepared(&prepared, &vals(&[id])).expect(sql);
        let mut session = SessionState::default();
        match core.execute_parsed_in_session(&ctx("tenant-b"), &mut session, &bound) {
            Ok(o) => format!("{o:?}"),
            Err(e) => e.wire_code().to_string(),
        }
    };
    let before = snapshot(&core);
    for sql in [
        "UPDATE t SET s = 'x' WHERE id = $1",
        "DELETE FROM t WHERE id = $1",
    ] {
        assert_eq!(run_as_b(sql, "1"), run_as_b(sql, "999"), "{sql}");
    }
    assert_eq!(before, snapshot(&core));
}

/// 明示トランザクション内でも同じ結果になる。
#[test]
fn bound_statements_work_inside_explicit_transaction() {
    let (core, _g) = setup("sql50-udp-txn");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx("owner"), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let p = core
        .parse_sql_prepared("UPDATE t SET s = $1 WHERE id = $2")
        .expect("parse");
    let b = core.bind_prepared(&p, &vals(&["tx", "1"])).expect("bind");
    let out = core
        .execute_parsed_in_txn(&ctx("owner"), &mut session, &mut txn, &b)
        .expect("update in txn");
    assert!(matches!(out, SqlOutcome::Update(ref o) if o.rows_affected == 1));
    let p = core
        .parse_sql_prepared("DELETE FROM t WHERE id = $1")
        .expect("parse");
    let b = core.bind_prepared(&p, &vals(&["2"])).expect("bind");
    let out = core
        .execute_parsed_in_txn(&ctx("owner"), &mut session, &mut txn, &b)
        .expect("delete in txn");
    assert!(matches!(out, SqlOutcome::Delete(ref o) if o.rows_affected == 1));
}
