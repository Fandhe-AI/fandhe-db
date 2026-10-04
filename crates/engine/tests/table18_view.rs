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
        // Issue #1192・#1360 以降も、ベクトル順位付け・USING PLAN・ウィンドウ項目・
        // 式項目・UDF 述語・集計の式引数・枝内 LIMIT 付きの括弧なし集合演算は本文に
        // 書けない（`LIMIT`・集計・スカラー `ORDER BY`・JOIN・CTE・括弧つき集合演算・
        // サブクエリは受理側へ移った）。
        "CREATE VIEW v AS SELECT * FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 10",
        "CREATE VIEW v AS SELECT * FROM docs USING PLAN('q') LIMIT 10",
        "CREATE VIEW v AS SELECT id FROM docs LIMIT 5 UNION SELECT id FROM docs LIMIT 5",
        "CREATE VIEW v AS SELECT id, ROW_NUMBER() OVER (ORDER BY lang) FROM docs LIMIT 5",
        "CREATE VIEW v AS SELECT lower(body) FROM docs LIMIT 5",
        "CREATE VIEW v AS SELECT id FROM docs WHERE lower(lang) = 'ja' LIMIT 5",
        "CREATE VIEW v AS SELECT SUM(id + 1) FROM docs",
        "CREATE VIEW v AS EXPLAIN SELECT * FROM docs LIMIT 5",
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
// =============================================================================
// Issue #1192: ビュー本文の受理形拡大（集計・LIMIT・ORDER BY・JOIN）と、
// 集計クエリからのビュー参照
// =============================================================================

use engine::sql::exec::Cell;

const NOTES: &str = "notes";

fn notes_schema() -> TableSchema {
    TableSchema::new(
        NOTES,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, false),
            ColumnDef::new("doc_id", ColumnType::BigInt, false),
        ],
    )
}

fn insert_note(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    title: &str,
    doc_id: i64,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        NOTES,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(title.to_string()),
            Value::BigInt(doc_id),
        ],
        &op_id(&format!("note-{id}")),
    )
    .expect("insert note");
}

/// `docs`（`seed_base_fixture`）と `notes` を投入する。notes は docs.id へ
/// 貼る: 100 = alice public → doc 1、101 = alice private → doc 5（bob の公開行）、
/// 102 = bob public → doc 5、103 = carol public → doc 7。
fn seed_join_fixture(storage: &Storage) {
    storage.create_table(&notes_schema()).expect("create notes");
    insert_note(storage, &ctx("alice"), 100, "a-pub", 1, Visibility::Public);
    insert_note(
        storage,
        &ctx("alice"),
        101,
        "a-priv",
        5,
        Visibility::Private,
    );
    insert_note(storage, &ctx("bob"), 102, "b-pub", 5, Visibility::Public);
    insert_note(storage, &ctx("carol"), 103, "c-pub", 7, Visibility::Public);
}

fn cells(result: &QueryResult) -> Vec<Vec<Cell>> {
    result.rows.iter().map(|r| r.cells.clone()).collect()
}

fn open_core(name: &str, join: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    if join {
        seed_join_fixture(&storage);
    }
    (new_core(storage), guard)
}

const TENANTS: [&str; 3] = ["alice", "bob", "carol"];

/// 集計・DISTINCT の FROM が単純形ビューでも、述語を手でマージした直接クエリと
/// 参照セッションの ctx の下で結果が一致する（3 テナント対照。RLS-10 (b)）。
#[test]
fn aggregate_over_simple_view_matches_direct_query() {
    let (core, _g) = open_core("view-agg-simple", false);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW ja_docs AS SELECT id, lang, body FROM docs WHERE lang = 'ja'",
    )
    .expect("create view");

    let pairs = [
        (
            "SELECT COUNT(*) FROM ja_docs",
            "SELECT COUNT(*) FROM docs WHERE lang = 'ja'",
        ),
        (
            "SELECT lang, COUNT(*) AS n FROM ja_docs GROUP BY lang ORDER BY lang LIMIT 10",
            "SELECT lang, COUNT(*) AS n FROM docs WHERE lang = 'ja' GROUP BY lang ORDER BY lang LIMIT 10",
        ),
        (
            "SELECT COUNT(body) FROM ja_docs WHERE body LIKE '%public%'",
            "SELECT COUNT(body) FROM docs WHERE lang = 'ja' AND body LIKE '%public%'",
        ),
        (
            "SELECT DISTINCT lang FROM ja_docs",
            "SELECT DISTINCT lang FROM docs WHERE lang = 'ja'",
        ),
    ];
    for tenant in TENANTS {
        for (via_view, direct) in pairs {
            let a = scan(&core, tenant, via_view).expect(via_view);
            let b = scan(&core, tenant, direct).expect(direct);
            assert_eq!(cells(&a), cells(&b), "tenant={tenant} sql={via_view}");
            assert!(!a.rows.is_empty());
        }
    }
    // 参照者ごとに件数が異なる（作成者 alice の可視性が引き継がれない）。
    let alice = scan(&core, "alice", "SELECT COUNT(*) FROM ja_docs").expect("alice");
    let carol = scan(&core, "carol", "SELECT COUNT(*) FROM ja_docs").expect("carol");
    assert_ne!(cells(&alice), cells(&carol));
}

