//! 更新可能ビューへの `UPDATE`／`DELETE` の基表への書き換え（TABLE-43・SQL-49・RLS-10。
//! Issue #1672）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43。
//!
//! `table43_view_insert.rs` と同じ流儀（実 `Storage`＋`EngineCore::execute_sql_in_session`）。
//! 検証する契約: (1) ビューの外の行は更新・削除されない（ビュー述語は利用者の述語に AND 合成）
//! (2) テナント境界は基表と同じ (3) 影響行数の上限（`max_affected_rows`）がビュー経由でも効く
//! (4) 公開されていない列は `42703`・別名で疑似列 `id` へ写る列は `0A000` (5) `RETURNING` は公開名
//! (6) 台帳はビュー経由と基表直接を区別する (7) 明示トランザクション・Parse／Execute の分離でも
//! 現在の定義で書き換わる (8) `PARTITIONED` は `0A000`・自動更新不可は `55000`。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::parser::DmlLimits;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

fn open_core_with(label: &str, limits: Option<DmlLimits>) -> (EngineCore, CleanupGuard) {
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
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(limits) = limits {
        core = core.with_dml_limits(limits);
    }
    (core, guard)
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    open_core_with(label, None)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query for {sql}, got {other:?}"),
    }
}

fn ids(core: &EngineCore, tenant: &str, table: &str) -> Vec<u64> {
    let mut v: Vec<u64> = query(core, tenant, &format!("SELECT id FROM {table} LIMIT 100"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

fn text(cell: &Cell) -> String {
    match cell {
        Cell::Text(s) => s.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn returning(outcome: SqlOutcome) -> engine::sql::exec::ReturningOutcome {
    match outcome {
        SqlOutcome::Returning(r) => r,
        other => panic!("expected Returning, got {other:?}"),
    }
}

fn column_names(columns: &[ColumnMeta]) -> Vec<String> {
    columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. } => name.clone(),
        })
        .collect()
}

/// 基表 `docs` の全行（id 順）の本文を、alice の可視範囲で返す。
fn bodies(core: &EngineCore, tenant: &str) -> Vec<(u64, String)> {
    let q = query(core, tenant, "SELECT body FROM docs LIMIT 100");
    let mut v: Vec<(u64, String)> = q.rows.iter().map(|r| (r.id, text(&r.cells[0]))).collect();
    v.sort();
    v
}

fn affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Update(o) => o.rows_affected,
        SqlOutcome::Delete(o) => o.rows_affected,
        other => panic!("expected Update/Delete, got {other:?}"),
    }
}

/// ビューの外の行（`lang <> 'ja'`・他テナントの行）は UPDATE でも DELETE でも変わらない。
#[test]
fn rows_outside_the_view_are_untouched() {
    let (core, _g) = open_core("t43u-outside");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let before = bodies(&core, "alice");
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE sv SET body = 'y' WHERE body LIKE 'body%' USING OPERATION_ID 'op-u-1'"
        )),
        1
    );
    // alice の ja 行（id 1）だけが変わる。en 行（id 2）と bob の行（id 3）は同じ。
    let after = bodies(&core, "alice");
    assert_eq!(
        after,
        vec![
            (1, "y".to_string()),
            (2, "body 2".to_string()),
            (3, "body 3".to_string())
        ]
    );
    assert_ne!(before, after);

    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM sv WHERE body LIKE '%' USING OPERATION_ID 'op-u-2'"
        )),
        1
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![2, 3]);
}

/// 単一行形 `WHERE id = <n>`: ビューの外の id は 0 行、ビューの中の id は 1 行。
#[test]
fn single_row_form_respects_the_view_predicate() {
    let (core, _g) = open_core("t43u-single");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    // id 2 は en 行（ビューの外）。
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE sv SET body = 'y' WHERE id = 2 USING OPERATION_ID 'op-s-1'"
        )),
        0
    );
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM sv WHERE id = 2 USING OPERATION_ID 'op-s-2'"
        )),
        0
    );
    assert_eq!(bodies(&core, "alice")[1], (2, "body 2".to_string()));
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
    // id 1 はビューの中。
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE sv SET body = 'z' WHERE id = 1 USING OPERATION_ID 'op-s-3'"
        )),
        1
    );
    assert_eq!(bodies(&core, "alice")[0], (1, "z".to_string()));
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "DELETE FROM sv WHERE id = 1 USING OPERATION_ID 'op-s-4'"
        )),
        1
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![2, 3]);
}

/// テナント境界は基表と同じ。bob のビュー経由 DELETE は alice の行に触れない。
#[test]
fn tenant_boundary_is_the_same_as_the_base_table() {
    let (core, _g) = open_core("t43u-tenant");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    assert_eq!(
        affected(ok(
            &core,
            "bob",
            "DELETE FROM sv WHERE lang = 'ja' USING OPERATION_ID 'op-t-1'"
        )),
        1
    );
    // bob の行（id 3）だけが消え、alice の行は残る。
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2]);
    assert_eq!(
        affected(ok(
            &core,
            "bob",
            "UPDATE sv SET body = 'hijack' WHERE lang = 'ja' USING OPERATION_ID 'op-t-2'"
        )),
        0
    );
    assert_eq!(
        bodies(&core, "alice"),
        vec![(1, "body 1".to_string()), (2, "body 2".to_string())]
    );

    // 拒否の応答は閲覧テナントに依存しない。
    let sql = "UPDATE sv SET body = 'x' WHERE hidden = 'a' USING OPERATION_ID 'op-t-3'";
    let a = run(&core, "alice", sql).expect_err("alice");
    let b = run(&core, "bob", sql).expect_err("bob");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(a.to_string(), b.to_string());
}

