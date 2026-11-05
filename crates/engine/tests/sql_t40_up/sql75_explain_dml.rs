//! 述語形 UPDATE／DELETE の `EXPLAIN` 結合テスト（Issue #2050・SQL-75 (1)(2)(3)(5)(6)・SQL-27・
//! SQL-40・RLS-10 ポインタ）。
//!
//! 文を実行せず、段の木（`delete`／`update` と全走査の `scan`）を返す。行・台帳（operation_id）・
//! トランザクション状態に触れず、束縛エラーは実行したときと同じ `wire_code`、テナントによって
//! 出力が変わらない。未対応の形（単一行形・UPDATE FROM／DELETE USING・ビュー・部分クエリ・
//! 一時表・セッション無し）は固定の `42601`／`0A000`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// a(k, v)・pt(id PK, k)。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, k BIGINT)",
        "CREATE TABLE nn (k BIGINT NOT NULL)",
        "INSERT INTO nn (k) VALUES (1)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO pt (id, k) VALUES (1, 2), (2, 3)",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// 表の全内容の文字列化（副作用ゼロの検証用）。
fn snapshot(core: &EngineCore, table: &str) -> String {
    format!(
        "{:?}",
        run(core, "tenant-a", &format!("SELECT * FROM {table}")).expect("select")
    )
}

#[test]
fn delete_output_shape() {
    let (core, _g) = seeded("x75d-del");
    assert_eq!(
        lines(&core, "tenant-a", "EXPLAIN DELETE FROM a WHERE k > 1"),
        strs(&[
            "delete",
            "filter: present",
            "returning: none",
            "operation_id: none",
            "-> scan",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ])
    );
    assert_eq!(
        lines(
            &core,
            "tenant-a",
            "EXPLAIN DELETE FROM a WHERE k > 1 RETURNING k USING OPERATION_ID 'x1'",
        ),
        strs(&[
            "delete",
            "filter: present",
            "returning: present",
            "operation_id: present",
            "-> scan",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ])
    );
}

#[test]
fn update_output_shape() {
    let (core, _g) = seeded("x75d-upd");
    assert_eq!(
        lines(
            &core,
            "tenant-a",
            "EXPLAIN UPDATE a SET v = 'z', k = 2 WHERE k = 1 RETURNING k USING OPERATION_ID 'y'",
        ),
        strs(&[
            "update",
            "assignments: 2",
            "filter: present",
            "returning: present",
            "operation_id: present",
            "-> scan",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ])
    );
}

#[test]
fn declared_pk_still_reports_full_scan() {
    // DML 実行器は PK 点検索を使わず全走査する（`enumerate_dml_candidates_with`）。実経路と一致させる。
    let (core, _g) = seeded("x75d-pk");
    let out = lines(&core, "tenant-a", "EXPLAIN DELETE FROM pt WHERE id = 1");
    assert!(
        out.contains(&"   access_path: full_scan".to_string()),
        "{out:?}"
    );
    assert!(!out.iter().any(|l| l.contains("primary_key")), "{out:?}");
}

#[test]
fn explain_has_no_side_effects() {
    let (core, _g) = seeded("x75d-noside");
    let before = snapshot(&core, "a");
    lines(
        &core,
        "tenant-a",
        "EXPLAIN DELETE FROM a WHERE k > 0 USING OPERATION_ID 'op1'",
    );
    lines(
        &core,
        "tenant-a",
        "EXPLAIN UPDATE a SET v = 'z' WHERE k > 0 USING OPERATION_ID 'op2'",
    );
    assert_eq!(snapshot(&core, "a"), before);
    // 台帳に記録されていないので、同じ operation_id の本実行は再利用扱い（23505）にならず成功する。
    run(
        &core,
        "tenant-a",
        "DELETE FROM a WHERE k = 1 USING OPERATION_ID 'op1'",
    )
    .expect("real delete");
    run(
        &core,
        "tenant-a",
        "UPDATE a SET v = 'q' WHERE k = 2 USING OPERATION_ID 'op2'",
    )
    .expect("real update");
}

