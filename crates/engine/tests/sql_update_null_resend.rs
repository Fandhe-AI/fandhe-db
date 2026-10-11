//! NULL を含む DML の content_hash／台帳（RECOVER-10・SQL-41・Issue #1525）の結合テスト。
//! ポインタ: `docs/design/update-single-row.md`・`docs/design/implementation-status.md`。
//!
//! 同一 `operation_id` の再送で、同じ内容なら `23505`（適用済み）、NULL の有無・`'NULL'`
//! 文字列との違いなど内容が違えば `22023` に振り分けられること（fail-closed）を、
//! 単一行・述語形・分割実行・UPSERT・再オープン・0 行更新の各経路で固定する。
//! `sql_update_null_literal.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn core_at(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
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

fn seed(core: &EngineCore) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (lang TEXT, n BIGINT)",
    )
    .expect("create table");
    for id in 1..=3 {
        ok(
            core,
            "alice",
            &format!(
                "INSERT INTO docs (id, lang, n) VALUES ({id}, 'ja', {id}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
}

fn setup(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let core = core_at(&path);
    seed(&core);
    (core, CleanupGuard(path.clone()), path)
}

#[test]
fn single_row_null_resend_is_idempotent_and_distinguishes_content() {
    let (core, _g, _p) = setup("null-resend-single");
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = 'a', n = NULL WHERE id = 1 USING OPERATION_ID 'r1'",
    );
    // 同一内容の再送は適用済み。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'a', n = NULL WHERE id = 1 USING OPERATION_ID 'r1'"
        ),
        "23505"
    );
    // NULL 列を書かない文は内容違い。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'a' WHERE id = 1 USING OPERATION_ID 'r1'"
        ),
        "22023"
    );
    // NULL を別の値へ置き換えた文も内容違い。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'a', n = 0 WHERE id = 1 USING OPERATION_ID 'r1'"
        ),
        "22023"
    );
}

#[test]
fn null_and_quoted_null_text_are_distinct_for_same_operation_id() {
    let (core, _g, _p) = setup("null-resend-text");
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE id = 1 USING OPERATION_ID 'r2'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'NULL' WHERE id = 1 USING OPERATION_ID 'r2'"
        ),
        "22023"
    );
    // 内容違いの再送は副作用を持たない（行は NULL のまま）。
    let rows = match ok(
        &core,
        "alice",
        "SELECT lang FROM docs WHERE id = 1 LIMIT 10",
    ) {
        SqlOutcome::Query(r) => r.rows,
        other => panic!("expected Query, got {other:?}"),
    };
    assert!(matches!(rows[0].cells[0], Cell::Null));
}

#[test]
fn predicate_and_partitioned_null_resend() {
    let (core, _g, _p) = setup("null-resend-pred");
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = NULL WHERE lang = 'ja' USING OPERATION_ID 'p1'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = NULL WHERE lang = 'ja' USING OPERATION_ID 'p1'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET lang = 'NULL' WHERE lang = 'ja' USING OPERATION_ID 'p1'"
        ),
        "22023"
    );

    ok(
        &core,
        "alice",
        "UPDATE docs SET n = NULL WHERE n > 0 USING OPERATION_ID 'pp1' PARTITIONED CHUNK 1",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = NULL WHERE n > 0 USING OPERATION_ID 'pp1' PARTITIONED CHUNK 1"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = 0 WHERE n > 0 USING OPERATION_ID 'pp1' PARTITIONED CHUNK 1"
        ),
        "22023"
    );
}

#[test]
fn upsert_do_update_null_resend() {
    let (core, _g, _p) = setup("null-resend-upsert");
    let base = "INSERT INTO docs (id, lang, n) VALUES (1, 'en', 9) ON CONFLICT (id)";
    ok(
        &core,
        "alice",
        &format!("{base} DO UPDATE SET lang = NULL USING OPERATION_ID 'up1'"),
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("{base} DO UPDATE SET lang = NULL USING OPERATION_ID 'up1'")
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("{base} DO UPDATE SET lang = 'NULL' USING OPERATION_ID 'up1'")
        ),
        "22023"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            &format!("{base} DO NOTHING USING OPERATION_ID 'up1'")
        ),
        "22023"
    );
}

#[test]
fn null_resend_is_recognized_after_reopen() {
    let (core, _g, path) = setup("null-resend-reopen");
    ok(
        &core,
        "alice",
        "UPDATE docs SET lang = 'a', n = NULL WHERE id = 1 USING OPERATION_ID 'ro1'",
    );
    drop(core);
    let reopened = core_at(&path);
    assert_eq!(
        code(
            &reopened,
            "alice",
            "UPDATE docs SET lang = 'a', n = NULL WHERE id = 1 USING OPERATION_ID 'ro1'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &reopened,
            "alice",
            "UPDATE docs SET lang = 'a' WHERE id = 1 USING OPERATION_ID 'ro1'"
        ),
        "22023"
    );
}

#[test]
fn zero_row_null_update_is_still_recorded() {
    let (core, _g, _p) = setup("null-resend-zero");
    ok(
        &core,
        "alice",
        "UPDATE docs SET n = NULL WHERE id = 999 USING OPERATION_ID 'z1'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = NULL WHERE id = 999 USING OPERATION_ID 'z1'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET n = 1 WHERE id = 999 USING OPERATION_ID 'z1'"
        ),
        "22023"
    );
}