/// 影響行数の上限（`max_affected_rows`）はビュー経由の対象集合で数える。
#[test]
fn max_affected_rows_applies_through_views() {
    let limits = DmlLimits {
        max_affected_rows: std::num::NonZeroUsize::new(2),
        max_insert_rows_per_statement: None,
    };
    let (core, _g) = open_core_with("t43u-limit", Some(limits));
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO docs (id, embedding, lang, body) VALUES (10, '[1,1]', 'ja', 'body 10'), (11, '[1,1]', 'ja', 'body 11') USING OPERATION_ID 'op-l-0'",
    );
    // alice の ja 行は 1・10・11 の 3 行 → 上限 2 を超える。副作用は無い。
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE sv SET body = 'y' WHERE body LIKE 'body%' USING OPERATION_ID 'op-l-1'"
        ),
        "54000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM sv WHERE body LIKE 'body%' USING OPERATION_ID 'op-l-2'"
        ),
        "54000"
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3, 10, 11]);
    // ビューの中で 2 行（10・11）に絞れば成功する（基表全体では 4 行該当するので、上限はビュー経由の
    // 対象集合で数えられている）。
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE sv SET body = 'z' WHERE id > 5 USING OPERATION_ID 'op-l-3'"
        )),
        2
    );
}

/// 列スコープ: 非公開列は WHERE・SET・SET の式・RETURNING のどれでも `42703`。副作用は無い。
#[test]
fn hidden_columns_are_rejected_in_every_position() {
    let (core, _g) = open_core("t43u-hidden");
    ddl(&core, "CREATE VIEW slim AS SELECT id, body FROM docs");
    let before = bodies(&core, "alice");
    for sql in [
        "UPDATE slim SET body = 'x' WHERE lang = 'ja' USING OPERATION_ID 'op-h-1'",
        "UPDATE slim SET lang = 'x' WHERE body LIKE 'b%' USING OPERATION_ID 'op-h-2'",
        "UPDATE slim SET body = lang || 'x' WHERE body LIKE 'b%' USING OPERATION_ID 'op-h-3'",
        "UPDATE slim SET body = 'x' WHERE body LIKE 'b%' RETURNING lang USING OPERATION_ID 'op-h-4'",
        "DELETE FROM slim WHERE lang = 'ja' USING OPERATION_ID 'op-h-5'",
        "DELETE FROM slim WHERE body LIKE 'b%' RETURNING lang USING OPERATION_ID 'op-h-6'",
    ] {
        assert_eq!(code(&core, "alice", sql), "42703", "{sql}");
    }
    assert_eq!(before, bodies(&core, "alice"));
}

/// 列別名ビューは公開名で書ける。別名で `id` へ写る公開名は `0A000`。`id` を公開しないビューで
/// `WHERE id = n` は `42703`。
#[test]
fn aliased_views_use_public_names() {
    let (core, _g) = open_core("t43u-alias");
    ddl(
        &core,
        "CREATE VIEW av (doc_id, language, text_body) AS SELECT id, lang, body FROM docs",
    );
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE av SET text_body = 'renamed' WHERE language = 'ja' USING OPERATION_ID 'op-a-1'"
        )),
        1
    );
    assert_eq!(bodies(&core, "alice")[0], (1, "renamed".to_string()));
    for sql in [
        "UPDATE av SET text_body = 'x' WHERE doc_id > 0 USING OPERATION_ID 'op-a-2'",
        "DELETE FROM av WHERE doc_id > 0 USING OPERATION_ID 'op-a-3'",
    ] {
        assert_eq!(code(&core, "alice", sql), "0A000", "{sql}");
    }
    ddl(&core, "CREATE VIEW noid AS SELECT body FROM docs");
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM noid WHERE id = 1 USING OPERATION_ID 'op-a-4'"
        ),
        "42703"
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
}

