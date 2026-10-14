//! `EngineCore::execute_bound_update_in_session`／
//! `execute_bound_delete_in_session`（NoSQL 表層向けの束縛済みセッション入口。
//! TASK-186・NOSQL-12・Issue #876。対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-6・NOSQL-12・
//! `docs/spec/04-behavior/sql-surface.md` SQL-17・SQL-18・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10）が engine
//! クレート外から到達可能な公開 API であり、SQL 表層
//! （`execute_update_sql`／`execute_delete_sql`）と**同一の実行器**・**同一の
//! 台帳キー空間**（`(tenant, table, operation_id)`）に到達することを固定する
//! 結合テスト（`tests/sql_insert_explain_public_api.rs` と同じ流儀）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::ValidatedUpdate;
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_update, BoundDelete};
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

/// SQL `INSERT`（宣言的入力・TASK-80）経由で行を投入する
/// （`tests/sql_update_single_row.rs::insert_row` と同じ理由: `insert_row`
/// 系の raw metadata ではなく、`row_codec::encode_scalar_columns` が書く
/// 正規のレイアウトで投入することで、read-merge-write（`update_row_columns_
/// unchecked`）が既存行を正しくデコードできるようにする）。可視性は常に
/// `Private`（`execute_insert` の固定仕様）。
fn seed_row(core: &EngineCore, ctx: &PolicyContext, id: u64, op: &str) {
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID '{op}'"
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .expect("seed insert should succeed");
}

fn op(raw: &str) -> OperationId {
    OperationId::parse(raw).expect("valid operation_id")
}

// ---------------------------------------------------------------------
// update: execute_bound_update_in_session の外部到達性・判定順序
// ---------------------------------------------------------------------

