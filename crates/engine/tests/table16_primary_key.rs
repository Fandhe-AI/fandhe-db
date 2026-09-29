//! `PRIMARY KEY` 宣言構文（単一列・複合キー。TABLE-16・TASK-204、Issue #903）の
//! 結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-204・
//! `docs/spec/04-behavior/table-model.md` TABLE-16・
//! `docs/spec/04-behavior/rls.md` RLS-10 (c)・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_create_table.rs` と同じ流儀（`EngineCore::execute_sql_in_session`・
//! 実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`）で、
//! `CREATE TABLE` の列制約・表制約両形の `PRIMARY KEY` 構文、テナント内
//! 一意性制約の検査点（`constraint::enforce_unique_keys_in_txn`）を production
//! 経路（SQL 表層）から検証する。

use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
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

// --- 構文の受理（単一列・複合キー） -------------------------------------

#[test]
fn create_table_accepts_single_column_primary_key_via_column_constraint() {
    let (core, path) = new_core("pk-single-column-constraint");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
        )
        .expect("CREATE TABLE with a column-constraint PRIMARY KEY should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));

    let schema = core
        .execute_sql(&alice, "SELECT id FROM docs LIMIT 1")
        .map(|_| ())
        .is_ok();
    assert!(schema, "table must be queryable immediately");
}

#[test]
fn create_table_accepts_composite_primary_key_via_table_constraint() {
    let (core, path) = new_core("pk-composite-table-constraint");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4), tenant_code TEXT, region TEXT, PRIMARY KEY (tenant_code, region))",
        )
        .expect("CREATE TABLE with a composite PRIMARY KEY should succeed");
    assert!(matches!(outcome, SqlOutcome::CreateTable(_)));
}

#[test]
fn create_table_accepts_id_only_primary_key_as_a_no_op() {
    let (core, path) = new_core("pk-id-only-noop");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), body TEXT, PRIMARY KEY (id))",
    )
    .expect(
        "PRIMARY KEY (id) must be accepted as an explicit declaration of the implicit primary key",
    );

    // 主キー未宣言と完全に同じ挙動（`id` の物理キー衝突のみが検査される）で
    // あることを、同一 `id` の再挿入が `23505` になることで確認する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.5,0.6,0.7,0.8]', 'b') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate id must be rejected");
    assert_eq!(err.wire_code(), "23505");
    // 別 operation_id での id 衝突は行制約由来（Issue #1180・ERR-6）。
    assert_eq!(
        ClassifiedError::error_class(&err),
        ErrorClass::UniqueViolation
    );

    // 同一 operation_id・同一内容の再送は台帳照合が PK 検査より先に走り、
    // 台帳由来の DUPLICATE_OPERATION_ID になる（RECOVER-12 の判定根拠）。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
        )
        .expect_err("identical resend must be a ledger duplicate");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(
        ClassifiedError::error_class(&err),
        ErrorClass::DuplicateOperationId
    );
}

// --- 拒否系（構造検証段階。42601／42701／54000） -------------------------

#[test]
fn create_table_rejects_multiple_primary_key_declarations() {
    let (core, path) = new_core("pk-multiple-declarations");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT PRIMARY KEY, b TEXT PRIMARY KEY)",
        )
        .expect_err("two column-constraint PRIMARY KEY declarations must be rejected");
    assert_eq!(err.wire_code(), "42601");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs2 (a TEXT PRIMARY KEY, b TEXT, PRIMARY KEY (b))",
        )
        .expect_err("a column-constraint and a table-constraint PRIMARY KEY must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_empty_primary_key_column_list() {
    let (core, path) = new_core("pk-empty-list");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, PRIMARY KEY ())",
        )
        .expect_err("PRIMARY KEY () must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_primary_key_referencing_unknown_column() {
    let (core, path) = new_core("pk-unknown-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (a TEXT, PRIMARY KEY (missing))",
        )
        .expect_err("PRIMARY KEY referencing an unknown column must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_vector_column_as_primary_key() {
    let (core, path) = new_core("pk-vector-column");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (embedding VECTOR(4) PRIMARY KEY)",
        )
        .expect_err("VECTOR column must not be usable as a primary key");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_id_mixed_with_other_columns_in_primary_key() {
    let (core, path) = new_core("pk-id-mixed");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (code TEXT, PRIMARY KEY (id, code))",
        )
        .expect_err("mixing the implicit id column with other columns must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn create_table_rejects_duplicate_column_name_within_primary_key() {
    let (core, path) = new_core("pk-duplicate-in-list");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut session,
            "CREATE TABLE docs (code TEXT, PRIMARY KEY (code, code))",
        )
        .expect_err("duplicate column name within PRIMARY KEY must be rejected");
    assert_eq!(err.wire_code(), "42701");
}