/// `RETURNING`: UPDATE は更新後、DELETE は削除前の値を公開名で返し、Describe の列メタも同じ。
#[test]
fn returning_uses_public_names_and_describe_matches() {
    let (core, _g) = open_core("t43u-returning");
    ddl(
        &core,
        "CREATE VIEW nv (id, heading) AS SELECT id, body FROM docs WHERE lang = 'ja'",
    );
    let r = returning(ok(
        &core,
        "alice",
        "UPDATE nv SET heading = 'new' WHERE id = 1 RETURNING heading USING OPERATION_ID 'op-r-1'",
    ));
    assert_eq!(column_names(&r.result.columns), vec!["heading"]);
    assert_eq!(text(&r.result.rows[0].cells[0]), "new");
    let r = returning(ok(
        &core,
        "alice",
        "DELETE FROM nv WHERE id = 1 RETURNING * USING OPERATION_ID 'op-r-2'",
    ));
    assert_eq!(column_names(&r.result.columns), vec!["id", "heading"]);
    assert_eq!(text(&r.result.rows[0].cells[1]), "new");

    let session = SessionState::default();
    for (sql, expected) in [
        (
            "UPDATE nv SET heading = 'x' WHERE heading = 'a' RETURNING heading USING OPERATION_ID 'op-r-3'",
            vec!["heading"],
        ),
        (
            "DELETE FROM nv WHERE heading = 'a' RETURNING * USING OPERATION_ID 'op-r-4'",
            vec!["id", "heading"],
        ),
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        let meta = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("columns");
        assert_eq!(column_names(&meta), expected, "{sql}");
    }
}

/// 台帳: ビュー経由の同一再送は `23505`、同じ `operation_id` を基表へ直接送ると `22023`。
#[test]
fn ledger_distinguishes_view_writes_from_direct_writes() {
    let (core, _g) = open_core("t43u-ledger");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let via_view = "UPDATE sv SET body = 'x' WHERE id = 1 USING OPERATION_ID 'op-l-1'";
    ok(&core, "alice", via_view);
    assert_eq!(code(&core, "alice", via_view), "23505");
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE docs SET body = 'x' WHERE id = 1 USING OPERATION_ID 'op-l-1'"
        ),
        "22023"
    );
}

/// 明示トランザクション: `COMMIT` で反映、`ROLLBACK` で未反映。
#[test]
fn explicit_transaction_commit_and_rollback() {
    let (core, _g) = open_core("t43u-txn");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let caller = ctx("alice");
    for (end, expected) in [("ROLLBACK", "body 1"), ("COMMIT", "changed")] {
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        core.execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            &format!("UPDATE sv SET body = 'changed' WHERE id = 1 USING OPERATION_ID 'op-x-{end}'"),
        )
        .expect("update in txn");
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, end)
            .expect("end");
        assert_eq!(bodies(&core, "alice")[0].1, expected, "{end}");
    }
}

/// Parse と Execute の間に定義が変わっても、Execute は現在の定義で書き換える。
#[test]
fn execute_uses_the_definition_current_at_execute_time() {
    let (core, _g) = open_core("t43u-split");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    let mut session = SessionState::default();

    // (1) 述語を `en` へ置換した後の Execute は新しい述語で書き換わる（id 2 が対象になる）。
    let parsed = core
        .parse_sql("UPDATE sv SET body = 'q' WHERE body LIKE 'body%' USING OPERATION_ID 'op-d-1'")
        .expect("parse");
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT * FROM docs WHERE lang = 'en'",
    );
    core.execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect("execute against the replaced definition");
    assert_eq!(
        bodies(&core, "alice"),
        vec![
            (1, "body 1".to_string()),
            (2, "q".to_string()),
            (3, "body 3".to_string())
        ]
    );

    // (2) 評価後射影形へ置換すると、Execute は `55000` で副作用が無い。
    let parsed = core
        .parse_sql("DELETE FROM sv WHERE body = 'q' USING OPERATION_ID 'op-d-2'")
        .expect("parse");
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT * FROM docs WHERE lang = 'en' LIMIT 5",
    );
    let e = core
        .execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect_err("no longer auto-updatable");
    assert_eq!(e.wire_code(), "55000");
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);

    // (3) `DROP VIEW` を挟むと `42P01` で副作用が無い。
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT * FROM docs WHERE lang = 'en'",
    );
    let parsed = core
        .parse_sql("DELETE FROM sv WHERE body = 'q' USING OPERATION_ID 'op-d-3'")
        .expect("parse");
    ddl(&core, "DROP VIEW sv");
    let e = core
        .execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect_err("dropped view");
    assert_eq!(e.wire_code(), "42P01");
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
}

/// 連鎖ビューの述語はすべて合成される。`PARTITIONED` は `0A000`、自動更新不可は `55000`。
#[test]
fn chain_predicates_partitioned_and_non_updatable() {
    let (core, _g) = open_core("t43u-chain");
    ddl(
        &core,
        "CREATE VIEW v1 AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ddl(
        &core,
        "CREATE VIEW v2 AS SELECT * FROM v1 WHERE body LIKE 'body 1%'",
    );
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    assert_eq!(
        affected(ok(
            &core,
            "alice",
            "UPDATE v2 SET body = 'c' WHERE id > 0 USING OPERATION_ID 'op-c-1'"
        )),
        1
    );
    assert_eq!(bodies(&core, "alice")[0], (1, "c".to_string()));
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM v2 WHERE id > 0 USING OPERATION_ID 'op-c-2' PARTITIONED CHUNK 2"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE b SET body = 'x' WHERE id = 1 USING OPERATION_ID 'op-c-3'"
        ),
        "55000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "DELETE FROM b WHERE id = 1 USING OPERATION_ID 'op-c-4'"
        ),
        "55000"
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
}