#[test]
fn execute_bound_update_in_session_is_reachable_and_updates_the_targeted_row() {
    let (core, _guard) = open_core("sql-update-session-reachable");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    let operation_id = op("nosql-update-1");
    let outcome = core
        .execute_bound_update_in_session(&owner, TABLE, Some(&operation_id), |schema| {
            let validated = ValidatedUpdate {
                table_name: TABLE.to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: Some(operation_id.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect("update ok");
    assert_eq!(outcome.rows_affected, 1);
}

#[test]
fn execute_bound_update_in_session_requires_operation_id_before_schema_lookup() {
    let owner = ctx(TENANT_A, [Visibility::Private]);
    // テーブルを一切作らない core でも、`operation_id` 欠落は `23502` として
    // スキーマ取得より先に判定される（判定順序が契約であることの固定）。
    let (bare_core, _bare_guard) = {
        let path = unique_db_path("sql-update-session-missing-opid-bare");
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        (
            EngineCore::from_storage(storage, Box::new(engine::kernel::CpuScalarProvider)),
            guard,
        )
    };
    let err = bare_core
        .execute_bound_update_in_session(&owner, "does_not_exist", None, |schema| {
            let validated = ValidatedUpdate {
                table_name: "does_not_exist".to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: None,
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn execute_bound_update_in_session_rejects_undefined_table_with_42p01() {
    let (core, _guard) = open_core("sql-update-session-undefined-table");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    let operation_id = op("nosql-update-undefined");
    let err = core
        .execute_bound_update_in_session(&owner, "does_not_exist", Some(&operation_id), |schema| {
            let validated = ValidatedUpdate {
                table_name: "does_not_exist".to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: Some(operation_id.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn execute_bound_update_in_session_rejects_bound_plan_operation_id_mismatch() {
    let (core, _guard) = open_core("sql-update-session-opid-mismatch");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    let requested = op("nosql-update-outer");
    let err = core
        .execute_bound_update_in_session(&owner, TABLE, Some(&requested), |schema| {
            // `bind` closure が引数と異なる `operation_id` を持つ計画を返す
            // （早期ガードと実書き込みの乖離を防ぐ判定の固定）。
            let inner = op("nosql-update-inner");
            let validated = ValidatedUpdate {
                table_name: TABLE.to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: Some(inner),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn execute_bound_update_in_session_matches_sql_ledger_key_space_for_resend_detection() {
    let (core, _guard) = open_core("sql-update-session-ledger-parity");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    // SQL 表層で同一 operation_id・同一内容の UPDATE をまず実行する。
    core.execute_update_sql(
        &owner,
        "UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'shared-op'",
    )
    .expect("sql update ok");

    // NoSQL 表層（束縛済み計画経由）が同じ operation_id・同じ内容で再送すると、
    // SQL 表層が書き込んだ台帳エントリと衝突し `23505`（同一内容の再送）になる
    // ことで、台帳キー空間が表層を跨いで共有されていることを固定する。
    let shared = op("shared-op");
    let err = core
        .execute_bound_update_in_session(&owner, TABLE, Some(&shared), |schema| {
            let validated = ValidatedUpdate {
                table_name: TABLE.to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: Some(shared.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect_err("must reject as duplicate");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn execute_bound_update_in_session_matches_sql_ledger_key_space_for_content_mismatch() {
    let (core, _guard) = open_core("sql-update-session-ledger-mismatch");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    core.execute_update_sql(
        &owner,
        "UPDATE docs SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'shared-op-2'",
    )
    .expect("sql update ok");

    let shared = op("shared-op-2");
    let err = core
        .execute_bound_update_in_session(&owner, TABLE, Some(&shared), |schema| {
            let validated = ValidatedUpdate {
                table_name: TABLE.to_string(),
                // 内容を変えて再送 → 内容不一致（`22023`）。
                assignments: vec![("lang".to_string(), engine_insert_literal_string("fr"))],
                id_literal: "1".to_string(),
                operation_id: Some(shared.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect_err("must reject as content mismatch");
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn execute_bound_update_in_session_returns_zero_rows_for_other_tenant_row() {
    let (core, _guard) = open_core("sql-update-session-other-tenant");
    let owner_a = ctx(TENANT_A, [Visibility::Private]);
    let owner_b = ctx(TENANT_B, [Visibility::Private]);
    seed_row(&core, &owner_b, 1, "seed-b-1");

    let operation_id = op("nosql-update-other-tenant");
    let outcome = core
        .execute_bound_update_in_session(&owner_a, TABLE, Some(&operation_id), |schema| {
            let validated = ValidatedUpdate {
                table_name: TABLE.to_string(),
                assignments: vec![("lang".to_string(), engine_insert_literal_string("en"))],
                id_literal: "1".to_string(),
                operation_id: Some(operation_id.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        })
        .expect("update ok (0 rows)");
    assert_eq!(outcome.rows_affected, 0);
}

// ---------------------------------------------------------------------
// delete: execute_bound_delete_in_session の外部到達性・判定順序
// ---------------------------------------------------------------------

#[test]
fn execute_bound_delete_in_session_is_reachable_and_deletes_the_targeted_row() {
    let (core, _guard) = open_core("sql-delete-session-reachable");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    let bound = BoundDelete {
        table: TABLE.to_string(),
        id: 1,
        operation_id: Some(op("nosql-delete-1")),
    };
    let outcome = core
        .execute_bound_delete_in_session(&owner, &bound)
        .expect("delete ok");
    assert_eq!(outcome.rows_affected, 1);
}

#[test]
fn execute_bound_delete_in_session_requires_operation_id() {
    let (core, _guard) = open_core("sql-delete-session-missing-opid");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    let bound = BoundDelete {
        table: TABLE.to_string(),
        id: 1,
        operation_id: None,
    };
    let err = core
        .execute_bound_delete_in_session(&owner, &bound)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn execute_bound_delete_in_session_rejects_undefined_table_with_42p01() {
    let (core, _guard) = open_core("sql-delete-session-undefined-table");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    let bound = BoundDelete {
        table: "does_not_exist".to_string(),
        id: 1,
        operation_id: Some(op("nosql-delete-undefined")),
    };
    let err = core
        .execute_bound_delete_in_session(&owner, &bound)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn execute_bound_delete_in_session_matches_sql_ledger_key_space_for_resend_detection() {
    let (core, _guard) = open_core("sql-delete-session-ledger-parity");
    let owner = ctx(TENANT_A, [Visibility::Private]);
    seed_row(&core, &owner, 1, "seed-1");

    core.execute_delete_sql(
        &owner,
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'del-op'",
    )
    .expect("sql delete ok");

    let bound = BoundDelete {
        table: TABLE.to_string(),
        id: 1,
        operation_id: Some(op("del-op")),
    };
    let err = core
        .execute_bound_delete_in_session(&owner, &bound)
        .expect_err("must reject as duplicate");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn execute_bound_delete_in_session_returns_zero_rows_for_missing_id() {
    let (core, _guard) = open_core("sql-delete-session-missing-row");
    let owner = ctx(TENANT_A, [Visibility::Private]);

    let bound = BoundDelete {
        table: TABLE.to_string(),
        id: 999,
        operation_id: Some(op("nosql-delete-missing")),
    };
    let outcome = core
        .execute_bound_delete_in_session(&owner, &bound)
        .expect("delete ok (0 rows)");
    assert_eq!(outcome.rows_affected, 0);
}

/// `ValidatedUpdate::assignments` 用の `SetValue::Literal(InsertLiteral::String)` を構築する
/// 薄いヘルパー（`engine::sql::allowlist::InsertLiteral` はテスト対象クレートの
/// 型そのもの。本ファイル内で複数回使うための命名の重複回避。Issue #1544 で
/// SET 右辺の型が `SetValue` になったため包む）。
fn engine_insert_literal_string(s: &str) -> engine::sql::allowlist::SetValue {
    engine::sql::allowlist::SetValue::Literal(engine::sql::allowlist::InsertLiteral::String(
        s.to_string(),
    ))
}
