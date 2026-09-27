//! UNIQUE 制約（TABLE-16・TASK-204、Issue #905）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-204・`docs/spec/04-behavior/data-model.md`
//! TABLE-16・TABLE-12・`docs/spec/04-behavior/rls.md` RLS-9・RLS-10 (c)。
//!
//! `sql_create_table.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`）。SQL 表層（`CREATE TABLE ... UNIQUE`・
//! `INSERT`・`UPDATE`・`UPSERT`・明示トランザクション）経由の検証を主とし、
//! `Storage::alter_table_add_unique_constraint` は最小限の確認に留める。
//! 一意性検査は主キー（Issue #903）と共有する単一の検査点
//! `constraint::enforce_unique_keys_in_txn` が担う。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::Storage;

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

// --- CREATE TABLE 構文（列制約・表制約） -------------------------------

#[test]
fn create_table_accepts_column_level_and_table_level_unique() {
    let (core, path) = new_core("uniq-create-ok");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("CREATE TABLE with column and table UNIQUE constraints must succeed");
}

#[test]
fn create_table_rejects_unique_on_undeclared_column() {
    let (core, path) = new_core("uniq-create-undeclared");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, UNIQUE (z))",
        )
        .expect_err("UNIQUE referencing an undeclared column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_unique_on_vector_column() {
    let (core, path) = new_core("uniq-create-vector");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) UNIQUE)",
        )
        .expect_err("UNIQUE on a VECTOR column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

// --- INSERT: 単一列・複合列・NULL・テナントスコープ ---------------------

#[test]
fn insert_rejects_duplicate_single_column_and_allows_distinct_values() {
    let (core, path) = new_core("uniq-insert-single");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (3, 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("distinct value must succeed");
}

#[test]
fn insert_allows_multiple_null_rows_for_unique_column() {
    let (core, path) = new_core("uniq-insert-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (1) USING OPERATION_ID 'op-1'",
    )
    .expect("first NULL row must succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (2) USING OPERATION_ID 'op-2'",
    )
    .expect("second NULL row must also succeed (NULLS DISTINCT)");
}

#[test]
fn insert_composite_unique_requires_all_columns_to_match() {
    let (core, path) = new_core("uniq-insert-composite");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 片方だけ一致では違反にならない。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (2, 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("partial match must succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (3, 'x', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("full match must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn insert_uniqueness_is_scoped_per_tenant() {
    let (core, path) = new_core("uniq-insert-tenant-scope");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-alice'",
    )
    .expect("alice insert must succeed");
    // 他テナントが同じ値を保持していても成功する（RLS-9）。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-bob'",
    )
    .expect("bob insert of the same value under a different tenant must succeed");
}

#[test]
fn insert_uniqueness_includes_private_rows_not_just_visible_ones() {
    // 母集合はテナント所有の全行（Public/Private を問わない）であり、可視
    // スナップショットではないことを固定する。書き込みセッション自身は常に
    // 自分の書いた行を書けるため、Private 可視性を持たない別コンテキストで
    // 同じテナントとして重複挿入を試みて確認する。
    let (core, path) = new_core("uniq-insert-private-scope");
    let _guard = CleanupGuard(path);
    let alice_all =
        PolicyContext::with_visibilities("alice", [engine::storage::Visibility::Private])
            .expect("valid tenant");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice_all,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice_all,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert (private) must succeed");
    let err = core
        .execute_insert_sql(
            &alice_all,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate against a private row must still be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- バッチ内重複・台帳優先 ---------------------------------------------

#[test]
fn multi_row_insert_batch_rejects_internal_duplicate_with_no_partial_effect() {
    let (core, path) = new_core("uniq-insert-batch");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x'), (2, 'x') USING OPERATION_ID 'op-batch'",
        )
        .expect_err("batch-internal duplicate must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: id=1 も反映されていない。
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

#[test]
fn ledger_duplicate_operation_id_takes_priority_over_unique_violation() {
    // 同一 operation_id・同一内容の再送は、値そのものが重複していても
    // 台帳由来の 23505（DuplicateOperationId）として確定済み処理の再送を示す
    // 契約を維持する（TASK-101・RECOVER-10 の優先順位。UNIQUE 制約導入後も
    // 台帳照合が一意性検査より先に走ることを固定する）。
    let (core, path) = new_core("uniq-ledger-priority");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    // 同一 operation_id・同一内容の再送は台帳由来の 23505
    // （`DuplicateOperationId`。commit 済み確定の根拠）として拒否される。
    // 台帳照合が一意性検査より先に走るため、値そのものが UNIQUE 制約と
    // 衝突していても `UniqueConstraintViolation` 経由の別分類には化けない
    // （両者は現行アーキテクチャでは同一 wire_code `23505` を共有する）。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
        )
        .expect_err("identical resend must be classified as a ledger duplicate (23505)");
    assert_eq!(err.wire_code(), "23505");
}

// --- UPDATE / UPSERT ----------------------------------------------------

#[test]
fn update_rejects_conflicting_value_but_allows_self_assignment() {
    let (core, path) = new_core("uniq-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET a = 'x' WHERE id = 2 USING OPERATION_ID 'op-upd-1'",
        )
        .expect_err("updating to another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 自身の現在値へ再度 SET するのは成功する（自己比較を除外する）。
    core.execute_update_sql(
        &alice,
        "UPDATE docs SET a = 'y' WHERE id = 2 USING OPERATION_ID 'op-upd-2'",
    )
    .expect("re-assigning the current value must succeed");
}

#[test]
fn upsert_do_update_rejects_conflicting_value() {
    let (core, path) = new_core("uniq-upsert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert-1'",
        )
        .expect_err("DO UPDATE that collides with another row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 新規挿入分岐での違反も拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (3, 'x') ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'",
        )
        .expect_err("DO NOTHING branch insert colliding on UNIQUE column must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

/// 複数行 UPSERT のバッチ内で、いずれも既存行と衝突しない「新規挿入」同士
/// （`DO UPDATE` 分岐を経由しない）が UNIQUE 列で衝突するケース（codex-review
/// 指摘・Issue #905 PR レビュー: 単一行版・`DO UPDATE` 版・INSERT バッチ内衝突版の
/// 結合テストは既存だったが、この組み合わせが欠けていた）。
/// 単一の検査点 `constraint::enforce_unique_keys_in_txn` は、書き込んだ行
/// （`written_ids`）同士のキー衝突を第 1 段で検出するため、新規挿入行同士の
/// バッチ内重複もここで拒否される。
#[test]
fn multi_row_upsert_rejects_internal_duplicate_among_new_insert_branches() {
    let (core, path) = new_core("uniq-upsert-batch-new-insert");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    // id=1・id=2 はいずれもテーブルに存在しない（新規挿入分岐）。同じ値 'z' を
    // 持つため、既存行との比較では衝突しないがバッチ内候補同士では衝突する。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'z'), (2, 'z') \
             ON CONFLICT (id) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'op-upsert-batch-1'",
        )
        .expect_err("new-insert branches colliding with each other must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: どちらの行も反映されていない。
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

// --- ON CONFLICT の UNIQUE 制約列への拡張（TABLE-16、Issue #1074） --------

/// 単一列 UNIQUE への `ON CONFLICT (a) DO NOTHING`: 既存行と衝突する場合は
/// 変更なし、衝突しない場合は新規挿入になる。
#[test]
fn upsert_on_conflict_unique_column_do_nothing() {
    let (core, path) = new_core("uniq-on-conflict-do-nothing");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT)",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'old') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    // 既存の `a='x'` と衝突 → 変更なし・新規挿入もされない。
    let outcome = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a, b) VALUES (99, 'x', 'new') \
             ON CONFLICT (a) DO NOTHING USING OPERATION_ID 'op-conflict'",
        )
        .expect("DO NOTHING on UNIQUE column conflict must succeed");
    assert_eq!(outcome.rows_affected, 0);
    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert_eq!(rows.len(), 1, "no new row must have been inserted");

    // 衝突しない場合は通常どおり新規挿入になる。
    let outcome = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a, b) VALUES (2, 'y', 'new') \
             ON CONFLICT (a) DO NOTHING USING OPERATION_ID 'op-no-conflict'",
        )
        .expect("non-conflicting insert must succeed");
    assert_eq!(outcome.rows_affected, 1);
}

/// 単一列 UNIQUE への `ON CONFLICT (a) DO UPDATE SET ...`: 衝突した既存行を
/// 更新し、`VALUES` 側の `id`（99）の行は作られない。
#[test]
fn upsert_on_conflict_unique_column_do_update_updates_existing_row() {
    let (core, path) = new_core("uniq-on-conflict-do-update");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT)",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'old') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let outcome = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a, b) VALUES (99, 'x', 'new') \
             ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b USING OPERATION_ID 'op-update'",
        )
        .expect("DO UPDATE on UNIQUE column conflict must succeed");
    assert_eq!(outcome.rows_affected, 1);

    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert_eq!(
        rows.len(),
        1,
        "the VALUES-side id (99) must not create a new row"
    );
    assert_eq!(
        rows[0].id, 1,
        "the existing row (id=1) must be the one updated"
    );
}

/// 複合 UNIQUE `(b, c)`: 対象列を `(c, b)`（宣言順と逆）で書いても同じ制約に
/// 解決され、衝突判定が働く。
#[test]
fn upsert_on_conflict_composite_unique_target_order_independent() {
    let (core, path) = new_core("uniq-on-conflict-composite");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (b TEXT, c TEXT, UNIQUE (b, c))",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (1, 'x', 'y') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    let outcome = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (99, 'x', 'y') \
             ON CONFLICT (c, b) DO NOTHING USING OPERATION_ID 'op-reversed'",
        )
        .expect("target written in reverse declaration order must still resolve");
    assert_eq!(outcome.rows_affected, 0);

    // `(b)` だけの部分一致は制約全体と一致しないため `42601`。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (2, 'x', 'z') \
             ON CONFLICT (b) DO NOTHING USING OPERATION_ID 'op-partial'",
        )
        .expect_err("partial composite target must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// NULL を含む UNIQUE 対象キーは NULLS DISTINCT のため衝突しない（新規挿入）。
#[test]
fn upsert_on_conflict_unique_column_null_never_conflicts() {
    let (core, path) = new_core("uniq-on-conflict-null");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id) VALUES (1) USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row with a=NULL (omitted, nullable)");

    let outcome = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id) VALUES (2) ON CONFLICT (a) DO NOTHING \
             USING OPERATION_ID 'op-null-2'",
        )
        .expect("NULL target key must never conflict");
    assert_eq!(outcome.rows_affected, 1);
}

