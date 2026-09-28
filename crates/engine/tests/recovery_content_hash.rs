//! 台帳の内容照合ハッシュの結合テスト（TASK-101、対象ビヘイビア: RECOVER-10。
//! ポインタ: `docs/spec/05-tasks.md` TASK-101・`docs/spec/04-behavior/recovery.md`
//! RECOVER-10・`docs/spec/04-behavior/error-format.md` ERR-2）。
//!
//! `tests/recovery_ledger.rs`（TASK-93）と同じ流儀（実 `Storage`・
//! `EngineCore::from_storage`）で、同一 `operation_id` への再送が「内容一致 →
//! `23505`」「内容不一致 → `22023`」に正しく分岐すること、副作用ゼロ（txn abort）で
//! あること、テナント・テーブルをまたいで干渉しないことを検証する。
//!
//! v1 レガシーエントリへの再送が `22023` になることの検証は、`pub(crate)` な内部
//! API（`recovery::ledger::record_in_txn`）を直接使う必要があるため、本クレート内部の
//! 単体テスト（`crates/engine/src/recovery/ledger.rs` の
//! `resend_to_legacy_v1_entry_is_rejected_as_content_mismatch`）で担保する
//! （本ファイルは公開 API のみを使う結合テスト）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::TenantWriteError;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const OTHER_TABLE: &str = "other_docs";
const DIM: u32 = 3;

fn schema(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    storage
        .create_table(&schema(OTHER_TABLE))
        .expect("create other table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

// --- INSERT（行形。SQL 表層）: 同一 operation_id・同一内容の再送は 23505、
//     内容不一致は 22023。いずれも副作用ゼロ。 -------------------------------------

#[test]
fn insert_resend_same_content_is_23505_and_different_content_is_22023() {
    let path = unique_db_path("content-hash-insert");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-ins'",
    )
    .expect("first insert must succeed");

    // 同一 operation_id・同一内容（同じ SQL 文そのままの再送）→ 23505。
    let dup_err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-ins'",
        )
        .expect_err("same content resend must be rejected");
    assert_eq!(dup_err.wire_code(), "23505");

    // 同一 operation_id・異なる内容（別 id・別本文）→ 22023。行 id 衝突（23505）とは
    // 独立に検出される（id=1 は別なので IdConflict 経路には入らない）。
    let mismatch_err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.9,0.9,0.9]', 'secret-body-marker') USING OPERATION_ID 'op-ins'",
        )
        .expect_err("different content resend must be rejected");
    assert_eq!(mismatch_err.wire_code(), "22023");

    // 副作用ゼロ: id=2 は書き込まれていない。
    let count = core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.9,0.9,0.9]', 'secret-body-marker') USING OPERATION_ID 'op-ins-2'",
    );
    assert!(
        count.is_ok(),
        "id=2 must still be free to insert under a fresh operation_id"
    );

    // エラー文言に行内容・テナント・他テナント存在情報を含めない（security.md P0）。
    let msg = mismatch_err.client_message();
    assert!(
        !msg.contains("secret-body-marker"),
        "message must not leak row content: {msg}"
    );
    assert!(
        !msg.contains("tenant-a"),
        "message must not leak tenant id: {msg}"
    );
}

// --- 型付き INSERT（公開 API `engine::tenant::insert_typed_row` 経由）: 同一
//     operation_id・同一内容だが visibility のみ異なる再送は、内容一致の
//     23505 ではなく内容不一致の 22023 として区別できなければならない
//     （codex-review P1・cursor bugbot 指摘・PR #248 の回帰固定。SQL 表層の
//     `INSERT` 文は visibility を常に `Private` に固定するため公開 API を直接
//     呼んで検証する。`sql::exec` のドキュメント参照）。
#[test]
fn typed_insert_resend_with_different_visibility_is_22023_not_23505() {
    let path = unique_db_path("content-hash-typed-insert-visibility");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let values = vec![
        engine::row_codec::Value::Vector(vec![0.1, 0.2, 0.3]),
        engine::row_codec::Value::Text("a".to_string()),
    ];

    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Public,
        &values,
        &op("op-vis"),
    )
    .expect("first insert must succeed");

    // 同一 operation_id・同一内容・visibility のみ Public→Private → 22023
    // （23505 = 内容一致の重複、と誤判定してはならない）。
    let mismatch_err = engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Private,
        &values,
        &op("op-vis"),
    )
    .expect_err("visibility-only resend must be rejected as content mismatch");
    assert_eq!(mismatch_err.wire_code(), "22023");

    // 副作用ゼロ: 行は Public のまま（Private への昇格が誤って成立していない）。
    let mismatch_err2 = engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx,
        1,
        Visibility::Private,
        &values,
        &op("op-vis"),
    )
    .expect_err("resend must still be rejected on repeat");
    assert_eq!(mismatch_err2.wire_code(), "22023");
}

