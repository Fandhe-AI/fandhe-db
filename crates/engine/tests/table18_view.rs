//! `CREATE VIEW` / `DROP VIEW`（非マテリアライズド。TABLE-18・SQL-23・
//! TASK-205、Issue #909）の結合テスト。ポインタ: `docs/spec/05-tasks.md`
//! TASK-205・`docs/spec/04-behavior/table-behavior.md` TABLE-18・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23・`docs/spec/04-behavior/
//! rls.md` RLS-10 (b)。
//!
//! `sql_drop_table.rs`・`scalar_index_prune.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、
//! `engine::tenant::insert_typed_row` による投入、`EngineCore::
//! execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約（受入基準 1〜4。詳細は `docs/design/create-view.md` 参照）:
//! 1. ビュー定義は許可リストを通った文だけを受理する
//! 2. ビュー経由の読み取りには参照したセッションの `PolicyContext` で RLS が
//!    暗黙適用される（作成者の可視性は引き継がれない）
//! 3. ネスト深さの上限
//! 4. 循環参照の拒否
//!
//! 加えて、DDL 実行権限ゲート・名前空間の共有・依存オブジェクト検査・
//! ビューへの書き込み拒否・列スコープ検査・永続化を検証する。

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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

fn create_view(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn drop_view(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn scan(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn seed_base_fixture(storage: &Storage) {
    // alice: public 3 件（ja 2・en 1）・private 1 件（ja）
    insert_row(
        storage,
        &ctx("alice"),
        1,
        "ja",
        "alice public ja 1",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        2,
        "ja",
        "alice public ja 2",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        3,
        "en",
        "alice public en",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        4,
        "ja",
        "alice private ja",
        Visibility::Private,
    );
    // bob: public 1 件（ja）・private 1 件（ja）
    insert_row(
        storage,
        &ctx("bob"),
        5,
        "ja",
        "bob public ja",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("bob"),
        6,
        "ja",
        "bob private ja",
        Visibility::Private,
    );
    // carol: public 1 件（ja）
    insert_row(
        storage,
        &ctx("carol"),
        7,
        "ja",
        "carol public ja",
        Visibility::Public,
    );
}

// --- 権限（DDL 実行権限ゲート） ---------------------------------------------

/// DDL 実行権限を持たない既定セッションは、対象の実在有無に関わらず常に
/// `42501` で拒否される（`DROP TABLE` と同じ設計）。
#[test]
fn ddl_permission_denies_regardless_of_existence() {
    let path = unique_db_path("view-perm");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);

    let mut denied_session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut denied_session,
            "CREATE VIEW v AS SELECT * FROM docs",
        )
        .expect_err("must be denied");
    assert_eq!(err.wire_code(), "42501");

    let err = core
        .execute_sql_in_session(&ctx("alice"), &mut denied_session, "DROP VIEW nonexistent")
        .expect_err("must be denied regardless of existence");
    assert_eq!(err.wire_code(), "42501");
}

// --- 受入基準 1: 許可リスト ---------------------------------------------------

#[test]
fn disallowed_view_body_forms_are_rejected_and_not_persisted() {
    let path = unique_db_path("view-allowlist");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    let forms = [
        "CREATE VIEW v AS SELECT * FROM docs LIMIT 10",
        "CREATE VIEW v AS SELECT * FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 10",
        "CREATE VIEW v AS SELECT COUNT(*) FROM docs",
        "CREATE OR REPLACE VIEW v AS SELECT * FROM docs",
        "CREATE VIEW IF NOT EXISTS v AS SELECT * FROM docs",
        "CREATE VIEW v (a, b) AS SELECT * FROM docs",
        "DROP VIEW IF EXISTS v",
        "DROP VIEW v CASCADE",
    ];
    for sql in forms {
        let err = create_view(&core, &mut session, sql).expect_err(&format!("must reject: {sql}"));
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }

    // 何も永続化されていないことを確認する。
    let err = scan(&core, "alice", "SELECT * FROM v LIMIT 10").expect_err("view must not exist");
    assert_eq!(err.wire_code(), "42P01");
}

// --- 受入基準 2: RLS の暗黙適用（参照者の PolicyContext） ---------------------

#[test]
fn view_read_applies_referencing_session_rls_not_creator_visibility() {
    let path = unique_db_path("view-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW ja_docs AS SELECT id, lang, body FROM docs WHERE lang = 'ja'",
    )
    .expect("create view should succeed");

    for tenant in ["alice", "bob", "carol"] {
        let via_view = scan(&core, tenant, "SELECT id FROM ja_docs LIMIT 100").expect("view scan");
        let direct = scan(
            &core,
            tenant,
            "SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
        )
        .expect("direct scan");
        assert_eq!(
            result_ids(&via_view),
            result_ids(&direct),
            "tenant={tenant}: view result must match direct query under the SAME session ctx"
        );
    }

    // alice（作成者）の private 行が他テナントの結果に混入しない。
    let bob_via_view = scan(&core, "bob", "SELECT id FROM ja_docs LIMIT 100").expect("bob view");
    assert!(!result_ids(&bob_via_view).contains(&4));
    let carol_via_view =
        scan(&core, "carol", "SELECT id FROM ja_docs LIMIT 100").expect("carol view");
    assert!(!result_ids(&carol_via_view).contains(&4));
    assert!(!result_ids(&carol_via_view).contains(&6));

    // COUNT 経由でも他テナントの private 行数が漏れない対照比較
    // （private 行の有無でカタログ以外の応答が変わらないことは、bob/carol の
    // 結果集合が「alice の private 行を除いた直接クエリ」と完全一致することで
    // 既に固定済み）。
}

/// SQL-24・TASK-208、Issue #914: `CREATE VIEW` 本体の `LIKE` も中間一致の
/// 一般形を受理し、往復（定義の保存・参照時の再検証）で意味論が壊れない
/// ことを固定する（`sql::allowlist::render_view_body` は生パターンを無加工で
/// 保持するため、`DeclarativeFilter::like` の振り分けは参照のたびに再実行
/// される）。
#[test]
fn view_body_like_general_form_matches_direct_query() {
    let path = unique_db_path("view-like-general-form");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW body_has_public AS SELECT id, lang, body FROM docs WHERE body LIKE '%public%'",
    )
    .expect("create view should succeed");

    for tenant in ["alice", "bob", "carol"] {
        let via_view =
            scan(&core, tenant, "SELECT id FROM body_has_public LIMIT 100").expect("view scan");
        let direct = scan(
            &core,
            tenant,
            "SELECT id FROM docs WHERE body LIKE '%public%' LIMIT 100",
        )
        .expect("direct scan");
        assert_eq!(
            result_ids(&via_view),
            result_ids(&direct),
            "tenant={tenant}: view result must match direct query for LIKE general form"
        );
    }
}