/// テナント境界（受け入れ基準 2）: 同じテナントが所有する**不可視**行
/// （`Private`）とも UNIQUE 対象で衝突する（可視性フィルタを経由しない
/// 母集合。RLS-9・RLS-10 (c)）。
#[test]
fn upsert_on_conflict_unique_column_conflicts_with_invisible_own_row() {
    let (core, path) = new_core("uniq-on-conflict-invisible");
    let _guard = CleanupGuard(path);
    let alice_full = ctx("alice");
    let alice_public_only =
        PolicyContext::with_visibilities("alice", [engine::storage::Visibility::Public])
            .expect("valid tenant");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice_full,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");
    core.execute_sql_in_session(
        &alice_full,
        &mut SessionState::default(),
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-private'",
    )
    .expect("insert as Private (default insert_visibility)");

    // `alice_public_only` からは `id=1` は不可視だが、UNIQUE 対象の衝突判定は
    // 可視性を問わずテナント所有の全行を母集合にするため、`DO NOTHING` は
    // 既存の不可視行に吸収される（新規挿入されない）。
    let outcome = core
        .execute_insert_sql(
            &alice_public_only,
            "INSERT INTO docs (id, a) VALUES (2, 'x') ON CONFLICT (a) DO NOTHING \
             USING OPERATION_ID 'op-invisible-conflict'",
        )
        .expect("conflict with an invisible own row must succeed as DO NOTHING");
    assert_eq!(outcome.rows_affected, 0);
}