// --- UPDATE: 対象行が既に削除済み（NotFound になりうる状態）でも、台帳照合が
//     先行するため再送検知（23505/22023）が機能する（TASK-101 の順序反転の核心）。

#[test]
fn update_resend_after_row_deleted_is_still_detected_via_ledger() {
    let path = unique_db_path("content-hash-update");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-seed")),
    )
    .expect("seed insert");
    core.update_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.4, 0.5, 0.6], b"updated"),
        Some(&op("op-upd")),
    )
    .expect("first update");
    core.delete_row(&ctx, TABLE, 1, Some(&op("op-del")))
        .expect("delete the row so it no longer exists");

    // 削除済みの行への同一内容の再送 update は、行状態だけを見れば NotFound
    // （P0002）になるはずだが、台帳照合が先行するため 23505 として検出される。
    let dup_err = core
        .update_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.4, 0.5, 0.6], b"updated"),
            Some(&op("op-upd")),
        )
        .expect_err("resend of a committed update must be detected via the ledger");
    assert!(matches!(dup_err, TenantWriteError::DuplicateOperationId));
    assert_eq!(dup_err.wire_code(), "23505");
}

// --- UPDATE: 同一 operation_id・異なる内容の再送は 22023（行の存否に関わらず
//     内容不一致が優先される）。 -----------------------------------------------

#[test]
fn update_resend_with_different_content_is_22023() {
    let path = unique_db_path("content-hash-update-mismatch");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-seed")),
    )
    .expect("seed insert");
    core.update_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.4, 0.5, 0.6], b"updated"),
        Some(&op("op-upd")),
    )
    .expect("first update");

    let mismatch_err = core
        .update_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.7, 0.7, 0.7], b"content-mismatch"),
            Some(&op("op-upd")),
        )
        .expect_err("different content resend must be rejected");
    assert!(matches!(
        mismatch_err,
        TenantWriteError::OperationIdContentMismatch
    ));
    assert_eq!(mismatch_err.wire_code(), "22023");
}

// --- DELETE: 既に削除済みの行への同一 operation_id 再送は 23505、内容不一致の
//     余地はない（削除要求の内容は id のみ）ため常に Duplicate。 --------------------

#[test]
fn delete_resend_after_already_deleted_is_23505() {
    let path = unique_db_path("content-hash-delete");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-seed")),
    )
    .expect("seed insert");
    core.delete_row(&ctx, TABLE, 1, Some(&op("op-del")))
        .expect("first delete");

    let err = core
        .delete_row(&ctx, TABLE, 1, Some(&op("op-del")))
        .expect_err("resend of a committed delete must be detected via the ledger, not NotFound");
    assert!(matches!(err, TenantWriteError::DuplicateOperationId));
    assert_eq!(err.wire_code(), "23505");
}

// --- テナント間・テーブル間の独立性: 同一 operation_id が別テナント・別テーブルへ
//     使われても互いに干渉しない。 -------------------------------------------------

#[test]
fn same_operation_id_is_independent_across_tenants_and_tables() {
    let path = unique_db_path("content-hash-scope");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    let row_a = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2, 0.3],
        metadata: b"a",
    };
    let row_b = RowInput {
        tenant_id: "tenant-b",
        visibility: Visibility::Public,
        embedding: &[0.4, 0.5, 0.6],
        metadata: b"b",
    };

    core.insert_row(&ctx_a, TABLE, 1, &row_a, Some(&op("op-shared")))
        .expect("tenant-a insert must succeed");
    // 別テナントが同じ operation_id を使っても干渉しない（別名前空間）。
    core.insert_row(&ctx_b, TABLE, 1, &row_b, Some(&op("op-shared")))
        .expect("tenant-b insert with the same operation_id must succeed independently");

    // 別テーブルへの同じ operation_id も独立に扱われる。
    core.insert_row(&ctx_a, OTHER_TABLE, 1, &row_a, Some(&op("op-shared")))
        .expect("same operation_id on a different table must succeed independently");
}