/// ビュー経由のクエリでも、ビュー自身の未知列参照は `22000` で拒否する。
#[test]
fn view_column_scope_rejects_unknown_column() {
    let path = unique_db_path("view-column-scope");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create view");

    let err = scan(&core, "alice", "SELECT body FROM id_only LIMIT 10")
        .expect_err("body is not exposed by id_only");
    assert_eq!(err.wire_code(), "22000");
}

/// ビュー越しの列スコープ検査は式項目（TASK-79・SQL-9 の `SelectItem::Expr`）が
/// 隠れた列を参照する場合も適用される（codex-review 指摘・PR #1048）。
/// `id_only` は `id` のみを公開するが、`vec_norm(embedding)` は `embedding`
/// （非公開の `VECTOR` 列）を式の内側で参照するため、単純な `column` フィールド
/// だけを見る旧実装では素通りしていた。
#[test]
fn view_column_scope_rejects_expr_item_referencing_hidden_column() {
    let path = unique_db_path("view-column-scope-expr-item");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create view");

    let err = scan(
        &core,
        "alice",
        "SELECT vec_norm(embedding) FROM id_only LIMIT 10",
    )
    .expect_err("embedding is not exposed by id_only, even inside an expression item");
    assert_eq!(err.wire_code(), "22000");
}

/// ビュー越しの列スコープ検査は式述語（`WherePredicate::Expression`）が隠れた
/// 列を参照する場合も適用される（codex-review 指摘・PR #1048。上記テストの
/// `WHERE` 版）。
#[test]
fn view_column_scope_rejects_expression_predicate_referencing_hidden_column() {
    let path = unique_db_path("view-column-scope-expr-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create view");

    let err = scan(
        &core,
        "alice",
        "SELECT id FROM id_only WHERE vec_norm(embedding) > 0 LIMIT 10",
    )
    .expect_err("embedding is not exposed by id_only, even inside a WHERE expression predicate");
    assert_eq!(err.wire_code(), "22000");
}

