//! `DROP TABLE <table>`（TASK-203、対象ビヘイビア: SQL-23・TABLE-15）の結合テスト。
//! ポインタ: `docs/spec/05-tasks.md` TASK-203・`docs/spec/04-behavior/sql-surface.md`
//! SQL-23・`docs/spec/04-behavior/table-behavior.md` TABLE-15。関連ポインタ:
//! RECOVER-2（`operation_id` 台帳のテーブル単位削除）・RLS-9（存在情報の
//! 非漏えい）。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `DROP` の覗き見判定 →
//! `sql::allowlist::validate_drop_table_tokens`（カタログ照会なし） →
//! `sql::ddl::require_ddl_permission`（DDL 実行権限ゲート） →
//! `sql::ddl::execute_drop_table` → `catalog::Storage::drop_table`）を
//! production 経路として検証する。`truncate_table.rs` と同じ流儀
//! （実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("drop-table");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn insert_row(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, seq: u64) {
    core.insert_row(
        ctx,
        table,
        id,
        &RowInput {
            tenant_id: ctx.tenant_id(),
            visibility: Visibility::Public,
            embedding: &[0.1f32, 0.2f32],
            metadata: &[],
        },
        Some(&op_id(&format!("seed-{table}-{id}-{seq}"))),
    )
    .expect("insert row");
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

/// SQL-23: DDL 実行権限を持たない既定セッションは、対象テーブルの実在有無に
/// 関わらず常に `42501` で拒否される（DDL 権限をテーブル存在のオラクルに
/// しない。`sql::ddl::require_ddl_permission` ドキュメント参照）。
#[test]
fn drop_table_without_permission_is_rejected_with_42501_regardless_of_table_existence() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(&alice, &mut session, &format!("DROP TABLE {TABLE}"))
        .expect_err("default session must be denied");
    assert_eq!(err.wire_code(), "42501");

    let err_missing = core
        .execute_sql_in_session(&alice, &mut session, "DROP TABLE table_does_not_exist")
        .expect_err("default session must be denied even for a nonexistent table");
    assert_eq!(err_missing.wire_code(), "42501");
    assert_eq!(
        err.wire_code(),
        err_missing.wire_code(),
        "existing vs nonexistent table must be indistinguishable to an unprivileged session"
    );

    // 権限が無いので当然テーブルは残ったまま。
    let mut allowed = allowed_session();
    core.execute_sql_in_session(
        &alice,
        &mut allowed,
        &format!("SELECT COUNT(*) FROM {TABLE}"),
    )
    .expect("table must still exist");
}

/// 成功系: DDL 実行権限を持つセッションが `DROP TABLE` を実行すると、カタログ
/// エントリ・全テナントの行・`operation_id` 台帳エントリが削除され、以後
/// SELECT／INSERT／TRUNCATE／UPDATE／DELETE がいずれも `42P01`（テーブル
/// 未存在）になる。
#[test]
fn drop_table_removes_catalog_rows_and_ledger_for_all_tenants() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    insert_row(&core, &alice, TABLE, 1, 1);
    insert_row(&core, &bob, TABLE, 2, 2);

    let mut session = allowed_session();
    let outcome = core
        .execute_sql_in_session(&alice, &mut session, &format!("DROP TABLE {TABLE}"))
        .expect("DROP TABLE should succeed");
    assert!(matches!(outcome, SqlOutcome::DropTable(_)));

    for sql in [
        format!("SELECT * FROM {TABLE} LIMIT 1"),
        format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (99, '[0.1,0.2]') USING OPERATION_ID 'post-drop'"
        ),
        format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'post-drop-truncate'"),
        format!("UPDATE {TABLE} SET embedding = '[0.1,0.2]' WHERE id = 1 USING OPERATION_ID 'post-drop-update'"),
        format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'post-drop-delete'"),
    ] {
        let mut s = SessionState::default();
        let err = core
            .execute_sql_in_session(&alice, &mut s, &sql)
            .expect_err(&format!("{sql} must fail after DROP TABLE"));
        assert_eq!(err.wire_code(), "42P01", "unexpected wire_code for: {sql}");
    }
}

/// 応答の同一性（RLS-9 と同型の存在情報非漏えい）: 他テナントの行が 0 件でも
/// 多数でも、`DROP TABLE` の成功応答は区別不能（`DropTableOutcome` は件数を
/// 一切持たないフィールドなし構造体）。
#[test]
fn drop_table_response_is_identical_regardless_of_other_tenant_row_count() {
    // ケース 1: bob の行が 0 件。
    {
        let (core, path) = new_core_with_table();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice");
        insert_row(&core, &alice, TABLE, 1, 1);

        let mut session = allowed_session();
        let outcome = core
            .execute_sql_in_session(&alice, &mut session, &format!("DROP TABLE {TABLE}"))
            .expect("DROP TABLE should succeed with zero other-tenant rows");
        assert!(matches!(outcome, SqlOutcome::DropTable(_)));
    }

    // ケース 2: bob の行が多数。応答の型・成否は同一。
    {
        let (core, path) = new_core_with_table();
        let _guard = CleanupGuard(path);
        let alice = ctx_for("alice");
        let bob = ctx_for("bob");
        insert_row(&core, &alice, TABLE, 1, 1);
        for id in 10..30 {
            insert_row(&core, &bob, TABLE, id, id);
        }

        let mut session = allowed_session();
        let outcome = core
            .execute_sql_in_session(&alice, &mut session, &format!("DROP TABLE {TABLE}"))
            .expect("DROP TABLE should succeed with many other-tenant rows");
        assert!(matches!(outcome, SqlOutcome::DropTable(_)));
    }
}