/// テナント境界: 他テナントが同じ値を持っていても、UNIQUE 対象の母集合は
/// 自テナントの行に限られるため衝突せず新規挿入になる。
#[test]
fn upsert_on_conflict_unique_column_does_not_conflict_across_tenants() {
    let (core, path) = new_core("uniq-on-conflict-cross-tenant");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-alice'",
    )
    .expect("alice seed row");

    let outcome = core
        .execute_insert_sql(
            &bob,
            "INSERT INTO docs (id, a) VALUES (1, 'x') ON CONFLICT (a) DO NOTHING \
             USING OPERATION_ID 'op-bob'",
        )
        .expect("other tenant's same value must not conflict");
    assert_eq!(outcome.rows_affected, 1);
}

/// 対象がどの UNIQUE 制約とも一致しない場合は `42601`。
#[test]
fn upsert_on_conflict_target_not_matching_any_unique_constraint_is_rejected() {
    let (core, path) = new_core("uniq-on-conflict-no-match");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT, b TEXT UNIQUE)",
    )
    .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'y') \
             ON CONFLICT (a) DO NOTHING USING OPERATION_ID 'op-no-match'",
        )
        .expect_err("target not matching any UNIQUE constraint must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// バッチ内の対象キー重複（NULL 以外）は束縛時に `22000` で拒否され、副作用は
/// ゼロ（台帳・行のいずれも変更されない）。
#[test]
fn upsert_on_conflict_unique_column_rejects_duplicate_target_key_within_batch() {
    let (core, path) = new_core("uniq-on-conflict-batch-dup");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'x'), (2, 'x') \
             ON CONFLICT (a) DO NOTHING USING OPERATION_ID 'op-batch-dup'",
        )
        .expect_err("duplicate target key within the same batch must be rejected");
    assert_eq!(err.wire_code(), "22000");

    let rows = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 10")
        .expect("scan should succeed")
        .rows;
    assert!(rows.is_empty(), "no row must have been written");
}