/// ネストしたビューの列スコープ検査（レビュー指摘対応）: 内側ビューが列を
/// 絞り込んでいる場合、外側ビューが `SELECT *` で内側ビューを参照しても
/// その制限を引き継ぐ。`resolve_from` が連鎖の最も外側の射影だけを記録して
/// いると、`SELECT * FROM inner_view` を重ねるだけで内側ビューが隠していた
/// 列（ここでは `body`）へ到達できてしまう。
#[test]
fn nested_view_star_inherits_inner_column_restriction() {
    let path = unique_db_path("view-nested-star");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create inner view");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only_wrapped AS SELECT * FROM id_only",
    )
    .expect("create outer view wrapping inner via *");

    // 外側ビューが `*` を使っていても、内側ビューが公開しない `body`・`lang`
    // へは到達できない。
    let err = scan(&core, "alice", "SELECT body FROM id_only_wrapped LIMIT 10")
        .expect_err("body must stay hidden through the outer * wrapper");
    assert_eq!(err.wire_code(), "22000");
    let err = scan(&core, "alice", "SELECT lang FROM id_only_wrapped LIMIT 10")
        .expect_err("lang must stay hidden through the outer * wrapper");
    assert_eq!(err.wire_code(), "22000");

    // 内側ビューが公開する `id` は引き続き参照でき、結果は内側ビューを直接
    // 引いた場合と一致する。
    let via_outer =
        scan(&core, "alice", "SELECT id FROM id_only_wrapped LIMIT 100").expect("outer scan");
    let via_inner = scan(&core, "alice", "SELECT id FROM id_only LIMIT 100").expect("inner scan");
    assert_eq!(result_ids(&via_outer), result_ids(&via_inner));
    assert!(!result_ids(&via_outer).is_empty());
}

/// ネストしたビューの列スコープ検査: 外側ビューが明示列指定で内側ビューの
/// 非公開列を参照した場合も、積集合が空になり `22000` で拒否される
/// （`CREATE VIEW` 自体はカタログ照会を行わないため作成時には検出されず、
/// 参照時の `resolve_from` が唯一の検査点になる）。
#[test]
fn nested_view_explicit_column_not_exposed_by_inner_is_rejected() {
    let path = unique_db_path("view-nested-explicit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create inner view");
    // 内側ビューは `id` しか公開しないが、外側ビューは作成時点でその制限を
    // 検証されないため `body` を明示的に指定できてしまう。
    create_view(
        &core,
        &mut session,
        "CREATE VIEW leaky AS SELECT body FROM id_only",
    )
    .expect("create outer view referencing a column hidden by the inner view");

    let err = scan(&core, "alice", "SELECT body FROM leaky LIMIT 10")
        .expect_err("body is not exposed by the inner view id_only");
    assert_eq!(err.wire_code(), "22000");
}

/// ネストしたビューの列スコープ検査: 外側ビュー自身の `WHERE` 述語が内側
/// ビューの非公開列を参照している場合も `22000` で拒否する（列漏えいは
/// 投影〔`SELECT`〕経由だけでなく `WHERE` 経由でも起こりうる。内側ビュー
/// `id_only` は `id` のみを公開するが、外側ビュー `probe` はカタログ照会を
/// 経ない作成時には検出されない `body` 列を `WHERE` に埋め込める）。
#[test]
fn nested_view_where_predicate_not_exposed_by_inner_is_rejected() {
    let path = unique_db_path("view-nested-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW id_only AS SELECT id FROM docs WHERE lang = 'ja'",
    )
    .expect("create inner view");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW probe AS SELECT id FROM id_only WHERE body = 'alice private ja'",
    )
    .expect("create outer view whose own WHERE references a column hidden by the inner view");

    let err = scan(&core, "alice", "SELECT id FROM probe LIMIT 10")
        .expect_err("probe's own WHERE references body, which id_only does not expose");
    assert_eq!(err.wire_code(), "22000");
}

// --- 受入基準 3: ネスト深さ上限 ----------------------------------------------

