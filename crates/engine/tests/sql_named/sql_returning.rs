//! `INSERT`／`DELETE`／`UPDATE`／UPSERT の `RETURNING` 句（Issue #873・#1182・
//! SQL-21）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-193・
//! `docs/spec/04-behavior/sql-surface.md` SQL-21。関連ポインタ: SQL-10・
//! SQL-18・RLS-7・RLS-9・RLS-10・RECOVER-1〜3・RECOVER-10。
//!
//! `EngineCore::execute_sql_in_session` の INSERT／DELETE 分岐が
//! `stmt.returning.is_some()` で `SqlOutcome::Returning` へ切り替わる経路を
//! production 経路として検証する。`sql_delete_single_row.rs`・
//! `insert_multi_row.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）でヘルパを共有する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::returning::DmlCommand;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core_with_table() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-returning");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn count_star(core: &EngineCore, ctx: &PolicyContext, table: &str) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count(*) should succeed");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn expect_returning(outcome: SqlOutcome) -> engine::sql::exec::ReturningOutcome {
    match outcome {
        SqlOutcome::Returning(o) => o,
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

// --- INSERT ... RETURNING ---

/// `INSERT ... RETURNING *` の投影列・行内容が、同一行を `SELECT * ... LIMIT 1`
/// で読み戻した結果と完全一致すること（列メタ・`Cell::Vector` を含む）。
/// `rows_affected == result.rows.len() == 1`。
#[test]
fn insert_returning_star_matches_select_readback() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING * \
                 USING OPERATION_ID 'op-insert-returning'"
            ),
        )
        .expect("INSERT RETURNING should succeed"),
    );

    assert_eq!(outcome.command, DmlCommand::Insert);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);

    let select = core
        .execute_sql(&alice, &format!("SELECT * FROM {TABLE} LIMIT 10"))
        .expect("select readback");
    assert_eq!(select.rows.len(), 1);
    assert_eq!(outcome.result.columns, select.columns);
    assert_eq!(outcome.result.rows[0].id, select.rows[0].id);
    assert_eq!(outcome.result.rows[0].cells, select.rows[0].cells);
}

/// RETURNING の `RowDescription`（列メタ）は `RETURNING` に列挙した投影
/// （`id`・`body`）のみで、投影に含めなかった列は現れない。
#[test]
fn insert_returning_projects_only_listed_columns() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING id, body \
                 USING OPERATION_ID 'op-insert-cols'"
            ),
        )
        .expect("INSERT RETURNING should succeed"),
    );

    assert_eq!(
        outcome.result.columns,
        vec![
            ColumnMeta::Id,
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: ColumnType::Text,
            },
        ]
    );
    assert_eq!(outcome.result.rows[0].cells.len(), 2);
    assert_eq!(outcome.result.rows[0].cells[0], Cell::Integer(1));
    assert_eq!(
        outcome.result.rows[0].cells[1],
        Cell::Text("hello".to_string())
    );
}

/// 不可視な挿入行（Private 固定）の RETURNING は、黙って空にせず `XX000` で書き込み前に
/// 拒否する（fail-closed。SQL-21・RLS-7、Issue #1252）。行も台帳も消費しないので、同じ
/// `operation_id` を可視集合の広い ctx で再実行すると `rows_affected == 返却行数 == 1` になる。
#[test]
fn insert_returning_rejects_invisible_inserted_row_fail_closed() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let alice_private = ctx_for("alice", true);
    let mut session = SessionState::default();
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
         (1, '[0.1,0.2]', 'ja', 'secret-body') RETURNING * \
         USING OPERATION_ID 'op-insert-public-only'"
    );

    let err = core
        .execute_sql_in_session(&alice_public_only, &mut session, &sql)
        .expect_err("invisible inserted row must be rejected");
    assert_eq!(err.wire_code(), "XX000");
    assert!(!err.client_message().contains("secret-body"));
    assert_eq!(count_star(&core, &alice_private, TABLE), 0);

    let outcome = expect_returning(
        core.execute_sql_in_session(&alice_private, &mut session, &sql)
            .expect("same operation_id must be reusable: ledger was not consumed"),
    );
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    assert_eq!(count_star(&core, &alice_private, TABLE), 1);
}

/// 複数行 `VALUES` でも全行書き込まれずに `XX000` で拒否される。
#[test]
fn insert_returning_multi_row_rejects_invisible_rows_without_writing() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice_public_only,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'a'), (2, '[0.3,0.4]', 'ja', 'b') \
                 RETURNING id USING OPERATION_ID 'op-insert-multi-public-only'"
            ),
        )
        .expect_err("must be rejected");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(count_star(&core, &ctx_for("alice", true), TABLE), 0);
}

/// UPSERT の新規挿入行（`DO NOTHING`・`DO UPDATE` の双方）も不可視なら `XX000` で
/// 書き込みを中止し、行・台帳とも永続化されない（Issue #1252）。
#[test]
fn upsert_returning_rejects_invisible_inserted_row_and_persists_nothing() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let alice_private = ctx_for("alice", true);
    let mut session = SessionState::default();

    for (clause, op) in [
        ("DO NOTHING", "op-upsert-nothing-invisible"),
        (
            "DO UPDATE SET body = EXCLUDED.body",
            "op-upsert-update-invisible",
        ),
    ] {
        let sql = format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'new') ON CONFLICT (id) {clause} \
             RETURNING id, body USING OPERATION_ID '{op}'"
        );
        let err = core
            .execute_sql_in_session(&alice_public_only, &mut session, &sql)
            .expect_err("invisible upsert-inserted row must be rejected");
        assert_eq!(err.wire_code(), "XX000");
        assert_eq!(count_star(&core, &alice_private, TABLE), 0);

        let outcome = expect_returning(
            core.execute_sql_in_session(&alice_private, &mut session, &sql)
                .expect("ledger was not consumed, so the same operation_id is reusable"),
        );
        assert_eq!(outcome.rows_affected, 1);
        assert_eq!(outcome.result.rows.len(), 1);
        // 次ループ用に消す（台帳は別 operation_id）。
        core.execute_sql_in_session(
            &alice_private,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID '{op}-del'"),
        )
        .expect("cleanup delete");
    }
}

/// 明示トランザクション内の行形 `INSERT ... RETURNING` も、不可視なら `XX000` で
/// 拒否され、トランザクション内にも行は書かれない。可視集合が広ければ
/// `rows_affected == 返却行数`。
#[test]
fn insert_returning_in_explicit_txn_rejects_invisible_row_and_writes_nothing() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let alice_private = ctx_for("alice", true);
    let mut session = SessionState::default();
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
         (1, '[0.1,0.2]', 'ja', 'x') RETURNING id USING OPERATION_ID 'op-txn-invisible'"
    );

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice_public_only, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&alice_public_only, &mut session, &mut txn, &sql)
        .expect_err("invisible row must be rejected inside a transaction");
    assert_eq!(err.wire_code(), "XX000");
    let _ = core.execute_sql_in_txn(&alice_public_only, &mut session, &mut txn, "ROLLBACK");
    assert_eq!(count_star(&core, &alice_private, TABLE), 0);

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice_private, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = expect_returning(
        core.execute_sql_in_txn(&alice_private, &mut session, &mut txn, &sql)
            .expect("visible row is returned"),
    );
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    core.execute_sql_in_txn(&alice_private, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count_star(&core, &alice_private, TABLE), 1);
}

/// 他テナントの行は、自テナントの INSERT／UPSERT の RETURNING に混入しない。
#[test]
fn insert_returning_never_returns_other_tenant_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let bob = ctx_for("bob", true);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &bob, 1, "ja", "bob-secret", "op-bob-seed");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'alice-row') ON CONFLICT (id) DO NOTHING \
                 RETURNING id, body USING OPERATION_ID 'op-alice-upsert'"
            ),
        )
        .expect("alice's own id 1 is new in her namespace"),
    );
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    assert_eq!(
        outcome.result.rows[0].cells[1],
        Cell::Text("alice-row".to_string())
    );
}

