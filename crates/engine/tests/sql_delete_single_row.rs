//! `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`（SQL-18、
//! TASK-191、#867）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-191・
//! `docs/spec/04-behavior/sql-surface.md` SQL-18。関連ポインタ: RLS-9（他テナント
//! 存在情報の非漏えい）・RLS-10・RECOVER-1〜3・RECOVER-4（対象行不存在／他テナント
//! 所有を区別しない `NotFound`）・RECOVER-10（台帳照合による再送判定）・TABLE-12
//! （テナント名前空間キー）。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `DELETE` の覗き見判定 →
//! `sql::allowlist::validate_delete_tokens` → `sql::parser::bind_delete` →
//! `sql::exec::execute_delete`）を production 経路として検証する。
//! `truncate_table.rs`・`rls11_read_your_writes.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::HnswParams;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::search_engine;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
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
    let path = unique_db_path("sql-delete-single-row");
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

/// SQL `INSERT`（行形）経由で行を投入する（TASK-82・SQL-10。可視性は常に
/// `Visibility::Private` に固定される契約——`sql::exec::execute_insert_with_
/// schema` ドキュメント参照。本テストは所有テナントからの読み取りのみを
/// 検証するため（`ctx_for(tenant, true)` は `Public`＋`Private` を許可）、
/// 可視性の固定は検証内容に影響しない）。
fn insert_row(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, body: &str, seq: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        &format!(
            "INSERT INTO {table} (id, embedding, lang, body) VALUES \
             ({id}, '[0.1,0.2]', 'ja', '{body}') USING OPERATION_ID 'seed-{table}-{id}-{seq}'"
        ),
    )
    .expect("insert row");
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

fn distance_hit_ids(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<u64> {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {table} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 50"),
    )
    .expect("select should succeed")
    .rows
    .iter()
    .map(|r| r.id)
    .collect()
}

fn hybrid_hit_ids(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<u64> {
    core.execute_sql(
        ctx,
        &format!(
            "SELECT id FROM {table} ORDER BY HYBRID(embedding, '[0.1,0.2]', body, 'marker-term') LIMIT 50"
        ),
    )
    .expect("hybrid select should succeed")
    .rows
    .iter()
    .map(|r| r.id)
    .collect()
}

fn scan_hit_ids(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<u64> {
    core.execute_sql(ctx, &format!("SELECT id FROM {table} LIMIT 50"))
        .expect("scan should succeed")
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

fn where_dense_hit_ids(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<u64> {
    core.execute_sql(
        ctx,
        &format!(
            "SELECT id FROM {table} WHERE lang = 'ja' ORDER BY embedding <=> '[0.1,0.2]' LIMIT 50"
        ),
    )
    .expect("WHERE dense select should succeed")
    .rows
    .iter()
    .map(|r| r.id)
    .collect()
}

fn hnsw_engine_kind() -> engine::search_engine::SearchEngineKind {
    search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params")
}

/// 成功系・SQL-18 の物理削除: 自テナント行の DELETE は `rows_affected: 1` を
/// 返し、以後 COUNT(*)・DISTANCE・hybrid・scan・`WHERE` 付き DISTANCE の
/// いずれにも現れない。加えて削除後の同一 id への再 INSERT が `23505`
/// にならず成功する（物理削除の確認）。
#[test]
fn delete_removes_row_from_all_read_shapes_and_allows_reinsert() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "marker-term body", "1");
    insert_row(&core, &alice, TABLE, 2, "other body", "2");

    assert_eq!(count_star(&core, &alice, TABLE), 2);
    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![1, 2]);
    assert!(hybrid_hit_ids(&core, &alice, TABLE).contains(&1));
    assert!(scan_hit_ids(&core, &alice, TABLE).contains(&1));
    assert!(where_dense_hit_ids(&core, &alice, TABLE).contains(&1));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-delete-1'"),
        )
        .expect("DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }

    assert_eq!(count_star(&core, &alice, TABLE), 1);
    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![2]);
    assert!(!hybrid_hit_ids(&core, &alice, TABLE).contains(&1));
    assert!(!scan_hit_ids(&core, &alice, TABLE).contains(&1));
    assert!(!where_dense_hit_ids(&core, &alice, TABLE).contains(&1));

    // 物理削除の確認: 同一 id への再 INSERT が id 衝突（23505）にならない。
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (1, '[0.5,0.6]', 'en', 'post-delete body') \
             USING OPERATION_ID 'op-post-delete-insert'"
        ),
    )
    .expect("re-INSERT after physical DELETE must succeed");
    assert_eq!(count_star(&core, &alice, TABLE), 2);
}

