//! 永続一意索引（TABLE-16・TASK-204、Issue #1070）の結合テスト。
//!
//! `unique_constraint.rs`・`table16_primary_key.rs` は主キー・UNIQUE 制約の
//! 検査そのもの（衝突検出・NULLS DISTINCT・テナント境界）を検証済みであり、
//! 新しい索引実装でも無変更のまま通ることを既に確認済み（実装の置き換え前後
//! で契約が変わっていないことの証拠）。本ファイルはそれらではカバーされない、
//! 索引実装固有のシナリオ——削除・TRUNCATE 後の後片付け（衛生措置）と
//! stale エントリの読み戻し判定、`DROP TABLE` 後の索引非残留、既存 DB
//! （索引テーブル未作成）からの遅延バックフィル——を検証する。
//!
//! `unique_constraint.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore::execute_sql_in_session` 経由の
//! production 経路）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn core_from_path(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("reopen storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

fn granted_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn count_rows(core: &EngineCore, ctx: &PolicyContext) -> usize {
    core.execute_sql(ctx, "SELECT id FROM docs LIMIT 100")
        .expect("scan should succeed")
        .rows
        .len()
}

/// DELETE で削除した行の一意キー値は、別の commit された文で再利用できる
/// （索引の後片付け〔[`crate::constraint::unique_index::forget_rows_in_txn`]
/// 相当〕が効いていること、または stale エントリの読み戻し判定が正しく
/// 動くことのいずれかによって成立する。§モジュールドキュメント参照）。
#[test]
fn deleted_row_key_can_be_reused_by_a_later_committed_insert() {
    let (core, path) = new_core("uniq-index-delete-reuse");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert");

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-del'",
    )
    .expect("delete");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("re-inserting a value freed by a committed DELETE must succeed");

    // 削除されていない別の値との衝突は引き続き検出される。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (3, 'x') USING OPERATION_ID 'op-3'",
    )
    .expect_err("the value is now held by id=2 and must still be enforced");

    assert_eq!(count_rows(&core, &alice), 1);
}

/// committed TRUNCATE の後は、テナント全体の一意キー値が再利用できる
/// （[`crate::constraint::unique_index::clear_tenant_in_txn`] 相当）。
#[test]
fn truncated_table_keys_can_be_reused_by_a_later_committed_insert() {
    let (core, path) = new_core("uniq-index-truncate-reuse");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert");

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-trunc'",
    )
    .expect("truncate");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("re-inserting a value after a committed TRUNCATE must succeed");

    assert_eq!(count_rows(&core, &alice), 1);
}

/// TRUNCATE はテナント範囲だけを掃除し、他テナントの索引には触れない。
#[test]
fn truncate_does_not_affect_another_tenants_unique_keys() {
    let (core, path) = new_core("uniq-index-truncate-tenant-scope");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-bob-1'",
    )
    .expect("bob insert");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'y') USING OPERATION_ID 'op-alice-1'",
    )
    .expect("alice insert");

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-trunc'",
    )
    .expect("alice truncate");

    // alice の TRUNCATE は bob の索引エントリに影響しない: bob は同じ値を
    // 再度使おうとすると引き続き衝突する。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-bob-2'",
    )
    .expect_err("bob's own unique key must still be enforced after alice's truncate");

    assert_eq!(count_rows(&core, &bob), 1);
}

/// `DROP TABLE` の後、同名テーブルを再作成すると索引は空の状態から始まる
/// （旧索引のマーカー・エントリが残留しない。`Storage::drop_table` が行ストア
/// と同一 txn で `user_uniq/{table}` も削除する契約）。
#[test]
fn drop_table_and_recreate_does_not_leak_the_old_unique_index() {
    let (core, path) = new_core("uniq-index-drop-recreate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert");

    core.execute_sql_in_session(&alice, &mut session, "DROP TABLE docs")
        .expect("drop table");
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("recreate table");

    // 旧テーブルで使っていた値は、新しいテーブルでは初回の値として問題なく
    // 使える（旧索引が残っていれば誤って衝突判定される）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("the recreated table must start with an empty unique index");

    assert_eq!(count_rows(&core, &alice), 1);
}