/// 複数行 `VALUES ... RETURNING *`（SQL-16 との併用）: `rows_affected` ・
/// 投影行数が挿入行数と一致し、投影順が `VALUES` の宣言順と一致する。
#[test]
fn insert_returning_multi_row_preserves_values_order() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'first'), (2, '[0.3,0.4]', 'ja', 'second') \
                 RETURNING id, body USING OPERATION_ID 'op-insert-multi'"
            ),
        )
        .expect("multi-row INSERT RETURNING should succeed"),
    );

    assert_eq!(outcome.rows_affected, 2);
    assert_eq!(outcome.result.rows.len(), 2);
    assert_eq!(outcome.result.rows[0].id, 1);
    assert_eq!(
        outcome.result.rows[0].cells[1],
        Cell::Text("first".to_string())
    );
    assert_eq!(outcome.result.rows[1].id, 2);
    assert_eq!(
        outcome.result.rows[1].cells[1],
        Cell::Text("second".to_string())
    );
}

/// ファイル形 `INSERT`（`path`/`body` 列指定）＋ `RETURNING` は `42601`
/// （サーバー側チャンク化行を返す応答形が未定義のため fail-closed）。行は
/// 一切書き込まれない。
#[test]
fn insert_returning_file_form_is_rejected_and_writes_no_rows() {
    let path_file = unique_db_path("sql-returning-file-form");
    let storage = Storage::open(&path_file).expect("open storage");
    let file_schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    storage.create_table(&file_schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let _guard = CleanupGuard(path_file);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "INSERT INTO docs (path, body) VALUES ('a.txt', 'hello world') \
             RETURNING * USING OPERATION_ID 'op-file-returning'",
        )
        .expect_err("file-form INSERT RETURNING must be rejected");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(count_star(&core, &alice, "docs"), 0);
}

/// 非セッション入口（`execute_insert_sql`）は `RETURNING` 付き文を検証直後・
/// 書き込み前に `42601` で拒否し、台帳を一切消費しない——同一 `operation_id`
/// をその後セッション経由（`RETURNING` なし）で使うと成功する。
#[test]
fn insert_returning_is_rejected_on_nonsession_entry_without_consuming_the_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);

    let err = core
        .execute_insert_sql(
            &alice,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING * \
                 USING OPERATION_ID 'op-nonsession'"
            ),
        )
        .expect_err("RETURNING must be rejected on the session-less entry point");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(count_star(&core, &alice, TABLE), 0);

    // 同一 operation_id をセッション経由・RETURNING なしで再送すると成功する
    // （台帳が未消費であることの証拠）。
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-nonsession'"
            ),
        )
        .expect("same operation_id must still be usable (ledger was not consumed)");
    match outcome {
        SqlOutcome::Insert(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Insert, got {other:?}"),
    }
}

/// 内容照合ハッシュ（TASK-101・RECOVER-10）は SQL テキストではなく符号化済み
/// 行・`id` から計算するため `RETURNING` の有無に依存しない: 同一
/// `operation_id`・同一内容で「RETURNING あり → なし」の順に送ると 2 回目は
/// `23505`（逆順も同様）。
#[test]
fn insert_returning_content_hash_is_independent_of_returning_clause() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let values = "(1, '[0.1,0.2]', 'ja', 'hello')";
    let with_returning = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES {values} \
         RETURNING * USING OPERATION_ID 'op-content-hash'"
    );
    let without_returning = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES {values} \
         USING OPERATION_ID 'op-content-hash'"
    );

    core.execute_sql_in_session(&alice, &mut session, &with_returning)
        .expect("first RETURNING insert should succeed");
    let err = core
        .execute_sql_in_session(&alice, &mut session, &without_returning)
        .expect_err("resend without RETURNING must be detected as a duplicate");
    assert_eq!(err.wire_code(), "23505");
}

// --- DELETE ... RETURNING ---

/// `DELETE ... RETURNING *` は削除**前**の値を返し、直後の `SELECT` では
/// 対象行が見つからない（削除は実際に完了している）。
#[test]
fn delete_returning_returns_pre_delete_values_and_removes_the_row() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert");

    let outcome = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 RETURNING * USING OPERATION_ID 'op-delete-returning'"),
        )
        .expect("DELETE RETURNING should succeed"),
    );

    assert_eq!(outcome.command, DmlCommand::Delete);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    assert_eq!(outcome.result.rows[0].id, 1);
    assert_eq!(
        outcome.result.rows[0].cells,
        vec![
            Cell::Integer(1),
            Cell::Vector(vec![0.1, 0.2]),
            Cell::Text("ja".to_string()),
            Cell::Text("hello".to_string()),
        ]
    );

    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// 0 行成功（RLS-9・RLS-10）: 他テナント保持 id・未存在 id への
/// `DELETE ... RETURNING` は、`ReturningOutcome`（列・0 行・`rows_affected: 0`）
/// が完全に一致し、対象行の有無（他テナント所有か未存在か）を区別しない。
#[test]
fn delete_returning_notfound_response_is_identical_for_other_tenant_and_nonexistent_id() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &bob,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (7, '[0.1,0.2]', 'ja', 'bob body') USING OPERATION_ID 'op-seed-bob'"
        ),
    )
    .expect("seed insert for bob");

    let outcome_other_tenant = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE id = 7 RETURNING * USING OPERATION_ID 'op-other-tenant'"
            ),
        )
        .expect("DELETE against another tenant's row must succeed as a 0-row no-op"),
    );
    let outcome_nonexistent = expect_returning(
        core.execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 RETURNING * USING OPERATION_ID 'op-nonexistent'"),
        )
        .expect("DELETE against a nonexistent row must succeed as a 0-row no-op"),
    );

    assert_eq!(outcome_other_tenant.rows_affected, 0);
    assert_eq!(outcome_nonexistent.rows_affected, 0);
    assert!(outcome_other_tenant.result.rows.is_empty());
    assert!(outcome_nonexistent.result.rows.is_empty());
    assert_eq!(outcome_other_tenant.result, outcome_nonexistent.result);
    assert_eq!(outcome_other_tenant.command, outcome_nonexistent.command);

    // bob の行は無傷。
    assert_eq!(count_star(&core, &bob, TABLE), 1);
}

/// 非セッション入口（`execute_delete_sql`）は `RETURNING` 付き文を検証直後・
/// 書き込み前に `42601` で拒否し、台帳を一切消費しない。
#[test]
fn delete_returning_is_rejected_on_nonsession_entry_without_consuming_the_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-seed'"
        ),
    )
    .expect("seed insert");

    let err = core
        .execute_delete_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE id = 1 RETURNING * USING OPERATION_ID 'op-delete-nonsession'"),
        )
        .expect_err("RETURNING must be rejected on the session-less entry point");
    assert_eq!(err.wire_code(), "42601");
    // 行は削除されていない。
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    // 同一 operation_id をセッション経由・RETURNING なしで再送すると成功する
    // （台帳が未消費であることの証拠）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-delete-nonsession'"),
        )
        .expect("same operation_id must still be usable (ledger was not consumed)");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// codex-review Low 指摘（Issue #873）: `RETURNING` 付き複数行 INSERT
// （`execute_insert_returning_form` の `RowBatch` 分岐）が、非 `RETURNING`
// 経路（`insert_multi_row.rs`）と同一の `self.batch_limits`（INDEX-4・
// `EngineCore::validate_insert_row_batch_limits`）を共有していることを固定する。
// ---------------------------------------------------------------------