/// 集計クエリでも、ビューが公開しない列を集計キー・引数・フィルタ・並べ替えへ
/// 使えない（filter oracle の遮断。`22000`）。
#[test]
fn aggregate_over_view_rejects_hidden_columns() {
    let (core, _g) = open_core("view-agg-hidden", false);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW v AS SELECT id, lang FROM docs",
    )
    .expect("create view");
    for sql in [
        "SELECT COUNT(body) FROM v",
        "SELECT body, COUNT(*) FROM v GROUP BY body",
        "SELECT COUNT(*) FROM v WHERE body = 'x'",
        "SELECT COUNT(*) FROM v WHERE lang = 'ja' OR body = 'x'",
        "SELECT lang, COUNT(*) FROM v GROUP BY lang ORDER BY body LIMIT 5",
        "SELECT DISTINCT body FROM v",
    ] {
        let err = scan(&core, "alice", sql).expect_err(sql);
        assert_eq!(err.wire_code(), "22000", "sql={sql}");
    }
    // 公開列のみなら通る。
    scan(&core, "alice", "SELECT lang, COUNT(*) FROM v GROUP BY lang").expect("visible columns");
}

/// 評価後射影形ビュー（LIMIT／ORDER BY／集計／DISTINCT）の参照結果が、本文を
/// 直接実行した結果へ外側の射影・OFFSET・LIMIT を適用したものと一致する。
#[test]
fn buffered_view_bodies_match_direct_execution() {
    let (core, _g) = open_core("view-buffered-forms", false);
    let mut session = allowed_session();
    let bodies = [
        (
            "top_ja",
            "SELECT id, lang, body FROM docs WHERE lang = 'ja' ORDER BY id DESC LIMIT 3",
        ),
        (
            "lang_counts",
            "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n >= 1 ORDER BY lang LIMIT 10",
        ),
        ("langs", "SELECT DISTINCT lang FROM docs ORDER BY lang"),
        ("total", "SELECT COUNT(*) AS n FROM docs"),
    ];
    for (name, body) in bodies {
        create_view(
            &core,
            &mut session,
            &format!("CREATE VIEW {name} AS {body}"),
        )
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
    }
    for tenant in TENANTS {
        for (name, body) in bodies {
            let direct = scan(&core, tenant, body).expect(body);
            // 全列・OFFSET なし
            let all = scan(&core, tenant, &format!("SELECT * FROM {name} LIMIT 100")).expect(name);
            assert_eq!(cells(&all), cells(&direct), "tenant={tenant} view={name}");
            assert_eq!(all.columns, direct.columns);
            // 外側 LIMIT が本文より小さい
            let one = scan(&core, tenant, &format!("SELECT * FROM {name} LIMIT 1")).expect(name);
            assert_eq!(
                cells(&one),
                cells(&direct)[..direct.rows.len().min(1)].to_vec()
            );
            // OFFSET
            let off = scan(
                &core,
                tenant,
                &format!("SELECT * FROM {name} LIMIT 100 OFFSET 1"),
            )
            .expect(name);
            assert_eq!(
                cells(&off),
                cells(&direct).into_iter().skip(1).collect::<Vec<_>>()
            );
            // OFFSET が行数を超える
            let none = scan(
                &core,
                tenant,
                &format!("SELECT * FROM {name} LIMIT 5 OFFSET 500"),
            )
            .expect(name);
            assert!(none.rows.is_empty());
        }
        // 列射影
        let proj = scan(&core, tenant, "SELECT n FROM lang_counts LIMIT 100").expect("proj");
        let full = scan(&core, tenant, "SELECT * FROM lang_counts LIMIT 100").expect("full");
        let expected: Vec<Vec<Cell>> = cells(&full)
            .into_iter()
            .map(|r| vec![r[1].clone()])
            .collect();
        assert_eq!(cells(&proj), expected);
    }
    // ORDER BY／LIMIT 本文は参照者ごとの可視行だけから決まる（他テナント行は
    // 作成者・参照者いずれの ctx でも混入しない）。
    let bob = scan(&core, "bob", "SELECT id FROM top_ja LIMIT 100").expect("bob");
    let carol = scan(&core, "carol", "SELECT id FROM top_ja LIMIT 100").expect("carol");
    assert!(!result_ids(&carol).contains(&4));
    assert!(!result_ids(&carol).contains(&6));
    assert!(!result_ids(&bob).contains(&4));
    // 集計本文の件数も参照者の可視行のみから計算される。
    let total_bob = scan(&core, "bob", "SELECT n FROM total LIMIT 1").expect("bob total");
    let total_carol = scan(&core, "carol", "SELECT n FROM total LIMIT 1").expect("carol total");
    assert_ne!(cells(&total_bob), cells(&total_carol));
}

