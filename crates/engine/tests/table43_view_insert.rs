//! 更新可能ビューへの `INSERT` の基表への書き換え（TABLE-43・SQL-49・RLS-10。Issue #1671）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-43。
//!
//! `table43_view_updatability.rs` と同じ流儀（実 `Storage`＋`EngineCore::execute_sql_in_session`）。
//! 検証する契約: (1) 単一行・複数行 `VALUES`・`RETURNING`・`ON CONFLICT` が基表へ反映される
//! (2) ビューに現れない基表の列は基表の `DEFAULT`（無ければ NULL） (3) 公開されていない列は
//! `42703`・別名で疑似列 `id` へ写る列は `0A000` (4) テナント境界は基表と同じ (5) 台帳はビュー経由と
//! 基表直接を区別する (6) 明示トランザクション・Parse／Execute の分離でも現在の定義で書き換わる。

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
// SQL 表層の `CREATE TABLE` は暗黙の `id` を持たない採番方式の表を作る（TABLE-23・Issue #2199）。
// このテストは疑似列 `id`・従来方式の契約を検証するため、`CREATE TABLE` を従来方式の表作成へ
// 振り向ける（`LegacyExec`）。
#[path = "../src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;
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
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut session, sql)
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
    core.execute_sql_in_session_legacy(&ctx("alice"), &mut session, sql)
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

/// 単一行・複数行 `VALUES` が基表へ反映され、`rows_affected` が正しい。ビュー条件の外の行も
/// 挿入される（`WITH CHECK OPTION` は #1673 の担当）。
#[test]
fn insert_through_simple_view_lands_in_base_table() {
    let (core, _g) = open_core("t43i-basic");
    ddl(
        &core,
        "CREATE VIEW ja AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    match ok(
        &core,
        "alice",
        "INSERT INTO ja (id, embedding, lang, body) VALUES (10, '[1,1]', 'ja', 'one') USING OPERATION_ID 'op-v-1'",
    ) {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected Insert, got {other:?}"),
    }
    match ok(
        &core,
        "alice",
        "INSERT INTO ja (id, embedding, lang, body) VALUES (11, '[1,2]', 'ja', 'two'), (12, '[1,3]', 'en', 'three') USING OPERATION_ID 'op-v-2'",
    ) {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected Insert, got {other:?}"),
    }
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3, 10, 11, 12]);
    // ビュー条件の外（`lang = 'en'`）の行は基表にはあるが、ビュー経由では見えない。
    assert_eq!(ids(&core, "alice", "ja"), vec![1, 3, 10, 11]);
}

/// 列を絞ったビューでは、省略した基表の列に基表の `DEFAULT`（無ければ NULL）が入り、
/// 非公開列の指定は `42703` で副作用が無い。
#[test]
fn omitted_columns_take_base_defaults_and_hidden_columns_are_rejected() {
    let (core, _g) = open_core("t43i-default");
    ddl(
        &core,
        "CREATE TABLE notes (title TEXT NOT NULL, tag TEXT DEFAULT 'none', extra TEXT)",
    );
    ddl(&core, "CREATE VIEW nv AS SELECT id, title FROM notes");
    ok(
        &core,
        "alice",
        "INSERT INTO nv (id, title) VALUES (1, 'a') USING OPERATION_ID 'op-n-1'",
    );
    let q = query(
        &core,
        "alice",
        "SELECT id, title, tag, extra FROM notes LIMIT 10",
    );
    assert_eq!(q.rows.len(), 1);
    assert_eq!(text(&q.rows[0].cells[2]), "none");
    assert_eq!(q.rows[0].cells[3], Cell::Null);

    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO nv (id, title, tag) VALUES (2, 'b', 'x') USING OPERATION_ID 'op-n-2'"
        ),
        "42703"
    );
    assert_eq!(ids(&core, "alice", "notes"), vec![1]);
}

/// 列別名つきビューは公開名で書ける。別名で疑似列 `id` へ写る列は `0A000`。物理名は非公開。
#[test]
fn aliased_view_accepts_public_names() {
    let (core, _g) = open_core("t43i-alias");
    ddl(
        &core,
        "CREATE VIEW av (doc_id, language, text_body) AS SELECT id, lang, body FROM docs",
    );
    // 別名で疑似列 `id` へ写る公開名は書けない（fail-closed）。
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO av (doc_id, language, text_body) VALUES (20, 'ja', 'x') USING OPERATION_ID 'op-a-1'"
        ),
        "0A000"
    );
    // `id`（疑似列）を書かず基表の列だけを公開名で書く。`embedding` は非公開。
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO av (language, text_body, embedding) VALUES ('ja', 'x', '[1,1]') USING OPERATION_ID 'op-a-2'"
        ),
        "42703"
    );
    // 物理名は公開されていない。
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO av (lang) VALUES ('ja') USING OPERATION_ID 'op-a-3'"
        ),
        "42703"
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
}