/// `RETURNING` 付き複数行 `INSERT ... VALUES` が、運用者が絞った
/// `self.batch_limits.max_files_per_batch`（ここでは 2）を超える行数（3 行）
/// で `54000` 拒否され、行が一切書き込まれないこと（
/// `insert_multi_row.rs::multi_row_insert_over_batch_limits_row_count_is_rejected_with_54000`
/// の `RETURNING` 版。`execute_insert_returning_form` の `RowBatch` 分岐が
/// 非 `RETURNING` 経路と同一の判定本体〔`validate_insert_row_batch_limits`〕を
/// 共有していることの直接証拠）。
#[test]
fn insert_returning_multi_row_over_batch_limits_row_count_is_rejected_with_54000() {
    let path = unique_db_path("sql-returning-batch-limits-row-count");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 2,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'first'), (2, '[0.3,0.4]', 'ja', 'second'), \
                 (3, '[0.5,0.6]', 'ja', 'third') \
                 RETURNING id USING OPERATION_ID 'op-insert-returning-batch-limit'"
            ),
        )
        .expect_err("row count over batch_limits.max_files_per_batch must be rejected");
    assert_eq!(err.wire_code(), "54000");

    // 行は一切書き込まれていない（副作用ゼロ）。
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}
// ---------------------------------------------------------------------
// UPDATE・述語形 DELETE・UPSERT の RETURNING（Issue #1182・SQL-21）
// ---------------------------------------------------------------------

/// 非 `RETURNING` の `INSERT` で 1 行を投入する（シード用。常に `Private`）。
fn seed(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, body: &str, op: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             ({id}, '[0.1,0.2]', '{lang}', '{body}') USING OPERATION_ID '{op}'"
        ),
    )
    .expect("seed insert");
}

fn run(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, session, sql)
}

fn ids(outcome: &engine::sql::exec::ReturningOutcome) -> Vec<u64> {
    outcome.result.rows.iter().map(|r| r.id).collect()
}

/// 単一行 `UPDATE ... RETURNING *` は更新**後**の値を返し、直後の `SELECT` の
/// 読み戻し（列メタ・`Cell::Vector` を含む）と完全一致する。SET していない列は
/// 既存値を保持する。
#[test]
fn update_returning_single_row_returns_post_update_values() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "hello", "op-seed-1");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING * \
                 USING OPERATION_ID 'op-update-returning'"
            ),
        )
        .expect("UPDATE RETURNING should succeed"),
    );

    assert_eq!(outcome.command, DmlCommand::Update);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(outcome.result.rows.len(), 1);
    let select = core
        .execute_sql(&alice, &format!("SELECT * FROM {TABLE} LIMIT 10"))
        .expect("select readback");
    assert_eq!(select.rows.len(), 1);
    assert_eq!(outcome.result.columns, select.columns);
    assert_eq!(outcome.result.rows[0].cells, select.rows[0].cells);
    assert!(outcome.result.rows[0]
        .cells
        .contains(&Cell::Text("en".to_string())));
    assert!(outcome.result.rows[0]
        .cells
        .contains(&Cell::Text("hello".to_string())));
}

/// 単一行 `UPDATE ... RETURNING` の対象が他テナント所有・不存在のいずれでも、
/// 応答（列・行・件数）は完全に一致し区別できない（RLS-9・RLS-10）。
#[test]
fn update_returning_single_row_notfound_is_identical_for_other_tenant_and_nonexistent_id() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut session = SessionState::default();
    seed(&core, &bob, 7, "ja", "bob body", "op-seed-bob");

    let other = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 7 RETURNING * USING OPERATION_ID 'op-u-other'"
            ),
        )
        .expect("0-row no-op"),
    );
    let missing = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 999 RETURNING * USING OPERATION_ID 'op-u-missing'"
            ),
        )
        .expect("0-row no-op"),
    );
    assert_eq!(other.rows_affected, 0);
    assert!(other.result.rows.is_empty());
    assert_eq!(other.result, missing.result);
    assert_eq!(other.rows_affected, missing.rows_affected);
    assert_eq!(other.command, missing.command);

    // bob の行は無傷。
    let bob_rows = core
        .execute_sql(&bob, &format!("SELECT lang FROM {TABLE} LIMIT 10"))
        .expect("bob select");
    assert_eq!(bob_rows.rows[0].cells, vec![Cell::Text("ja".to_string())]);
}

/// 述語形 `UPDATE ... RETURNING` は一致した全行の更新後の値を `id` 昇順で返す。
#[test]
fn update_returning_predicate_form_returns_all_matched_rows_post_update() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "a", "op-s1");
    seed(&core, &alice, 3, "ja", "c", "op-s3");
    seed(&core, &alice, 2, "en", "b", "op-s2");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'x' WHERE lang = 'ja' RETURNING id, body \
                 USING OPERATION_ID 'op-upd-pred'"
            ),
        )
        .expect("predicate UPDATE RETURNING"),
    );
    assert_eq!(outcome.command, DmlCommand::Update);
    assert_eq!(outcome.rows_affected, 2);
    assert_eq!(ids(&outcome), vec![1, 3]);
    for row in &outcome.result.rows {
        assert_eq!(
            row.cells,
            vec![Cell::Integer(row.id), Cell::Text("x".to_string())]
        );
    }
    // 一致しなかった行は変化していない。
    let en = core
        .execute_sql(
            &alice,
            &format!("SELECT body FROM {TABLE} WHERE lang = 'en' LIMIT 10"),
        )
        .expect("select en");
    assert_eq!(en.rows[0].cells, vec![Cell::Text("b".to_string())]);
}

/// 述語形 `DELETE ... RETURNING` は削除**前**の値を返し、行は実際に削除される。
#[test]
fn delete_returning_predicate_form_returns_pre_delete_values_and_removes_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "a", "op-s1");
    seed(&core, &alice, 2, "en", "b", "op-s2");
    seed(&core, &alice, 3, "ja", "c", "op-s3");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING id, lang, body \
                 USING OPERATION_ID 'op-del-pred'"
            ),
        )
        .expect("predicate DELETE RETURNING"),
    );
    assert_eq!(outcome.command, DmlCommand::Delete);
    assert_eq!(outcome.rows_affected, 2);
    assert_eq!(ids(&outcome), vec![1, 3]);
    assert_eq!(
        outcome.result.rows[0].cells,
        vec![
            Cell::Integer(1),
            Cell::Text("ja".to_string()),
            Cell::Text("a".to_string())
        ]
    );
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 述語形 `UPDATE`／`DELETE ... RETURNING` は他テナントの一致行を返さず、変更もしない。
#[test]
fn predicate_dml_returning_never_returns_or_modifies_other_tenant_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "alice", "op-sa");
    seed(&core, &bob, 1, "ja", "bob", "op-sb1");
    seed(&core, &bob, 2, "ja", "bob2", "op-sb2");

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'changed' WHERE lang = 'ja' RETURNING * \
                 USING OPERATION_ID 'op-a-upd'"
            ),
        )
        .expect("alice update"),
    );
    assert_eq!(upd.rows_affected, 1);
    assert_eq!(ids(&upd), vec![1]);

    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING * USING OPERATION_ID 'op-a-del'"
            ),
        )
        .expect("alice delete"),
    );
    assert_eq!(del.rows_affected, 1);
    assert_eq!(ids(&del), vec![1]);
    assert!(del
        .result
        .rows
        .iter()
        .all(|r| !r.cells.contains(&Cell::Text("bob".to_string()))));

    // bob の 2 行は無傷（内容も変化していない）。
    assert_eq!(count_star(&core, &bob, TABLE), 2);
    let bob_bodies = core
        .execute_sql(&bob, &format!("SELECT body FROM {TABLE} LIMIT 10"))
        .expect("bob select");
    assert!(bob_bodies
        .rows
        .iter()
        .all(|r| r.cells != vec![Cell::Text("changed".to_string())]));
}

