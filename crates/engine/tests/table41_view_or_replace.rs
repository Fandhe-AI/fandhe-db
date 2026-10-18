//! `CREATE OR REPLACE VIEW` の互換性検査（TABLE-41 (2)・TABLE-18・Issue #1660）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-41・TABLE-18・RLS-10 (b)。
//!
//! `table41_view_column_aliases.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。
//!
//! 検証する契約:
//! 1. 不在なら通常の作成、既存なら「既存列を同名・同型・同順で保ち末尾に足すだけ」の置換のみ成功
//! 2. 列の削除・改名・順序変更・型変更は `42P16` で、既存定義は変わらない
//! 3. 同名がテーブル・索引なら `42809`、自己参照は `42P17`
//! 4. 依存ビューを持つビューの置換は `0A000`（Issue #1661 までの暫定制限。状態不変）
//! 5. DDL 権限ゲートが先（`42501`）、明示トランザクション内 DDL は `0A000`
//! 6. RLS は参照セッションの `PolicyContext` で評価され、置換後も永続化される

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{ColumnMeta, QueryResult};
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
    tenant: &str,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件（ja）。
fn seed(storage: &Storage) {
    insert_row(storage, "alice", 1, "ja", "a1", Visibility::Public);
    insert_row(storage, "alice", 2, "ja", "a2", Visibility::Public);
    insert_row(storage, "alice", 3, "en", "a3", Visibility::Public);
    insert_row(storage, "alice", 4, "ja", "a4", Visibility::Private);
    insert_row(storage, "bob", 5, "ja", "b1", Visibility::Public);
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed(&storage);
    (new_core(storage), guard)
}

fn ddl(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx("alice"), session, sql)
}