#[test]
fn explain_in_transactions() {
    let (core, _g) = seeded("x75d-txn");
    let before = snapshot(&core, "a");
    let c = ctx("tenant-a");
    for (begin, sql) in [
        ("BEGIN READ ONLY", "EXPLAIN DELETE FROM a WHERE k > 0"),
        ("BEGIN", "EXPLAIN UPDATE a SET v = 'z' WHERE k > 0"),
    ] {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        // READ ONLY でも EXPLAIN は可（何も書かず、writer も取らない）。
        for stmt in [begin, sql, "COMMIT"] {
            core.execute_sql_in_txn(&c, &mut session, &mut txn, stmt)
                .unwrap_or_else(|e| panic!("{stmt}: {}", e.wire_code()));
        }
    }
    assert_eq!(snapshot(&core, "a"), before);
}

#[test]
fn errors_match_execution() {
    let (core, _g) = seeded("x75d-err");
    for sql in [
        "DELETE FROM nope WHERE k = 1",
        "DELETE FROM a WHERE nocol = 1",
        "DELETE FROM a WHERE k > 0 RETURNING nocol",
        "UPDATE a SET nocol = 1 WHERE k = 1",
        "UPDATE a SET v = 'z' WHERE nocol = 1",
        "UPDATE a SET k = 'text' WHERE k = 1",
        "UPDATE a SET v = 'z' WHERE k > 0 RETURNING nocol",
        // RETURNING の束縛は WHERE/SET より先（通常実行と同順）。NOT NULL 違反より未知列が優先される。
        "UPDATE nn SET k = NULL WHERE k > 0 RETURNING nonexistent",
    ] {
        let expected = code(&core, sql);
        assert_eq!(code(&core, &format!("EXPLAIN {sql}")), expected, "{sql}");
    }
}

#[test]
fn output_is_tenant_independent() {
    let (core, _g) = seeded("x75d-tenant");
    // tenant-b には行が無いが、出力は完全に一致する。
    for sql in [
        "EXPLAIN DELETE FROM a WHERE k > 1",
        "EXPLAIN UPDATE a SET v = 'z' WHERE k = 1",
    ] {
        assert_eq!(
            lines(&core, "tenant-a", sql),
            lines(&core, "tenant-b", sql),
            "{sql}"
        );
    }
}

#[test]
fn unsupported_forms_are_rejected_without_effects() {
    let (core, _g) = seeded("x75d-reject");
    exec_as(&core, "tenant-a", "CREATE VIEW va AS SELECT k, v FROM a");
    let before = snapshot(&core, "a");
    for (sql, expected) in [
        (
            "EXPLAIN UPDATE a SET v = 'z' FROM pt WHERE a.k = pt.k",
            "42601",
        ),
        ("EXPLAIN DELETE FROM a USING pt WHERE a.k = pt.k", "42601"),
        ("EXPLAIN DELETE FROM va WHERE k = 1", "42601"),
        (
            "EXPLAIN DELETE FROM a WHERE k IN (SELECT k FROM pt)",
            "42601",
        ),
        ("EXPLAIN DELETE FROM nope WHERE k = 1", "42P01"),
    ] {
        assert_eq!(code(&core, sql), expected, "{sql}");
    }
    assert_eq!(snapshot(&core, "a"), before);
}

#[test]
fn temp_table_is_rejected() {
    let (core, _g) = seeded("x75d-temp");
    let c = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&c, &mut session, "CREATE TEMP TABLE tt (k BIGINT)")
        .expect("temp");
    let err = core
        .execute_sql_in_session(&c, &mut session, "EXPLAIN DELETE FROM tt WHERE k = 1")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn sessionless_entry_rejects_explain_dml() {
    let (core, _g) = seeded("x75d-nosession");
    let r = core.execute_sql(&ctx("tenant-a"), "EXPLAIN DELETE FROM a WHERE k = 1");
    assert_eq!(r.expect_err("rejected").wire_code(), "42601");
}

/// `$n` の扱いは既存の EXPLAIN 前置（SELECT）と同じ規則に従う。WHERE の値位置は Parse で受理され
/// （束縛後に同じ木を返す）、SET の右辺の `$n` は実行側の UPDATE と同じく Parse で `42601`。
#[test]
fn placeholders_follow_the_existing_explain_rules() {
    let (core, _g) = seeded("x75d-param");
    assert!(core
        .parse_sql_prepared("EXPLAIN DELETE FROM a WHERE k = $1")
        .is_ok());
    let err = core
        .parse_sql_prepared("EXPLAIN UPDATE a SET v = $1 WHERE k = 1")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}
