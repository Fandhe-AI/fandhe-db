//! 自動更新可能でないビューへの書き込み拒否（TABLE-43 (3)・Issue #1670）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43・`docs/spec/04-behavior/
//! error-format.md` ERR-6（`55000`）。
//!
//! `table18_view.rs` と同じ流儀（実 `Storage`＋`EngineCore::execute_sql_in_session`）。
//! 検証する契約: (1) 自動更新不可（評価後射影形）ビューへの `INSERT`／`UPDATE`／`DELETE` は
//! 文種別ごとの固定文言の `55000` で、行・台帳に副作用が無い (2) 自動更新可能な単純形ビューは
//! `INSERT`／`UPDATE`／`DELETE` は基表へ書き換わる（Issue #1671・#1672。`table43_view_insert.rs`・
//! `table43_view_update_delete.rs`） (3) `TRUNCATE`・matview・
//! 索引名への書き込みは `42809` のまま (4) 応答は閲覧テナントに依存しない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, tenant, lang) in [(1u64, "alice", "ja"), (2, "alice", "en"), (3, "bob", "ja")] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx(tenant),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::Text(format!("body {id}")),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("seed row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    match run(core, tenant, "SELECT id FROM docs LIMIT 100").expect("select") {
        SqlOutcome::Query(QueryResult { rows, .. }) => {
            let mut v: Vec<u64> = rows.iter().map(|r| r.id).collect();
            v.sort_unstable();
            v
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 評価後射影形ビューの各形（TABLE-43 (3) の「自動更新不可」）。
const BUFFERED_VIEWS: [(&str, &str); 6] = [
    (
        "v_limit",
        "CREATE VIEW v_limit AS SELECT * FROM docs LIMIT 5",
    ),
    (
        "v_agg",
        "CREATE VIEW v_agg AS SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang",
    ),
    (
        "v_count",
        "CREATE VIEW v_count AS SELECT COUNT(*) AS n FROM docs",
    ),
    (
        "v_union",
        "CREATE VIEW v_union AS (SELECT id FROM docs) UNION (SELECT id FROM docs)",
    ),
    (
        "v_cte",
        "CREATE VIEW v_cte AS WITH x AS (SELECT id FROM docs) SELECT id FROM x LIMIT 5",
    ),
    (
        "v_distinct",
        "CREATE VIEW v_distinct AS SELECT DISTINCT lang FROM docs",
    ),
];

fn assert_not_updatable(err: &SqlSurfaceError, text: &str) {
    assert_eq!(err.wire_code(), "55000", "{text}");
    assert_eq!(err.to_string(), text);
}

/// 自動更新不可のビューへの INSERT／UPDATE／DELETE は `55000`・固定文言で、副作用が無く、
/// `USING OPERATION_ID` を台帳に残さない（同じ ID を基表への書き込みで再利用できる）。
#[test]
fn non_updatable_views_reject_writes_with_fixed_messages() {
    let (core, _g) = open_core("t43-reject");
    for (_, sql) in BUFFERED_VIEWS {
        ddl(&core, sql);
    }
    let before = ids(&core, "alice");
    for (name, _) in BUFFERED_VIEWS {
        let e = run(
            &core,
            "alice",
            &format!("INSERT INTO {name} (id, embedding, lang, body) VALUES (200, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-t43-shared'"),
        )
        .expect_err(name);
        assert_not_updatable(&e, "cannot insert into view");
        let e = run(
            &core,
            "alice",
            &format!(
                "UPDATE {name} SET lang = 'x' WHERE id = 1 USING OPERATION_ID 'op-t43-shared'"
            ),
        )
        .expect_err(name);
        assert_not_updatable(&e, "cannot update view");
        let e = run(
            &core,
            "alice",
            &format!("DELETE FROM {name} WHERE id = 1 USING OPERATION_ID 'op-t43-shared'"),
        )
        .expect_err(name);
        assert_not_updatable(&e, "cannot delete from view");
    }
    assert_eq!(before, ids(&core, "alice"), "no side effects");
    // 台帳に残っていなければ、同じ operation_id で基表へ書き込める。
    run(
        &core,
        "alice",
        "INSERT INTO docs (id, embedding, lang, body) VALUES (200, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-t43-shared'",
    )
    .expect("operation id must not have been recorded by the rejected writes");
}

/// 単純形ビュー → 評価後射影形ビューの連鎖も自動更新不可（`55000`）。
#[test]
fn chain_over_buffered_view_is_not_updatable() {
    let (core, _g) = open_core("t43-chain");
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    ddl(&core, "CREATE VIEW s AS SELECT id FROM b");
    let e = run(
        &core,
        "alice",
        "DELETE FROM s WHERE id = 1 USING OPERATION_ID 'op-t43-chain'",
    )
    .expect_err("chain");
    assert_not_updatable(&e, "cannot delete from view");
}

/// 自動更新可能な単純形ビューへの `UPDATE`／`DELETE` は基表へ書き換わる（Issue #1672。詳細な契約は
/// `table43_view_update_delete.rs`）。`INSERT` は Issue #1671（`table43_view_insert.rs`）。
#[test]
fn simple_view_update_delete_are_rewritten_to_the_base_table() {
    let (core, _g) = open_core("t43-simple");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let before = ids(&core, "alice");
    run(
        &core,
        "alice",
        "UPDATE sv SET body = 'y' WHERE id = 1 USING OPERATION_ID 'op-t43-s2'",
    )
    .expect("update through view");
    match run(&core, "alice", "SELECT body FROM docs WHERE id = 1 LIMIT 1").expect("select") {
        engine::sql::SqlOutcome::Query(q) => match &q.rows[0].cells[0] {
            engine::sql::exec::Cell::Text(t) => assert_eq!(t, "y"),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected Query, got {other:?}"),
    }
    run(
        &core,
        "alice",
        "DELETE FROM sv WHERE id = 1 USING OPERATION_ID 'op-t43-s3'",
    )
    .expect("delete through view");
    let after = ids(&core, "alice");
    assert_eq!(
        after,
        before.into_iter().filter(|id| *id != 1).collect::<Vec<_>>()
    );
}

/// `TRUNCATE`（ビューを含む）は従来どおり `42809`。
#[test]
fn truncate_of_a_view_stays_42809() {
    let (core, _g) = open_core("t43-truncate");
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    for sql in [
        "TRUNCATE TABLE b USING OPERATION_ID 'op-t43-t1'",
        "TRUNCATE docs, b USING OPERATION_ID 'op-t43-t2'",
    ] {
        let e = run(&core, "alice", sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42809", "{sql}");
    }
    assert_eq!(ids(&core, "alice"), vec![1, 2, 3]);
}

/// 応答（コード・文言）は閲覧テナントに依存しない（判定はカタログのみに依存する）。
#[test]
fn rejection_is_identical_across_tenants() {
    let (core, _g) = open_core("t43-tenants");
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    let sql = "UPDATE b SET lang = 'x' WHERE id = 3 USING OPERATION_ID 'op-t43-x'";
    let a = run(&core, "alice", sql).expect_err("alice");
    let b = run(&core, "bob", sql).expect_err("bob");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(a.to_string(), b.to_string());
}

/// 明示トランザクション内でも `55000` で拒否される。
#[test]
fn rejection_inside_explicit_transaction() {
    let (core, _g) = open_core("t43-txn");
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "UPDATE b SET lang = 'x' WHERE id = 1 USING OPERATION_ID 'op-t43-tx'",
        )
        .expect_err("update in txn");
    assert_not_updatable(&e, "cannot update view");
}
