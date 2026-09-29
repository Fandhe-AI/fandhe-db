//! `operation_id` 内容不一致エラー（`22023`）の wire_code 契約の結合テスト
//! （TASK-154、対象ビヘイビア: ERR-3。ポインタ: `docs/spec/05-tasks.md` TASK-154・
//! `docs/spec/04-behavior/error-format.md` ERR-3・`docs/spec/04-behavior/
//! recovery.md` RECOVER-10）。
//!
//! `tests/recovery_content_hash.rs`（TASK-101・RECOVER-10）が「内容一致→`23505`／
//! 内容不一致→`22023`」の分岐そのものを公開書き込み経路ごとに検証済みであるのに対し、
//! 本ファイルは ERR-3 固有の契約——「`22023` は
//! [`ErrorClass::OperationIdContentMismatch`] **以外のどの分類にも写像しない**」こと
//! （とりわけ `23505`〔`UniqueViolation`〕への誤写像がないこと）——を、行 id 衝突と
//! 内容不一致が重畳する条件・決定性・情報漏えい耐性の観点から検証する。流儀は
//! `tests/recovery_content_hash.rs`・`tests/error_format.rs` を踏襲し、実 `Storage`・
//! `EngineCore::from_storage`・`test_util/temp_db.rs` ヘルパと公開 API のみを使う。
//!
//! v1 レガシー台帳エントリ（内容一致の証明が取れない旧フォーマット）への再送が
//! `22023` になることの検証は、`pub(crate)` な内部 API を直接使う必要があるため、
//! 本クレート内部の単体テスト（`crates/engine/src/recovery/ledger.rs` の
//! `resend_to_legacy_v1_entry_is_rejected_as_content_mismatch`）で担保済み
//! （本ファイルは公開 API のみを使う結合テストのため対象外）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::TenantWriteError;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
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

// --- (b) 排他性: `22023` は `OperationIdContentMismatch` 以外のどの分類の
//     `wire_code` とも一致しない（`ErrorClass::ALL` の全走査。特に `23505`
//     〔`UniqueViolation`〕でないことを明示する）。ERR-3 の確定契約そのもの。 --------

#[test]
fn err3_content_mismatch_wire_code_is_exclusive_to_its_own_class() {
    let mismatch_class = ErrorClass::OperationIdContentMismatch;
    let mismatch_code = mismatch_class.wire_code();
    assert_eq!(mismatch_code, "22023");

    // 特に `23505`（commit 済み判定に誤用されうる重複拒否分類）ではないこと。
    assert_ne!(
        mismatch_code,
        ErrorClass::UniqueViolation.wire_code(),
        "22023 は 23505 に写像してはならない（fail-closed の核心契約）"
    );

    // 全分類間の wire_code 一意性は tests/error_format.rs::err2_all_classes_have_unique_wire_codes
    // が既にグローバルに保証しているため、ここでの ALL 走査による再検証は行わない。

    // `from_wire_code` の逆引きも `OperationIdContentMismatch` へのみ戻る。
    assert_eq!(
        ErrorClass::from_wire_code(mismatch_code),
        Some(mismatch_class)
    );

    // `SqlSurfaceError::OperationIdContentMismatch` 自身の写像を明示的に検証する。
    // ここまでの検証は `ErrorClass::OperationIdContentMismatch` という値そのものに
    // 閉じており、SQL 表層の variant → `ErrorClass` の対応（TASK-152・ERR-2 の
    // `match` 表）が誤って `UniqueViolation` 等の別分類を返しても検出できない
    // （codex-review 指摘）。`ClassifiedError` トレイト経由で `wire_code()` /
    // `error_class()` の両方を突き合わせ、`UniqueViolation`（`23505`）への
    // 誤写像がないことも合わせて確認する。
    let sql_err = SqlSurfaceError::OperationIdContentMismatch;
    assert_eq!(
        ClassifiedError::error_class(&sql_err),
        ErrorClass::OperationIdContentMismatch
    );
    assert_eq!(ClassifiedError::wire_code(&sql_err), "22023");
    assert_ne!(
        ClassifiedError::error_class(&sql_err),
        ErrorClass::UniqueViolation,
        "SqlSurfaceError::OperationIdContentMismatch は UniqueViolation へ誤写像してはならない"
    );
    assert_ne!(
        ClassifiedError::error_class(&sql_err),
        ErrorClass::DuplicateOperationId,
        "SqlSurfaceError::OperationIdContentMismatch は DuplicateOperationId（commit 済み確定の根拠）へ誤写像してはならない"
    );
    assert_ne!(
        ClassifiedError::error_class(&TenantWriteError::OperationIdContentMismatch),
        ErrorClass::DuplicateOperationId,
        "TenantWriteError::OperationIdContentMismatch も DuplicateOperationId へ誤写像してはならない"
    );
}

// --- (c) 重畳条件下の優先順位: 内容不一致の再送が、行 id 衝突（23505 の発生条件）も
//     同時に満たす場合でも、`23505` ではなく `22023` が返る（台帳照合が行キー一意性
//     チェックより先に行われる契約。`tenant::insert_row_unchecked` の実装順序が
//     この優先順位を担保する）。 --------------------------------------------------

