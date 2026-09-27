//! `EngineCore::execute_bound_predicate_update_in_session`／
//! `execute_bound_predicate_delete_in_session`（NoSQL 表層 `update`／
//! `delete` op の `filter`〔述語形〕向けの束縛済みセッション入口。
//! TASK-186・NOSQL-12・Issue #1062。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-6・NOSQL-12・
//! `docs/spec/04-behavior/sql-surface.md` SQL-19・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10・RECOVER-11）が
//! engine クレート外から到達可能な公開 API であり、SQL 表層の述語形
//! `UPDATE`／`DELETE ... WHERE ... USING OPERATION_ID`（`execute_sql_in_session`
//! 経由）と**同一の実行器**・**同一の台帳キー空間**
//! （`(tenant, table, operation_id)`）に到達することを固定する結合テスト
//! （`tests/sql_update_delete_session_public_api.rs` と同じ流儀）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{InsertLiteral, SqlSurfaceError, WherePredicate};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const TENANT_A: &str = "tenant-a";
const TENANT_B: &str = "tenant-b";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str, visibilities: impl IntoIterator<Item = Visibility>) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, visibilities).expect("valid tenant ctx")
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));
    (core, guard)
}

fn seed_row(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, op: &str) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') \
         USING OPERATION_ID '{op}'"
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .expect("seed insert should succeed");
}

fn op(raw: &str) -> OperationId {
    OperationId::parse(raw).expect("valid operation_id")
}

fn lang_eq(value: &str) -> Vec<WherePredicate> {
    vec![WherePredicate::Equality {
        column: "lang".to_string(),
        value: value.to_string(),
    }]
}

// ---------------------------------------------------------------------
// update: execute_bound_predicate_update_in_session の外部到達性・判定順序
// ---------------------------------------------------------------------

#[test]
fn execute_bound_predicate_update_in_session_updates_matching_rows() {
    let (core, _guard) = open_core("predicate-update-session-reachable");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-1");
    seed_row(&core, &owner, 2, "ja", "seed-2");
    seed_row(&core, &owner, 3, "en", "seed-3");

    let operation_id = op("pred-update-1");
    let outcome = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("fr".to_string()))],
                lang_eq("ja"),
            ))
        })
        .expect("predicate update should succeed");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn execute_bound_predicate_update_in_session_requires_operation_id_before_schema_lookup() {
    let (core, _guard) = open_core("predicate-update-session-op-id-gate");
    let owner = ctx(TENANT_A, [Visibility::Private]);

    let err = core
        .execute_bound_predicate_update_in_session(&owner, "missing_table", None, |_schema| {
            panic!("bind closure must not run before the operation_id gate");
        })
        .expect_err("missing operation_id must be rejected");
    assert!(matches!(err, SqlSurfaceError::MissingOperationId));
}

#[test]
fn execute_bound_predicate_update_in_session_rejects_undefined_table_with_42p01() {
    let (core, _guard) = open_core("predicate-update-session-undefined-table");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    let operation_id = op("pred-update-undef");

    let err = core
        .execute_bound_predicate_update_in_session(
            &owner,
            "missing_table",
            Some(&operation_id),
            |_schema| panic!("bind closure must not run for an undefined table"),
        )
        .expect_err("undefined table must be rejected");
    assert!(matches!(err, SqlSurfaceError::UndefinedTable { .. }));
}

#[test]
fn execute_bound_predicate_update_in_session_rejects_empty_predicate_list_with_42601() {
    let (core, _guard) = open_core("predicate-update-session-empty-predicate");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-empty");
    let operation_id = op("pred-update-empty");

    let err = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("fr".to_string()))],
                Vec::new(),
            ))
        })
        .expect_err("empty predicate list must be rejected");
    assert!(matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }));
}