/// UPSERT `DO NOTHING ... RETURNING` は新規挿入した行のみ返し、衝突して何も
/// 変えなかった行は返さない。`rows_affected` は挿入行数。
#[test]
fn upsert_returning_do_nothing_returns_only_inserted_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "existing", "op-seed");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[1.0,0.0]', 'en', 'dup'), (2, '[0.3,0.4]', 'ja', 'new') \
                 ON CONFLICT (id) DO NOTHING RETURNING id, body \
                 USING OPERATION_ID 'op-upsert-nothing'"
            ),
        )
        .expect("UPSERT DO NOTHING RETURNING"),
    );
    assert_eq!(outcome.command, DmlCommand::Insert);
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(ids(&outcome), vec![2]);
    assert_eq!(
        outcome.result.rows[0].cells,
        vec![Cell::Integer(2), Cell::Text("new".to_string())]
    );
    // 既存行は変化していない。
    let existing = core
        .execute_sql(
            &alice,
            &format!("SELECT body FROM {TABLE} WHERE lang = 'ja' LIMIT 10"),
        )
        .expect("select");
    assert!(existing
        .rows
        .iter()
        .any(|r| r.cells == vec![Cell::Text("existing".to_string())]));
}

/// UPSERT `DO UPDATE ... RETURNING` は更新した行の更新後の値と新規挿入行の値を
/// `VALUES` 記述順に返し、`rows_affected == inserted + updated`。
#[test]
fn upsert_returning_do_update_returns_post_update_and_inserted_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "old", "op-seed");

    let outcome = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (2, '[0.3,0.4]', 'ja', 'new'), (1, '[1.0,0.0]', 'en', 'upd') \
                 ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body \
                 RETURNING id, lang, body USING OPERATION_ID 'op-upsert-update'"
            ),
        )
        .expect("UPSERT DO UPDATE RETURNING"),
    );
    assert_eq!(outcome.rows_affected, 2);
    assert_eq!(ids(&outcome), vec![2, 1]);
    assert_eq!(
        outcome.result.rows[0].cells,
        vec![
            Cell::Integer(2),
            Cell::Text("ja".to_string()),
            Cell::Text("new".to_string())
        ]
    );
    // 更新行は SET した body のみ変わり、lang は既存値（ja）を保持する。
    assert_eq!(
        outcome.result.rows[1].cells,
        vec![
            Cell::Integer(1),
            Cell::Text("ja".to_string()),
            Cell::Text("upd".to_string())
        ]
    );
}

/// UPDATE／DELETE（id 指定・述語形）の対象は「所有かつ可視」（RLS-10・SQL-21。
/// Issue #1253）: 可視集合が Public のみの `PolicyContext` では自テナントの Private
/// 行は対象にならず、影響行数と返却行数が一致する（不可視行は影響行数にも現れない）。
/// Public＋Private でも同じ形の文で両者が一致することを固定する。
#[test]
fn dml_returning_update_delete_targets_only_visible_rows_and_count_matches_returned_rows() {
    let alice_private = ctx_for("alice", true);
    let alice_public_only = ctx_for("alice", false);
    // 陽性対照: SQL の INSERT は Private 固定のため、Public 行は `EngineCore` へ
    // 渡す前に API で投入する。
    let path = unique_db_path("sql-returning-visible-targets");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice_private,
        5,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Text("pub".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("op-s5").expect("valid operation_id"),
    )
    .expect("seed public row");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    seed(&core, &alice_private, 1, "ja", "a", "op-s1");
    seed(&core, &alice_private, 2, "ja", "b", "op-s2");

    let upd = expect_returning(
        run(
            &core,
            &alice_public_only,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'x' WHERE lang = 'ja' RETURNING id \
                 USING OPERATION_ID 'op-vis-upd'"
            ),
        )
        .expect("predicate UPDATE"),
    );
    assert_eq!(upd.rows_affected, 1);
    assert_eq!(upd.result.rows.len(), 1);
    assert_eq!(ids(&upd), vec![5]);

    // id 指定 DELETE: 自テナントの Private 行は不可視のため 0 行成功。
    let del_one = expect_returning(
        run(
            &core,
            &alice_public_only,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE id = 1 RETURNING * USING OPERATION_ID 'op-vis-del1'"
            ),
        )
        .expect("single-row DELETE"),
    );
    assert_eq!(del_one.rows_affected, 0);
    assert!(del_one.result.rows.is_empty());

    let del = expect_returning(
        run(
            &core,
            &alice_public_only,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-vis-del'"
            ),
        )
        .expect("predicate DELETE"),
    );
    assert_eq!(del.rows_affected, 1);
    assert_eq!(ids(&del), vec![5]);
    // Private 行（id 1, 2）は UPDATE／DELETE の対象外で残る。
    assert_eq!(count_star(&core, &alice_private, TABLE), 2);

    // Public＋Private の可視集合では全行が対象で、影響行数と返却行数が一致する。
    let upd_all = expect_returning(
        run(
            &core,
            &alice_private,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'y' WHERE lang = 'ja' RETURNING id \
                 USING OPERATION_ID 'op-vis-upd-all'"
            ),
        )
        .expect("predicate UPDATE (all visible)"),
    );
    assert_eq!(upd_all.rows_affected, 2);
    assert_eq!(upd_all.result.rows.len(), 2);
    let del_all = expect_returning(
        run(
            &core,
            &alice_private,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-vis-del-all'"
            ),
        )
        .expect("predicate DELETE (all visible)"),
    );
    assert_eq!(del_all.rows_affected, 2);
    assert_eq!(del_all.result.rows.len(), 2);
    assert_eq!(count_star(&core, &alice_private, TABLE), 0);
}

/// UPSERT の新規挿入行（Private 固定）が可視集合 Public のみの `PolicyContext` から
/// 不可視なら、黙って返却から外さず `XX000` で中止する（Issue #1252・SQL-21）。
#[test]
fn upsert_returning_rows_affected_is_independent_of_result_row_visibility() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_public_only = ctx_for("alice", false);
    let mut session = SessionState::default();

    // UPSERT の新規挿入行は不可視なら `XX000`（Issue #1252）。書き込まれない。
    let ups = run(
        &core,
        &alice_public_only,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (3, '[0.1,0.2]', 'ja', 'c') ON CONFLICT (id) DO NOTHING RETURNING * \
             USING OPERATION_ID 'op-vis-ups'"
        ),
    )
    .expect_err("invisible upsert-inserted row must be rejected");
    assert_eq!(ups.wire_code(), "XX000");
}

/// 内容照合ハッシュ（RECOVER-10・RECOVER-11）は `RETURNING` の有無に依存しない:
/// 同一 `operation_id`・同一内容で「あり→なし」「なし→あり」のどちらの順でも
/// 2 回目は `23505`。単一行 UPDATE・述語形 UPDATE・述語形 DELETE・UPSERT の全経路。
#[test]
fn dml_returning_content_hash_is_independent_of_returning_clause() {
    let upsert = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES (5, '[0.1,0.2]', 'ja', 'u') \
         ON CONFLICT (id) DO NOTHING"
    );
    let stmts: [(&str, String, String); 4] = [
        (
            "single-row UPDATE",
            format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING * USING OPERATION_ID 'op-h'"),
            format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-h'"),
        ),
        (
            "predicate UPDATE",
            format!("UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-h'"),
            format!("UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' USING OPERATION_ID 'op-h'"),
        ),
        (
            "predicate DELETE",
            format!("DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING id USING OPERATION_ID 'op-h'"),
            format!("DELETE FROM {TABLE} WHERE lang = 'ja' USING OPERATION_ID 'op-h'"),
        ),
        (
            "UPSERT",
            format!("{upsert} RETURNING * USING OPERATION_ID 'op-h'"),
            format!("{upsert} USING OPERATION_ID 'op-h'"),
        ),
    ];
    for (label, with_returning, without_returning) in stmts {
        for (first, second) in [
            (&with_returning, &without_returning),
            (&without_returning, &with_returning),
        ] {
            let (core, path) = new_core_with_table();
            let _guard = CleanupGuard(path);
            let alice = ctx_for("alice", true);
            let mut session = SessionState::default();
            seed(&core, &alice, 1, "ja", "a", "op-seed");
            run(&core, &alice, &mut session, first)
                .unwrap_or_else(|e| panic!("{label}: first statement should succeed: {e:?}"));
            let err = run(&core, &alice, &mut session, second)
                .expect_err("resend must be detected as a duplicate");
            assert_eq!(err.wire_code(), "23505", "{label}");
        }
    }
}