#[test]
fn nesting_depth_limit_is_enforced() {
    let path = unique_db_path("view-nesting");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    // v1(1) -> v2(2、`lang = 'ja'` の述語を持つ) -> v3(3) -> v4(4) は成功する
    // はず（既定上限 4）。
    create_view(&core, &mut session, "CREATE VIEW v1 AS SELECT * FROM docs")
        .expect("depth 1 should succeed");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW v2 AS SELECT * FROM v1 WHERE lang = 'ja'",
    )
    .expect("depth 2 should succeed");
    create_view(&core, &mut session, "CREATE VIEW v3 AS SELECT * FROM v2")
        .expect("depth 3 should succeed");
    create_view(&core, &mut session, "CREATE VIEW v4 AS SELECT * FROM v3")
        .expect("depth 4 should succeed");

    let err = create_view(&core, &mut session, "CREATE VIEW v5 AS SELECT * FROM v4")
        .expect_err("depth 5 must exceed the limit");
    assert_eq!(err.wire_code(), "54000");

    // v5 は永続化されていない。
    let err = scan(&core, "alice", "SELECT * FROM v5 LIMIT 1").expect_err("v5 must not exist");
    assert_eq!(err.wire_code(), "42P01");

    // v2 の述語（`lang = 'ja'`）が v3・v4 を通じて連鎖的に合成されることを
    // 確認する（`docs` へ ja/en 各 1 行投入し、`v4` 経由の結果が
    // `WHERE lang = 'ja'` を直接指定した場合と一致することを固定する）。
    let mut write_session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut write_session,
        "INSERT INTO docs (id, embedding, lang, body) VALUES (100, '[1,0]', 'ja', 'x') USING OPERATION_ID 'op-nesting-1'",
    )
    .expect("insert ja row into docs");
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut write_session,
        "INSERT INTO docs (id, embedding, lang, body) VALUES (101, '[2,0]', 'en', 'y') USING OPERATION_ID 'op-nesting-2'",
    )
    .expect("insert en row into docs");
    let via_v4 = scan(&core, "alice", "SELECT id FROM v4 LIMIT 100").expect("v4 scan");
    let direct = scan(
        &core,
        "alice",
        "SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
    )
    .expect("direct scan");
    assert_eq!(result_ids(&via_v4), result_ids(&direct));
    assert!(!result_ids(&via_v4).is_empty());
}

// --- 受入基準 4: 循環参照の拒否 ----------------------------------------------

#[test]
fn self_reference_is_rejected_and_not_persisted() {
    let path = unique_db_path("view-cycle-self");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    let err = create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM v")
        .expect_err("self reference must fail");
    assert_eq!(err.wire_code(), "42P01");

    let err = scan(&core, "alice", "SELECT * FROM v LIMIT 1").expect_err("v must not exist");
    assert_eq!(err.wire_code(), "42P01");
}

/// 作り直しによる循環構築の阻止: 他のビューから参照されているビューの
/// `DROP VIEW` は `2BP01` で拒否され、参照先を差し替えて再作成できない。
#[test]
fn drop_view_referenced_by_another_view_is_rejected() {
    let path = unique_db_path("view-cycle-drop");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(
        &core,
        &mut session,
        "CREATE VIEW base AS SELECT * FROM docs",
    )
    .expect("base");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW dependent AS SELECT * FROM base",
    )
    .expect("dependent");

    let err =
        drop_view(&core, &mut session, "DROP VIEW base").expect_err("base has a dependent view");
    assert_eq!(err.wire_code(), "2BP01");

    // dependent を先に消せば成功する。
    drop_view(&core, &mut session, "DROP VIEW dependent").expect("drop dependent");
    drop_view(&core, &mut session, "DROP VIEW base").expect("drop base after dependent removed");
}

// --- 名前空間・オブジェクト種別 ----------------------------------------------

#[test]
fn view_and_table_share_namespace() {
    let path = unique_db_path("view-namespace");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    // 既存テーブル名と衝突。
    let err = create_view(
        &core,
        &mut session,
        "CREATE VIEW docs AS SELECT * FROM docs",
    )
    .expect_err("must collide with table name");
    assert_eq!(err.wire_code(), "42P07");

    // ビュー名同士の衝突。
    create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs").expect("create v");
    let err = create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs")
        .expect_err("must collide with view name");
    assert_eq!(err.wire_code(), "42P07");
}

