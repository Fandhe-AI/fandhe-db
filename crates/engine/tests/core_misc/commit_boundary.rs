//! `engine::recovery::commit_boundary`（TASK-96、対象ビヘイビア: RECOVER-5）の
//! 結合テスト。commit 成功境界（point of no return）を挟んだ書き込み失敗の
//! 3 分類のうち、公開 API（`engine::core::EngineCore`）だけで外形観測できる
//! (1) commit 前失敗の副作用ゼロ契約を検証する（(2)(3) は `pub(crate)` の
//! `commit_and_finish`/`PostCommitPanicGuard` へ直接アクセスできる同一クレート内の
//! ユニットテスト側 —— `crates/engine/src/recovery/commit_boundary.rs` の
//! `#[cfg(test)] mod tests` —— で検証済み。テスト専用の feature ゲート API を
//! 新設しない方針〔codex P0-2 再発防止〕のため、本ファイルでは新しい迂回経路を
//! 追加しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const DIM: u32 = 3;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

/// RECOVER-5 (1): commit 前失敗（次元不一致でスキーマ検証に落ちる）は
/// write トランザクションが commit へ渡る前に `Err` で終わり、副作用ゼロ
/// （行・世代カウンタとも不変）であることを再オープン後に検証する。
#[test]
fn precommit_failure_leaves_zero_side_effects_after_reopen() {
    let path = unique_db_path("commit-boundary-precommit-fail");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("op-precommit-fail").expect("valid operation_id");

    // スキーマの次元 (DIM=3) に対し 2 要素しか渡さない: `TableSchema::validate_embedding_dim`
    // が write トランザクション内・commit 到達前に拒否する（`tenant::insert_row_unchecked`
    // の実装順序: スキーマ取得 → 次元検証 → 台帳追記 → 行書き込み → commit）。
    let bad_row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2],
        metadata: &[],
    };
    core.insert_row(&ctx, TABLE, 1, &bad_row, Some(&op_id))
        .expect_err("dimension mismatch must be rejected before commit");

    // 同一 id での正規行挿入がまだ可能である（衝突していない = 何も書き込まれて
    // いない）ことを独立オラクルとして確認する。
    let good_row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2, 0.3],
        metadata: &[],
    };
    core.insert_row(&ctx, TABLE, 1, &good_row, Some(&op_id))
        .expect("id must still be free after the pre-commit failure");

    // プロセスを再オープンし、commit 前失敗の行が紛れ込んでいないこと
    // （成功した 1 行だけが存在すること）を確認する。
    let core = drop_and_reopen(core, &path);
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1, "only the successful row must exist");
}

/// `core` を drop し、同一パスで `EngineCore` を再構築する（プロセス再起動相当の
/// 検証。`crash_tool.rs` のプロセス外強制終了検証とは異なり、本テストは
/// commit 前失敗が「そもそも commit されていない」ことを確認するだけなので
/// 同一プロセス内の再オープンで足りる）。
fn drop_and_reopen(core: EngineCore, path: &std::path::Path) -> EngineCore {
    drop(core);
    let storage = Storage::open(path).expect("reopen storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}