/// 非セッション入口（`execute_update_sql`）は `RETURNING` 付き `UPDATE` を書き込み前に
/// `42601` で拒否し、台帳を消費しない（同一 `operation_id` をセッション経由・
/// `RETURNING` なしで使うと成功する）。
#[test]
fn update_returning_is_rejected_on_nonsession_entry_without_consuming_the_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    seed(&core, &alice, 1, "ja", "hello", "op-seed");

    let err = core
        .execute_update_sql(
            &alice,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING * \
                 USING OPERATION_ID 'op-nonsession-upd'"
            ),
        )
        .expect_err("RETURNING must be rejected on the session-less UPDATE entry point");
    assert_eq!(err.wire_code(), "42601");

    let mut session = SessionState::default();
    let outcome = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-nonsession-upd'"
        ),
    )
    .expect("same operation_id must still be usable (ledger was not consumed)");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
}

/// 述語形 `UPDATE`／`DELETE ... RETURNING` が影響行数上限（`54000`）を超えた場合も、
/// 行・台帳とも副作用ゼロ（同一 `operation_id` を上限内の文で再利用できる）。
#[test]
fn predicate_dml_returning_over_affected_row_limit_is_rejected_with_54000_and_changes_nothing() {
    let path = unique_db_path("sql-returning-limit");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_dml_limits(
        engine::sql::parser::DmlLimits {
            max_affected_rows: Some(std::num::NonZeroUsize::new(2).expect("2 is nonzero")),
            ..engine::sql::parser::DmlLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    for id in 1..=3u64 {
        seed(&core, &alice, id, "ja", "b", &format!("op-seed-{id}"));
    }

    let err = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET body = 'x' WHERE lang = 'ja' RETURNING * USING OPERATION_ID 'op-lim'"
        ),
    )
    .expect_err("over-limit UPDATE RETURNING must be rejected");
    assert_eq!(err.wire_code(), "54000");
    let err = run(
        &core,
        &alice,
        &mut session,
        &format!("DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING * USING OPERATION_ID 'op-lim'"),
    )
    .expect_err("over-limit DELETE RETURNING must be rejected");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(count_star(&core, &alice, TABLE), 3);

    // 台帳は消費されていない: 同一 operation_id を上限内の文で使える。
    let outcome = run(
        &core,
        &alice,
        &mut session,
        &format!("DELETE FROM {TABLE} WHERE id = 1 RETURNING id USING OPERATION_ID 'op-lim'"),
    )
    .expect("operation_id must be reusable after a rejected over-limit statement");
    assert_eq!(expect_returning(outcome).rows_affected, 1);
}

/// 未知列の `RETURNING` は書き込み前に拒否され、行・台帳とも変化しない。
#[test]
fn dml_returning_unknown_column_is_rejected_before_write() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "hello", "op-seed");

    for sql in [
        format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING nope USING OPERATION_ID 'op-bad'"),
        format!("UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' RETURNING nope USING OPERATION_ID 'op-bad'"),
        format!("DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING nope USING OPERATION_ID 'op-bad'"),
        format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES (9, '[0.1,0.2]', 'ja', 'x') \
             ON CONFLICT (id) DO NOTHING RETURNING nope USING OPERATION_ID 'op-bad'"
        ),
    ] {
        let err = run(&core, &alice, &mut session, &sql)
            .expect_err("unknown RETURNING column must be rejected");
        assert_eq!(err.wire_code(), "42703", "{sql}");
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);
    let readback = core
        .execute_sql(&alice, &format!("SELECT lang FROM {TABLE} LIMIT 10"))
        .expect("select");
    assert_eq!(readback.rows[0].cells, vec![Cell::Text("ja".to_string())]);
    // 台帳未消費。
    run(
        &core,
        &alice,
        &mut session,
        &format!("UPDATE {TABLE} SET lang = 'en' WHERE id = 1 USING OPERATION_ID 'op-bad'"),
    )
    .expect("operation_id must still be usable");
}

/// 明示トランザクション内の UPSERT `RETURNING` は受理され、挿入行を返し
/// `rows_affected` と一致する（Issue #1273。拒否していた旧挙動からの仕様変更）。
#[test]
fn upsert_returning_inside_explicit_transaction_returns_changed_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    seed(&core, &alice, 1, "ja", "hello", "op-seed");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES (9, '[0.1,0.2]', 'ja', 'x') \
         ON CONFLICT (id) DO NOTHING RETURNING * USING OPERATION_ID 'op-tx'"
    );
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = expect_returning(
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, &sql)
            .expect("UPSERT RETURNING inside a transaction must be accepted"),
    );
    assert_eq!(outcome.rows_affected, 1, "{sql}");
    assert_eq!(outcome.result.rows.len(), 1, "{sql}");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    let readback = core
        .execute_sql(&alice, &format!("SELECT lang FROM {TABLE} LIMIT 10"))
        .expect("select");
    assert_eq!(readback.rows.len(), 2);
}
// ---------------------------------------------------------------------
// RETURNING の `AS` 別名（Issue #3248・SQL-21・SQL-42 のポインタ）
// ---------------------------------------------------------------------

/// 別名付き列は出力列名だけが別名になり、値・型は別名なしの `RETURNING` と一致する
/// （疑似列 `id` の別名を含む。INSERT・単一行 UPDATE・述語形 UPDATE・DELETE の各形）。
#[test]
fn returning_as_alias_renames_columns_and_keeps_values() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let ins = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING id AS rid, lang AS l, body \
                 USING OPERATION_ID 'op-alias-ins'"
            ),
        )
        .expect("INSERT RETURNING alias"),
    );
    assert_eq!(
        ins.result.columns,
        vec![
            ColumnMeta::AliasedId {
                name: "rid".to_string()
            },
            ColumnMeta::Scalar {
                name: "l".to_string(),
                ty: ColumnType::Text
            },
            ColumnMeta::Scalar {
                name: "body".to_string(),
                ty: ColumnType::Text
            },
        ]
    );
    assert_eq!(
        ins.result.rows[0].cells,
        vec![
            Cell::Integer(1),
            Cell::Text("ja".to_string()),
            Cell::Text("hello".to_string())
        ]
    );

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING lang AS l \
                 USING OPERATION_ID 'op-alias-upd'"
            ),
        )
        .expect("UPDATE RETURNING alias"),
    );
    assert_eq!(upd.result.rows[0].cells, vec![Cell::Text("en".to_string())]);

    let upd_pred = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'en' RETURNING id AS rid \
                 USING OPERATION_ID 'op-alias-upd-pred'"
            ),
        )
        .expect("predicate UPDATE RETURNING alias"),
    );
    assert_eq!(upd_pred.result.rows[0].cells, vec![Cell::Integer(1)]);

    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE id = 1 RETURNING lang AS l, id AS rid \
                 USING OPERATION_ID 'op-alias-del'"
            ),
        )
        .expect("DELETE RETURNING alias"),
    );
    assert_eq!(
        del.result.rows[0].cells,
        vec![Cell::Text("fr".to_string()), Cell::Integer(1)]
    );
}

/// 別名が重複しても受理し、同名の 2 列になる（PostgreSQL 準拠）。
#[test]
fn returning_duplicate_aliases_yield_two_same_named_columns() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    let out = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING lang AS x, body AS x \
                 USING OPERATION_ID 'op-alias-dup'"
            ),
        )
        .expect("duplicate aliases"),
    );
    assert_eq!(out.result.columns.len(), 2);
    for col in &out.result.columns {
        assert_eq!(
            *col,
            ColumnMeta::Scalar {
                name: "x".to_string(),
                ty: ColumnType::Text
            }
        );
    }
}