/// JOIN 本文（INNER／LEFT）。両辺に参照者の RLS が独立に効き、他テナント行が
/// 結合結果・NULL 補完・件数に現れない（RLS-10 (b)）。
#[test]
fn buffered_join_view_applies_referencing_session_rls_on_both_sides() {
    let (core, _g) = open_core("view-buffered-join", true);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW doc_notes AS SELECT docs.id, notes.title FROM docs INNER JOIN notes ON docs.id = notes.doc_id LIMIT 100",
    )
    .expect("create inner join view");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW doc_notes_left AS SELECT docs.id, notes.title FROM docs LEFT JOIN notes ON docs.id = notes.doc_id LIMIT 100",
    )
    .expect("create left join view");
    let inner_body = "SELECT docs.id, notes.title FROM docs INNER JOIN notes ON docs.id = notes.doc_id LIMIT 100";
    let left_body =
        "SELECT docs.id, notes.title FROM docs LEFT JOIN notes ON docs.id = notes.doc_id LIMIT 100";
    for tenant in TENANTS {
        let v = scan(&core, tenant, "SELECT * FROM doc_notes LIMIT 100").expect("inner view");
        let d = scan(&core, tenant, inner_body).expect("inner direct");
        assert_eq!(cells(&v), cells(&d), "tenant={tenant} inner");
        let v = scan(&core, tenant, "SELECT * FROM doc_notes_left LIMIT 100").expect("left view");
        let d = scan(&core, tenant, left_body).expect("left direct");
        assert_eq!(cells(&v), cells(&d), "tenant={tenant} left");
    }
    let alice = scan(&core, "alice", "SELECT * FROM doc_notes LIMIT 100").expect("alice");
    assert!(!alice.rows.is_empty(), "fixture must produce joined rows");
    // carol は alice の private ノート（a-priv）を結合結果に見ない。
    let carol = scan(&core, "carol", "SELECT title FROM doc_notes LIMIT 100").expect("carol");
    for row in &carol.rows {
        assert_ne!(row.cells[0], Cell::Text("a-priv".to_string()));
    }
    // `SELECT *` の JOIN 本文は id 列が重複するため、名前指定の射影は 42702。
    create_view(
        &core,
        &mut session,
        "CREATE VIEW star_join AS SELECT * FROM docs INNER JOIN notes ON docs.id = notes.doc_id LIMIT 100",
    )
    .expect("create star join view");
    let err = scan(&core, "alice", "SELECT id FROM star_join LIMIT 5").expect_err("ambiguous");
    assert_eq!(err.wire_code(), "42702");
    scan(&core, "alice", "SELECT * FROM star_join LIMIT 5").expect("star is fine");
}