/// 別名でも `id` を含まないビューなら公開名で INSERT でき、`RETURNING` は公開名で返る。
#[test]
fn aliased_view_insert_returning_uses_public_names() {
    let (core, _g) = open_core("t43i-alias-ret");
    ddl(
        &core,
        "CREATE TABLE notes (title TEXT NOT NULL, tag TEXT DEFAULT 'none')",
    );
    ddl(
        &core,
        "CREATE VIEW nv (id, heading, label) AS SELECT id, title, tag FROM notes",
    );
    let r = returning(ok(
        &core,
        "alice",
        "INSERT INTO nv (id, heading) VALUES (1, 'hello') RETURNING heading, label USING OPERATION_ID 'op-ar-1'",
    ));
    assert_eq!(column_names(&r.result.columns), vec!["heading", "label"]);
    assert_eq!(text(&r.result.rows[0].cells[0]), "hello");
    assert_eq!(text(&r.result.rows[0].cells[1]), "none");
}

/// 連鎖（v2 → v1 → 基表）への INSERT。
#[test]
fn insert_through_view_chain() {
    let (core, _g) = open_core("t43i-chain");
    ddl(
        &core,
        "CREATE VIEW v1 AS SELECT * FROM docs WHERE lang = 'ja'",
    );
    ddl(
        &core,
        "CREATE VIEW v2 AS SELECT id, embedding, lang, body FROM v1",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO v2 (id, embedding, lang, body) VALUES (30, '[1,1]', 'ja', 'chain') USING OPERATION_ID 'op-c-1'",
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3, 30]);
}

/// `RETURNING *`／`RETURNING <公開名>` はビューの公開列だけを公開名で返し、Describe の列メタも同じ。
#[test]
fn returning_uses_view_columns_and_describe_matches() {
    let (core, _g) = open_core("t43i-returning");
    ddl(
        &core,
        "CREATE TABLE notes (title TEXT, tag TEXT DEFAULT 'none')",
    );
    ddl(&core, "CREATE VIEW slim AS SELECT id, title FROM notes");
    let r = returning(ok(
        &core,
        "alice",
        "INSERT INTO slim (id, title) VALUES (40, 'hi') RETURNING * USING OPERATION_ID 'op-r-1'",
    ));
    // 非公開の `tag` は返らない。
    assert_eq!(column_names(&r.result.columns), vec!["id", "title"]);
    assert_eq!(r.result.rows.len(), 1);
    assert_eq!(r.result.rows[0].cells.len(), 2);
    assert_eq!(text(&r.result.rows[0].cells[1]), "hi");

    let parsed = core
        .parse_sql("INSERT INTO slim (id, title) VALUES (41, 'x') RETURNING title USING OPERATION_ID 'op-r-2'")
        .expect("parse");
    let session = SessionState::default();
    let meta = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(column_names(&meta), vec!["title"]);

    let parsed = core
        .parse_sql(
            "INSERT INTO slim (id, title) VALUES (42, 'x') RETURNING * USING OPERATION_ID 'op-r-3'",
        )
        .expect("parse");
    let meta = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(column_names(&meta), vec!["id", "title"]);
}

/// `ON CONFLICT` は基表で働く。`DO NOTHING` は衝突行を変えず、`DO UPDATE` は公開名で書ける。
#[test]
fn upsert_through_view() {
    let (core, _g) = open_core("t43i-upsert");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    // 自テナントの行 1 への `DO NOTHING` は何も変えない。
    ok(
        &core,
        "alice",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (1, '[9,9]', 'xx', 'changed') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-u-1'",
    );
    let q = query(&core, "alice", "SELECT body FROM docs WHERE id = 1 LIMIT 1");
    assert_eq!(text(&q.rows[0].cells[0]), "body 1");
    // `DO UPDATE` は公開名で書ける。
    ok(
        &core,
        "alice",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (1, '[9,9]', 'xx', 'changed') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body USING OPERATION_ID 'op-u-2'",
    );
    let q = query(&core, "alice", "SELECT body FROM docs WHERE id = 1 LIMIT 1");
    assert_eq!(text(&q.rows[0].cells[0]), "changed");
}

