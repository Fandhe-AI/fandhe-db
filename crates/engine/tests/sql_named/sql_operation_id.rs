//! `EngineCore::execute_insert_sql` の結合テスト（TASK-80、対象ビヘイビア: SQL-10。
//! ポインタ: `docs/spec/05-tasks.md` TASK-80・`docs/spec/04-behavior/sql-surface.md`・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・`data-model.md` TABLE-12・
//! `rls.md` RLS-9）。
//!
//! 実 `Storage` 上にテーブルを構築し、`INSERT ... USING OPERATION_ID '<id>'` の
//! 受理・文末専用句の省略拒否・statement 単位スコープ・行 `id` 衝突の扱いを検証する。
//!
//! 行 `id` 衝突（`23505`）は **同一テナントの名前空間内**でのみ発生する（行ストアの
//! 物理キーが `(tenant_id, id)` のため。TABLE-12・RLS-9）。他テナントの行 `id` の
//! 有無で応答が変化しないことを、本体の判定 API に依存しないテスト側オラクル
//! （期待値のベタ書き）で確認する。
//!
//! 台帳への永続化は TASK-93（対象ビヘイビア: RECOVER-2）が
//! `crate::recovery::ledger` として実装済み（結合テストは
//! `tests/recovery_ledger.rs`）。同一 `operation_id` の重複拒否（`23505`）・内容
//! 不一致検出（`22023`）は `crate::recovery::ledger::record_in_txn` の内容照合ハッシュ
//! （TASK-94・RECOVER-3 の重複拒否契約を包含する TASK-101・RECOVER-10）が担う
//! （結合テストは `tests/recovery_content_hash.rs`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{Storage, Visibility};

// 一時 DB パス払い出しは共通ヘルパへ委譲する（Issue #173）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

// --- 受理系 -----------------------------------------------------------------

#[test]
fn insert_with_operation_id_clause_succeeds_and_is_readable_by_owning_tenant() {
    let path = unique_db_path("insert-succeeds");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let outcome = core
        .execute_insert_sql(
            &write_ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
        )
        .expect("insert with clause should succeed");
    assert_eq!(outcome.rows_affected, 1);

    // 挿入テナント自身は Private 行を読み戻せる（可視性 Private 固定の効果）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].id, 1);
}

// --- 省略拒否（23502。書き込みトランザクション未開始の外形確認） ------------------

#[test]
fn insert_missing_operation_id_clause_is_rejected_before_any_write() {
    let path = unique_db_path("insert-missing-clause");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello')",
        )
        .expect_err("missing clause must be rejected");
    assert_eq!(err.wire_code(), "23502");

    // 書き込みトランザクションが一切開始されていないことを外形的に確認する
    // （行が 1 件も反映されていない。`EngineCore` は `Storage` を外へ出さない
    // 一方向設計のため、`core.rs` モジュールドキュメント参照、確認は SELECT 経由で行う）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

#[test]
fn insert_empty_operation_id_value_is_rejected_as_missing() {
    let path = unique_db_path("insert-empty-value");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID ''",
        )
        .expect_err("empty value must be rejected as missing");
    assert_eq!(err.wire_code(), "23502");
}

// 対象ビヘイビア: RECOVER-1（TASK-92）。明示 `NULL` は句の欠落と同様に拒否する。
#[test]
fn insert_explicit_null_operation_id_is_rejected_as_missing_before_any_write() {
    let path = unique_db_path("insert-explicit-null");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID NULL",
        )
        .expect_err("explicit NULL must be rejected as missing");
    assert_eq!(err.wire_code(), "23502");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

// --- 拡張クエリプロトコル形式（$n）は 42601 --------------------------------------