// --- テナント内一意性制約（書き込み経路。23505） -------------------------

#[test]
fn insert_rejects_duplicate_primary_key_value_within_the_same_tenant() {
    let (core, path) = new_core("pk-write-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'dup') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");

    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'dup') USING OPERATION_ID 'op-2'",
        )
        .expect_err("duplicate primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 拒否された `operation_id` は台帳に残らない（内容を変えた
    // 再送は通常どおり成功する）。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'not-dup') USING OPERATION_ID 'op-2'",
    )
    .expect("retry with a fixed value under the same operation_id must succeed");
}

#[test]
fn insert_rejects_duplicate_composite_primary_key_value() {
    let (core, path) = new_core("pk-write-composite-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), a TEXT, b TEXT, PRIMARY KEY (a, b))",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, a, b) VALUES (1, '[0.1,0.2,0.3,0.4]', 'x', 'y') USING OPERATION_ID 'op-1'",
    )
    .expect("first insert should succeed");

    // 片方の列だけ一致する行は成功する。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, a, b) VALUES (2, '[0.1,0.2,0.3,0.4]', 'x', 'z') USING OPERATION_ID 'op-2'",
    )
    .expect("differing in one column must succeed");

    // 両方一致する行は拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, a, b) VALUES (3, '[0.1,0.2,0.3,0.4]', 'x', 'y') USING OPERATION_ID 'op-3'",
        )
        .expect_err("matching both columns must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn update_rejects_change_that_would_create_a_duplicate_primary_key_value() {
    let (core, path) = new_core("pk-write-update-duplicate");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'b') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 should succeed");

    // id=2 の code を 'a' へ変更しようとすると id=1 と衝突する。
    let err = core
        .execute_update_sql(
            &alice,
            "UPDATE docs SET code = 'a' WHERE id = 2 USING OPERATION_ID 'op-update'",
        )
        .expect_err("update that creates a duplicate primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 自己更新（値を変えない SET）は自己衝突しない。
    core.execute_update_sql(
        &alice,
        "UPDATE docs SET code = 'a' WHERE id = 1 USING OPERATION_ID 'op-self-update'",
    )
    .expect("updating a row to its own existing primary key value must succeed");
}

// --- テナント境界（RLS-9・RLS-10 (c)） -----------------------------------

#[test]
fn different_tenants_may_share_the_same_primary_key_value() {
    let (core, path) = new_core("pk-cross-tenant");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let bob = ctx("bob");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");

    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'shared') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant alice insert should succeed");
    core.execute_insert_sql(
        &bob,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'shared') USING OPERATION_ID 'op-b'",
    )
    .expect("tenant bob must be able to use the same primary key value as tenant alice");
}

// --- UPSERT（`tenant::upsert_typed_rows_unchecked`。`DO NOTHING` 行の
//     `written_ids` 除外） -----------------------------------------------