/// 評価後射影形ビューに対する外側クエリの禁止形と、連鎖・作成時の拒否。
#[test]
fn buffered_view_rejects_unsupported_outer_forms_and_chaining() {
    let (core, _g) = open_core("view-buffered-reject", true);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW top_ja AS SELECT id, lang, body FROM docs WHERE lang = 'ja' ORDER BY id DESC LIMIT 3",
    )
    .expect("create");
    for sql in [
        "SELECT * FROM top_ja WHERE lang = 'ja' LIMIT 5",
        "SELECT * FROM top_ja ORDER BY id LIMIT 5",
        "SELECT COUNT(*) FROM top_ja",
        "SELECT DISTINCT lang FROM top_ja",
        "SELECT lang, COUNT(*) FROM top_ja GROUP BY lang",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY lang) FROM top_ja LIMIT 5",
        "EXPLAIN SELECT * FROM top_ja LIMIT 5",
        "SELECT id FROM docs WHERE id IN (SELECT id FROM top_ja LIMIT 5) LIMIT 5",
        "WITH x AS (SELECT * FROM top_ja) SELECT * FROM x LIMIT 5",
        "SELECT * FROM top_ja INNER JOIN docs ON top_ja.id = docs.id LIMIT 5",
    ] {
        let err = scan(&core, "alice", sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
    let err = scan(&core, "alice", "SELECT nope FROM top_ja LIMIT 5").expect_err("unknown col");
    assert_eq!(err.wire_code(), "22000");
    let err = scan(&core, "alice", "SELECT * FROM top_ja LIMIT 0").expect_err("limit 0");
    assert_eq!(err.wire_code(), "22000");

    // 連鎖: 評価後射影形 → 評価後射影形、単純形 → 評価後射影形、いずれも作成不可。
    for sql in [
        "CREATE VIEW v2 AS SELECT * FROM top_ja LIMIT 5",
        "CREATE VIEW v3 AS SELECT id FROM top_ja",
        "CREATE VIEW v4 AS SELECT COUNT(*) AS n FROM top_ja",
    ] {
        let err = create_view(&core, &mut session, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
    let err = scan(&core, "alice", "SELECT * FROM v2 LIMIT 5").expect_err("not persisted");
    assert_eq!(err.wire_code(), "42P01");

    // 単純形ビューを源にする評価後射影形は作成できる。
    create_view(
        &core,
        &mut session,
        "CREATE VIEW ja_docs AS SELECT id, lang FROM docs WHERE lang = 'ja'",
    )
    .expect("simple view");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW ja_count AS SELECT COUNT(*) AS n FROM ja_docs",
    )
    .expect("buffered over simple");
    let via = scan(&core, "alice", "SELECT n FROM ja_count LIMIT 1").expect("read");
    let direct = scan(
        &core,
        "alice",
        "SELECT COUNT(*) FROM docs WHERE lang = 'ja'",
    )
    .expect("d");
    assert_eq!(cells(&via), cells(&direct));
    // 存在しない relation を指す本文は 42P01、JOIN 内のビューは 42601。
    let err = create_view(
        &core,
        &mut session,
        "CREATE VIEW bad AS SELECT COUNT(*) AS n FROM missing_table",
    )
    .expect_err("missing");
    assert_eq!(err.wire_code(), "42P01");
    let err = create_view(
        &core,
        &mut session,
        "CREATE VIEW bad AS SELECT docs.id FROM docs INNER JOIN ja_docs ON docs.id = ja_docs.id LIMIT 5",
    )
    .expect_err("join over a view");
    assert_eq!(err.wire_code(), "42601");
}

/// 依存関係の検査: JOIN 右辺の DROP TABLE は 2BP01、DROP VIEW は成功。
/// 参照テーブルの DROP COLUMN は保守的に拒否され、無関係なテーブルは影響しない。
#[test]
fn buffered_view_dependency_checks() {
    let (core, _g) = open_core("view-buffered-deps", true);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW doc_notes AS SELECT docs.id, notes.title FROM docs INNER JOIN notes ON docs.id = notes.doc_id LIMIT 100",
    )
    .expect("create");
    let err = create_view(&core, &mut session, "DROP TABLE notes").expect_err("dependent");
    assert_eq!(err.wire_code(), "2BP01");
    let err = create_view(&core, &mut session, "DROP TABLE docs").expect_err("dependent");
    assert_eq!(err.wire_code(), "2BP01");
    // 参照テーブルの列削除は保守的に拒否される。
    let err = create_view(&core, &mut session, "ALTER TABLE notes DROP COLUMN title")
        .expect_err("conservative");
    assert_eq!(err.wire_code(), "2BP01");
    drop_view(&core, &mut session, "DROP VIEW doc_notes").expect("drop view");
    create_view(&core, &mut session, "DROP TABLE notes").expect("drop table after view is gone");

    // 無関係なテーブルを読む評価後射影形ビューは DROP COLUMN を妨げない。
    create_view(
        &core,
        &mut session,
        "CREATE VIEW cnt AS SELECT COUNT(*) AS n FROM docs",
    )
    .expect("create cnt");
    create_view(
        &core,
        &mut session,
        "CREATE TABLE other (title TEXT, extra TEXT)",
    )
    .expect("create other");
    create_view(&core, &mut session, "ALTER TABLE other DROP COLUMN extra")
        .expect("unrelated buffered view must not block DROP COLUMN");
}

/// 64 KiB を超える本文は 54000（永続化されない）。
#[test]
fn buffered_view_body_size_limit() {
    let (core, _g) = open_core("view-buffered-size", false);
    let mut session = allowed_session();
    let big = "x".repeat(70 * 1024);
    let err = create_view(
        &core,
        &mut session,
        &format!("CREATE VIEW big AS SELECT id FROM docs WHERE lang = '{big}' LIMIT 5"),
    )
    .expect_err("too large");
    assert_eq!(err.wire_code(), "54000");
}

/// 評価後射影形ビュー（文字列リテラルに `'` を含む本文）が再オープン後も
/// 参照できる（正規化描画 → 再パースの往復）。
#[test]
fn buffered_view_persists_across_reopen() {
    let path = unique_db_path("view-buffered-persist");
    let _guard = CleanupGuard(path.clone());
    let expected;
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed_base_fixture(&storage);
        let core = new_core(storage);
        let mut session = allowed_session();
        create_view(
            &core,
            &mut session,
            "CREATE VIEW v AS SELECT lang, COUNT(*) AS n FROM docs WHERE body = 'it''s' OR lang = 'ja' GROUP BY lang LIMIT 10;",
        )
        .expect("create view");
        expected = cells(&scan(&core, "alice", "SELECT * FROM v LIMIT 10").expect("read"));
    }
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let core = new_core(storage);
        let result = scan(&core, "alice", "SELECT * FROM v LIMIT 10").expect("after reopen");
        assert_eq!(cells(&result), expected);
    }
}

