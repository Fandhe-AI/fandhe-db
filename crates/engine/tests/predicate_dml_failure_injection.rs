//! 述語つき `UPDATE`／`DELETE`（SQL-19・TASK-192、Issue #871）の失敗注入試験。
//! ポインタ: `docs/spec/04-behavior/recovery.md` RECOVER-9・RECOVER-11。
//! `crates/engine/tests/index_failure_injection.rs`（RECOVER-9）と同じ設計
//! 方針（commit 前の途中失敗が副作用ゼロで収束すること）を、公開 API のみ
//! （`EngineCore::execute_sql_in_session`）から検証する。`tenant.rs` 内部の
//! `#[cfg(test)]` 注入シームは本 Issue の時間的スコープでは追加しない
//! （`docs/design/predicate-dml-exec.md`「申し送り」参照。redb の
//! write トランザクションは commit 前のあらゆる `Err` で drop され副作用が
//! 残らないという構造的性質そのものは、式評価エラーという実際に起こりうる
//! 経路を通じて機械検証できる）。
//!
//! 式評価エラー（0 除算）を候補列挙の途中で誘発し、(1) 全行が変更前と一致する、
//! (2) 台帳エントリが記録されない（同一 `operation_id` の再利用が可能）ことを、
//! drop → `Storage::open` 再オープン（プロセス再起動相当）後の読み取りでも
//! 確認する。除数は疑似列 `id` の算術（`sql::allowlist::ColumnType` に数値専用
//! 列型が存在しないため、`WHERE` 式中で数値として参照できるのは疑似列 `id` と
//! `VECTOR` 列のみ——`TEXT` 列は文字列比較専用で算術には使えない）。

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

const TABLE: &str = "documents";

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: &str, seq: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        ctx,
        &mut session,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
             ({id}, '[0.1,0.2]', '{lang}') USING OPERATION_ID 'seed-{id}-{seq}'"
        ),
    )
    .expect("insert row");
}

fn count_star(core: &EngineCore, ctx: &PolicyContext) -> u64 {
    let result = core
        .execute_sql(ctx, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("count(*) should succeed");
    match &result.rows[0].cells[0] {
        engine::sql::exec::Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn execute(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(ctx, &mut session, sql)
}

/// `1 / (id - 2) < 1000`: id=2 の行を走査したときにだけ 0 除算が発生する
/// （id 昇順の物理走査順のため、id=1 の判定は必ず id=2 より先に行われる）。
/// write トランザクション全体が commit されず副作用ゼロのまま拒否されることを
/// 固定する。
#[test]
fn predicate_delete_expression_error_mid_enumeration_leaves_no_side_effects() {
    let path = unique_db_path("predicate-dml-failure-injection-delete");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");

    insert_row(&core, &alice, 1, "ja", "1");
    insert_row(&core, &alice, 2, "ja", "2");
    insert_row(&core, &alice, 3, "ja", "3");

    let before = count_star(&core, &alice);
    assert_eq!(before, 3);

    let err = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' AND 1 / (id - 2) < 1000 \
             USING OPERATION_ID 'op-fault-1'"
        ),
    )
    .expect_err("division by zero mid-enumeration must reject the whole statement");
    assert_eq!(err.wire_code(), "22012");

    // (1) 副作用ゼロ: 全行が変更前のまま（id=1 も削除されていない）。
    assert_eq!(count_star(&core, &alice), before);

    // (2) 台帳未記録: 同一 operation_id の再実行が「再送」として拒否されず、
    // 同じ壊れた文をそのまま再実行しても同じ 22000 になる（23505/22023 には
    // ならない＝台帳に一切残っていない）ことで確認する。
    let err_again = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' AND 1 / (id - 2) < 1000 \
             USING OPERATION_ID 'op-fault-1'"
        ),
    )
    .expect_err("resend of an unrecorded operation_id must be the same rejection, not 23505/22023");
    assert_eq!(err_again.wire_code(), "22012");

    // 同一 operation_id を意味論的に正しい文で使い回せる（台帳が一切
    // 消費されていない証拠）。
    let outcome = execute(
        &core,
        &alice,
        &format!(
            "DELETE FROM {TABLE} WHERE lang = 'ja' AND id = 1 USING OPERATION_ID 'op-fault-1'"
        ),
    )
    .expect("the same operation_id must still be usable for a well-formed statement");
    match outcome {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
    assert_eq!(count_star(&core, &alice), before - 1);

    // (3) drop → 再オープン（プロセス再起動相当）後も上記の状態がそのまま
    // 永続化されている（id=2・id=3 は残存、id=1 のみ削除済み）。
    drop(core);
    let reopened = Storage::open(&path).expect("reopen storage");
    let core2 = EngineCore::from_storage(reopened, Box::new(CpuScalarProvider));
    assert_eq!(count_star(&core2, &alice), before - 1);
}

/// UPDATE 側でも同様に、式評価エラーが commit を阻止し SET が一切適用
/// されないことを固定する。
#[test]
fn predicate_update_expression_error_mid_enumeration_leaves_no_side_effects() {
    let path = unique_db_path("predicate-dml-failure-injection-update");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let alice = ctx_for("alice");

    insert_row(&core, &alice, 1, "ja", "1");
    insert_row(&core, &alice, 2, "ja", "2");

    let err = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' AND 1 / (id - 2) < 1000 \
             USING OPERATION_ID 'op-fault-upd-1'"
        ),
    )
    .expect_err("division by zero mid-enumeration must reject the whole statement");
    assert_eq!(err.wire_code(), "22012");

    // id=1 の SET も適用されていない（全行不変）。
    let result = core
        .execute_sql(&alice, &format!("SELECT id, lang FROM {TABLE} LIMIT 10"))
        .expect("scan should succeed");
    for row in &result.rows {
        match &row.cells[1] {
            engine::sql::exec::Cell::Text(s) => assert_eq!(s, "ja"),
            other => panic!("expected Cell::Text, got {other:?}"),
        }
    }

    // 台帳未記録: 同一 operation_id を意味論的に正しい文で使い回せる。
    let outcome = execute(
        &core,
        &alice,
        &format!(
            "UPDATE {TABLE} SET lang = 'fr' WHERE lang = 'ja' AND id = 1 \
             USING OPERATION_ID 'op-fault-upd-1'"
        ),
    )
    .expect("the same operation_id must still be usable for a well-formed statement");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 1),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
}