/// `Storage::create_table`（Rust API・SQL 表層を経由しない直接呼び出し）も、
/// 既存のビュー名との衝突を検出する（`TableAlreadyExists`）。
#[test]
fn storage_create_table_detects_view_name_collision() {
    let path = unique_db_path("view-namespace-create-table");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
        .create_view("v", "docs", "SELECT * FROM docs")
        .expect("create view via Rust API");

    let err = storage
        .create_table(&TableSchema::new(
            "v",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect_err("create_table must detect the existing view name");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::TableAlreadyExists(name) if name == "v"
    ));
}

/// `DROP TABLE` にビュー名、`DROP VIEW` にテーブル名を指定するといずれも
/// `42809`。
#[test]
fn drop_wrong_object_kind_is_rejected() {
    let path = unique_db_path("view-wrong-kind");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs").expect("create v");

    let err = core
        .execute_sql_in_session(&ctx("alice"), &mut session, "DROP TABLE v")
        .expect_err("DROP TABLE on a view name must fail");
    assert_eq!(err.wire_code(), "42809");

    let err = drop_view(&core, &mut session, "DROP VIEW docs")
        .expect_err("DROP VIEW on a table name must fail");
    assert_eq!(err.wire_code(), "42809");

    let err = drop_view(&core, &mut session, "DROP VIEW nosuchview")
        .expect_err("nonexistent view name must fail");
    assert_eq!(err.wire_code(), "42P01");
}

/// テーブルを参照するビューが残っている間は `DROP TABLE` を拒否する。
#[test]
fn drop_table_referenced_by_view_is_rejected() {
    let path = unique_db_path("view-drop-table-dependent");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs").expect("create v");

    let err = core
        .execute_sql_in_session(&ctx("alice"), &mut session, "DROP TABLE docs")
        .expect_err("table has a dependent view");
    assert_eq!(err.wire_code(), "2BP01");

    drop_view(&core, &mut session, "DROP VIEW v").expect("drop view");
    core.execute_sql_in_session(&ctx("alice"), &mut session, "DROP TABLE docs")
        .expect("drop table should succeed once the view is gone");
}

// --- ビューへの書き込み拒否 ---------------------------------------------------

#[test]
fn writes_to_a_view_are_rejected_with_no_side_effects() {
    let path = unique_db_path("view-write-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs").expect("create v");

    let before = scan(&core, "alice", "SELECT id FROM docs LIMIT 100").expect("before");

    let mut write_session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut write_session,
            "INSERT INTO v (id, embedding, lang, body) VALUES (200, '[1,1]', 'ja', 'x') USING OPERATION_ID 'op-view-write-1'",
        )
        .expect_err("insert into a view must be rejected");
    assert_eq!(err.wire_code(), "42809");

    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut write_session,
            "TRUNCATE TABLE v USING OPERATION_ID 'op-view-write-2'",
        )
        .expect_err("truncate a view must be rejected");
    assert_eq!(err.wire_code(), "42809");

    let after = scan(&core, "alice", "SELECT id FROM docs LIMIT 100").expect("after");
    assert_eq!(result_ids(&before), result_ids(&after), "no side effects");
}

// --- ビューを対象にした禁止形 -------------------------------------------------

#[test]
fn vector_search_and_explain_against_a_view_are_rejected() {
    let path = unique_db_path("view-search-reject");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let mut session = allowed_session();

    create_view(&core, &mut session, "CREATE VIEW v AS SELECT * FROM docs").expect("create v");

    let err = scan(
        &core,
        "alice",
        "SELECT * FROM v ORDER BY embedding <=> '[0,0]' LIMIT 10",
    )
    .expect_err("vector search against a view must be rejected");
    // FROM の table_exists 判定は `Statement::Select` 分岐では resolve_from を
    // 経由しないため（ビュー展開の対象外。§2.1「本リポの実装既定値」）、
    // `42P01`（未定義テーブル）として fail-closed に拒否される。
    assert_eq!(err.wire_code(), "42P01");
}

// --- 永続化 -------------------------------------------------------------------

#[test]
fn view_persists_across_reopen() {
    let path = unique_db_path("view-persist");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed_base_fixture(&storage);
        let core = new_core(storage);
        let mut session = allowed_session();
        create_view(
            &core,
            &mut session,
            "CREATE VIEW ja_docs AS SELECT id FROM docs WHERE lang = 'ja'",
        )
        .expect("create view");
    }
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let core = new_core(storage);
        let result = scan(&core, "alice", "SELECT id FROM ja_docs LIMIT 100")
            .expect("view must survive reopen");
        assert!(!result.rows.is_empty());
    }
}