/// RLS-9・RLS-10: 他テナント保持 id・未存在 id のいずれへの DELETE も
/// `rows_affected: 0` の成功として区別なく返る。他テナントの行はそのまま
/// 所有者から読める（副作用が一切発生していない確認）。
#[test]
fn delete_response_is_identical_for_other_tenant_row_and_nonexistent_id() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    insert_row(&core, &bob, TABLE, 7, "bob body", "7");

    let mut session = SessionState::default();

    // (b) 他テナント（bob）保持 id への DELETE。
    let outcome_other_tenant = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 7 USING OPERATION_ID 'op-other-tenant'"),
        )
        .expect("DELETE against another tenant's row must succeed as a 0-row no-op");
    match outcome_other_tenant {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    // bob の行は無傷。
    assert_eq!(count_star(&core, &bob, TABLE), 1);

    // (c) 未存在 id への DELETE。応答が (b) と完全に一致する（別 operation_id・
    // 同一結果型・同一件数）。
    let outcome_nonexistent = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-nonexistent'"),
        )
        .expect("DELETE against a nonexistent id must succeed as a 0-row no-op");
    match outcome_nonexistent {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(outcome_other_tenant, outcome_nonexistent);

    // 0 行 DELETE でも台帳へ commit される（codex-review P1 指摘・PR #983 対応。
    // `tenant::delete_row_ledgered_unchecked` ドキュメント参照）ため、同一 id
    // への再送は内容一致で `23505` になる（0 行成功の再現ではない）。
    let err_same_id_resend = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 7 USING OPERATION_ID 'op-other-tenant'"),
        )
        .expect_err(
            "resending a 0-row DELETE's operation_id against the same id must be a duplicate",
        );
    assert_eq!(err_same_id_resend.wire_code(), "23505");

    // 異なる id への再送は内容不一致で `22023`（他テナント保持 id・未存在 id の
    // 両方で応答が完全に一致し、存在情報を漏らさない。RLS-9・RLS-10）。
    let err_other_tenant_id_mismatch = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-other-tenant'"),
        )
        .expect_err("resending against a different id must mismatch on content");
    assert_eq!(err_other_tenant_id_mismatch.wire_code(), "22023");
}

