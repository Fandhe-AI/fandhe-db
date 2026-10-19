//! `EngineCore::execute_sql_in_session` 経由の `INSERT` 実行結合テスト
//! （TASK-82、対象ビヘイビア: SQL-10。ポインタ: `docs/spec/05-tasks.md`
//! TASK-82・`docs/spec/04-behavior/sql-surface.md` SQL-10）。
//!
//! `INSERT` の検証・実行本体（許可リスト検証・`operation_id` 必須化ガード・
//! 台帳照合・行/ファイル形の束縛）は既に `EngineCore::execute_insert_sql`
//! （TASK-80）に対する結合テスト `tests/sql_operation_id.rs` が確定オラクルとして
//! 検証済みのため、本ファイルはそれとは独立した経路である**セッション経由の
//! 呼び出し**（`execute_sql_in_session` が先頭トークンを見て `INSERT` を検出し
//! `execute_insert_sql` へ委譲し `SqlOutcome::Insert` を返す分岐。
//! `crates/engine/src/core.rs` 参照）が同じ契約を維持することに徹する
//! （`tests/sql_explain.rs`・`tests/wire_using_plan.rs` と同方針）。
//! wire プロトコル経由・生バイトクライアントでの検証は
//! `crates/wire-server/tests/wire1_simple_query.rs`
//! （`wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant`。
//! RLS-11・TASK-195 導入後の読み取り可視性契約は同テストのドキュメンテーション
//! コメント参照）が担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
}

/// セッション経由の `INSERT` は `SqlOutcome::Insert` を返し、行が永続化される
/// （`execute_insert_sql` 直呼び出しと同じ契約）。
#[test]
fn session_insert_succeeds_and_returns_insert_outcome() {
    let path = unique_db_path("session-insert-succeeds");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-session-0001'",
        )
        .expect("session INSERT should succeed");
    match outcome {
        SqlOutcome::Insert(insert_outcome) => assert_eq!(insert_outcome.rows_affected, 1),
        other => panic!("expected SqlOutcome::Insert, got {other:?}"),
    }

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

/// `operation_id` 句省略はセッション経由でも `23502` で書き込みトランザクション
/// 開始前に拒否される（`execute_insert_sql` 直呼び出しと同じ契約。
/// `tests/sql_operation_id.rs::insert_missing_operation_id_clause_is_rejected_before_any_write`
/// の同型確認）。
#[test]
fn session_insert_missing_operation_id_clause_is_rejected_with_23502() {
    let path = unique_db_path("session-insert-missing-clause");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello')",
        )
        .expect_err("missing clause must be rejected");
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

/// 同一内容の `INSERT` 再送はセッション経由でも `23505`（commit 済み判定）で
/// 拒否される（RECOVER-7 の SQL 表層表現・RECOVER-10 の内容照合契約。
/// `tests/sql_operation_id.rs::resending_the_same_statement_is_rejected_with_23505_by_operation_id_dedup`
/// の同型確認）。
#[test]
fn session_insert_resending_the_same_statement_is_rejected_with_23505() {
    let path = unique_db_path("session-insert-resend-23505");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    let sql = "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-session-resend'";

    core.execute_sql_in_session(&ctx, &mut session, sql)
        .expect("first INSERT should succeed");
    let err = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("identical resend must be rejected as duplicate commit");
    assert_eq!(err.wire_code(), "23505");
}

/// 同一 `operation_id` で内容の異なる `INSERT` を再発行するとセッション経由でも
/// `22023` で拒否される（RECOVER-10 の内容照合契約）。
#[test]
fn session_insert_same_operation_id_different_content_is_rejected_with_22023() {
    let path = unique_db_path("session-insert-content-mismatch-22023");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-session-mismatch'",
    )
    .expect("first INSERT should succeed");

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'world') USING OPERATION_ID 'op-session-mismatch'",
        )
        .expect_err("same operation_id with different content must be rejected");
    assert_eq!(err.wire_code(), "22023");
}

/// `USING OPERATION_ID $1`（パラメータ形式）はセッション経由でも構文エラー
/// （`42601`）で拒否される（MVP は拡張クエリプロトコル未対応。SQL-10 の
/// 「専用句を唯一の規範経路とする」契約は簡易クエリプロトコルの文字列
/// リテラル規範形に限る）。
#[test]
fn session_insert_operation_id_dollar_placeholder_is_rejected_as_syntax_error() {
    let path = unique_db_path("session-insert-dollar-placeholder");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID $1",
        )
        .expect_err("dollar placeholder must be rejected as a syntax error");
    assert_eq!(err.wire_code(), "42601");
}

/// `EXPLAIN INSERT ...` は許可形状に存在しないため `42601` で拒否される
/// （`EXPLAIN` は `USING PLAN` を伴う検索 SELECT の前置専用。TASK-78・SQL-6）。
/// セッション経由の INSERT 検出（先頭トークン `INSERT` の覗き見）を追加した
/// 後も、先頭トークンが `EXPLAIN` である限り既存の `validate_sql` の `EXPLAIN`
/// 分岐へそのまま流れ、挙動が変わらないことを確認する。
#[test]
fn session_explain_insert_is_rejected_as_unsupported_syntax() {
    let path = unique_db_path("session-explain-insert-rejected");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-explain-insert'",
        )
        .expect_err("EXPLAIN INSERT must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// 複数行 `VALUES`（SQL-16、TASK-190）もセッション経由（`execute_sql_in_session`
/// → `execute_insert_sql` → `execute_insert_form`）で同じ契約のまま実行される
/// （`crates/engine/tests/insert_multi_row.rs` が `execute_insert_sql` 直呼び出しで
/// 固定する契約の、セッション経由経路での到達性確認。実行本体は共有のため
/// 契約自体の再検証はしない）。
#[test]
fn session_multi_row_insert_writes_all_rows_and_returns_insert_outcome() {
    let path = unique_db_path("session-insert-multi-row");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "INSERT INTO documents (id, embedding, body) VALUES \
             (1, '[0.1,0.2,0.3]', 'a'), (2, '[0.4,0.5,0.6]', 'b'), (3, '[0.7,0.8,0.9]', 'c') \
             USING OPERATION_ID 'op-session-multi-row'",
        )
        .expect("session multi-row INSERT should succeed");
    match outcome {
        SqlOutcome::Insert(insert_outcome) => assert_eq!(insert_outcome.rows_affected, 3),
        other => panic!("expected SqlOutcome::Insert, got {other:?}"),
    }
}
