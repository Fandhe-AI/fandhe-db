//! `trigger::fire`・`trigger::plan` の単体テスト（TABLE-45・Issue #1723・#1724）。
//!
//! 呼び出し口を差し替えて、連鎖・`NULL` 返却・返却行の構造検査・`WHEN` の fail-closed を固定する。
//! 書き込み経路を通した結合テストは `core/trigger_insert_tests.rs`。

use std::sync::Arc;

use super::call::{TriggerCall, TriggerFunctionInvoker, TriggerOutcome};
use super::fire::{check_returned_row, fire_after_row_insert, fire_before_row_insert};
use super::runtime::TriggerRuntime;
use super::write_handle::TriggerWriteHandle;
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

/// 書き込み窓口 `$h` を、一時 DB の write txn・テナント文脈とともに束縛する（txn は文の終わりまで生存）。
macro_rules! handle {
    ($h:ident) => {
        let dir = crate::test_util::temp_db::tempdir();
        let storage = crate::storage::Storage::open(dir.path().join("db.redb")).expect("open");
        let txn = storage.begin_write_txn().expect("txn");
        let pctx = crate::policy::PolicyContext::with_visibilities(
            "tenant-a",
            [crate::storage::Visibility::Private],
        )
        .expect("ctx");
        let $h = TriggerWriteHandle::new(&txn, &pctx);
    };
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
    handle!(h);
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
        &h,
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
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|_| Ok(TriggerOutcome::Skip))));
    let out = fire_before_row_insert(
        &rt,
        &h,
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
    handle!(h);
    let rt = TriggerRuntime::unwired();
    let err =
        fire_before_row_insert(&rt, &h, "t", &schema(), &[trig("a", None)], row()).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

#[test]
fn when_condition_is_rejected_at_fire_time() {
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|c| {
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })));
    let err = fire_before_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[trig("a", Some("a = 'x'"))],
        row(),
    )
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

fn after_trig(name: &str, when: Option<&str>) -> TriggerDef {
    TriggerDef {
        timing: TriggerTiming::After,
        ..trig(name, when)
    }
}

// AFTER は渡された最終 NEW をそのまま、`TG_WHEN` を AFTER で、トリガ名の順に呼ぶ。戻り値は無視する。
#[test]
fn after_fires_in_order_and_ignores_outcome() {
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|c| {
        assert_eq!(c.tg_when(), "AFTER");
        assert_eq!(c.new_row.len(), 2);
        // Skip・構造違いの行のどちらを返しても無視される。
        if c.trigger_name == "a" {
            Ok(TriggerOutcome::Skip)
        } else {
            Ok(TriggerOutcome::Proceed(vec![Value::Null]))
        }
    })));
    fire_after_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[after_trig("a", None), after_trig("b", None)],
        &row(),
    )
    .expect("fire");
}

// AFTER のエラーは SQLSTATE を保って返り、後続のトリガは呼ばれない。
#[test]
fn after_error_stops_and_propagates() {
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|c| {
        assert_eq!(c.trigger_name, "a");
        Err(SqlSurfaceError::InvalidInput {
            detail: "x".to_string(),
        })
    })));
    let err = fire_after_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[after_trig("a", None), after_trig("b", None)],
        &row(),
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn after_when_condition_is_rejected_at_fire_time() {
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(Fixed(|_| Ok(TriggerOutcome::Skip))));
    let err = fire_after_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[after_trig("a", Some("a = 'x'"))],
        &row(),
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

// 窓口は SCHEMA 配下の内部キーを不在と同じ応答（`42P01`）で拒否する。
#[test]
fn write_handle_rejects_internal_schema_keys() {
    handle!(h);
    let err = h.insert_row("#1.t", None, &[Value::Null]).unwrap_err();
    assert_eq!(err.wire_code(), "42P01");
    assert!(h.written_tables().is_empty());
}