/// codex-review P1 指摘（PR #983）の再現シナリオの回帰: 0 行 DELETE の
/// `operation_id` を、対象 `id` へ後から INSERT した後に再送しても、実削除
/// ではなく台帳照合による `23505`（同一内容）で拒否される。
///
/// (a) alice 自身のテナント内で対象 id が未存在 → 後から同じ id へ INSERT。
/// (b) 対象 id が他テナント（bob）保持 → 後から alice 自身が同じ id へ
///     INSERT（TABLE-12: id はテナント内スコープのため許可される）。
/// いずれも再送は `23505` で拒否され、新規に作られた行は無傷のまま残る。
#[test]
fn delete_resending_a_0_row_operation_id_after_the_target_id_is_inserted_does_not_delete_it() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    insert_row(&core, &bob, TABLE, 7, "bob body", "7");

    let mut session = SessionState::default();

    // (a) 未存在 id（999）への 0 行 DELETE。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-replay-a'"),
        )
        .expect("DELETE against a nonexistent id must succeed as a 0-row no-op");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    // 通信断等で対象 id へ新規 INSERT が行われた後の再送。
    insert_row(&core, &alice, TABLE, 999, "alice new body", "999");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-replay-a'"),
        )
        .expect_err("resend after the target id now exists must not silently delete it");
    assert_eq!(err.wire_code(), "23505");
    // 新規に挿入した行は無傷のまま残る。
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    // (b) 他テナント（bob）保持 id（7）への 0 行 DELETE。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 7 USING OPERATION_ID 'op-replay-b'"),
        )
        .expect("DELETE against another tenant's row must succeed as a 0-row no-op");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    // alice 自身が同じ id（7）へ INSERT（TABLE-12: テナント内スコープのため
    // bob の行 7 とは独立に成功する）。
    insert_row(&core, &alice, TABLE, 7, "alice body", "7");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 7 USING OPERATION_ID 'op-replay-b'"),
        )
        .expect_err("resend after alice inserts her own id=7 must not silently delete it");
    assert_eq!(err.wire_code(), "23505");
    // alice の新規行・bob の行の双方が無傷のまま残る。
    assert_eq!(count_star(&core, &alice, TABLE), 2);
    assert_eq!(count_star(&core, &bob, TABLE), 1);
}

/// RECOVER-4・RECOVER-10 の優先順位: 使用済み `operation_id` を再送すると、
/// 所有権判定（`NotFound`〔0 行成功〕）より**前**に台帳照合が働く。
/// `content_hash::for_delete` は削除対象 `id` のみを内容とするため（`tenant.rs`
/// ドキュメント参照）、同一 `id` への再送は内容一致（`23505`）、異なる `id`
/// （他テナント保持 id・未存在 id）への再送は内容不一致（`22023`）として
/// 区別される——いずれの場合も所有権判定（`NotFound`）までは到達しない。
#[test]
fn delete_resending_used_operation_id_hits_ledger_before_ownership_check() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let bob = ctx_for("bob", true);
    insert_row(&core, &alice, TABLE, 1, "alice body", "1");
    insert_row(&core, &bob, TABLE, 7, "bob body", "7");

    let mut session = SessionState::default();
    // 使用済みにする: alice 自身の行 1 を消費して operation_id を確定させる。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-used'"),
        )
        .expect("first DELETE should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }

    // (a) 同一 id（1）への再送 → 内容一致 → 23505（対象行は既に削除済みだが
    // `NotFound` ではなく重複commitとして検出される）。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-used'"),
        )
        .expect_err("resend against the same id must be rejected as duplicate commit");
    assert_eq!(err.wire_code(), "23505");

    // (b) 使用済み operation_id で他テナント保持 id（7）へ再送 → id が異なる
    // ため内容不一致 → 22023（`NotFound` ではない）。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 7 USING OPERATION_ID 'op-used'"),
        )
        .expect_err("resend against another tenant's row must mismatch on content");
    assert_eq!(err.wire_code(), "22023");
    // bob の行は無傷（ledger 照合が先に働いても副作用は一切発生しない）。
    assert_eq!(count_star(&core, &bob, TABLE), 1);

    // (c) 使用済み operation_id で未存在 id（999）へ再送 → 同じく内容不一致 →
    // 22023。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 999 USING OPERATION_ID 'op-used'"),
        )
        .expect_err("resend against a nonexistent id must mismatch on content");
    assert_eq!(err.wire_code(), "22023");
}