/// UNIQUE 対象の新規挿入分岐で `VALUES` の `id` が同じテナントの既存 `id` と
/// 衝突した場合は通常の行キー衝突と同じ `23505`。
#[test]
fn upsert_on_conflict_unique_column_new_insert_branch_id_conflict() {
    let (core, path) = new_core("uniq-on-conflict-insert-id-conflict");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed row");

    // `a='y'` は衝突しない（非衝突・新規挿入分岐）が、`id=1` は既存行と衝突する。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (1, 'y') ON CONFLICT (a) DO NOTHING \
             USING OPERATION_ID 'op-insert-id-conflict'",
        )
        .expect_err("new-insert branch id collision must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

// --- ALTER TABLE ADD UNIQUE（Rust API）・DROP COLUMN 依存検査 ----------

#[test]
fn alter_table_add_unique_constraint_rejects_existing_duplicates_with_no_side_effect() {
    let (core, path) = new_core("uniq-alter-add-reject");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 (duplicate value, no constraint yet)");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect_err("adding UNIQUE over a column with existing duplicates must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::UniqueConstraintViolation
    ));
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert!(
        schema.unique_constraints().is_empty(),
        "the constraint must not have been persisted"
    );
}

#[test]
fn alter_table_add_unique_constraint_succeeds_when_no_duplicates_and_is_enforced_afterward() {
    let (core, path) = new_core("uniq-alter-add-ok");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT)")
        .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect("adding UNIQUE with no existing duplicates must succeed");
    let schema = storage.get_table_schema("docs").expect("schema must exist");
    assert_eq!(schema.unique_constraints().len(), 1);

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("the newly added constraint must now be enforced");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn alter_table_drop_column_rejects_when_column_is_used_by_a_unique_constraint() {
    let (core, path) = new_core("uniq-drop-column-dependent");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT UNIQUE, b TEXT)",
    )
    .expect("create table");
    drop(core);

    let storage = Storage::open(&path).expect("reopen storage");
    let err = storage
        .alter_table_drop_column("docs", "a")
        .expect_err("dropping a column referenced by a UNIQUE constraint must be rejected");
    assert!(matches!(
        err,
        engine::catalog::CatalogError::DependentObjectsStillExist(_)
    ));
}