/// `ALTER TABLE ... ADD UNIQUE` は索引を無効化し、以降の書き込みで
/// テナントごとに遅延再構築される。既存の重複しない行に対しては、
/// 制約追加後も一意性が正しく検査され続ける。
#[test]
fn add_unique_constraint_invalidates_index_and_lazily_rebuilds_on_next_write() {
    let (core, path) = new_core("uniq-index-add-constraint-rebuild");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert before constraint");
    drop(core);

    // `alter_table_add_unique_constraint`（Rust API）は `EngineCore` を経由
    // しない `Storage` 直叩き DDL のため、`unique_constraint.rs` と同じ流儀で
    // 一旦 `core` を drop してから同じファイルを開き直す。
    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect("add unique constraint (no existing duplicates)");
    drop(storage);
    let core = core_from_path(&path);

    // 制約追加後の書き込みで、既存行 id=1 の値 'x' との衝突が検査される
    // （遅延バックフィルにより id=1 が索引へ反映されているはず）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect_err("existing row's value must be picked up by the lazily-rebuilt index");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("a genuinely distinct value must still be accepted");

    assert_eq!(count_rows(&core, &alice), 2);
}

/// `UPSERT ... ON CONFLICT (a) DO UPDATE`（UNIQUE 列を対象にした ON CONFLICT。
/// base（main）取り込みマージで統合された Issue #1074・#1134 の経路）が、
/// 永続一意索引を正しく維持し続けることの回帰。`DO UPDATE` で id=1 の値が
/// 'x' → 'y' に変わった後、'x' は別行へ再利用でき、'y' は既存行に対して
/// 引き続き検査される（索引の後片付け漏れがあれば、いずれかが誤って判定
/// される）。
#[test]
fn on_conflict_do_update_targeting_a_unique_column_keeps_the_index_consistent() {
    let (core, path) = new_core("uniq-index-on-conflict-do-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'y') \
         ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert'",
    )
    .expect("upsert that changes the unique column via DO UPDATE");

    // 'x' は id=1 が手放したはずなので、別行が新たに使える。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("the value freed by the upsert's DO UPDATE must be reusable");

    // 'y' は id=1 が現に保持しているので、別行からは引き続き衝突する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (3, 'y') USING OPERATION_ID 'op-3'",
    )
    .expect_err("the value now held by id=1 via DO UPDATE must still be enforced");

    assert_eq!(count_rows(&core, &alice), 2);
}

/// UPDATE で一意キー列が全て NULL になった行は、NULLS DISTINCT により以後の
/// 検査対象から外れるが、更新前に保持していた索引エントリ（旧キー）自体も
/// 後片付けされなければならない（`check_and_update` が `written_keys`
/// 〔新しい正引きキー集合〕の空チェックで早期 return すると、段 3 の
/// 旧エントリ削除に到達せず stale な正引きエントリが残り続ける。Codex
/// レビュー指摘・PR #1123）。stale なエントリが残っていると、後続の
/// 別行 INSERT が旧キー値を使った時点で誤って `23505` を返す。
///
/// SQL の `UPDATE ... SET <col> = NULL` は許可リスト外（`NULL` リテラルは
/// `CASE`／`COALESCE`／`NULLIF` の引数以外の式位置では受理しない）ため、
/// 行全体置換 UPDATE（[`RowInput`] 経由。`update_row_without_vector_column.rs`
/// と同じ流儀）で一意キー列を NULL にする。
#[test]
fn key_freed_by_updating_it_to_null_can_be_reused_by_a_later_committed_insert() {
    let (core, path) = new_core("uniq-index-update-to-null-reuse");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert");
    drop(core);

    // `docs` の実スキーマ（`CREATE TABLE` が確定した物理列レイアウト）をそのまま
    // 読み出し、行全体置換 UPDATE のメタデータエンコードに使う（手書きスキーマとの
    // 乖離リスクを避ける。`add_unique_constraint_invalidates_index_and_lazily_
    // rebuilds_on_next_write` と同じ「一旦 core を drop して Storage を直接叩く」
    // 流儀）。
    let storage = Storage::open(&path).expect("reopen storage");
    let schema = storage.get_table_schema("docs").expect("read back schema");
    let null_metadata =
        engine::row_codec::encode_scalar_columns(&schema, &[Value::Null]).expect("encode NULL");
    engine::tenant::update_row(
        &storage,
        "docs",
        &alice,
        1,
        &RowInput {
            tenant_id: "alice",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &null_metadata,
        },
        &OperationId::parse("op-update-null").expect("valid operation_id"),
    )
    .expect("update the unique column to NULL");
    drop(storage);
    let core = core_from_path(&path);

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect(
        "a value freed by updating the sole holder's unique column to NULL \
         must be reusable (stale forward/reverse entries must not linger)",
    );

    assert_eq!(count_rows(&core, &alice), 2);
}
