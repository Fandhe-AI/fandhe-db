//! [`super::plan_merge`]・[`super::MergeEval`] の単体テスト（SQL-79・Issue #3354）。`WHEN NOT MATCHED
//! [BY TARGET]` の条件が元の行だけで評価されること、INSERT 値の評価（指定された列の印・疑似列 `id` の規則）を、
//! カタログ・エンジンを使わずスキーマと元の 1 行だけで固定する。行の書き込み・対象行を要する評価
//! （MATCHED・BY SOURCE）は `tests/sql_t40_up/sql79_merge_not_matched.rs` の SQL 経由のテストが担う。

use super::*;
use crate::catalog::{ColumnDef, ColumnType};
use crate::sql::allowlist::{validate_merge_tokens_with_limits, TableLookup};
use crate::sql::lexer;
use crate::sql::merge_bind::{bind_merge, MergeBindInput};
use crate::sql::udf_call::UdfRegistry;

struct Cat;
impl TableLookup for Cat {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(["t", "s"].contains(&name))
    }
}

fn schema(name: &str, value_col: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("k", ColumnType::Text, false),
            ColumnDef::new(value_col, ColumnType::Integer, true),
        ],
    )
}

/// 従来方式（疑似列 `id` あり）の `t` と `s` で計画を組む。
fn plan_of(sql: &str) -> (MergeExecPlan, TableSchema) {
    let (t, s) = (schema("t", "x"), schema("s", "v"));
    let tokens = lexer::tokenize(sql).expect("lex");
    let stmt = validate_merge_tokens_with_limits(
        &tokens,
        crate::recovery::required_op_id::LedgerMode::Optional,
        crate::nesting_limits::NestingLimits::default(),
        &Cat,
    )
    .expect("syntax");
    let bound = bind_merge(
        &stmt,
        &MergeBindInput {
            target_schema: &t,
            target_view: None,
            source_schema: &s,
            source_visible: None,
            dummy_flags: &[],
        },
        &UdfRegistry::default(),
    )
    .expect("bind");
    let plan = plan_merge(
        &bound,
        "t",
        &t,
        MergeSourceInput {
            table: "s".to_string(),
            schema: s,
            view_predicates: Vec::new(),
        },
    )
    .expect("plan");
    (plan, t)
}

/// 走査投影の並びに合わせた元の 1 行（疑似列 `id` を含む計画なら先頭が `id`）。
fn source_row(plan: &MergeExecPlan, id: u64, k: &str, v: Option<i64>) -> ResultRow {
    let joined = &plan.joined;
    let mut cells = Vec::new();
    if joined.source_id_in_cells() {
        cells.push(Cell::Integer(id));
    }
    for c in joined.scan_columns() {
        cells.push(match c {
            0 => Cell::Text(k.to_string()),
            _ => v.map_or(Cell::Null, Cell::SignedInteger),
        });
    }
    ResultRow {
        id,
        score: 0.0,
        cells,
    }
}

const HEAD: &str = "MERGE INTO t USING s ON t.k = s.k";

#[test]
fn a_by_target_condition_is_evaluated_on_the_source_row_alone() {
    let (plan, _t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED AND s.v > 5 THEN INSERT (id, k, x) VALUES (s.id, s.k, s.v) \
         WHEN NOT MATCHED THEN DO NOTHING \
         WHEN NOT MATCHED BY SOURCE THEN DELETE"
    ));
    let mut eval = MergeEval::new(&plan).expect("eval");
    // 真になる行は最初の句（INSERT）、偽の行は次の句（DO NOTHING）。BY SOURCE の句は BY TARGET の評価に現れない。
    assert_eq!(
        eval.choose_by_target(&source_row(&plan, 1, "a", Some(10)))
            .expect("choose"),
        Choice::Insert(0)
    );
    assert_eq!(
        eval.choose_by_target(&source_row(&plan, 2, "b", Some(1)))
            .expect("choose"),
        Choice::DoNothing
    );
    // NULL との比較は真にならない。
    assert_eq!(
        eval.choose_by_target(&source_row(&plan, 3, "c", None))
            .expect("choose"),
        Choice::DoNothing
    );
}

#[test]
fn eval_insert_marks_only_the_listed_columns_as_provided() {
    let (plan, t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED THEN INSERT (id, k) VALUES (s.id, s.k)"
    ));
    let eval = MergeEval::new(&plan).expect("eval");
    let (id, values, provided) = eval
        .eval_insert(0, &source_row(&plan, 7, "z", Some(3)), &t, &mut |_| Ok(()))
        .expect("insert");
    assert_eq!(id, 7);
    assert_eq!(values.len(), 2);
    assert_eq!(provided, vec![true, false]);
    assert_eq!(values.first(), Some(&Value::Text("z".to_string())));
}

#[test]
fn eval_insert_takes_the_id_from_a_literal_or_a_source_column() {
    let (plan, t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED THEN INSERT (id, k, x) VALUES (42, s.k, s.v)"
    ));
    let eval = MergeEval::new(&plan).expect("eval");
    let (id, values, provided) = eval
        .eval_insert(0, &source_row(&plan, 7, "z", Some(3)), &t, &mut |_| Ok(()))
        .expect("insert");
    assert_eq!(id, 42);
    assert_eq!(provided, vec![true, true]);
    assert_eq!(values.get(1), Some(&Value::Integer(3)));

    // 元の整数列を `id` へ流す。NULL・負数は `22000`。
    let (plan, t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED THEN INSERT (id, k) VALUES (s.v, s.k)"
    ));
    let eval = MergeEval::new(&plan).expect("eval");
    let ok = eval
        .eval_insert(0, &source_row(&plan, 1, "z", Some(9)), &t, &mut |_| Ok(()))
        .expect("insert");
    assert_eq!(ok.0, 9);
    for bad in [None, Some(-1)] {
        let err = eval
            .eval_insert(0, &source_row(&plan, 1, "z", bad), &t, &mut |_| Ok(()))
            .expect_err("bad id");
        assert_eq!(err.wire_code(), "22000", "{bad:?}");
    }
}

#[test]
fn eval_insert_charges_the_shared_budget_and_propagates_its_failure() {
    let (plan, t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED THEN INSERT (id, k, x) VALUES (s.id, s.k, s.v)"
    ));
    let eval = MergeEval::new(&plan).expect("eval");
    let mut charged = 0usize;
    eval.eval_insert(0, &source_row(&plan, 1, "z", Some(3)), &t, &mut |n| {
        charged += n;
        Ok(())
    })
    .expect("insert");
    assert!(charged > 0);
    let err = eval
        .eval_insert(0, &source_row(&plan, 1, "z", Some(3)), &t, &mut |_| {
            Err(SqlSurfaceError::payload_too_large("budget"))
        })
        .expect_err("budget");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn eval_insert_on_a_non_insert_clause_is_internal() {
    let (plan, t) = plan_of(&format!(
        "{HEAD} WHEN NOT MATCHED THEN DO NOTHING WHEN MATCHED THEN DELETE"
    ));
    let eval = MergeEval::new(&plan).expect("eval");
    let err = eval
        .eval_insert(0, &source_row(&plan, 1, "z", Some(3)), &t, &mut |_| Ok(()))
        .expect_err("not insert");
    assert_eq!(err.wire_code(), "XX000");
}