#[test]
fn upsert_do_nothing_row_is_excluded_from_primary_key_check_and_does_not_block_new_insert() {
    let (core, path) = new_core("pk-upsert-do-nothing-excluded");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed insert should succeed");

    // id=1 は物理キー衝突により `DO NOTHING`（`code` は書き込まれない）。
    // 同一 UPSERT 文の id=2 は新規行で、`code` は既存行と異なる値のため、
    // `DO NOTHING` 行が `written_ids` に誤って含まれていても含まれていなくても
    // 直接の衝突は起きないが、`written_ids` 除外が壊れて `DO NOTHING` 行の
    // デコードに失敗するような回帰があればここで検出できる。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.9,0.9,0.9,0.9]', 'ignored'), \
         (2, '[0.5,0.6,0.7,0.8]', 'b') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-1'",
    )
    .expect("upsert with one DO NOTHING row and one new row should succeed");
}

#[test]
fn upsert_do_nothing_row_value_still_participates_in_primary_key_check_via_existing_scan() {
    let (core, path) = new_core("pk-upsert-do-nothing-still-conflicts");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'dup') USING OPERATION_ID 'op-seed'",
    )
    .expect("seed insert should succeed");

    // id=1 は `DO NOTHING`（既存の code='dup' は変わらない）。id=2 は新規行で
    // `code='dup'` を書き込もうとする。id=1 が `written_ids` から除外されて
    // いても、テナント全行走査（`enforce_unique_keys_in_txn` の第 2 パス）が
    // 未変更の id=1 を拾い、id=2 との衝突を検出しなければならない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.9,0.9,0.9,0.9]', 'ignored'), \
             (2, '[0.5,0.6,0.7,0.8]', 'dup') \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'",
        )
        .expect_err("new row duplicating an untouched DO NOTHING row's value must be rejected");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn upsert_do_update_rows_that_would_collide_are_rejected() {
    let (core, path) = new_core("pk-upsert-do-update-collide");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code) VALUES (2, '[0.5,0.6,0.7,0.8]', 'b') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 should succeed");

    // 両行とも `DO UPDATE` で `code='dup'` へ更新しようとする。`DO UPDATE` は
    // `written_ids` に含まれるため、書き込んだ行同士の衝突として拒否される。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (id, embedding, code) VALUES (1, '[0.1,0.2,0.3,0.4]', 'dup'), \
             (2, '[0.5,0.6,0.7,0.8]', 'dup') \
             ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code \
             USING OPERATION_ID 'op-upsert-collide'",
        )
        .expect_err("two DO UPDATE rows converging on the same primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 拒否後も両行の `code` は元の値のまま。
    assert_eq!(
        id_code_pairs(&core, &alice, "docs"),
        vec![(1, "a".to_string()), (2, "b".to_string())]
    );
}

/// `id`・`code` 列を `id` 昇順で読み出す（`SELECT ... ORDER BY id` は
/// distance/hybrid 専用の `ORDER BY` しか受理しない広域取得構文のため、
/// `LIMIT` のみで走査してテスト側で `id` ソートする。SQL-15 参照）。
fn id_code_pairs(core: &EngineCore, ctx: &PolicyContext, table: &str) -> Vec<(u64, String)> {
    let result = core
        .execute_sql(ctx, &format!("SELECT id, code FROM {table} LIMIT 100"))
        .expect("scan should succeed");
    let mut pairs: Vec<(u64, String)> = result
        .rows
        .iter()
        .map(|row| {
            let code = match &row.cells[1] {
                engine::sql::exec::Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            };
            (row.id, code)
        })
        .collect();
    pairs.sort_by_key(|(id, _)| *id);
    pairs
}

// --- 述語つき UPDATE（`tenant::update_rows_where_unchecked`。複数一致行への
//     一括適用） ------------------------------------------------------------

#[test]
fn predicate_update_matching_multiple_rows_without_touching_primary_key_succeeds() {
    let (core, path) = new_core("pk-predicate-update-benign");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY, tag TEXT)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code, tag) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a', 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code, tag) VALUES (2, '[0.5,0.6,0.7,0.8]', 'b', 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code, tag) VALUES (3, '[0.9,0.9,0.9,0.9]', 'c', 'y') USING OPERATION_ID 'op-3'",
    )
    .expect("insert 3 should succeed");

    // `tag` は主キー対象外なので、複数一致行（id=1・id=2）への一括適用は
    // 一意性制約に触れず成功する。述語つき（`id = <n>` 以外の）`WHERE` は
    // `execute_sql_in_session` の predicate-form 実行経路が受理する
    // （`execute_update_sql` は単一行・`id` 等価形のみ。SQL-19 参照）。
    let mut dml_session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut dml_session,
            "UPDATE docs SET tag = 'z' WHERE tag = 'x' USING OPERATION_ID 'op-update-benign'",
        )
        .expect("predicate update touching multiple rows without primary key change must succeed");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 2),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
}