/// 暗黙の別名・未知の列・未定義関数・サブクエリ・集計は書き込み前に拒否し、台帳も消費しない。
/// 文言に旧来の誤解を招く表現（function-call）を含まない。`*, lang` の `0A000` は #3248 の
/// 意図的な拒否（定義済み UDF の受理は #3250 の専用テストで検証する）。
// intentional: docs/design/sql-returning.md
#[test]
fn returning_unsupported_items_are_rejected_before_writing() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    for (items, code) in [
        ("lang l", "42601"),
        ("nope AS n", "42703"),
        ("my_udf(lang)", "42883"),
        ("(SELECT 1)", "42601"),
        ("count(*)", "42601"),
        ("*, lang", "0A000"),
        // 束縛時の型エラー（TEXT + 整数）。
        ("lang + 1", "42883"),
    ] {
        let err = run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING {items} \
                 USING OPERATION_ID 'op-alias-rej'"
            ),
        )
        .expect_err(items);
        assert!(!err.to_string().contains("function-call"), "{items}: {err}");
        assert_eq!(err.wire_code(), code, "{items}: {err}");
        assert_eq!(count_star(&core, &alice, TABLE), 0, "{items}");
    }
    // 台帳が消費されていないため、同じ operation_id で正常に実行できる。
    let ok = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING lang AS l \
                 USING OPERATION_ID 'op-alias-rej'"
            ),
        )
        .expect("ledger must be unconsumed"),
    );
    assert_eq!(ok.rows_affected, 1);
}

// ---------------------------------------------------------------------
// RETURNING のスカラー式・組み込み関数（Issue #3249・SQL-21）
// ---------------------------------------------------------------------

/// RETURNING・SELECT の双方で評価する式リスト（型・値・列名の一致を検証する）。
const EXPRS: &str = "upper(lang) AS u, length(body) + 1 AS n, lower(body) AS s, \
                     vec_norm(embedding) AS vn, length(body) * 2";

fn select_exprs(core: &EngineCore, ctx: &PolicyContext) -> engine::sql::exec::QueryResult {
    core.execute_sql(ctx, &format!("SELECT {EXPRS} FROM {TABLE} LIMIT 10"))
        .expect("select exprs")
}

fn assert_matches_select(
    out: &engine::sql::exec::ReturningOutcome,
    select: &engine::sql::exec::QueryResult,
) {
    assert_eq!(out.result.columns, select.columns);
    assert_eq!(out.result.rows.len(), select.rows.len());
    for (a, b) in out.result.rows.iter().zip(select.rows.iter()) {
        assert_eq!(a.cells, b.cells);
    }
}

/// INSERT／UPDATE（単一行・述語形）／UPSERT／DELETE の RETURNING 式は、書き込み後（DELETE は
/// 削除前）の行に対する同じ式の SELECT と、列メタ（名前・型）・値が一致する。
#[test]
fn returning_expressions_match_select_for_every_dml() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let ins = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.3,0.4]', 'ja', 'hello') RETURNING {EXPRS} \
                 USING OPERATION_ID 'op-e-ins'"
            ),
        )
        .expect("insert returning expr"),
    );
    assert_eq!(ins.rows_affected, 1);
    assert_matches_select(&ins, &select_exprs(&core, &alice));
    assert_eq!(
        ins.result.rows[0].cells[0],
        Cell::Text("JA".to_string()),
        "upper(lang)"
    );
    assert_eq!(ins.result.rows[0].cells[2], Cell::Text("hello".to_string()));

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'hello world' WHERE id = 1 RETURNING {EXPRS} \
                 USING OPERATION_ID 'op-e-upd1'"
            ),
        )
        .expect("single-row update returning expr"),
    );
    assert_matches_select(&upd, &select_exprs(&core, &alice));

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE lang = 'ja' RETURNING {EXPRS} \
                 USING OPERATION_ID 'op-e-upd2'"
            ),
        )
        .expect("predicate update returning expr"),
    );
    assert_eq!(upd.rows_affected, 1);
    assert_matches_select(&upd, &select_exprs(&core, &alice));

    // UPSERT（衝突 → DO UPDATE、新規 → 挿入）。
    let ups = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.3,0.4]', 'fr', 'bonjour'), (2, '[0.5,0.6]', 'de', 'hallo') \
                 ON CONFLICT (id) DO UPDATE SET lang = 'es' RETURNING {EXPRS} \
                 USING OPERATION_ID 'op-e-ups'"
            ),
        )
        .expect("upsert returning expr"),
    );
    assert_eq!(ups.result.rows.len(), 2);
    let select = select_exprs(&core, &alice);
    assert_eq!(ups.result.columns, select.columns);
    let mut got: Vec<_> = ups.result.rows.iter().map(|r| r.cells.clone()).collect();
    let mut want: Vec<_> = select.rows.iter().map(|r| r.cells.clone()).collect();
    got.sort_by_key(|c| format!("{c:?}"));
    want.sort_by_key(|c| format!("{c:?}"));
    assert_eq!(got, want);

    // DELETE は削除前の値を返す。
    let before = select_exprs(&core, &alice);
    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'es' RETURNING {EXPRS} \
                 USING OPERATION_ID 'op-e-del'"
            ),
        )
        .expect("delete returning expr"),
    );
    assert_eq!(del.rows_affected, 1);
    assert_eq!(del.result.columns, before.columns);
    assert_eq!(del.result.rows.len(), 1);
    assert!(before
        .rows
        .iter()
        .any(|r| r.cells == del.result.rows[0].cells));
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 束縛時の型エラーでは書き込みが起きず、台帳も消費されない（同じ operation_id で再実行できる）。
#[test]
fn returning_expression_bind_error_writes_nothing_and_keeps_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "hello", "op-seed");

    let err = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING lang + 1 \
             USING OPERATION_ID 'op-bind-err'"
        ),
    )
    .expect_err("type error");
    assert_eq!(err.wire_code(), "42883");
    let r = core
        .execute_sql(&alice, &format!("SELECT lang FROM {TABLE}"))
        .expect("select");
    assert_eq!(r.rows[0].cells, vec![Cell::Text("ja".to_string())]);
    let ok = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 RETURNING upper(lang) \
                 USING OPERATION_ID 'op-bind-err'"
            ),
        )
        .expect("ledger unconsumed"),
    );
    assert_eq!(ok.result.rows[0].cells, vec![Cell::Text("EN".to_string())]);
}

/// 評価時エラー（0 除算 `22012`）は commit 前に書き込みを中止する。UPDATE 前の値・DELETE 対象行が
/// 残り、台帳も消費されない。
#[test]
fn returning_expression_eval_error_aborts_write_and_keeps_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "hello", "op-seed");

    let err = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "UPDATE {TABLE} SET lang = 'en' WHERE id = 1 \
             RETURNING 1 / (length(body) - length(body)) USING OPERATION_ID 'op-eval-upd'"
        ),
    )
    .expect_err("division by zero");
    assert_eq!(err.wire_code(), "22012");
    let err = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' \
             RETURNING 1 / (length(body) - length(body)) USING OPERATION_ID 'op-eval-del'"
        ),
    )
    .expect_err("division by zero");
    assert_eq!(err.wire_code(), "22012");
    let err = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (2, '[0.1,0.2]', 'x', 'y') \
             RETURNING 1 / (length(body) - length(body)) USING OPERATION_ID 'op-eval-ins'"
        ),
    )
    .expect_err("division by zero");
    assert_eq!(err.wire_code(), "22012");
    let r = core
        .execute_sql(&alice, &format!("SELECT lang FROM {TABLE}"))
        .expect("select");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells, vec![Cell::Text("ja".to_string())]);
    // 台帳は消費されていない。
    let ok = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'ja' RETURNING upper(lang) \
                 USING OPERATION_ID 'op-eval-del'"
            ),
        )
        .expect("ledger unconsumed"),
    );
    assert_eq!(ok.rows_affected, 1);
}