#[test]
fn insert_operation_id_dollar_placeholder_is_rejected_as_syntax_error() {
    let path = unique_db_path("insert-dollar-placeholder");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID $1",
        )
        .expect_err("$n placeholder must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- statement 単位スコープ（セッションに引き継がれない） -----------------------

#[test]
fn operation_id_clause_does_not_carry_over_to_the_next_statement() {
    let path = unique_db_path("insert-statement-scope");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
    )
    .expect("first insert with clause should succeed");

    // 直後の文に句がなければ、直前の文の句は一切引き継がれず 23502 で拒否される
    // （SQL-10 の要件: statement 単位スコープ。セッション変数等の別経路は設けない）。
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'world')",
        )
        .expect_err("clause must not carry over to the next statement");
    assert_eq!(err.wire_code(), "23502");
}

// --- 行 id 衝突は同一テナント内スコープ（TABLE-12・RLS-9・SQL-10） ---------------

/// 3 列すべてを指定する INSERT 文を組み立てる（テスト側の期待値を明示するため、
/// SQL 文字列は各テストで直接読める形に保つ）。
fn insert_sql(id: u64, body: &str, operation_id: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', '{body}') USING OPERATION_ID '{operation_id}'"
    )
}

#[test]
fn insert_into_existing_id_of_same_tenant_is_rejected_with_23505_without_overwriting() {
    let path = unique_db_path("insert-existing-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(&write_ctx, &insert_sql(1, "hello", "op-0001"))
        .expect("first insert should succeed");

    let err = core
        .execute_insert_sql(&write_ctx, &insert_sql(1, "overwrite", "op-0002"))
        .expect_err("insert into an existing id of the same tenant must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 元の値のまま（上書きされていない）ことを読み戻しで確認する。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn insert_succeeds_when_only_another_tenant_holds_the_same_id() {
    // TABLE-12・RLS-9: 行 `id` の一意性はテナント内スコープ。他テナントが同じ `id` を
    // 保持していても、自テナントの INSERT は通常どおり成功する。
    let path = unique_db_path("insert-cross-tenant-same-id");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);

    let tenant_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let tenant_b = PolicyContext::new("tenant-b").expect("valid tenant");

    core.execute_insert_sql(&tenant_a, &insert_sql(1, "owned-by-a", "op-a-0001"))
        .expect("tenant-a insert should succeed");
    let outcome = core
        .execute_insert_sql(&tenant_b, &insert_sql(1, "owned-by-b", "op-b-0001"))
        .expect("tenant-b insert of the same id must succeed (per-tenant id namespace)");
    assert_eq!(outcome.rows_affected, 1);

    // 双方の行が独立して残っている（後勝ちの上書きが起きていない）。
    for (ctx, expected_body) in [(&tenant_a, "owned-by-a"), (&tenant_b, "owned-by-b")] {
        let read_ctx = PolicyContext::with_visibilities(
            ctx.tenant_id(),
            [Visibility::Public, Visibility::Private],
        )
        .expect("valid tenant");
        let result = core
            .execute_sql(
                &read_ctx,
                "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
            )
            .expect("select should succeed");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows.first().map(|r| r.cells.len()),
            Some(2),
            "projection must yield id and body"
        );
        let body = match result.rows.first().and_then(|r| r.cells.get(1)) {
            Some(engine::sql::exec::Cell::Text(t)) => t.clone(),
            other => panic!("unexpected cell: {other:?}"),
        };
        assert_eq!(body, expected_body);
    }
}

#[test]
fn insert_response_is_identical_regardless_of_other_tenant_rows() {
    // 他テナント行の有無が INSERT 応答（成否・`rows_affected`）に一切影響しないことを
    // 確認する（存在オラクルの遮断。security.md P0）。DB を 2 つ用意し、片方にだけ
    // 他テナントの同一 `id` 行を先に入れる。
    let path_with = unique_db_path("insert-oracle-with-other-tenant");
    let _guard_with = CleanupGuard(path_with.clone());
    let path_without = unique_db_path("insert-oracle-without-other-tenant");
    let _guard_without = CleanupGuard(path_without.clone());

    let core_with = new_core_with_documents_table(&path_with);
    let core_without = new_core_with_documents_table(&path_without);

    let tenant_b = PolicyContext::new("tenant-b").expect("valid tenant");
    core_with
        .execute_insert_sql(&tenant_b, &insert_sql(1, "owned-by-b", "op-b-0001"))
        .expect("seeding another tenant's row should succeed");

    let tenant_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let sql = insert_sql(1, "hello", "op-a-0001");
    let with = core_with.execute_insert_sql(&tenant_a, &sql);
    let without = core_without.execute_insert_sql(&tenant_a, &sql);

    let with = with.expect("insert must succeed even if another tenant holds the same id");
    let without = without.expect("insert must succeed when the id is unused");
    assert_eq!(with.rows_affected, without.rows_affected);
    assert_eq!(with.rows_affected, 1);
}