/// 明示トランザクション内の参照と、拡張クエリの Describe（本文は実行しない）。
#[test]
fn buffered_view_in_transaction_and_describe() {
    let (core, _g) = open_core("view-buffered-txn", false);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW lang_counts AS SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY lang LIMIT 10",
    )
    .expect("create");

    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "SELECT n FROM lang_counts LIMIT 5",
        )
        .expect("read in txn");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query");
    };
    assert!(!result.rows.is_empty());
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "COMMIT")
        .expect("commit");

    // Describe: 列メタデータは実行結果と一致する。
    let parsed = core
        .parse_sql("SELECT n, lang FROM lang_counts LIMIT 5")
        .expect("parse");
    let described = core
        .describe_parsed_in_session(&s, &parsed)
        .expect("describe")
        .expect("has columns");
    let executed = scan(&core, "alice", "SELECT n, lang FROM lang_counts LIMIT 5").expect("exec");
    assert_eq!(described, executed.columns);
}

/// カーソルの DECLARE は評価後射影形ビューを対象にできない（42601）。
#[test]
fn cursor_declare_over_buffered_view_is_rejected() {
    let (core, _g) = open_core("view-buffered-cursor", false);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW top_ja AS SELECT id FROM docs ORDER BY id LIMIT 3",
    )
    .expect("create");
    let caller = ctx("alice");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "DECLARE c CURSOR FOR SELECT * FROM top_ja LIMIT 5",
        )
        .expect_err("declare over buffered view");
    assert_eq!(err.wire_code(), "42601");
}

/// 評価後射影形ビューが単純形ビュー経由で対象テーブルへ到達する場合も、
/// `DROP COLUMN` は保守的に拒否される（`2BP01`）。
#[test]
fn buffered_view_over_simple_view_blocks_drop_column_transitively() {
    let (core, _g) = open_core("view-buffered-transitive", false);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW all_docs AS SELECT id, body FROM docs",
    )
    .expect("simple view");
    create_view(
        &core,
        &mut session,
        "CREATE VIEW cb AS SELECT COUNT(body) AS n FROM all_docs",
    )
    .expect("buffered over simple");
    let err = create_view(&core, &mut session, "ALTER TABLE docs DROP COLUMN body")
        .expect_err("transitive dependency must block DROP COLUMN");
    assert_eq!(err.wire_code(), "2BP01");
}

// =============================================================================
// Issue #1360: ビュー本文の受理形の拡大（多者結合・CTE・集合演算・サブクエリ）
// ポインタ: TABLE-18・SQL-28・SQL-29・RLS-10 (b)
// =============================================================================

const TAGS: &str = "tags";

fn tags_schema() -> TableSchema {
    TableSchema::new(
        TAGS,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("tag", ColumnType::Text, false),
            ColumnDef::new("note_id", ColumnType::BigInt, false),
        ],
    )
}

fn insert_tag(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    tag: &str,
    note_id: i64,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TAGS,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(tag.to_string()),
            Value::BigInt(note_id),
        ],
        &op_id(&format!("tag-{id}")),
    )
    .expect("insert tag");
}

/// `docs`・`notes`（`seed_join_fixture`）に加えて `tags` を投入する。
/// tags は notes.id へ貼る: 200 = alice public → note 100、201 = alice private →
/// note 102、202 = bob public → note 102、203 = carol public → note 103。
fn seed_three_way_fixture(storage: &Storage) {
    storage.create_table(&tags_schema()).expect("create tags");
    insert_tag(
        storage,
        &ctx("alice"),
        200,
        "t-a-pub",
        100,
        Visibility::Public,
    );
    insert_tag(
        storage,
        &ctx("alice"),
        201,
        "t-a-priv",
        102,
        Visibility::Private,
    );
    insert_tag(
        storage,
        &ctx("bob"),
        202,
        "t-b-pub",
        102,
        Visibility::Public,
    );
    insert_tag(
        storage,
        &ctx("carol"),
        203,
        "t-c-pub",
        103,
        Visibility::Public,
    );
}

fn open_three_way_core(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    seed_join_fixture(&storage);
    seed_three_way_fixture(&storage);
    (new_core(storage), guard)
}