/// 他テナントの行は RETURNING 式の対象にも結果にも現れない。
#[test]
fn returning_expressions_never_touch_other_tenant_rows() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut session = SessionState::default();
    seed(&core, &alice, 1, "ja", "a", "op-a");
    seed(&core, &bob, 2, "ja", "b", "op-b");
    let out = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'z' WHERE lang = 'ja' RETURNING upper(body) AS b \
                 USING OPERATION_ID 'op-iso'"
            ),
        )
        .expect("update"),
    );
    assert_eq!(out.result.rows.len(), 1);
    assert_eq!(out.result.rows[0].cells, vec![Cell::Text("Z".to_string())]);
}
// ---------------------------------------------------------------------
// RETURNING での UDF 評価（Issue #3250・SQL-21 のポインタ）
// ---------------------------------------------------------------------

fn define(core: &EngineCore, ctx: &PolicyContext, session: &mut SessionState, sql: &str) {
    run(core, ctx, session, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// 宣言的 UDF は RETURNING の全 DML 形で評価でき、列メタ・値が同じ式の SELECT と一致する
/// （別名付き・UDF の入れ子・組み込み関数との混在を含む）。
#[test]
fn returning_declarative_udf_matches_select_for_every_dml() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION shout(x) AS upper(x)",
    );
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION twice(x) AS shout(shout(x))",
    );
    let exprs = "shout(lang) AS u, twice(body) AS t, length(shout(body)) + 1 AS n";

    let ins = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.3,0.4]', 'ja', 'hello'), (2, '[0.5,0.6]', 'en', 'abc') \
                 RETURNING {exprs} USING OPERATION_ID 'op-udf-ins'"
            ),
        )
        .expect("insert returning udf"),
    );
    assert_eq!(ins.rows_affected, 2);
    let select = match run(
        &core,
        &alice,
        &mut session,
        &format!("SELECT {exprs} FROM {TABLE} LIMIT 10"),
    )
    .expect("select udf")
    {
        SqlOutcome::Query(q) => q,
        other => panic!("expected Query, got {other:?}"),
    };
    assert_eq!(ins.result.columns, select.columns);
    let mut a: Vec<String> = ins
        .result
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect();
    let mut b: Vec<String> = select
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 RETURNING shout(lang) AS u \
                 USING OPERATION_ID 'op-udf-upd'"
            ),
        )
        .expect("update returning udf"),
    );
    assert_eq!(upd.result.rows.len(), 1);
    assert_eq!(upd.result.rows[0].cells, vec![Cell::Text("FR".to_string())]);

    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "UPDATE {TABLE} SET body = 'zzz' WHERE lang = 'en' RETURNING twice(body) AS t \
                 USING OPERATION_ID 'op-udf-updp'"
            ),
        )
        .expect("predicate update returning udf"),
    );
    assert_eq!(
        upd.result.rows[0].cells,
        vec![Cell::Text("ZZZ".to_string())]
    );

    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE id = 1 RETURNING shout(body) AS b \
                 USING OPERATION_ID 'op-udf-del'"
            ),
        )
        .expect("delete returning udf"),
    );
    assert_eq!(
        del.result.rows[0].cells,
        vec![Cell::Text("HELLO".to_string())]
    );

    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'en' RETURNING shout(lang) AS l \
                 USING OPERATION_ID 'op-udf-delp'"
            ),
        )
        .expect("predicate delete returning udf"),
    );
    assert_eq!(del.result.rows[0].cells, vec![Cell::Text("EN".to_string())]);
}

/// 評価時エラー（0 除算）は commit 前に abort し、行も台帳も残さない。
#[test]
fn returning_udf_eval_error_aborts_without_consuming_ledger() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION inv(x) AS 1 / x",
    );
    let sql = |ret: &str| {
        format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') RETURNING {ret} USING OPERATION_ID 'op-udf-err'"
        )
    };
    let err = run(&core, &alice, &mut session, &sql("inv(length(body) - 5)"))
        .expect_err("division by zero");
    assert_eq!(err.wire_code(), "22012", "{err}");
    assert_eq!(count_star(&core, &alice, TABLE), 0);
    expect_returning(
        run(&core, &alice, &mut session, &sql("inv(length(body))"))
            .expect("ledger must be unconsumed"),
    );
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// `LANGUAGE sql|plpgsql` 関数（書き込みを試みうる UDF）は RETURNING から呼べず、書き込み前に
/// `0A000` で拒否する。対象表・ルーチンが書く表・台帳のいずれも残さない。
// intentional: docs/design/sql-returning.md
#[test]
fn returning_routine_call_is_rejected_before_any_write() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE TABLE audit (embedding VECTOR(2) NOT NULL, lang TEXT NOT NULL, body TEXT NOT NULL)",
    );
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION w(x text) RETURNS text LANGUAGE sql AS \
         $$ INSERT INTO audit (embedding, lang, body) VALUES ('[0.1,0.2]', 'x', 'y'); SELECT x $$",
    );
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION pw(x text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS \
         $$ BEGIN RETURN x; END $$",
    );
    for f in ["w(lang)", "pw(lang)"] {
        let err = run(
            &core,
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (1, '[0.1,0.2]', 'ja', 'hello') RETURNING {f} USING OPERATION_ID 'op-udf-rt'"
            ),
        )
        .expect_err(f);
        assert_eq!(err.wire_code(), "0A000", "{f}: {err}");
        assert_eq!(count_star(&core, &alice, TABLE), 0, "{f}");
        assert_eq!(count_star(&core, &alice, "audit"), 0, "{f}");
    }
    // 台帳が未消費なので、同じ operation_id で RETURNING なしの INSERT が通る。
    run(
        &core,
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-udf-rt'"
        ),
    )
    .expect("ledger must be unconsumed");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
    // UPDATE・DELETE でも同じく書き込み前に拒否する。
    for sql in [
        format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 RETURNING w(lang) \
             USING OPERATION_ID 'op-udf-ru'"
        ),
        format!(
            "DELETE FROM {TABLE} WHERE id = 1 RETURNING w(lang) USING OPERATION_ID 'op-udf-rd'"
        ),
    ] {
        let err = run(&core, &alice, &mut session, &sql).expect_err(&sql);
        assert_eq!(err.wire_code(), "0A000", "{sql}: {err}");
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);
    assert_eq!(count_star(&core, &alice, "audit"), 0);
}