/// 不在テーブル: 権限を持つセッションでも、対象テーブルが元々存在しなければ
/// `42P01`（他テナントの `TableNotFound` と区別しない既存 `CatalogError`
/// 契約をそのまま継承する）。
#[test]
fn drop_table_missing_table_is_rejected_with_42p01() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let mut session = allowed_session();
    let err = core
        .execute_sql_in_session(&alice, &mut session, "DROP TABLE table_does_not_exist")
        .expect_err("missing table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

/// SQL-8 の許可リスト原則: `CASCADE`／`RESTRICT` の併記・`USING OPERATION_ID` 句・
/// `EXPLAIN DROP TABLE` はいずれも `42601`
/// （構造上受理しない。権限の有無に関わらない——構文検証はカタログ照会・
/// 権限判定より前段のため）。
#[test]
fn drop_table_rejects_unsupported_shapes_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let mut session = allowed_session();

    for sql in [
        // `CASCADE`／`RESTRICT` 単独は TABLE-37 (2)(3)（Issue #1641）で受理に変わった。
        // `sql_drop_table_cascade.rs` が検証する。2 語並べた形は余剰トークンとして拒否する。
        format!("DROP TABLE {TABLE} CASCADE RESTRICT"),
        format!("DROP TABLE {TABLE} CASCADE CASCADE"),
        // 複数テーブル指定は TABLE-37 (1)（Issue #1639）で受理に変わった。
        // `sql_drop_table_multi.rs` が受理側と不正形の拒否を検証する。
        format!("DROP TABLE {TABLE} USING OPERATION_ID 'op-1'"),
        format!("EXPLAIN DROP TABLE {TABLE}"),
        "DROP TABLE".to_string(),
    ] {
        let err = core
            .execute_sql_in_session(&alice, &mut session, &sql)
            .expect_err(&format!("{sql} must be rejected"));
        assert_eq!(err.wire_code(), "42601", "unexpected wire_code for: {sql}");
    }

    // 権限を持つセッションでも構文が拒否された以上、テーブルは無傷のまま。
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("SELECT COUNT(*) FROM {TABLE}"),
    )
    .expect("table must still exist after rejected DROP TABLE variants");
}

/// セッションを持たない後方互換 API（`execute_sql`）は `DROP TABLE` を一律
/// `42601` で拒否する（`SET`・`CREATE FUNCTION` と同じ「セッション必須の文は
/// 非セッション入口で一律 `42601`」の契約。DDL 実行権限はセッション単位の
/// 状態のため、セッションを持たない入口では原理的に権限を評価できない）。
#[test]
fn drop_table_via_non_session_entry_point_is_rejected_with_42601() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql(&alice, &format!("DROP TABLE {TABLE}"))
        .expect_err("non-session entry point must reject DROP TABLE");
    assert_eq!(err.wire_code(), "42601");
}

/// 再作成と台帳汚染の防止: drop 後に同名テーブルを異なる次元で再作成すると、
/// drop 前に使った `operation_id` を再利用した INSERT が成功する（台帳が
/// テーブルごと削除されているため `23505` にならない）。`EngineCore` は
/// `Storage` の再オープン用アクセサを公開しないため、`core` をスコープで
/// 閉じてファイルロックを解放してから同一パスを `Storage::open` で再オープン
/// する（redb は同一パスの再オープンを許容する。既存の再オープン系テスト
/// と同じ流儀）。
#[test]
fn drop_table_then_recreate_allows_reusing_prior_operation_ids() {
    let path = unique_db_path("drop-table-recreate");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema(TABLE)).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

        insert_row(&core, &alice, TABLE, 1, 1);

        let mut ddl_session = allowed_session();
        core.execute_sql_in_session(&alice, &mut ddl_session, &format!("DROP TABLE {TABLE}"))
            .expect("DROP TABLE should succeed");
    }

    // 異なる次元（3 次元）で再作成する。
    let storage = Storage::open(&path).expect("reopen storage");
    let new_schema = TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
    );
    storage.create_table(&new_schema).expect("recreate table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    // drop 前に使った `operation_id`（`seed-documents-1-1`）を再利用しても
    // 台帳が空のため成功する（内容照合ハッシュの再送判定は対象外）。
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (5, '[0.1,0.2,0.3]') \
             USING OPERATION_ID 'seed-documents-1-1'"
        ),
    )
    .expect("reusing a pre-drop operation_id must succeed (ledger was dropped with the table)");

    // 旧行（id=1、2 次元）は混入していない——新テーブルには id=5 のみ存在する。
    let mut inspect_session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut inspect_session,
            &format!("SELECT id FROM {TABLE} WHERE id = 1 LIMIT 10"),
        )
        .expect("scan should succeed");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert!(
        result.rows.is_empty(),
        "old row (id=1) from the pre-drop table must not resurface"
    );
}