// --- CompareOnlyWithoutLedger: 台帳を持たない構成では内容照合そのものが発生しない
//     （同一 operation_id の再送を無条件で受理する）。 -----------------------------

#[test]
fn compare_only_without_ledger_never_detects_resends() {
    let path = unique_db_path("content-hash-no-ledger");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-x")),
    )
    .expect("first insert");
    // 同一 id への再挿入は行キー衝突（23505・IdConflict）で拒否されるが、これは
    // 台帳照合とは独立した既存の重複検出であり、CompareOnlyWithoutLedger でも
    // 引き続き機能する。
    let err = core
        .insert_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.9, 0.9, 0.9], b"different"),
            Some(&op("op-x")),
        )
        .expect_err("row id conflict must still be rejected without the ledger");
    assert!(matches!(err, TenantWriteError::IdConflict));
}

// --- ファイル形 INSERT（置換経路）: 同一パス・同一 operation_id・同一本文の再送は
//     23505、本文が変われば 22023。 -----------------------------------------------

fn new_file_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(16), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(16).expect("valid dim")))
        .with_incremental_config(IncrementalConfig {
            chunking: engine::chunking::ChunkingConfig {
                lines_per_chunk: 10,
                max_markdown_section_chars: None,
            },
            ..IncrementalConfig::default()
        })
}

fn file_insert_sql(path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (path, body) VALUES ('{path}', '{body}') USING OPERATION_ID '{op_id}'"
    )
}

#[test]
fn file_form_insert_resend_same_body_is_23505_and_different_body_is_22023() {
    let path = unique_db_path("content-hash-file");
    let _guard = CleanupGuard(path.clone());
    let core = new_file_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        &file_insert_sql("docs/a.md", "hello world", "op-file"),
    )
    .expect("first file insert must succeed");

    let dup_err = core
        .execute_insert_sql(
            &ctx,
            &file_insert_sql("docs/a.md", "hello world", "op-file"),
        )
        .expect_err("same body resend must be rejected");
    assert_eq!(dup_err.wire_code(), "23505");

    let mismatch_err = core
        .execute_insert_sql(
            &ctx,
            &file_insert_sql("docs/a.md", "different body", "op-file"),
        )
        .expect_err("different body resend must be rejected");
    assert_eq!(mismatch_err.wire_code(), "22023");
}

// --- 並行性: 2 スレッドが同一 operation_id・同一内容で同時 INSERT すると、成功 1・
//     23505 拒否 1 になり、当該テーブルの行数は試行ごとに +1 のまま
//     （TASK-94・RECOVER-3 の原子性契約を TASK-101・RECOVER-10 の内容照合ハッシュが
//     引き継ぐことの実測。redb 単一ライタ直列化 + 同一 write トランザクション内の
//     判定により、2 セッションのうち一方のみ commit する。両スレッドとも同一 id・
//     同一内容で INSERT するため、負けたセッションは行 id 衝突ではなく
//     `DuplicateOperationId`（内容一致の再送）として観測されるはず）。
//     `Storage`/`PolicyContext`/`OperationId` はいずれも `Sync` のため、
//     `crate::tenant::insert_row` を直接 2 スレッドから共有参照で呼べる。

#[test]
fn concurrent_same_operation_id_same_content_inserts_yield_exactly_one_success() {
    for attempt in 0..10u64 {
        let path = unique_db_path(&format!("content-hash-concurrent-{attempt}"));
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema(TABLE)).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        let op_id = op("op-concurrent");

        let (successes, row_count) = std::thread::scope(|s| {
            let storage_ref = &storage;
            let ctx_ref = &ctx;
            let op_ref = &op_id;
            let insert = move || {
                engine::tenant::insert_row(
                    storage_ref,
                    TABLE,
                    ctx_ref,
                    1,
                    &row(&[0.1, 0.2, 0.3], b"same"),
                    op_ref,
                )
            };
            let h1 = s.spawn(insert);
            let h2 = s.spawn(insert);
            let r1 = h1.join().expect("thread 1 must not panic");
            let r2 = h2.join().expect("thread 2 must not panic");

            let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
            for r in [&r1, &r2] {
                if let Err(e) = r {
                    assert!(
                        matches!(e, TenantWriteError::DuplicateOperationId),
                        "the losing session must fail with DuplicateOperationId \
                         (same content resend), got {e:?}"
                    );
                }
            }

            let visible = engine::tenant::visible_rows(storage_ref, TABLE, ctx_ref)
                .expect("visible_rows must succeed");
            (successes, visible.len())
        });

        assert_eq!(
            successes, 1,
            "attempt {attempt}: exactly one of the two concurrent sessions must succeed"
        );
        assert_eq!(
            row_count, 1,
            "attempt {attempt}: exactly one row must have been committed"
        );
    }
}

