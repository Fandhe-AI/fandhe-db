//! `trigger::fire`・`trigger::plan` の単体テスト（TABLE-45・Issue #1723）。
//!
//! 呼び出し口を差し替えて、連鎖・`NULL` 返却・返却行の構造検査・`WHEN` の fail-closed を固定する。
//! 書き込み経路を通した結合テストは `core/trigger_insert_tests.rs`。

use std::sync::Arc;

use super::call::{TriggerCall, TriggerFunctionInvoker, TriggerOutcome};
use super::fire::{check_returned_row, fire_before_row_insert};
use super::runtime::TriggerRuntime;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;

struct Fixed(fn(&TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError>);

impl TriggerFunctionInvoker for Fixed {
    fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        (self.0)(call)
    }
}

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("a", ColumnType::Text, true),
        ],
    )
}

fn trig(name: &str, when: Option<&str>) -> TriggerDef {
    TriggerDef {
        id: 1,
        name: name.to_string(),
        timing: TriggerTiming::Before,
        events: TriggerEvents::from_bits(TriggerEvents::INSERT).expect("events"),
        level: TriggerLevel::Row,
        update_of: Vec::new(),
        function_key: "f".to_string(),
        tg_args: vec!["x".to_string()],
        when_sql: when.map(str::to_string),
    }
}

fn row() -> Vec<Value> {
    vec![Value::Vector(vec![1.0, 0.0]), Value::Text("v".to_string())]
}

#[test]
fn proceed_passes_rewritten_row_to_next_trigger() {
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|c| {
        let mut r = c.new_row.to_vec();
        let prev = match &r[1] {
            Value::Text(t) => t.clone(),
            _ => String::new(),
        };
        r[1] = Value::Text(format!("{prev}{}", c.trigger_name));
        assert_eq!(c.args, ["x".to_string()]);
        Ok(TriggerOutcome::Proceed(r))
    })));
    let out = fire_before_row_insert(
        &rt,
        "t",
        &schema(),
        &[trig("a", None), trig("b", None)],
        row(),
    )
    .expect("fire")
    .expect("row");
    assert_eq!(out[1], Value::Text("vab".to_string()));
}

#[test]
fn skip_stops_the_chain() {
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|_| Ok(TriggerOutcome::Skip))));
    let out = fire_before_row_insert(
        &rt,
        "t",
        &schema(),
        &[trig("a", None), trig("b", None)],
        row(),
    )
    .expect("fire");
    assert!(out.is_none());
}

#[test]
fn unwired_runtime_is_0a000() {
    let rt = TriggerRuntime::unwired();
    let err = fire_before_row_insert(&rt, "t", &schema(), &[trig("a", None)], row()).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn when_condition_is_rejected_at_fire_time() {
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|c| {
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })));
    let err = fire_before_row_insert(&rt, "t", &schema(), &[trig("a", Some("a = 'x'"))], row())
        .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn returned_row_shape_is_checked() {
    let s = schema();
    assert!(check_returned_row(&s, &row()).is_ok());
    // 列数違い。
    assert_eq!(
        check_returned_row(&s, &[Value::Null])
            .unwrap_err()
            .wire_code(),
        "42804"
    );
    // VECTOR 列へ非 Vector。
    assert_eq!(
        check_returned_row(&s, &[Value::Text("x".into()), Value::Null])
            .unwrap_err()
            .wire_code(),
        "42804"
    );
    // 次元違い。
    assert_eq!(
        check_returned_row(&s, &[Value::Vector(vec![1.0]), Value::Null])
            .unwrap_err()
            .wire_code(),
        "42804"
    );
    // 非 VECTOR 列へ Vector。
    assert_eq!(
        check_returned_row(
            &s,
            &[Value::Vector(vec![1.0, 0.0]), Value::Vector(vec![1.0])]
        )
        .unwrap_err()
        .wire_code(),
        "42804"
    );
}