#[test]
fn err3_content_mismatch_takes_priority_over_id_conflict_when_both_conditions_hold() {
    let path = unique_db_path("err3-priority-insert");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // id=1 を op-first で確定させる。
    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"first"),
        Some(&op("op-first")),
    )
    .expect("seed id=1");
    // id=2 を別 operation_id で確定させる（行 id 衝突の対象を用意する）。
    core.insert_row(
        &ctx,
        TABLE,
        2,
        &row(&[0.4, 0.5, 0.6], b"second"),
        Some(&op("op-second")),
    )
    .expect("seed id=2");

    // op-first を id=2・異なる内容で再送する。id=2 は既存行と衝突する
    // （行キー一意性チェックだけを見れば 23505 = IdConflict になり得る）が、
    // op-first は既に「id=1・内容 first」で台帳に記録済みのため、台帳照合が
    // 先に不一致を検出し 22023 を返さなければならない。
    let err = core
        .insert_row(
            &ctx,
            TABLE,
            2,
            &row(&[0.9, 0.9, 0.9], b"mismatched-content"),
            Some(&op("op-first")),
        )
        .expect_err("overlapping mismatch + id-conflict must still be rejected");
    assert!(
        matches!(err, TenantWriteError::OperationIdContentMismatch),
        "id 衝突条件と重畳しても IdConflict ではなく OperationIdContentMismatch でなければならない: {err:?}"
    );
    assert_eq!(err.wire_code(), "22023");
    assert_ne!(
        err.wire_code(),
        "23505",
        "commit 済み判定への誤用防止（fail-closed）が壊れている"
    );

    // 副作用ゼロ: id=2 の行は元の内容のまま（上書きも新規作成もされていない）。
    let reinsert_id2 = core.insert_row(
        &ctx,
        TABLE,
        2,
        &row(&[0.4, 0.5, 0.6], b"second"),
        Some(&op("op-second")),
    );
    assert!(
        matches!(reinsert_id2, Err(TenantWriteError::DuplicateOperationId)),
        "id=2 の行は変更されておらず、op-second の再送は内容一致の 23505 のまま: {reinsert_id2:?}"
    );
}

// --- (d) 決定的分類: 同一の不一致再送を繰り返しても常に 22023
//     （ERR-2 の決定的分類契約を ERR-3 行にも適用する）。 -------------------------

#[test]
fn err3_content_mismatch_classification_is_deterministic_across_repeats() {
    let path = unique_db_path("err3-deterministic");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"original"),
        Some(&op("op-repeat")),
    )
    .expect("seed insert");

    for attempt in 0..5 {
        let err = core
            .insert_row(
                &ctx,
                TABLE,
                1,
                &row(&[0.9, 0.9, 0.9], b"mismatched"),
                Some(&op("op-repeat")),
            )
            .expect_err("mismatch resend must be rejected every time");
        assert_eq!(
            err.wire_code(),
            "22023",
            "attempt {attempt}: 分類は毎回同一でなければならない"
        );
        assert!(matches!(err, TenantWriteError::OperationIdContentMismatch));
    }
}

// --- (e) 両側境界: 同一内容の再送は `23505`（重複拒否）であり `22023` へ
//     誤爆しない（ERR-3 が「内容不一致のみ」を対象とすることの反対側の確認）。 -----

#[test]
fn err3_same_content_resend_stays_23505_and_never_22023() {
    let path = unique_db_path("err3-boundary-duplicate");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"same"),
        Some(&op("op-dup")),
    )
    .expect("seed insert");

    let err = core
        .insert_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.1, 0.2, 0.3], b"same"),
            Some(&op("op-dup")),
        )
        .expect_err("same content resend must be rejected as duplicate");
    assert_eq!(err.wire_code(), "23505");
    assert_ne!(err.wire_code(), "22023");
    assert!(matches!(err, TenantWriteError::DuplicateOperationId));
}

// --- (f) 情報漏えいなし: `client_message()` に書き込み内容・テナント情報を
//     含めない（`.claude/rules/security.md` P0）。UPDATE 経路でも同様に確認する。 --

#[test]
fn err3_client_message_never_leaks_content_or_tenant_on_update() {
    let path = unique_db_path("err3-no-leak-update");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // 推測しにくい operation_id を用意する。固定の短い値（"op-upd" 等）だと、
    // client_message の固定文言（例: "operation_id already recorded with
    // different content"）に偶然含まれる部分文字列と衝突しない保証がなく、
    // operation_id 自体が client_message() に混入する回帰（codex-review 指摘）を
    // 検出できない。
    const SECRET_OP_ID: &str = "op-9f3ac71e2b8d4f0a9c6e5d1b7a4f2e08";

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"seed"),
        Some(&op("op-seed")),
    )
    .expect("seed insert");
    core.update_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.4, 0.5, 0.6], b"updated-once"),
        Some(&op(SECRET_OP_ID)),
    )
    .expect("first update");

    let err = core
        .update_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.7, 0.7, 0.7], b"top-secret-marker"),
            Some(&op(SECRET_OP_ID)),
        )
        .expect_err("different content resend must be rejected");
    assert_eq!(err.wire_code(), "22023");

    let msg = err.client_message();
    assert!(
        !msg.contains("top-secret-marker"),
        "client_message は書き込み内容を運ばない: {msg}"
    );
    assert!(
        !msg.contains("tenant-a"),
        "client_message はテナント id を運ばない: {msg}"
    );
    assert!(
        !msg.contains("updated-once"),
        "client_message は旧内容も運ばない: {msg}"
    );
    assert!(
        !msg.contains(SECRET_OP_ID),
        "client_message は operation_id を運ばない: {msg}"
    );
}