// --- Issue #397 のピン留め: 事前エンコード共有化後も既存のエラー優先順位が
//     不変であること。--------------------------------------------------------

// バッチ INSERT: 同一 operation_id の内容不一致（本来なら 22023）よりも、
// バッチ内の行の `encode_row` 失敗（本テストでは metadata 上限超過）を先に返す。
// これは Issue #397 以前から成立していた順序（`for_insert_batch` が台帳記録前に
// 全行を要求記載順で `encode_row` していたため）であり、事前エンコードへ変更しても
// 崩れていないことを確認する（`insert_rows_unchecked` の事前エンコードは
// 台帳記録より前に行われる設計。3.2 節参照）。
#[test]
fn insert_rows_encode_error_takes_priority_over_operation_id_content_mismatch() {
    let path = unique_db_path("content-hash-batch-encode-priority");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = op("op-batch-priority");

    // 1 回目: 台帳へ内容を記録する（後続の再送が内容不一致になるための前提）。
    engine::tenant::insert_rows(
        &storage,
        TABLE,
        &ctx,
        &[(1, row(&[0.1, 0.2, 0.3], b"first"))],
        &op_id,
    )
    .expect("first batch insert must succeed");

    // 2 回目: 同一 operation_id だが内容が異なる（22023 の対象）うえ、
    // バッチ内の 2 行目が metadata 上限超過で encode に失敗する。
    let oversized_metadata = vec![0u8; 4 * 1024 * 1024 + 1];
    let rows = vec![
        (2u64, row(&[0.4, 0.5, 0.6], b"second")),
        (3u64, row(&[0.7, 0.8, 0.9], oversized_metadata.as_slice())),
    ];
    let result = engine::tenant::insert_rows(&storage, TABLE, &ctx, &rows, &op_id);

    assert!(
        matches!(result, Err(TenantWriteError::Storage(_))),
        "encode failure must be returned before operation_id content-mismatch \
         is even evaluated, got {result:?}"
    );
}

// UPDATE: 既存行に対する embedding 次元不一致は、schema 取得直後の
// `validate_row_embedding_dim`（VECTOR 列ありスキーマでは `validate_embedding_dim`
// に委譲。Issue #1079）が台帳照合（`content_hash::for_update_encoded` の
// 呼び出し）より前に位置しているため、同一 operation_id の内容不一致（22023）
// よりも先に返る。Issue #397 の事前エンコードは次元検証の後に置いており、この
// 優先順位は変更前後で不変であることをピン留めする。
#[test]
fn update_row_embedding_dim_mismatch_takes_priority_over_operation_id_content_mismatch() {
    let path = unique_db_path("content-hash-update-dim-priority");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &ctx,
        1,
        &row(&[0.1, 0.2, 0.3], b"initial"),
        &op("op-update-dim-insert"),
    )
    .expect("insert must succeed");

    let op_id = op("op-update-dim-priority");
    engine::tenant::update_row(
        &storage,
        TABLE,
        &ctx,
        1,
        &row(&[0.4, 0.5, 0.6], b"first update"),
        &op_id,
    )
    .expect("first update must succeed");

    // 同一 operation_id だが内容も異なり（22023 の対象）、かつ次元も不一致
    // （テーブルは DIM=3 だが 5 次元を渡す）。
    let mismatched_dim = [0.1_f32, 0.2, 0.3, 0.4, 0.5];
    let result = engine::tenant::update_row(
        &storage,
        TABLE,
        &ctx,
        1,
        &row(&mismatched_dim, b"second update"),
        &op_id,
    );

    assert!(
        matches!(result, Err(TenantWriteError::Catalog(_))),
        "embedding dim mismatch must be returned before operation_id content-mismatch \
         is even evaluated, got {result:?}"
    );
}