#[test]
fn predicate_update_matching_multiple_rows_that_would_collide_on_primary_key_is_rejected() {
    let (core, path) = new_core("pk-predicate-update-collide");
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(4), code TEXT PRIMARY KEY, tag TEXT)",
    )
    .expect("CREATE TABLE should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code, tag) VALUES (1, '[0.1,0.2,0.3,0.4]', 'a', 'x') USING OPERATION_ID 'op-1'",
    )
    .expect("insert 1 should succeed");
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (id, embedding, code, tag) VALUES (2, '[0.5,0.6,0.7,0.8]', 'b', 'x') USING OPERATION_ID 'op-2'",
    )
    .expect("insert 2 should succeed");

    // `tag = 'x'` は id=1・id=2 の 2 行に一致する。両方を `code='dup'` へ
    // 一括更新しようとするため、更新後の行同士が一意性制約に違反する。
    let mut dml_session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut dml_session,
            "UPDATE docs SET code = 'dup' WHERE tag = 'x' USING OPERATION_ID 'op-update-collide'",
        )
        .expect_err("predicate update converging multiple rows onto the same primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 拒否後も両行の `code` は元の値のまま。
    assert_eq!(
        id_code_pairs(&core, &alice, "docs"),
        vec![(1, "a".to_string()), (2, "b".to_string())]
    );
}

// --- ファイル形 `INSERT`（`tenant::replace_typed_rows_by_text_key`。
//     採番 id 範囲 `first_id..first_id + inserted` の再構築） ---------------

fn new_core_with_embedder(label: &str, dim: u32) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(dim).expect("valid dim")));
    (core, path)
}

#[test]
fn file_insert_rejects_primary_key_collision_across_generated_chunk_rows() {
    const DIM: u32 = 8;
    let (core, path) = new_core_with_embedder("pk-file-insert-collision", DIM);
    let _guard = CleanupGuard(path);
    let alice = ctx("alice");
    let mut session = granted_session();
    core.execute_sql_in_session(
        &alice,
        &mut session,
        &format!(
            "CREATE TABLE docs (embedding VECTOR({DIM}), path TEXT, body TEXT, lang TEXT PRIMARY KEY)"
        ),
    )
    .expect("CREATE TABLE should succeed");

    // 1 ファイル目: 1 チャンクだけ生成される短い本文で `lang='ja'` を書き込む。
    core.execute_insert_sql(
        &alice,
        "INSERT INTO docs (path, body, lang) VALUES ('docs/a.txt', 'hello world', 'ja') \
         USING OPERATION_ID 'op-file-a'",
    )
    .expect("first file insert should succeed");

    // 2 ファイル目: 別パスだが同じ `lang='ja'` を書き込もうとする。採番 id は
    // `first_id..first_id + inserted` として再構築された範囲で一意性制約
    // 検査へ渡される（`tenant::replace_typed_rows_by_text_key` ドキュメント
    // 参照）ため、`23505` として拒否されなければならない。
    let err = core
        .execute_insert_sql(
            &alice,
            "INSERT INTO docs (path, body, lang) VALUES ('docs/b.txt', 'goodbye world', 'ja') \
             USING OPERATION_ID 'op-file-b'",
        )
        .expect_err("second file with a duplicate primary key value must be rejected");
    assert_eq!(err.wire_code(), "23505");

    // 副作用ゼロ: 拒否された 2 ファイル目のパスは検索できない。
    let result = core
        .execute_sql(&alice, "SELECT path FROM docs LIMIT 100")
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
}
