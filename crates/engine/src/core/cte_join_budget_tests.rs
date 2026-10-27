//! CTE を入力に持つ JOIN のメモリ予算の事前計上の単体テスト（SQL-62・Issue #1861、PR #2729 の
//! codex 指摘）。CTE 本文の評価結果・共有キャッシュ・参照ごとの複製が、後続の JOIN と共有する
//! 文全体の 1 つの予算へ確保の前に計上されることを、予算上限を小さく差し替えて固定する
//! （`EngineCore::cte_join_budget_cap_for_test`。本番の上限は 1 GiB で実データでは再現できない）。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::QueryResult;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const BODY_LEN: usize = 2000;
const ROWS: u64 = 4;

fn open() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("cte-join-budget");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::with_visibilities("alice", [Visibility::Public]).expect("tenant");
    for id in 1..=ROWS {
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("x".repeat(BODY_LEN)),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("insert");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let ctx = PolicyContext::with_visibilities("alice", [Visibility::Public]).expect("tenant");
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx, &mut session, sql)? {
        crate::sql::SqlOutcome::Query(r) => Ok(r),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// 側述語で結果が 0 行になる自己結合（共有 CTE を 2 回参照）。保持するのは CTE 本文の全行だが、
/// 最終的な JOIN の課金には現れない形。
const SELF_JOIN_EMPTY_RESULT: &str = "WITH c AS MATERIALIZED (SELECT id, body FROM docs) \
     SELECT a.id FROM c a JOIN c b ON a.id = b.id WHERE a.body = 'none' LIMIT 5";

/// 本文 1 部（約 8.4 KB）は収まるが、キャッシュ・参照ごとの複製を足すと収まらない上限では、
/// 側述語で結果が小さくなる文でも `54000` で拒否される（保持中の元入力が課金に現れない漏れの封じ）。
#[test]
fn cached_and_cloned_cte_rows_are_charged_to_the_shared_budget() {
    let (mut core, _g) = open();
    // 全体で 3 部（本体＋キャッシュ＋2 つ目の複製）が要るので、1 部強の上限では足りない。
    core.cte_join_budget_cap_for_test = Some(12_000);
    let err = run(&core, SELF_JOIN_EMPTY_RESULT).expect_err("budget must reject");
    assert_eq!(err.wire_code(), "54000");

    // 十分な上限では同じ文が成功する（上の拒否が予算以外の理由でないことの対照）。
    core.cte_join_budget_cap_for_test = Some(64 * 1024 * 1024);
    let ok = run(&core, SELF_JOIN_EMPTY_RESULT).expect("fits in a large budget");
    assert!(ok.rows.is_empty());
}

/// 共有しない CTE（`NOT MATERIALIZED`）でも評価結果の保持は計上される。1 部も収まらない上限では、
/// 走査の生成中に打ち切られ（確保の前）`54000`。
#[test]
fn scan_result_is_bounded_by_the_remaining_shared_budget() {
    let (mut core, _g) = open();
    core.cte_join_budget_cap_for_test = Some(4_000);
    let err = run(
        &core,
        "WITH c AS NOT MATERIALIZED (SELECT id, body FROM docs) \
         SELECT a.id FROM c a JOIN docs d ON a.id = d.id WHERE a.body = 'none' LIMIT 5",
    )
    .expect_err("one copy does not fit");
    assert_eq!(err.wire_code(), "54000");
}