// --- 主キーとの併用 -------------------------------------------------------

/// `PRIMARY KEY`（Issue #903）と UNIQUE 制約を同一テーブルで併用した場合も、
/// 単一の検査点がそれぞれのキーを独立に判定する（主キー列は NULL 不可、UNIQUE
/// 列は NULLS DISTINCT）。
#[test]
fn primary_key_and_unique_constraints_are_enforced_independently() {
    let (core, path) = new_core("uniq-with-pk");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (code TEXT PRIMARY KEY, a TEXT UNIQUE)",
    )
    .expect("create table");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code, a) VALUES (1, 'k1', 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert must succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, code, a) VALUES (2, 'k2', 'x') USING OPERATION_ID 'op-2'",
        )
        .expect_err("UNIQUE violation must be rejected");
    assert_eq!(err.wire_code(), "23505");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, code, a) VALUES (3, 'k1', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("PRIMARY KEY violation must be rejected");
    assert_eq!(err.wire_code(), "23505");
    // UNIQUE 列の NULL は何行でも共存できる。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code) VALUES (4, 'k4') USING OPERATION_ID 'op-4'",
    )
    .expect("NULL in UNIQUE column must be accepted");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, code) VALUES (5, 'k5') USING OPERATION_ID 'op-5'",
    )
    .expect("second NULL in UNIQUE column must be accepted");
}

// --- 明示トランザクション（SQL-31・TASK-221） -----------------------------

fn count_rows(core: &EngineCore, caller: &PolicyContext) -> usize {
    core.execute_sql(caller, "SELECT id FROM docs LIMIT 100")
        .expect("scan should succeed")
        .rows
        .len()
}

/// 明示トランザクション内の書き込みは共有 write トランザクションに未 commit の
/// まま積まれる。一意性検査は同じ write トランザクション内で走査するため、同一
/// トランザクション内の先行文が書いた未 commit 行との重複も見落とさず `23505`
/// で拒否し、トランザクションは `Failed` へ遷移する（ROLLBACK 後は何も残らない）。
#[test]
fn explicit_transaction_detects_duplicate_against_uncommitted_row_in_same_transaction() {
    let (core, path) = new_core("uniq-txn-dup");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl_session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    assert_eq!(
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
            .expect("begin"),
        SqlOutcome::Begin
    );
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-t1'",
    )
    .expect("first insert inside the transaction must succeed");
    let err = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "INSERT INTO docs (id, a) VALUES (2, 'x') USING OPERATION_ID 'op-t2'",
        )
        .expect_err("duplicate against an uncommitted row of the same transaction");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(
        count_rows(&core, &alice),
        0,
        "nothing must remain after ROLLBACK"
    );
}

/// 明示トランザクション内の distinct な値は受理され、COMMIT 後は autocommit の
/// 書き込みに対しても一意性が効く。また同一トランザクション内で先に TRUNCATE
/// した（未 commit の削除）行の値は、後続 INSERT の衝突相手にならない。
#[test]
fn explicit_transaction_commits_distinct_values_and_sees_uncommitted_truncate() {
    let (core, path) = new_core("uniq-txn-commit");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut ddl_session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut ddl_session,
        "CREATE TABLE docs (a TEXT UNIQUE)",
    )
    .expect("create table");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (1, 'x') USING OPERATION_ID 'op-c1'",
    )
    .expect("insert x");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (2, 'y') USING OPERATION_ID 'op-c2'",
    )
    .expect("insert y");
    assert_eq!(
        core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
            .expect("commit"),
        SqlOutcome::Commit
    );
    assert_eq!(count_rows(&core, &alice), 2);

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (3, 'x') USING OPERATION_ID 'op-c3'",
        )
        .expect_err("committed values must be enforced for later autocommit writes");
    assert_eq!(err.wire_code(), "23505");

    // 同一トランザクション内で TRUNCATE してから同じ値を入れ直すのは成功する
    // （未 commit の削除も同じ write トランザクションの走査に反映される）。
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-trunc'",
    )
    .expect("truncate inside the transaction");
    core.execute_sql_in_txn(
        &alice,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, a) VALUES (4, 'x') USING OPERATION_ID 'op-c4'",
    )
    .expect("re-inserting a value removed by an uncommitted TRUNCATE must succeed");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count_rows(&core, &alice), 1);
}