/// テナント境界は基表と同じ: 他テナントの行は変わらず見えず、エラーは閲覧テナントに依存しない。
#[test]
fn tenant_boundary_is_the_same_as_the_base_table() {
    let (core, _g) = open_core("t43i-tenant");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (50, '[1,1]', 'ja', 'mine') USING OPERATION_ID 'op-t-1'",
    );
    assert!(
        !ids(&core, "bob", "docs").contains(&50),
        "bob must not see alice's row"
    );
    assert!(ids(&core, "alice", "docs").contains(&50));

    // bob の行（id 3）への alice の UPSERT は bob の行に触れない。
    ok(
        &core,
        "alice",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (3, '[1,1]', 'ja', 'hijack') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-t-2'",
    );
    let q = query(&core, "bob", "SELECT body FROM docs WHERE id = 3 LIMIT 1");
    assert_eq!(text(&q.rows[0].cells[0]), "body 3");

    // 拒否の応答は閲覧テナントに依存しない。
    let sql = "INSERT INTO sv (id, hidden) VALUES (1, 'x') USING OPERATION_ID 'op-t-3'";
    let a = run(&core, "alice", sql).expect_err("alice");
    let b = run(&core, "bob", sql).expect_err("bob");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(a.to_string(), b.to_string());
}

/// 台帳: ビュー経由の同一再送は `23505`、同じ `operation_id` と値でも基表への直接の文は `22023`。
#[test]
fn ledger_distinguishes_view_writes_from_direct_writes() {
    let (core, _g) = open_core("t43i-ledger");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    let via_view = "INSERT INTO sv (id, embedding, lang, body) VALUES (60, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-l-1'";
    ok(&core, "alice", via_view);
    assert_eq!(code(&core, "alice", via_view), "23505");
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO docs (id, embedding, lang, body) VALUES (60, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-l-1'"
        ),
        "22023"
    );
    // `operation_id` の省略は成功する（台帳に残さない）。
    ok(
        &core,
        "alice",
        "INSERT INTO sv (id, embedding, lang, body) VALUES (61, '[1,1]', 'ja', 'y')",
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3, 60, 61]);
}

/// 明示トランザクション: `COMMIT` で反映、`ROLLBACK` で未反映。
#[test]
fn explicit_transaction_commit_and_rollback() {
    let (core, _g) = open_core("t43i-txn");
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    let caller = ctx("alice");
    for (end, expected) in [("COMMIT", true), ("ROLLBACK", false)] {
        let id = if expected { 70 } else { 71 };
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        core.execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            &format!("INSERT INTO sv (id, embedding, lang, body) VALUES ({id}, '[1,1]', 'ja', 't') USING OPERATION_ID 'op-x-{id}'"),
        )
        .expect("insert in txn");
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, end)
            .expect("end");
        assert_eq!(ids(&core, "alice", "docs").contains(&id), expected, "{end}");
    }
}

/// Parse と Execute の間に定義が変わっても、Execute は現在の定義で書き換える。
#[test]
fn execute_uses_the_definition_current_at_execute_time() {
    let (core, _g) = open_core("t43i-split");
    ddl(
        &core,
        "CREATE TABLE notes (title TEXT, tag TEXT DEFAULT 'none')",
    );
    ddl(&core, "CREATE VIEW sv AS SELECT id, title FROM notes");
    let mut session = SessionState::default();

    // (1) 置換（末尾列を追加）後の Execute は新しい定義で書き換わり、基表へ反映される。
    let parsed = core
        .parse_sql("INSERT INTO sv (id, title) VALUES (80, 'a') USING OPERATION_ID 'op-s-1'")
        .expect("parse");
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT id, title, tag FROM notes",
    );
    core.execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect("execute against the replaced definition");
    assert_eq!(ids(&core, "alice", "notes"), vec![80]);

    // (2) 評価後射影形へ置換すると、Execute は `55000` で副作用が無い。
    let parsed = core
        .parse_sql("INSERT INTO sv (id, title) VALUES (81, 'b') USING OPERATION_ID 'op-s-2'")
        .expect("parse");
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT id, title, tag FROM notes LIMIT 5",
    );
    let e = core
        .execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect_err("no longer auto-updatable");
    assert_eq!(e.wire_code(), "55000");
    assert_eq!(ids(&core, "alice", "notes"), vec![80]);

    // (3) `DROP VIEW` を挟むと `42P01` で副作用が無い。
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT id, title, tag FROM notes",
    );
    let parsed = core
        .parse_sql("INSERT INTO sv (id, title) VALUES (82, 'c') USING OPERATION_ID 'op-s-3'")
        .expect("parse");
    ddl(&core, "DROP VIEW sv");
    let e = core
        .execute_parsed_in_session(&ctx("alice"), &mut session, &parsed)
        .expect_err("dropped view");
    assert_eq!(e.wire_code(), "42P01");
    assert_eq!(ids(&core, "alice", "notes"), vec![80]);
}