/// 3 テーブル JOIN 本文（INNER／LEFT）。3 つ目のテーブルにも参照者の RLS が効き、
/// 3 つ目の `DROP TABLE` は `2BP01`、`DROP COLUMN` は保守的に `2BP01`（SQL-28・TABLE-18）。
#[test]
fn three_way_join_view_body_matches_direct_and_tracks_dependencies() {
    let (core, _g) = open_three_way_core("view-three-way");
    let mut session = allowed_session();
    let inner = "SELECT docs.id, notes.title, tags.tag FROM docs INNER JOIN notes ON docs.id = notes.doc_id INNER JOIN tags ON notes.id = tags.note_id LIMIT 100";
    let left = "SELECT docs.id, notes.title, tags.tag FROM docs LEFT JOIN notes ON docs.id = notes.doc_id LEFT JOIN tags ON notes.id = tags.note_id LIMIT 100";
    create_view(&core, &mut session, &format!("CREATE VIEW j3 AS {inner}")).expect("inner");
    create_view(&core, &mut session, &format!("CREATE VIEW j3l AS {left}")).expect("left");
    for tenant in TENANTS {
        let v = scan(&core, tenant, "SELECT * FROM j3 LIMIT 100").expect("view");
        let d = scan(&core, tenant, inner).expect("direct");
        assert_eq!(cells(&v), cells(&d), "tenant={tenant} inner");
        let v = scan(&core, tenant, "SELECT * FROM j3l LIMIT 100").expect("view");
        let d = scan(&core, tenant, left).expect("direct");
        assert_eq!(cells(&v), cells(&d), "tenant={tenant} left");
    }
    let alice = scan(&core, "alice", "SELECT * FROM j3 LIMIT 100").expect("alice");
    assert!(!alice.rows.is_empty(), "fixture must produce joined rows");
    // carol は alice の private タグを結合結果に見ない。
    let carol = scan(&core, "carol", "SELECT tag FROM j3l LIMIT 100").expect("carol");
    for row in &carol.rows {
        assert_ne!(row.cells[0], Cell::Text("t-a-priv".to_string()));
    }
    let err = create_view(&core, &mut session, "DROP TABLE tags").expect_err("third table");
    assert_eq!(err.wire_code(), "2BP01");
    let err = create_view(&core, &mut session, "ALTER TABLE tags DROP COLUMN tag")
        .expect_err("conservative");
    assert_eq!(err.wire_code(), "2BP01");
    drop_view(&core, &mut session, "DROP VIEW j3").expect("drop j3");
    let err = create_view(&core, &mut session, "DROP TABLE tags").expect_err("still used by j3l");
    assert_eq!(err.wire_code(), "2BP01");
    drop_view(&core, &mut session, "DROP VIEW j3l").expect("drop j3l");
    create_view(&core, &mut session, "DROP TABLE tags").expect("drop after views are gone");
}

/// 本文の新しい受理形（CTE・括弧つき集合演算・サブクエリ 3 種）。ビュー経由の結果が
/// 直接実行と一致し（3 テナント対照。内側のサブクエリ・各枝・CTE も参照者の RLS で
/// 評価される。RLS-10 (b)）、`SELECT *`・列射影・`LIMIT`・`OFFSET` が効く。
#[test]
fn widened_buffered_bodies_match_direct_execution() {
    let (core, _g) = open_core("view-widened-bodies", true);
    let mut session = allowed_session();
    let bodies = [
        (
            "cte_ja",
            "WITH ja AS (SELECT id, body FROM docs WHERE lang = 'ja') SELECT id, body FROM ja LIMIT 100",
        ),
        (
            "cte_two",
            "WITH ja AS (SELECT id, body FROM docs WHERE lang = 'ja'), unused AS (SELECT title FROM notes) SELECT id FROM ja LIMIT 100",
        ),
        (
            "union_text",
            "(SELECT lang FROM docs) UNION (SELECT title FROM notes)",
        ),
        (
            "union_limited",
            "(SELECT body FROM docs ORDER BY body LIMIT 2) UNION ALL (SELECT title FROM notes ORDER BY title LIMIT 2)",
        ),
        (
            "in_sub",
            "SELECT id, lang FROM docs WHERE lang IN (SELECT lang FROM docs WHERE body LIKE '%private%' LIMIT 100) LIMIT 100",
        ),
        (
            "not_in_sub",
            "SELECT id, lang FROM docs WHERE lang NOT IN (SELECT lang FROM docs WHERE body LIKE '%private%' LIMIT 100) LIMIT 100",
        ),
        (
            "exists_sub",
            "SELECT id, lang FROM docs WHERE EXISTS (SELECT id FROM notes LIMIT 1) LIMIT 100",
        ),
        (
            "scalar_sub",
            "SELECT id, lang FROM docs WHERE lang = (SELECT MIN(lang) FROM docs) LIMIT 100",
        ),
        (
            "agg_in_sub",
            "SELECT lang, COUNT(*) AS n FROM docs WHERE lang IN (SELECT lang FROM docs WHERE body LIKE '%private%' LIMIT 100) GROUP BY lang LIMIT 100",
        ),
    ];
    for (name, body) in bodies {
        create_view(
            &core,
            &mut session,
            &format!("CREATE VIEW {name} AS {body}"),
        )
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
    }
    for tenant in TENANTS {
        for (name, body) in bodies {
            let direct = scan(&core, tenant, body).unwrap_or_else(|e| panic!("{body}: {e:?}"));
            let all = scan(&core, tenant, &format!("SELECT * FROM {name} LIMIT 100"))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            assert_eq!(cells(&all), cells(&direct), "tenant={tenant} view={name}");
            assert_eq!(all.columns, direct.columns, "tenant={tenant} view={name}");
            let off = scan(
                &core,
                tenant,
                &format!("SELECT * FROM {name} LIMIT 100 OFFSET 1"),
            )
            .expect(name);
            assert_eq!(
                cells(&off),
                cells(&direct).into_iter().skip(1).collect::<Vec<_>>(),
                "tenant={tenant} view={name} offset"
            );
        }
    }
    // サブクエリの内側も参照者の RLS で評価される: private 行を持つのは alice だけの
    // ため、内側の結果は alice では非空、carol では空（作成者 alice の可視性は
    // 引き継がれない）。
    let carol = scan(&core, "carol", "SELECT id FROM in_sub LIMIT 100").expect("carol");
    assert!(carol.rows.is_empty());
    let alice = scan(&core, "alice", "SELECT id FROM in_sub LIMIT 100").expect("alice");
    assert!(!alice.rows.is_empty());
    // 集合演算・CTE 本文は他テナントの行を混ぜない。
    let carol_union = scan(&core, "carol", "SELECT * FROM union_text LIMIT 100").expect("union");
    for row in &carol_union.rows {
        assert_ne!(row.cells[0], Cell::Text("a-priv".to_string()));
    }
}