/// 他セッションが定義した UDF は解決できず `42883`。エラー文言に関数本体を含めない。
#[test]
fn returning_udf_is_not_visible_across_sessions() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    let mut bob_session = SessionState::default();
    bob_session.allow_ddl();
    let mut alice_session = SessionState::default();
    define(
        &core,
        &bob,
        &mut bob_session,
        "CREATE FUNCTION secretf(x) AS upper(x)",
    );
    let err = run(
        &core,
        &alice,
        &mut alice_session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') RETURNING secretf(lang) USING OPERATION_ID 'op-udf-x'"
        ),
    )
    .expect_err("other session's udf");
    assert_eq!(err.wire_code(), "42883", "{err}");
    assert!(!err.to_string().contains("upper"), "{err}");
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// データ変更 CTE の副文の RETURNING は、別名・式・宣言的 UDF を基表 DML と同じ束縛・評価で受理し、
/// 外側の SELECT がその公開名で引ける（Issue #3251）。
#[test]
fn data_modifying_cte_returning_expressions_and_udf_are_evaluated() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION shout(x) AS upper(x)",
    );
    run(
        &core,
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-cte-udf-seed'"
        ),
    )
    .expect("seed");
    let out = run(
        &core,
        &alice,
        &mut session,
        &format!(
            "WITH u AS (UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 \
             RETURNING id AS rid, shout(lang) AS s, upper(body)) \
             SELECT rid, s, upper FROM u USING OPERATION_ID 'op-cte-udf-u1'"
        ),
    )
    .expect("update cte");
    match out {
        SqlOutcome::Query(q) => {
            assert_eq!(q.rows.len(), 1);
            assert_eq!(
                q.rows[0].cells,
                vec![
                    Cell::Integer(1),
                    Cell::Text("FR".to_string()),
                    Cell::Text("HELLO".to_string())
                ]
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
    // 述語形 DELETE（参照されない副文）でも UDF が評価され、削除が確定する。
    run(
        &core,
        &alice,
        &mut session,
        &format!(
            "WITH d AS (DELETE FROM {TABLE} WHERE id > 0 RETURNING shout(lang)) \
             SELECT id FROM {TABLE} USING OPERATION_ID 'op-cte-udf-d1'"
        ),
    )
    .expect("delete cte");
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// データ変更 CTE の全副文の RETURNING は書き込み前に束縛される。未登録の関数（`42883`）・未知列
/// （`42703`）・`LANGUAGE sql|plpgsql` 関数（`0A000`）は、参照されない副文でも先行副文の書き込みと
/// 台帳より前に拒否され、行も台帳も残らない（Issue #3251）。
// intentional: docs/design/sql-returning.md
#[test]
fn data_modifying_cte_returning_errors_are_decided_before_any_write() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION pw(x text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS \
         $$ BEGIN RETURN x; END $$",
    );
    run(
        &core,
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') USING OPERATION_ID 'op-cte-pre-seed'"
        ),
    )
    .expect("seed");
    for (i, (ret, code)) in [
        ("nope(lang)", "42883"),
        ("lang || nope_col", "42703"),
        ("pw(lang)", "0A000"),
    ]
    .iter()
    .enumerate()
    {
        // 1 つ目の副文は有効な UPDATE（先に書き込まれてはならない）、2 つ目が無効な RETURNING。
        let sql = format!(
            "WITH a AS (UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 RETURNING id), \
             b AS (DELETE FROM {TABLE} WHERE id = 1 RETURNING {ret}) \
             SELECT id FROM a USING OPERATION_ID 'op-cte-pre-{i}'"
        );
        let err = run(&core, &alice, &mut session, &sql).expect_err(&sql);
        assert_eq!(err.wire_code(), *code, "#{i} {sql}: {err}");
        assert_eq!(count_star(&core, &alice, TABLE), 1, "#{i}");
        let lang = run(
            &core,
            &alice,
            &mut session,
            &format!("SELECT lang FROM {TABLE} LIMIT 5"),
        )
        .expect("select lang");
        match lang {
            SqlOutcome::Query(q) => assert_eq!(q.rows[0].cells, vec![Cell::Text("ja".to_string())]),
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

/// 明示 Tx 内の UDF 付き RETURNING は ROLLBACK で行が残らず、COMMIT で残る。
#[test]
fn returning_udf_in_explicit_transaction() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION shout(x) AS upper(x)",
    );
    for (end, expected, op) in [("ROLLBACK", 0, "op-udf-tx1"), ("COMMIT", 1, "op-udf-tx2")] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let out = expect_returning(
            core.execute_sql_in_txn(
                &alice,
                &mut session,
                &mut txn,
                &format!(
                    "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                     (1, '[0.1,0.2]', 'ja', 'hello') RETURNING shout(lang) AS u \
                     USING OPERATION_ID '{op}'"
                ),
            )
            .expect("insert in tx"),
        );
        assert_eq!(out.result.rows[0].cells, vec![Cell::Text("JA".to_string())]);
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, end)
            .expect("end tx");
        assert_eq!(count_star(&core, &alice, TABLE), expected, "{end}");
    }
}

/// ビュー経由の INSERT・UPDATE・DELETE の RETURNING は、別名・式・宣言的 UDF を基表 DML と同じ束縛・評価で
/// 受理する（Issue #3251。UDF はセッションのレジストリで解決する）。出力名は公開名から導かれる。
#[test]
fn returning_expressions_and_udf_through_view_are_evaluated() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION shout(x) AS upper(x)",
    );
    define(
        &core,
        &alice,
        &mut session,
        &format!("CREATE VIEW v_udf AS SELECT * FROM {TABLE}"),
    );
    let ins = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            "INSERT INTO v_udf (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hello') RETURNING shout(lang) AS s, upper(body) \
             USING OPERATION_ID 'op-udf-vins'",
        )
        .expect("view insert"),
    );
    assert_eq!(
        ins.result.rows[0].cells,
        vec![Cell::Text("JA".into()), Cell::Text("HELLO".into())]
    );
    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            "UPDATE v_udf SET lang = 'fr' WHERE id = 1 RETURNING id AS rid, shout(lang) AS s \
             USING OPERATION_ID 'op-udf-vu'",
        )
        .expect("view update single"),
    );
    assert_eq!(
        upd.result.rows[0].cells,
        vec![Cell::Integer(1), Cell::Text("FR".into())]
    );
    let upd = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            "UPDATE v_udf SET lang = 'de' WHERE lang = 'fr' RETURNING shout(lang) \
             USING OPERATION_ID 'op-udf-vup'",
        )
        .expect("view update predicate"),
    );
    assert_eq!(upd.result.rows[0].cells, vec![Cell::Text("DE".into())]);
    let del = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            "DELETE FROM v_udf WHERE lang = 'de' RETURNING shout(lang) AS s, id + 1 \
             USING OPERATION_ID 'op-udf-vdp'",
        )
        .expect("view delete predicate"),
    );
    assert_eq!(
        del.result.rows[0].cells,
        vec![Cell::Text("DE".into()), Cell::Float(2.0)]
    );
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// 列を絞る・改名するビューの RETURNING 式は公開名で書き、出力名の既定も公開名から導かれて物理名が
/// 漏れない。非公開列を式で参照すると `42703`、`LANGUAGE plpgsql` 関数は書き込み前に `0A000`
/// （Issue #3251）。
// intentional: docs/design/sql-returning.md
#[test]
fn returning_expressions_through_renaming_view_use_public_names() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();
    session.allow_ddl();
    define(
        &core,
        &alice,
        &mut session,
        "CREATE FUNCTION pw(x text) RETURNS text LANGUAGE plpgsql IMMUTABLE AS \
         $$ BEGIN RETURN x; END $$",
    );
    define(
        &core,
        &alice,
        &mut session,
        &format!("CREATE VIEW v_ren (lng, txt) AS SELECT lang, body FROM {TABLE}"),
    );
    run(
        &core,
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.1,0.2]', 'ja', 'hi') USING OPERATION_ID 'op-ren-seed'"
        ),
    )
    .expect("seed");
    let out = expect_returning(
        run(
            &core,
            &alice,
            &mut session,
            "UPDATE v_ren SET lng = 'ja' WHERE lng = 'ja' \
             RETURNING upper(lng), txt AS t2, lng USING OPERATION_ID 'op-ren-upd'",
        )
        .unwrap_or_else(|e| panic!("view rename update: {e}")),
    );
    let names: Vec<String> = out
        .result
        .columns
        .iter()
        .map(|c| match c {
            engine::sql::exec::ColumnMeta::Scalar { name, .. }
            | engine::sql::exec::ColumnMeta::Computed { name, .. } => name.clone(),
            other => panic!("unexpected column {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["upper", "t2", "lng"]);
    assert_eq!(
        out.result.rows[0].cells,
        vec![
            Cell::Text("JA".into()),
            Cell::Text("hi".into()),
            Cell::Text("ja".into())
        ]
    );
    for (sql, code) in [
        (
            "UPDATE v_ren SET lng = 'x' WHERE lng = 'ja' RETURNING upper(lang)",
            "42703",
        ),
        (
            "UPDATE v_ren SET lng = 'x' WHERE lng = 'ja' RETURNING pw(lng)",
            "0A000",
        ),
        (
            "DELETE FROM v_ren WHERE lng = 'ja' RETURNING pw(lng)",
            "0A000",
        ),
    ] {
        let err = run(&core, &alice, &mut session, sql).expect_err(sql);
        assert_eq!(err.wire_code(), code, "{sql}: {err}");
    }
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}
