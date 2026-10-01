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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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
        assert_eq!(err.wire_code(), "22000", "{sql}");
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

/// 明示トランザクション内の UPSERT `RETURNING` はまだ未対応として拒否される
/// （fail-closed の維持。#1273 で対応するまでの保護。UPDATE・DELETE は #1272 で受理）。
#[test]
fn upsert_returning_inside_explicit_transaction_is_still_rejected() {
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
    let err = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, &sql)
        .expect_err("UPSERT RETURNING inside a transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000", "{sql}");
    let _ = core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK");
    let readback = core
        .execute_sql(&alice, &format!("SELECT lang FROM {TABLE} LIMIT 10"))
        .expect("select");
    assert_eq!(readback.rows.len(), 1);
    assert_eq!(readback.rows[0].cells, vec![Cell::Text("ja".to_string())]);
}