/// 依存検査（`2BP01`）: CTE の参照されない定義・サブクエリの内側・集合演算の 2 番目の
/// 枝が読むテーブルの `DROP TABLE` は拒否され、`DROP COLUMN` は保守的に拒否される。
/// `DROP VIEW` の後は削除できる。
#[test]
fn widened_buffered_body_dependencies_cover_every_relation() {
    let forms = [
        (
            "cte_unused",
            "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja'), unused AS (SELECT title FROM notes) SELECT id FROM ja LIMIT 10",
        ),
        (
            "sub_inner",
            "SELECT id FROM docs WHERE lang IN (SELECT title FROM notes LIMIT 10) LIMIT 10",
        ),
        (
            "second_branch",
            "(SELECT lang FROM docs) UNION (SELECT title FROM notes)",
        ),
    ];
    for (name, body) in forms {
        let (core, _g) = open_core(&format!("view-widened-deps-{name}"), true);
        let mut session = allowed_session();
        create_view(
            &core,
            &mut session,
            &format!("CREATE VIEW {name} AS {body}"),
        )
        .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
        let err = create_view(&core, &mut session, "DROP TABLE notes").expect_err(name);
        assert_eq!(err.wire_code(), "2BP01", "{name}");
        let err = create_view(&core, &mut session, "DROP TABLE docs").expect_err(name);
        assert_eq!(err.wire_code(), "2BP01", "{name}");
        let err = create_view(&core, &mut session, "ALTER TABLE notes DROP COLUMN title")
            .expect_err(name);
        assert_eq!(err.wire_code(), "2BP01", "{name}");
        drop_view(&core, &mut session, &format!("DROP VIEW {name}")).expect("drop view");
        create_view(&core, &mut session, "DROP TABLE notes").expect("drop after view is gone");
    }
}