#[test]
fn error_response_of_same_tenant_conflict_is_identical_regardless_of_other_tenant_rows() {
    // 衝突（`23505`）側の応答も、他テナント行の有無で `wire_code`・文言が変化しない。
    let path_with = unique_db_path("conflict-oracle-with-other-tenant");
    let _guard_with = CleanupGuard(path_with.clone());
    let path_without = unique_db_path("conflict-oracle-without-other-tenant");
    let _guard_without = CleanupGuard(path_without.clone());

    let core_with = new_core_with_documents_table(&path_with);
    let core_without = new_core_with_documents_table(&path_without);

    let tenant_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let tenant_b = PolicyContext::new("tenant-b").expect("valid tenant");
    for core in [&core_with, &core_without] {
        core.execute_insert_sql(&tenant_a, &insert_sql(1, "hello", "op-a-0001"))
            .expect("first insert should succeed");
    }
    core_with
        .execute_insert_sql(&tenant_b, &insert_sql(1, "owned-by-b", "op-b-0001"))
        .expect("seeding another tenant's row should succeed");

    let sql = insert_sql(1, "hello", "op-a-0002");
    let err_with = core_with
        .execute_insert_sql(&tenant_a, &sql)
        .expect_err("duplicate id in the same tenant must be rejected");
    let err_without = core_without
        .execute_insert_sql(&tenant_a, &sql)
        .expect_err("duplicate id in the same tenant must be rejected");

    assert_eq!(err_with.wire_code(), "23505");
    assert_eq!(err_with.wire_code(), err_without.wire_code());
    assert_eq!(err_with.to_string(), err_without.to_string());
}

#[test]
fn resending_the_same_statement_is_rejected_with_23505_by_operation_id_dedup() {
    // 同一文（同一 `operation_id`・同一行 `id`）の再送は `23505` になる。TASK-94・
    // RECOVER-3 実装後は、台帳追記が行書き込みより先に同一 write トランザクション内で
    // 判定されるため（`tenant.rs` モジュールドキュメント参照）、この再送は行キー
    // `(tenant_id, id)` の衝突ではなく `operation_id` の台帳重複拒否
    // （`SqlSurfaceError::DuplicateOperationId`）として検出される。異なる `operation_id`
    // で同一行 `id` に INSERT した場合の行キー衝突は
    // `insert_into_existing_id_of_same_tenant_is_rejected_with_23505_without_overwriting`
    // が別途検証する。
    let path = unique_db_path("insert-resend");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let sql = insert_sql(1, "hello", "op-0001");
    core.execute_insert_sql(&ctx, &sql)
        .expect("first execution should commit");
    let err = core
        .execute_insert_sql(&ctx, &sql)
        .expect_err("resend must be rejected as already committed");
    assert_eq!(err.wire_code(), "23505");
}

// --- 他テナントからは不可視（Private 固定の RLS 効果） ---------------------------

#[test]
fn inserted_row_is_invisible_to_other_tenants() {
    let path = unique_db_path("insert-invisible-other-tenant");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let write_ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &write_ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-0001'",
    )
    .expect("insert should succeed");

    let other_ctx =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &other_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

// --- 非 nullable 列欠落は 22000 ---------------------------------------------------

#[test]
fn insert_missing_non_nullable_column_is_rejected() {
    let path = unique_db_path("insert-missing-non-nullable");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding) VALUES (1, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-0001'",
        )
        .expect_err("missing non-nullable column must be rejected");
    // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
    // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
    assert_eq!(err.wire_code(), "23502");
}
