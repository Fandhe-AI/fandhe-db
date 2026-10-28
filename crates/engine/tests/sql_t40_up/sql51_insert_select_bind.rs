//! `INSERT ... SELECT` の parser と束縛（SQL-51・Issue #1732）の結合テスト。
//!
//! 構文・内側 SELECT の検証・列数／型の束縛が `EngineCore` の入口（`parse_sql`・Describe・
//! autocommit／明示トランザクションの実行）で一貫して働くこと、および束縛エラーでは挿入より前に
//! 拒否され行が増えないこと（fail-closed）を固定する。実行本体の e2e は `sql51_insert_select_exec.rs`。
//! `sql45_insert_const_expr.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`）。
//! ポインタ: `docs/design/insert-select.md`。

use engine::core::{EngineCore, ParsedSql};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::ColumnMeta;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, std::path::PathBuf, SessionState) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE src (id BIGINT PRIMARY KEY, a INTEGER, t TEXT)",
        "CREATE TABLE dst (id BIGINT PRIMARY KEY, a BIGINT, t TEXT)",
        "CREATE VIEW v AS SELECT id, a FROM src",
    ] {
        core.execute_sql_in_session(&ctx("alice"), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    core.execute_insert_sql(
        &ctx("alice"),
        "INSERT INTO src (id, a, t) VALUES (1, 10, 'x') USING OPERATION_ID 'seed-1'",
    )
    .expect("seed");
    (core, path, session)
}

fn code(r: Result<impl std::fmt::Debug, engine::sql::allowlist::SqlSurfaceError>) -> String {
    match r {
        Ok(v) => panic!("expected error, got {v:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn count_rows(core: &EngineCore, session: &mut SessionState, table: &str) -> usize {
    match core
        .execute_sql_in_session(
            &ctx("alice"),
            session,
            &format!("SELECT id FROM {table} LIMIT 100"),
        )
        .expect("select")
    {
        SqlOutcome::Query(r) => r.rows.len(),
        _ => panic!("expected Query"),
    }
}

#[test]
fn parse_accepts_supported_forms() {
    let (core, path, _s) = setup("sql51-parse");
    let _g = CleanupGuard(path);
    for sql in [
        "INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 10",
        // 自己参照
        "INSERT INTO dst (id, a) SELECT id, a FROM dst LIMIT 10",
        "INSERT INTO dst (id, a, t) WITH w AS (SELECT id, a, t FROM src) SELECT id, a, t FROM w LIMIT 5",
        "INSERT INTO dst (id, a) (SELECT id, a FROM src LIMIT 3) UNION (SELECT id, a FROM src LIMIT 3)",
        "INSERT INTO dst (id, a) SELECT s.id, s.a FROM src s JOIN src r ON s.id = r.id LIMIT 3",
        "INSERT INTO dst (id, a) SELECT id, a FROM src LIMIT 3 RETURNING id, a",
        "INSERT INTO dst (id, a) SELECT id, a FROM src LIMIT 3 USING OPERATION_ID 'op-1';",
        "INSERT INTO dst (id, a) SELECT id, a FROM src LIMIT 3;",
        // 列リスト省略（Issue #2613）。括弧付き問い合わせも同じ
        "INSERT INTO dst SELECT id, a, t FROM src LIMIT 3",
        "INSERT INTO dst (SELECT id, a, t FROM src LIMIT 3)",
        "INSERT INTO dst SELECT id, a FROM src LIMIT 3 RETURNING id, a",
    ] {
        let parsed = core
            .parse_sql(sql)
            .unwrap_or_else(|e| panic!("parse failed for {sql}: {e:?}"));
        assert!(
            matches!(parsed, ParsedSql::InsertSelect(_)),
            "not InsertSelect: {sql}"
        );
    }
}

#[test]
fn parse_rejects_unsupported_forms_fail_closed() {
    let (core, path, _s) = setup("sql51-reject");
    let _g = CleanupGuard(path);
    let cases: &[(&str, &str)] = &[
        // 空の問い合わせ
        ("INSERT INTO dst (id, a) RETURNING id", "42601"),
        // `DEFAULT VALUES` の後ろに問い合わせは書けない
        (
            "INSERT INTO dst DEFAULT VALUES SELECT id, a, t FROM src LIMIT 3",
            "42601",
        ),
        // 内側が SELECT でない
        (
            "INSERT INTO dst (id, a) EXPLAIN SELECT id FROM src LIMIT 3",
            "42601",
        ),
        // OVERRIDING は既存どおり 0A000
        (
            "INSERT INTO dst (id, a) OVERRIDING SYSTEM VALUE SELECT id, a FROM src LIMIT 3",
            "0A000",
        ),
        // ビューへの INSERT ... SELECT
        (
            "INSERT INTO v (id, a) SELECT id, a FROM src LIMIT 3",
            "0A000",
        ),
        // 未知の表（挿入先・内側）
        (
            "INSERT INTO nope (id, a) SELECT id, a FROM src LIMIT 3",
            "42P01",
        ),
        (
            "INSERT INTO dst (id, a) SELECT id, a FROM nope LIMIT 3",
            "42P01",
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(code(core.parse_sql(sql)), *want, "{sql}");
    }
}

#[test]
fn describe_binds_column_count_and_types() {
    let (core, path, _s) = setup("sql51-describe");
    let _g = CleanupGuard(path);
    let session = SessionState::default();
    let describe = |sql: &str| {
        let parsed = core.parse_sql(sql).expect("parse");
        core.describe_parsed_in_session(&session, &parsed)
    };
    // RETURNING の列メタは VALUES 形と同じ導出
    let metas = describe("INSERT INTO dst (id, a) SELECT id, a FROM src LIMIT 3 RETURNING id, a")
        .expect("describe")
        .expect("returning columns");
    assert_eq!(metas.len(), 2);
    assert!(matches!(&metas[1], ColumnMeta::Scalar { name, .. } if name == "a"));
    assert!(
        describe("INSERT INTO dst (id, a) SELECT id, a FROM src LIMIT 3")
            .expect("describe")
            .is_none()
    );
    // 列数不一致
    assert_eq!(
        code(describe(
            "INSERT INTO dst (id, a) SELECT id, a, t FROM src LIMIT 3"
        )),
        "42601"
    );
    assert_eq!(
        code(describe(
            "INSERT INTO dst (id, a, t) SELECT id, a FROM src LIMIT 3"
        )),
        "42601"
    );
    // 型不一致（TEXT -> BIGINT）
    assert_eq!(
        code(describe(
            "INSERT INTO dst (id, a) SELECT id, t FROM src LIMIT 3"
        )),
        "42804"
    );
    // 代入可能な拡張（INTEGER -> BIGINT）は受理
    describe("INSERT INTO dst (id, a, t) SELECT id, a, t FROM src LIMIT 3").expect("assignable");
    // 列リスト省略（Issue #2613）: 宣言順（疑似 id が先頭）へ展開される。少ない分は受理、多い分は 42601
    describe("INSERT INTO dst SELECT id, a, t FROM src LIMIT 3").expect("omitted full width");
    describe("INSERT INTO dst SELECT id, a FROM src LIMIT 3").expect("omitted narrower");
    assert_eq!(
        code(describe(
            "INSERT INTO dst SELECT id, a, t, id FROM src LIMIT 3"
        )),
        "42601"
    );
    // 展開後の位置で型照合される（2 列目 BIGINT <- TEXT は 42804）
    assert_eq!(
        code(describe("INSERT INTO dst SELECT id, t FROM src LIMIT 3")),
        "42804"
    );
    // 未知の列・重複列
    assert_eq!(
        code(describe(
            "INSERT INTO dst (id, zz) SELECT id, a FROM src LIMIT 3"
        )),
        "42703"
    );
    assert_eq!(
        code(describe(
            "INSERT INTO dst (id, id) SELECT id, a FROM src LIMIT 3"
        )),
        "22000"
    );
}

#[test]
fn execution_binds_before_inserting_and_moves_no_rows_on_bind_error() {
    let (core, path, mut session) = setup("sql51-exec");
    let _g = CleanupGuard(path);
    let before = count_rows(&core, &mut session, "dst");
    // 列数不一致は挿入より前に 42601 で拒否され、行は増えない（autocommit）
    assert_eq!(
        code(core.execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "INSERT INTO dst (id, a) SELECT id, a, t FROM src LIMIT 10 \
             USING OPERATION_ID 'op-y'",
        )),
        "42601"
    );
    assert_eq!(count_rows(&core, &mut session, "dst"), before);
    // 明示トランザクションでも同様に拒否され、行は増えない
    let mut txn = core.new_session_transaction();
    let mut tsession = SessionState::default();
    core.execute_sql_in_txn(&ctx("alice"), &mut tsession, &mut txn, "BEGIN")
        .expect("begin");
    assert_eq!(
        code(core.execute_sql_in_txn(
            &ctx("alice"),
            &mut tsession,
            &mut txn,
            "INSERT INTO dst (id, a) SELECT id, a, t FROM src LIMIT 10 USING OPERATION_ID 'op-z'",
        )),
        "42601"
    );
    let _ = core.execute_sql_in_txn(&ctx("alice"), &mut tsession, &mut txn, "ROLLBACK");
    assert_eq!(count_rows(&core, &mut session, "dst"), before);
}

#[test]
fn extended_query_parameters_are_rejected_for_omitted_column_list() {
    let (core, path, _s) = setup("sql51-params-omitted");
    let _g = CleanupGuard(path);
    let r =
        core.parse_sql_prepared("INSERT INTO dst SELECT id, a, t FROM src WHERE id = $1 LIMIT 3");
    assert_eq!(code(r), "0A000");
}

#[test]
fn extended_query_parameters_are_rejected() {
    let (core, path, _s) = setup("sql51-params");
    let _g = CleanupGuard(path);
    let r = core
        .parse_sql_prepared("INSERT INTO dst (id, a) SELECT id, a FROM src WHERE id = $1 LIMIT 3");
    assert_eq!(code(r), "0A000");
}

#[test]
fn error_bytes_do_not_depend_on_other_tenants() {
    let (core, path, mut session) = setup("sql51-tenant");
    let _g = CleanupGuard(path);
    // 同じ文を別テナントで発行しても、エラーのバイト列が一致する（存在オラクルを作らない）。
    let sql = "INSERT INTO dst (id, a) SELECT id, t FROM src LIMIT 3";
    let mut msg = |tenant: &str| {
        let e = core
            .execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .expect_err("type mismatch");
        (e.wire_code().to_string(), e.client_message())
    };
    let a = msg("alice");
    let b = msg("bob");
    assert_eq!(a, b);
}