#[test]
fn execute_bound_predicate_update_in_session_rejects_predicate_call_form() {
    // NoSQL `filter` は列名として RLS 述語名を拒否するため
    // `PredicateCall`（`visible()`）を生成しない契約だが、`bind` closure
    // 自体は engine の外にあるため、契約が壊れた場合の多層防御
    // （`reject_unsupported_predicate_dml_forms`）を固定する。
    let (core, _guard) = open_core("predicate-update-session-predicate-call");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-call");
    let operation_id = op("pred-update-call");

    let err = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("fr".to_string()))],
                vec![WherePredicate::PredicateCall {
                    name: "visible".to_string(),
                }],
            ))
        })
        .expect_err("PredicateCall form must be rejected");
    assert!(matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }));
}

#[test]
fn execute_bound_predicate_update_in_session_matches_sql_ledger_key_space_for_resend_detection() {
    let (core, _guard) = open_core("predicate-update-session-ledger-parity");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-parity");

    // SQL 表層の述語形 UPDATE で先に記録する。
    core.execute_sql_in_session(
        &owner,
        &mut SessionState::default(),
        "UPDATE docs SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'pred-parity-1'",
    )
    .expect("sql predicate update should succeed");

    // 同一 `operation_id`・同一内容で NoSQL 経路から再送すると `23505`。
    let operation_id = op("pred-parity-1");
    let err = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("fr".to_string()))],
                lang_eq("ja"),
            ))
        })
        .expect_err("same-content resend must be a duplicate");
    assert!(matches!(err, SqlSurfaceError::DuplicateOperationId));
}

#[test]
fn execute_bound_predicate_update_in_session_matches_sql_ledger_key_space_for_content_mismatch() {
    let (core, _guard) = open_core("predicate-update-session-ledger-mismatch");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-mismatch");

    core.execute_sql_in_session(
        &owner,
        &mut SessionState::default(),
        "UPDATE docs SET lang = 'fr' WHERE lang = 'ja' USING OPERATION_ID 'pred-mismatch-1'",
    )
    .expect("sql predicate update should succeed");

    // 同一 `operation_id` だが SET 値が異なる ⇒ `22023`。
    let operation_id = op("pred-mismatch-1");
    let err = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("de".to_string()))],
                lang_eq("ja"),
            ))
        })
        .expect_err("different-content resend must be a content mismatch");
    assert!(matches!(err, SqlSurfaceError::OperationIdContentMismatch));
}

#[test]
fn execute_bound_predicate_update_in_session_does_not_affect_other_tenant_rows() {
    let (core, _guard) = open_core("predicate-update-session-tenant-isolation");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    let foreign = ctx(TENANT_B, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-a");
    seed_row(&core, &foreign, 2, "ja", "seed-b");

    let operation_id = op("pred-update-isolation");
    let outcome = core
        .execute_bound_predicate_update_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok((
                vec![("lang".to_string(), InsertLiteral::String("fr".to_string()))],
                lang_eq("ja"),
            ))
        })
        .expect("predicate update should succeed");
    // tenant-a の可視行は 1 件のみ（tenant-b の行は数えない・変更しない）。
    assert_eq!(outcome.rows_affected, 1);
}

// ---------------------------------------------------------------------
// delete: execute_bound_predicate_delete_in_session の外部到達性・判定順序
// ---------------------------------------------------------------------

#[test]
fn execute_bound_predicate_delete_in_session_deletes_matching_rows() {
    let (core, _guard) = open_core("predicate-delete-session-reachable");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-1");
    seed_row(&core, &owner, 2, "ja", "seed-2");
    seed_row(&core, &owner, 3, "en", "seed-3");

    let operation_id = op("pred-delete-1");
    let outcome = core
        .execute_bound_predicate_delete_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok(lang_eq("ja"))
        })
        .expect("predicate delete should succeed");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn execute_bound_predicate_delete_in_session_requires_operation_id() {
    let (core, _guard) = open_core("predicate-delete-session-op-id-gate");
    let owner = ctx(TENANT_A, [Visibility::Private]);

    let err = core
        .execute_bound_predicate_delete_in_session(&owner, "missing_table", None, |_schema| {
            panic!("bind closure must not run before the operation_id gate");
        })
        .expect_err("missing operation_id must be rejected");
    assert!(matches!(err, SqlSurfaceError::MissingOperationId));
}