/// 一意性違反の応答は、他テナントが同じ値を持つかどうかに依存しない（他テナント
/// の値は母集合に含まれず、違反時の文言も値・テナントを含まない固定文言）。
#[test]
fn unique_violation_response_does_not_depend_on_other_tenants_rows() {
    let (core, path) = new_core("uniq-no-leak");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(&alice, &mut session, "CREATE TABLE docs (a TEXT UNIQUE)")
        .expect("create table");

    // bob だけが 'secret' を保持している状態で、alice の書き込みは成功する。
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, a) VALUES (1, 'secret') USING OPERATION_ID 'op-b1'",
    )
    .expect("bob insert");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a) VALUES (1, 'secret') USING OPERATION_ID 'op-a1'",
    )
    .expect("alice insert of a value only bob holds must succeed");

    // alice 自身の重複による違反の文言には値もテナント名も含まれない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, a) VALUES (2, 'secret') USING OPERATION_ID 'op-a2'",
        )
        .expect_err("alice's own duplicate must be rejected");
    assert_eq!(err.wire_code(), "23505");
    let message = err.to_string();
    assert!(!message.contains("secret"), "{message}");
    assert!(!message.contains("bob"), "{message}");
    assert!(!message.contains("alice"), "{message}");
}

// --- ALTER TABLE ADD COLUMN（Issue #900）との併用 --------------------------

/// `ALTER TABLE ... ADD COLUMN` は既存の UNIQUE 制約を保持したまま列を追加し
/// （カタログ v6 の往復）、追加後も既存制約の構成列は名前で解決されるため、
/// 列の追加・削除（墓標）で物理位置がずれても検査対象の列を取り違えない。
/// ADD COLUMN 自体は列制約を受理しないため、追加列に UNIQUE は付かない。
#[test]
fn add_column_preserves_existing_unique_constraint_and_column_resolution() {
    let (core, path) = new_core("uniq-add-column");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (a TEXT, b TEXT UNIQUE)",
    )
    .expect("create table");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, a, b) VALUES (1, 'x', 'k') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1");
    drop(core);

    // 先頭列 `a` を削除して墓標を作り、`b` の論理位置をずらす。
    let storage = Storage::open(&path).expect("reopen storage");
    storage
        .alter_table_drop_column("docs", "a")
        .expect("drop a non-constrained column");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    core.execute_sql_in_session(&alice, &mut session, "ALTER TABLE docs ADD COLUMN c TEXT")
        .expect("add column");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "ALTER TABLE docs ADD COLUMN d TEXT UNIQUE",
        )
        .expect_err("ADD COLUMN must not accept a UNIQUE column constraint");
    assert_eq!(err.wire_code(), "42601");

    // 既存制約（`b`）は ADD COLUMN 後も有効で、追加列 `c` を取り違えない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, b, c) VALUES (2, 'k', 'z') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate on the existing UNIQUE column must still be rejected");
    assert_eq!(err.wire_code(), "23505");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (3, 'm', 'k') USING OPERATION_ID 'op-3'",
    )
    .expect("a value equal to b's in the unconstrained added column must be accepted");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, b, c) VALUES (4, 'n', 'k') USING OPERATION_ID 'op-4'",
    )
    .expect("the added column carries no UNIQUE constraint");
}