/// 本文の拒否（`42601`／`42P01`）と、評価後射影形ビューの連鎖拒否（CTE・サブクエリ・
/// 集合演算の枝のいずれから参照しても作成できない）。
#[test]
fn widened_buffered_body_rejections() {
    let (core, _g) = open_core("view-widened-reject", true);
    let mut session = allowed_session();
    create_view(
        &core,
        &mut session,
        "CREATE VIEW top_ja AS SELECT id, lang FROM docs ORDER BY id LIMIT 3",
    )
    .expect("buffered view");
    for sql in [
        // 評価後射影形ビューの連鎖
        "CREATE VIEW v AS WITH x AS (SELECT id FROM top_ja) SELECT id FROM x LIMIT 5",
        "CREATE VIEW v AS SELECT id FROM docs WHERE id IN (SELECT id FROM top_ja LIMIT 5) LIMIT 5",
        "CREATE VIEW v AS (SELECT id FROM docs) UNION (SELECT id FROM top_ja)",
        // UDF 述語・式述語（サブクエリ内側を含む）
        "CREATE VIEW v AS SELECT id FROM docs WHERE id IN (SELECT id FROM docs WHERE lower(lang) = 'ja' LIMIT 3) LIMIT 5",
        "CREATE VIEW v AS (SELECT id FROM docs WHERE lower(lang) = 'ja') UNION (SELECT id FROM docs)",
        // 本文として許可されない先頭
        "CREATE VIEW v AS EXPLAIN SELECT * FROM docs LIMIT 5",
        "CREATE VIEW v AS SET search_mode = 'hybrid'",
        // サブクエリの形（IN／EXISTS の内側は Scan のみ。集計は不可）
        "CREATE VIEW v AS SELECT id FROM docs WHERE lang IN (SELECT lang FROM docs GROUP BY lang LIMIT 3) LIMIT 5",
    ] {
        let err = create_view(&core, &mut session, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "sql={sql}");
    }
    for sql in [
        "CREATE VIEW v AS WITH x AS (SELECT id FROM missing_table) SELECT id FROM x LIMIT 5",
        "CREATE VIEW v AS SELECT id FROM docs WHERE id IN (SELECT id FROM missing_table LIMIT 5) LIMIT 5",
        "CREATE VIEW v AS (SELECT id FROM docs) UNION (SELECT id FROM missing_table)",
    ] {
        let err = create_view(&core, &mut session, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42P01", "sql={sql}");
    }
    // 何も永続化されていない。
    let err = scan(&core, "alice", "SELECT * FROM v LIMIT 5").expect_err("not persisted");
    assert_eq!(err.wire_code(), "42P01");
}

/// WITH・括弧つき集合演算・サブクエリ・二重引用符識別子を含む本文が、再オープン後も
/// 同じ結果で参照できる（検証済みトークン列の正規化描画 → 再検証の往復）。
#[test]
fn widened_buffered_bodies_persist_across_reopen() {
    let path = unique_db_path("view-widened-persist");
    let _guard = CleanupGuard(path.clone());
    let bodies = [
        (
            "p_cte",
            "WITH ja AS (SELECT id, body FROM docs WHERE body = 'it''s' AND lang = 'ja') SELECT id, body FROM ja LIMIT 50",
        ),
        (
            "p_union",
            "(SELECT lang FROM docs) UNION ALL (SELECT title FROM notes)",
        ),
        (
            "p_sub",
            "SELECT id FROM docs WHERE lang IN (SELECT lang FROM docs WHERE body LIKE '%private%' LIMIT 50) AND NOT EXISTS (SELECT id FROM notes WHERE title = 'zzz' LIMIT 1) LIMIT 50",
        ),
        (
            "p_quoted",
            "SELECT \"lang\", COUNT(*) AS n FROM docs GROUP BY \"lang\" LIMIT 50",
        ),
    ];
    let mut expected = Vec::new();
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed_base_fixture(&storage);
        seed_join_fixture(&storage);
        let core = new_core(storage);
        let mut session = allowed_session();
        for (name, body) in bodies {
            create_view(
                &core,
                &mut session,
                &format!("CREATE VIEW {name} AS {body};"),
            )
            .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
            let r = scan(&core, "alice", &format!("SELECT * FROM {name} LIMIT 100"))
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
            expected.push(cells(&r));
        }
    }
    {
        let storage = Storage::open(&path).expect("reopen storage");
        let core = new_core(storage);
        for ((name, _), want) in bodies.iter().zip(expected.iter()) {
            let r = scan(&core, "alice", &format!("SELECT * FROM {name} LIMIT 100"))
                .unwrap_or_else(|e| panic!("after reopen {name}: {e:?}"));
            assert_eq!(&cells(&r), want, "view={name}");
        }
    }
}

/// 新しい本文形（集合演算・CTE・サブクエリ）の Describe は本文を実行せず、実行結果と
/// 同じ列メタデータを返す。明示トランザクション内でも参照できる。
#[test]
fn widened_buffered_bodies_describe_and_run_in_transaction() {
    let (core, _g) = open_core("view-widened-describe", true);
    let mut session = allowed_session();
    for (name, body) in [
        (
            "d_union",
            "(SELECT lang FROM docs) UNION (SELECT title FROM notes)",
        ),
        (
            "d_cte",
            "WITH ja AS (SELECT id, body FROM docs WHERE lang = 'ja') SELECT id, body FROM ja LIMIT 20",
        ),
        (
            "d_sub",
            "SELECT id, lang FROM docs WHERE lang IN (SELECT lang FROM docs WHERE body LIKE '%private%' LIMIT 20) LIMIT 20",
        ),
    ] {
        create_view(&core, &mut session, &format!("CREATE VIEW {name} AS {body}"))
            .unwrap_or_else(|e| panic!("create {name}: {e:?}"));
        let sql = format!("SELECT * FROM {name} LIMIT 20");
        let parsed = core.parse_sql(&sql).expect("parse");
        let s = SessionState::default();
        let executed = scan(&core, "alice", &sql).expect("exec");
        if name == "d_sub" {
            // サブクエリ付きの SELECT は（ビュー経由でなくても）Describe 未対応
            // （束縛前にサブクエリを解決する実行アームのみが扱う）。fail-closed の 42601。
            let err = core
                .describe_parsed_in_session(&s, &parsed)
                .expect_err("describe of a subquery body");
            assert_eq!(err.wire_code(), "42601");
        } else {
            let described = core
                .describe_parsed_in_session(&s, &parsed)
                .expect("describe")
                .expect("has columns");
            assert_eq!(described, executed.columns, "view={name}");
        }

        let caller = ctx("alice");
        let mut s = SessionState::default();
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let SqlOutcome::Query(in_txn) = core
            .execute_sql_in_txn(&caller, &mut s, &mut txn, &sql)
            .expect("read in txn")
        else {
            panic!("expected Query");
        };
        assert_eq!(cells(&in_txn), cells(&executed), "view={name}");
        core.execute_sql_in_txn(&caller, &mut s, &mut txn, "COMMIT")
            .expect("commit");
    }
}