/// RECOVER-10: DELETE の台帳キー空間は INSERT と共有される
/// `(tenant, table, operation_id)`。表層をまたいだ再送も同じ空間で判定される。
#[test]
fn delete_shares_ledger_key_space_with_insert() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    // INSERT op X → DELETE op X（同一内容ではないため内容不一致 22023）。
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (10, '[0.1,0.2]', 'en', 'x body') USING OPERATION_ID 'op-x'"
        ),
    )
    .expect("INSERT op-x should succeed");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 10 USING OPERATION_ID 'op-x'"),
        )
        .expect_err("DELETE reusing INSERT's operation_id with different content must mismatch");
    assert_eq!(err.wire_code(), "22023");
    // 内容不一致は拒否されるため行は残る。
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    // DELETE op Y 成功 → 同文再送 → 23505。
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
             (20, '[0.3,0.4]', 'en', 'y body') USING OPERATION_ID 'op-y-seed'"
        ),
    )
    .expect("seed insert for op-y should succeed");
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 20 USING OPERATION_ID 'op-y'"),
        )
        .expect("DELETE op-y should succeed");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 20 USING OPERATION_ID 'op-y'"),
        )
        .expect_err("resending the same DELETE operation_id must be a duplicate");
    assert_eq!(err.wire_code(), "23505");

    // DELETE op Y → INSERT op Y（同一 operation_id・異なる内容）→ 22023。
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, body) VALUES \
                 (30, '[0.5,0.6]', 'en', 'z body') USING OPERATION_ID 'op-y'"
            ),
        )
        .expect_err("INSERT reusing DELETE's operation_id with different content must mismatch");
    assert_eq!(err.wire_code(), "22023");
    // 内容不一致は拒否されるため id=30 は挿入されていない。
    assert!(!distance_hit_ids(&core, &alice, TABLE).contains(&30));
}

/// RECOVER-1: `operation_id` 句の省略（明示 `NULL` を含む）は `Ledgered`
/// （既定）構成では `23502` で書き込みトランザクション開始前に拒否される。
#[test]
fn delete_missing_operation_id_is_rejected_with_23502_before_any_write() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "body", "1");

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1"),
        )
        .expect_err("missing operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_star(&core, &alice, TABLE), 1);

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID NULL"),
        )
        .expect_err("explicit NULL operation_id must be rejected");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// キャッシュ失効: `SqlArenaCache`（DISTANCE クエリ）・`VisibleBitmapCache`
/// （`COUNT(*)`）をそれぞれ DELETE 前に 1 度実行して温めてから DELETE し、
/// 同じクエリを再実行して削除済み行が一切ヒットしないことを確認する
/// （`truncate_table.rs::truncate_invalidates_arena_and_visible_bitmap_caches`
/// と同じ設計）。
#[test]
fn delete_invalidates_arena_and_visible_bitmap_caches() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "body-1", "1");
    insert_row(&core, &alice, TABLE, 2, "body-2", "2");

    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![1, 2]);
    assert_eq!(count_star(&core, &alice, TABLE), 2);

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-cache-invalidate'"),
    )
    .expect("DELETE should succeed");

    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![2]);
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// HNSW opt-in 時の索引キャッシュ失効: フィルタなし DISTANCE を DELETE 前に
/// ウォームし、DELETE 後は削除行が非混入のまま索引キャッシュが非 vacuous
/// （`hits+misses > 0`）に動作し続けることを確認する。
#[test]
fn delete_invalidates_hnsw_index_cache_and_excludes_deleted_row() {
    let path = unique_db_path("sql-delete-single-row-hnsw");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage_with_engine(storage, hnsw_engine_kind());
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "body-1", "1");
    insert_row(&core, &alice, TABLE, 2, "body-2", "2");

    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![1, 2]);
    let stats_before = core.hnsw_index_cache_stats();

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-hnsw-invalidate'"),
    )
    .expect("DELETE should succeed");

    assert_eq!(distance_hit_ids(&core, &alice, TABLE), vec![2]);
    // 本フィクスチャの行数（2 件）は `MIN_INDEXED_ROWS`（1,024）を大きく下回る
    // ため、索引は構築されず常に brute-force `fallbacks` 経路を通る
    // （`hnsw_cache.rs::MIN_INDEXED_ROWS` ドキュメント参照）。索引キャッシュ
    // 経路そのものが DELETE 後も非 vacuous に動作し続けていること（呼び出しが
    // 実際に発生していること）を `fallbacks` の増分で固定する。
    let stats_after = core.hnsw_index_cache_stats();
    assert!(
        stats_after.fallbacks > stats_before.fallbacks,
        "hnsw index cache must observe a non-vacuous lookup after DELETE: before={stats_before:?} after={stats_after:?}"
    );
}