#[test]
fn execute_bound_predicate_delete_in_session_rejects_empty_predicate_list_with_42601() {
    let (core, _guard) = open_core("predicate-delete-session-empty-predicate");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-empty");
    let operation_id = op("pred-delete-empty");

    let err = core
        .execute_bound_predicate_delete_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok(Vec::new())
        })
        .expect_err("empty predicate list must be rejected");
    assert!(matches!(err, SqlSurfaceError::UnsupportedSyntax { .. }));
}

#[test]
fn execute_bound_predicate_delete_in_session_matches_sql_ledger_key_space_for_resend_detection() {
    let (core, _guard) = open_core("predicate-delete-session-ledger-parity");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-parity");

    core.execute_sql_in_session(
        &owner,
        &mut SessionState::default(),
        "DELETE FROM docs WHERE lang = 'ja' USING OPERATION_ID 'pred-delete-parity-1'",
    )
    .expect("sql predicate delete should succeed");

    let operation_id = op("pred-delete-parity-1");
    let err = core
        .execute_bound_predicate_delete_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok(lang_eq("ja"))
        })
        .expect_err("same-content resend must be a duplicate");
    assert!(matches!(err, SqlSurfaceError::DuplicateOperationId));
}

#[test]
fn execute_bound_predicate_delete_in_session_does_not_affect_other_tenant_rows() {
    let (core, _guard) = open_core("predicate-delete-session-tenant-isolation");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    let foreign = ctx(TENANT_B, [Visibility::Private]);
    seed_row(&core, &owner, 1, "ja", "seed-a");
    seed_row(&core, &foreign, 2, "ja", "seed-b");

    let operation_id = op("pred-delete-isolation");
    let outcome = core
        .execute_bound_predicate_delete_in_session(&owner, TABLE, Some(&operation_id), |_schema| {
            Ok(lang_eq("ja"))
        })
        .expect("predicate delete should succeed");
    assert_eq!(outcome.rows_affected, 1);
}

// ---------------------------------------------------------------------
// NUMERIC 列 eq: `wire-server::http::query::filter::declare_one` が使う
// `CompareLiteral::Literal`（NoSQL scan/search・declare_one().bind() による
// 事前検証経路）と、述語形 DML の実際の実行経路（`WherePredicate::Equality`
// → `sql::parser::declarative_leaf_to_filter` → `DeclarativeFilter::compare`
// の `CompareLiteral::Text`）が同じ文字列リテラルテキストで整合すること、
// すなわち `filter.rs::bind_filter_where_predicates` の不変条件
// 「declare_one で検証してから変換する」が変換後の実行経路でも成功する
// ことを固定する（Issue #1062）。
// ---------------------------------------------------------------------

fn numeric_table_schema() -> TableSchema {
    TableSchema::new(
        "amounts",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn open_numeric_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&numeric_table_schema())
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider));
    (core, guard)
}

#[test]
fn execute_bound_predicate_update_in_session_accepts_numeric_eq_literal_text_matching_wire_mapping()
{
    let (core, _guard) = open_numeric_core("predicate-update-session-numeric-eq");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    core.execute_sql_in_session(
        &owner,
        &mut SessionState::default(),
        "INSERT INTO amounts (id, embedding, amount) VALUES (1, '[0.1,0.2,0.3]', 1.50) \
         USING OPERATION_ID 'seed-numeric-1'",
    )
    .expect("seed insert should succeed");

    // `typed_json::number_literal_text` が JSON 数値 `1.5` から生成するのと
    // 同じテキスト形（`filter.rs::where_predicate_for` の NUMERIC 分岐と同一）。
    let operation_id = op("pred-update-numeric-1");
    let outcome = core
        .execute_bound_predicate_update_in_session(
            &owner,
            "amounts",
            Some(&operation_id),
            |_schema| {
                Ok((
                    vec![(
                        "amount".to_string(),
                        InsertLiteral::Number("9.99".to_string()),
                    )],
                    vec![WherePredicate::Equality {
                        column: "amount".to_string(),
                        value: "1.5".to_string(),
                    }],
                ))
            },
        )
        .expect("predicate update with NUMERIC eq filter should succeed");
    assert_eq!(outcome.rows_affected, 1);
}