/// 自動更新不可のビューは `55000`。単純形ビューへの UPDATE／DELETE は基表へ書き換わる
/// （Issue #1672。契約は `table43_view_update_delete.rs`）。
#[test]
fn non_updatable_and_other_writes_keep_their_codes() {
    let (core, _g) = open_core("t43i-other");
    ddl(&core, "CREATE VIEW b AS SELECT * FROM docs LIMIT 5");
    ddl(&core, "CREATE VIEW sv AS SELECT * FROM docs");
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO b (id, embedding, lang, body) VALUES (90, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-o-1'"
        ),
        "55000"
    );
    ok(
        &core,
        "alice",
        "UPDATE sv SET body = 'y' WHERE id = 1 USING OPERATION_ID 'op-o-2'",
    );
    ok(
        &core,
        "alice",
        "DELETE FROM sv WHERE id = 1 USING OPERATION_ID 'op-o-3'",
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![2, 3]);
}

/// `--max-insert-rows` の上限はビュー経由でも効く（`54000`）。
#[test]
fn max_insert_rows_applies_through_views() {
    let limits = DmlLimits {
        max_affected_rows: None,
        max_insert_rows_per_statement: std::num::NonZeroUsize::new(1),
    };
    let (core, _g) = open_core_with("t43i-limit", Some(limits));
    ddl(
        &core,
        "CREATE VIEW sv AS SELECT id, embedding, lang, body FROM docs",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO sv (id, embedding, lang, body) VALUES (100, '[1,1]', 'ja', 'a'), (101, '[1,1]', 'ja', 'b') USING OPERATION_ID 'op-m-1'"
        ),
        "54000"
    );
    assert_eq!(ids(&core, "alice", "docs"), vec![1, 2, 3]);
}
/// Parse 時のパラメータ型推論（WIRE-12）は、ビューの公開列を基表の列へ解決して型を引く。
/// 基表が INTEGER の列にビュー経由で `$n` を渡すと、Text に倒れず整数として束縛される。
#[test]
fn prepared_view_insert_infers_param_types_from_base_columns() {
    let (core, _g) = open_core("view-insert-param-types");
    ddl(&core, "CREATE TABLE nums (n INTEGER, flag BOOLEAN, t TEXT)");
    ddl(&core, "CREATE VIEW nv AS SELECT n, flag, t FROM nums");
    ddl(
        &core,
        "CREATE VIEW nv_alias (renamed) AS SELECT n FROM nums",
    );
    let prepared = core
        .parse_sql_prepared(
            "INSERT INTO nv (n, flag, t) VALUES ($1, $2, $3) USING OPERATION_ID 'op-pt-1'",
        )
        .expect("parse");
    let types = prepared.param_types();
    assert_eq!(types.len(), 3);
    assert_ne!(types[0], engine::sql::params::PreparedParamType::Text);
    assert_ne!(types[1], engine::sql::params::PreparedParamType::Text);
    assert!(matches!(
        &types[2],
        engine::sql::params::PreparedParamType::Column(_)
    ));
    let binds = [
        Some(b"42".to_vec()),
        Some(b"true".to_vec()),
        Some(b"x".to_vec()),
    ];
    core.bind_prepared(&prepared, &binds).expect("bind");

    // 列別名ビューでも公開名から基表の列型を引く。
    let aliased = core
        .parse_sql_prepared(
            "INSERT INTO nv_alias (renamed) VALUES ($1) USING OPERATION_ID 'op-pt-2'",
        )
        .expect("parse aliased");
    assert_ne!(
        aliased.param_types()[0],
        engine::sql::params::PreparedParamType::Text
    );
}
