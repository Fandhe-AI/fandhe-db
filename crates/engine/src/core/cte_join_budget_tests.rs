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

/// 層（射影）の出力行の見積もりは、同じ列の複数回選択ぶんの複製を数える（PR #2729 の codex 指摘。
/// 親 1 部だけを追加計上すると、`SELECT body, body, body` が 3 部を確保してから拒否される）。
/// 見積もりは `JoinBudget::charge_layer_output` が唯一の入口で、適用の前に呼ばれる。
#[test]
fn layer_output_estimate_counts_repeated_projection_columns() {
    use crate::sql::allowlist::{BufferedRows, Projection};
    use crate::sql::exec::{Cell, ColumnMeta, ResultRow};
    use crate::sql::join::JoinBudget;
    use crate::sql::udf_call::UdfRegistry;

    let parent = QueryResult {
        columns: vec![ColumnMeta::Scalar {
            name: "body".to_string(),
            ty: ColumnType::Text,
        }],
        rows: (0..ROWS)
            .map(|id| ResultRow {
                id,
                score: 0.0,
                cells: vec![Cell::Text("x".repeat(BODY_LEN))],
            })
            .collect(),
    };
    let layer_with = |cols: &[&str]| BufferedRows {
        projection: Projection::Columns(cols.iter().map(|c| c.to_string()).collect()),
        where_predicates: Vec::new(),
        order_by: Vec::new(),
        order_keys: Vec::new(),
        window_items: Vec::new(),
        limit: u32::MAX,
        limit_explicit: false,
        limit_implicit: false,
        offset: 0,
    };
    let udfs = UdfRegistry::default();
    let plan = |cols: &[&str]| {
        crate::sql::view_buffered::plan_rows_layer(&parent.columns, "a", &layer_with(cols), &udfs)
            .expect("plan")
    };
    let one = plan(&["body"])
        .rows_output_bytes(&parent)
        .expect("estimable");
    let three = plan(&["body", "body", "body"])
        .rows_output_bytes(&parent)
        .expect("estimable");
    let payload = (BODY_LEN as u64 * ROWS) as usize;
    assert!(one >= payload, "one copy of the payload is counted");
    assert!(three >= 3 * payload, "three copies are counted: {three}");

    // 3 部は収まらず 1 部は収まる上限では、3 回選択の層だけが適用前に `54000` で拒否される。
    let cap = 2 * payload;
    let mut budget = JoinBudget::with_cap(cap);
    budget
        .charge_layer_output(&parent, &plan(&["body"]))
        .expect("one copy fits");
    let mut budget = JoinBudget::with_cap(cap);
    let err = budget
        .charge_layer_output(&parent, &plan(&["body", "body", "body"]))
        .expect_err("three copies exceed the budget");
    assert_eq!(err.wire_code(), "54000");
}

/// 同じ列を複数回選ぶ導出層を SQL から通す（列名リストで改名）。予算が十分なら結果が正しく、
/// 3 部ぶんの保持量を見込む小さい予算では `54000` になる。
#[test]
fn repeated_projection_in_a_derived_layer_is_charged_and_correct() {
    let (mut core, _g) = open();
    let sql = "WITH a AS MATERIALIZED (SELECT id, body FROM docs), \
               b(x, y, z) AS (SELECT body, body, body FROM a) \
               SELECT p.x, q.z FROM b p JOIN b q ON p.x = q.x WHERE p.x = 'none' LIMIT 5";
    core.cte_join_budget_cap_for_test = Some(64 * 1024 * 1024);
    assert!(run(&core, sql).expect("fits").rows.is_empty());
    core.cte_join_budget_cap_for_test = Some(30_000);
    assert_eq!(
        run(&core, sql).expect_err("over budget").wire_code(),
        "54000"
    );
}