/// `USING PLAN` と同じく `EXPLAIN DELETE ...` は許可形状に存在しないため
/// `42601` で拒否される（`truncate_table.rs::explain_truncate_is_rejected_as_
/// unsupported_syntax` と同型）。
#[test]
fn explain_delete_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("EXPLAIN DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-explain'"),
        )
        .expect_err("EXPLAIN DELETE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// `WHERE` を伴わない `DELETE` は許可形状外（`id` 等価指定のみ受理）として
/// `42601` で拒否される。
#[test]
fn delete_without_where_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            &format!("DELETE FROM {TABLE} USING OPERATION_ID 'op-no-where'"),
        )
        .expect_err("DELETE without WHERE must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// セッションを持たない `execute_sql` は `DELETE` を受理しない（`SELECT`／
/// `Aggregate`／`Scan` 専用のエントリポイントであり、`INSERT`／`TRUNCATE` と
/// 同じくセッション経由の `execute_sql_in_session` を要する）。
#[test]
fn execute_sql_without_session_rejects_delete() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "body", "1");

    let err = core
        .execute_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-no-session'"),
        )
        .expect_err("DELETE must be rejected on the session-less entry point");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(count_star(&core, &alice, TABLE), 1);
}

/// 存在しないテーブルへの DELETE は `42P01`（`UndefinedTable`）。
#[test]
fn delete_undefined_table_is_rejected_with_42p01() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    let mut session = SessionState::default();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "DELETE FROM ghost WHERE id = 1 USING OPERATION_ID 'op-undefined-table'",
        )
        .expect_err("undefined table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
}

/// `EngineCore::execute_delete_sql`（セッション非経由の直接エントリポイント）
/// が `execute_sql_in_session` の DELETE 分岐と同じ契約であることを確認する
/// （`truncate_table.rs::execute_truncate_sql_direct_entry_point_matches_
/// session_dispatch_contract` と同型）。
#[test]
fn execute_delete_sql_direct_entry_point_matches_session_dispatch_contract() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice", true);
    insert_row(&core, &alice, TABLE, 1, "body", "1");

    let outcome = core
        .execute_delete_sql(
            &alice,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-direct-entry'"),
        )
        .expect("direct entry point DELETE should succeed");
    let _: engine::sql::exec::DeleteOutcome = outcome;
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(count_star(&core, &alice, TABLE), 0);
}

/// 対象選定は「所有かつ可視」（RLS-10・SQL-21。Issue #1253）: Public のみ可視の
/// `PolicyContext` では自テナントの Private 行への id 指定 DELETE は 0 行成功になり
/// 行は残る。台帳は commit されるため同一 `operation_id` の再送は `23505`。
/// Public＋Private の可視集合では削除できる。
#[test]
fn delete_does_not_target_owned_but_invisible_row() {
    let (core, path) = new_core_with_table();
    let _guard = CleanupGuard(path);
    let alice_both = ctx_for("alice", true);
    let alice_public_only = ctx_for("alice", false);
    // SQL の INSERT は Private 固定。
    insert_row(&core, &alice_both, TABLE, 1, "a", "1");

    let mut session = SessionState::default();
    let sql = format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-invisible'");
    match core
        .execute_sql_in_session(&alice_public_only, &mut session, &sql)
        .expect("invisible target is a 0-row success")
    {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice_both, TABLE), 1);

    let err = core
        .execute_sql_in_session(&alice_public_only, &mut session, &sql)
        .expect_err("resend of a recorded operation_id must be rejected");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count_star(&core, &alice_both, TABLE), 1);

    match core
        .execute_sql_in_session(
            &alice_both,
            &mut session,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-visible'"),
        )
        .expect("visible target is deleted")
    {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice_both, TABLE), 0);
}