fn create_ok(core: &EngineCore, sql: &str) {
    let mut session = allowed_session();
    ddl(core, &mut session, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn create_err(core: &EngineCore, sql: &str) -> String {
    let mut session = allowed_session();
    ddl(core, &mut session, sql)
        .err()
        .unwrap_or_else(|| panic!("must fail: {sql}"))
        .wire_code()
        .to_string()
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn q(core: &EngineCore, sql: &str) -> QueryResult {
    query(core, "alice", sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. } => name.clone(),
        })
        .collect()
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

// --- 1. 作成と互換な置換 --------------------------------------------------------

/// 不在のビューへの `CREATE OR REPLACE VIEW` は通常の作成と同じ。
#[test]
fn replace_creates_view_when_absent() {
    let (core, _g) = open_core("t41r-absent");
    create_ok(&core, "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs");
    let r = q(&core, "SELECT * FROM v LIMIT 100");
    assert_eq!(names(&r), ["lang"]);
    assert_eq!(ids(&r), vec![1, 2, 3, 4, 5]);
}

/// 同一定義の再置換・末尾への列追加・`WHERE` のみの変更が成功し、参照結果が新定義に従う。
#[test]
fn compatible_replacements_succeed_and_take_effect() {
    let (core, _g) = open_core("t41r-compat");
    create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
    create_ok(&core, "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs");
    create_ok(
        &core,
        "CREATE OR REPLACE VIEW v AS SELECT lang, body FROM docs",
    );
    assert_eq!(
        names(&q(&core, "SELECT * FROM v LIMIT 100")),
        ["lang", "body"]
    );
    create_ok(
        &core,
        "CREATE OR REPLACE VIEW v AS SELECT lang, body FROM docs WHERE lang = 'en'",
    );
    assert_eq!(ids(&q(&core, "SELECT * FROM v LIMIT 100")), vec![3]);
}

/// `SELECT *` の旧ビューに対し、同じ列を同順で明示する新定義は成功する。
#[test]
fn star_view_can_be_replaced_by_explicit_same_columns() {
    let (core, _g) = open_core("t41r-star");
    create_ok(&core, "CREATE VIEW v AS SELECT * FROM docs");
    create_ok(
        &core,
        "CREATE OR REPLACE VIEW v AS SELECT id, embedding, lang, body FROM docs",
    );
    assert_eq!(
        names(&q(&core, "SELECT * FROM v LIMIT 100")),
        ["id", "embedding", "lang", "body"]
    );
}

/// 本文の形（単純形 → 評価後射影形）が変わっても、列の名前・型・順序が同じなら成功する。
#[test]
fn body_shape_change_with_same_columns_succeeds() {
    let (core, _g) = open_core("t41r-shape");
    create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
    create_ok(
        &core,
        "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs LIMIT 2",
    );
    assert_eq!(q(&core, "SELECT * FROM v LIMIT 100").rows.len(), 2);
}

/// キーワードの大文字小文字で結果は変わらない。
#[test]
fn keywords_are_case_insensitive() {
    let (core, _g) = open_core("t41r-case");
    create_ok(&core, "create view v as select lang from docs");
    create_ok(
        &core,
        "create or replace view v as select lang, body from docs",
    );
    assert_eq!(
        create_err(&core, "create or replace view v as select lang from docs"),
        "42P16"
    );
}

// --- 2. 非互換な置換は 42P16 で拒否し既存定義を変えない -----------------------------

fn assert_rejected_42p16_and_unchanged(label: &str, new_sql: &str) {
    let (core, _g) = open_core(label);
    create_ok(&core, "CREATE VIEW v AS SELECT lang, body FROM docs");
    let before = q(&core, "SELECT * FROM v LIMIT 100");
    assert_eq!(create_err(&core, new_sql), "42P16", "sql={new_sql}");
    let after = q(&core, "SELECT * FROM v LIMIT 100");
    assert_eq!(names(&after), names(&before));
    assert_eq!(ids(&after), ids(&before));
    assert_eq!(after.rows.len(), 5);
}

#[test]
fn dropping_a_column_is_rejected() {
    assert_rejected_42p16_and_unchanged(
        "t41r-drop",
        "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs",
    );
}

#[test]
fn renaming_a_column_is_rejected() {
    assert_rejected_42p16_and_unchanged(
        "t41r-rename",
        "CREATE OR REPLACE VIEW v AS SELECT lang, id FROM docs",
    );
    assert_rejected_42p16_and_unchanged(
        "t41r-alias-rename",
        "CREATE OR REPLACE VIEW v (x, body) AS SELECT lang, body FROM docs",
    );
}

#[test]
fn reordering_columns_is_rejected() {
    assert_rejected_42p16_and_unchanged(
        "t41r-reorder",
        "CREATE OR REPLACE VIEW v AS SELECT body, lang FROM docs",
    );
}

#[test]
fn changing_a_column_type_is_rejected() {
    assert_rejected_42p16_and_unchanged(
        "t41r-type",
        "CREATE OR REPLACE VIEW v AS SELECT lang, COUNT(*) AS body FROM docs GROUP BY lang",
    );
}

/// 別名を外すと導出名が変わる場合も改名として拒否する。
#[test]
fn dropping_an_alias_that_renamed_a_column_is_rejected() {
    let (core, _g) = open_core("t41r-unalias");
    create_ok(&core, "CREATE VIEW v (x) AS SELECT lang FROM docs");
    assert_eq!(
        create_err(&core, "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs"),
        "42P16"
    );
    assert_eq!(names(&q(&core, "SELECT * FROM v LIMIT 100")), ["x"]);
}

// --- 3. 種別違い・自己参照・構文 -----------------------------------------------------

#[test]
fn replacing_a_table_or_index_name_is_wrong_object_type() {
    let (core, _g) = open_core("t41r-kind");
    assert_eq!(
        create_err(
            &core,
            "CREATE OR REPLACE VIEW docs AS SELECT lang FROM docs"
        ),
        "42809"
    );
    create_ok(&core, "CREATE INDEX idx_lang ON docs (lang)");
    assert_eq!(
        create_err(
            &core,
            "CREATE OR REPLACE VIEW idx_lang AS SELECT lang FROM docs"
        ),
        "42809"
    );
    assert_eq!(q(&core, "SELECT id FROM docs LIMIT 100").rows.len(), 5);
}

#[test]
fn self_reference_is_rejected_as_invalid_object_definition() {
    let (core, _g) = open_core("t41r-self");
    create_ok(&core, "CREATE VIEW v AS SELECT * FROM docs");
    assert_eq!(
        create_err(&core, "CREATE OR REPLACE VIEW v AS SELECT * FROM v"),
        "42P17"
    );
    assert_eq!(q(&core, "SELECT id FROM v LIMIT 100").rows.len(), 5);
}

#[test]
fn syntax_and_body_errors_use_existing_codes() {
    let (core, _g) = open_core("t41r-syntax");
    create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
    for (sql, code) in [
        (
            "CREATE OR REPLACE VIEW IF NOT EXISTS v AS SELECT lang FROM docs",
            "42601",
        ),
        (
            "CREATE OR REPLACE TABLE v AS SELECT lang FROM docs",
            "42601",
        ),
        ("CREATE OR VIEW v AS SELECT lang FROM docs", "42601"),
        ("CREATE OR REPLACE VIEW v AS SELECT lang FROM nope", "42P01"),
        (
            "CREATE OR REPLACE VIEW v (a, b, c) AS SELECT lang FROM docs",
            "42601",
        ),
        (
            "CREATE OR REPLACE VIEW v (a, a) AS SELECT lang, body FROM docs",
            "42701",
        ),
    ] {
        assert_eq!(create_err(&core, sql), code, "sql={sql}");
    }
    assert_eq!(names(&q(&core, "SELECT * FROM v LIMIT 100")), ["lang"]);
}

// --- 4. 依存ビュー（Issue #1661 までの暫定制限） --------------------------------------

#[test]
fn replacing_a_view_with_dependents_is_not_supported_yet() {
    let (core, _g) = open_core("t41r-dep");
    create_ok(&core, "CREATE VIEW v1 AS SELECT lang FROM docs");
    create_ok(&core, "CREATE VIEW v2 AS SELECT lang FROM v1");
    assert_eq!(
        create_err(
            &core,
            "CREATE OR REPLACE VIEW v1 AS SELECT lang, body FROM docs"
        ),
        "0A000"
    );
    assert_eq!(names(&q(&core, "SELECT * FROM v1 LIMIT 100")), ["lang"]);
    assert_eq!(names(&q(&core, "SELECT * FROM v2 LIMIT 100")), ["lang"]);
    // 非互換は依存より先に判定する（PostgreSQL と同じ `42P16`）。
    assert_eq!(
        create_err(&core, "CREATE OR REPLACE VIEW v1 AS SELECT body FROM docs"),
        "42P16"
    );
}

// --- 5. 権限・トランザクション -------------------------------------------------------

#[test]
fn ddl_permission_gate_precedes_compatibility_check() {
    let (core, _g) = open_core("t41r-perm");
    create_ok(&core, "CREATE VIEW v AS SELECT lang, body FROM docs");
    let mut denied = SessionState::default();
    for sql in [
        "CREATE OR REPLACE VIEW v AS SELECT lang FROM docs",
        "CREATE OR REPLACE VIEW v AS SELECT lang, body, id FROM docs",
        "CREATE OR REPLACE VIEW nope AS SELECT lang FROM docs",
        "CREATE OR REPLACE VIEW docs AS SELECT lang FROM docs",
    ] {
        let err = ddl(&core, &mut denied, sql).expect_err("denied");
        assert_eq!(err.wire_code(), "42501", "sql={sql}");
    }
}

#[test]
fn replace_inside_explicit_transaction_is_not_supported() {
    let (core, _g) = open_core("t41r-txn");
    create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
    let caller = ctx("alice");
    let mut s = allowed_session();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut s,
            &mut txn,
            "CREATE OR REPLACE VIEW v AS SELECT lang, body FROM docs",
        )
        .expect_err("ddl in txn");
    assert_eq!(err.wire_code(), "0A000");
}

// --- 6. RLS・永続化 ---------------------------------------------------------------

#[test]
fn replaced_view_is_evaluated_under_the_callers_policy() {
    let (core, _g) = open_core("t41r-rls");
    create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
    create_ok(
        &core,
        "CREATE OR REPLACE VIEW v AS SELECT lang, body FROM docs",
    );
    let bob = query(&core, "bob", "SELECT * FROM v LIMIT 100").expect("bob");
    // public 行は全テナントから見え、alice の private 行（4）は bob から見えない。
    assert_eq!(ids(&bob), vec![1, 2, 3, 5]);
    let alice = q(&core, "SELECT * FROM v LIMIT 100");
    assert_eq!(ids(&alice), vec![1, 2, 3, 4, 5]);
}

#[test]
fn replaced_definition_survives_reopen() {
    let path = unique_db_path("t41r-persist");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        seed(&storage);
        let core = new_core(storage);
        create_ok(&core, "CREATE VIEW v AS SELECT lang FROM docs");
        create_ok(
            &core,
            "CREATE OR REPLACE VIEW v AS SELECT lang, body FROM docs",
        );
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let core = new_core(storage);
    assert_eq!(
        names(&q(&core, "SELECT * FROM v LIMIT 100")),
        ["lang", "body"]
    );
}

// --- 7. カタログ層の compare-and-swap ------------------------------------------------

/// 互換性検査に使った旧定義が現在の定義と食い違えば、置換せず `ViewDefinitionChanged` を返す。
#[test]
fn catalog_replace_requires_matching_expected_definition() {
    use engine::catalog::CatalogError;
    let path = unique_db_path("t41r-cas");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
        .create_view("v", "docs", "SELECT lang FROM docs")
        .expect("create view");
    let current = storage.view_definition("v").expect("read").expect("exists");

    // 期待が「不在」なのに存在する。
    let err = storage
        .create_or_replace_view("v", "docs", "SELECT lang, body FROM docs", &[], None)
        .expect_err("stale expectation");
    assert!(matches!(err, CatalogError::ViewDefinitionChanged(_)));

    // 期待が別の定義。
    let mut stale = current.clone();
    stale.body_sql = "SELECT body FROM docs".to_string();
    let err = storage
        .create_or_replace_view(
            "v",
            "docs",
            "SELECT lang, body FROM docs",
            &[],
            Some(&stale),
        )
        .expect_err("stale definition");
    assert!(matches!(err, CatalogError::ViewDefinitionChanged(_)));
    assert_eq!(
        storage.view_definition("v").expect("read"),
        Some(current.clone())
    );

    // 一致していれば置換される。
    storage
        .create_or_replace_view(
            "v",
            "docs",
            "SELECT lang, body FROM docs",
            &[],
            Some(&current),
        )
        .expect("replace");
    assert_ne!(storage.view_definition("v").expect("read"), Some(current));
}
