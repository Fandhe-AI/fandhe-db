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
        let $h = TriggerWriteHandle::new(&txn, &pctx, None);
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

// テスト専用の未結線 invoker（`cfg(test)` の `TriggerRuntime::unwired`）の単体。本番の既定ではない。
#[test]
fn unwired_runtime_is_0a000() {
    handle!(h);
    let rt = TriggerRuntime::unwired();
    // SCHEMA 配下の表（共有 public 表は別の拒否。下のテスト）。
    let err =
        fire_before_row_insert(&rt, &h, "#1.t", &schema(), &[trig("a", None)], row()).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

/// 成功を返し、呼ばれた回数を数えるテスト用ホスト。
struct CountingHost(std::sync::atomic::AtomicUsize);

impl super::call::TriggerHost for CountingHost {
    fn invoke_trigger(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(TriggerOutcome::Proceed(call.new_row.to_vec()))
    }

    fn eval_trigger_when(
        &self,
        _call: &TriggerCall<'_>,
        _w: &str,
    ) -> Result<bool, SqlSurfaceError> {
        Ok(true)
    }
}

// 共有 public 表（裸の表キー）では、成功するホストが束縛されていても本番の invoker は委譲せず固定文言の
// `0A000` を返す（定義時の拒否の導入前に残った定義への保険。Issue #3361）。SCHEMA 配下の表（`#1.t`）は
// ホストへ委譲する。
//
// intentional: docs/design/trigger-execution-model.md
// 共有 public 表のトリガは発火させない（fail-closed の保険。設計記録 D9 の改訂注記）。
#[test]
fn production_invoker_rejects_shared_public_tables_without_calling_host() {
    use super::call::{PlpgsqlTriggerInvoker, TriggerOp};
    handle!(h);
    let host = CountingHost(std::sync::atomic::AtomicUsize::new(0));
    let schema = schema();
    let new_row = row();
    let call = |table: &'static str| TriggerCall {
        trigger_name: "g",
        function_key: "f",
        table,
        schema: &schema,
        op: TriggerOp::Insert,
        level: TriggerLevel::Row,
        old_row: None,
        new_row: &new_row,
        args: &[],
        timing: TriggerTiming::Before,
        writes: &h,
        host: Some(&host),
    };
    for key in ["t", "not a key", ""] {
        let err = PlpgsqlTriggerInvoker.invoke(&call(key)).unwrap_err();
        assert_eq!(err.wire_code(), "0A000", "{key}");
    }
    assert_eq!(host.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(PlpgsqlTriggerInvoker.invoke(&call("#1.t")).is_ok());
    assert_eq!(host.0.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// 本番の invoker はホスト未束縛（`TriggerRuntime::bind` を通らない経路）では固定文言の `0A000`
// （Issue #3184。黙って飛ばさず、ホスト無しで本体を実行もしない）。
//
// intentional: docs/design/plpgsql-subset.md
// 未束縛の経路の拒否は fail-closed の安全弁（設計記録「Issue #3184 追記」）。
#[test]
fn production_invoker_without_host_is_rejected() {
    handle!(h);
    let rt = TriggerRuntime::with_invoker(Arc::new(super::call::PlpgsqlTriggerInvoker));
    let err =
        fire_before_row_insert(&rt, &h, "t", &schema(), &[trig("a", None)], row()).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

/// `WHEN` の評価結果を条件テキストで決め、評価回数・関数の呼び出し回数を数えるテスト用 invoker
/// （Issue #3190。`"true"`＝TRUE、`"false"`＝FALSE（NULL も同じ不発火）、`"err"`＝評価エラー）。
struct WhenInvoker {
    evals: std::sync::atomic::AtomicUsize,
    calls: std::sync::atomic::AtomicUsize,
}

impl WhenInvoker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            evals: std::sync::atomic::AtomicUsize::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

impl TriggerFunctionInvoker for Arc<WhenInvoker> {
    fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut r = call.new_row.to_vec();
        if let Some(Value::Text(t)) = r.get_mut(1) {
            t.push_str(call.trigger_name);
        }
        Ok(TriggerOutcome::Proceed(r))
    }

    fn eval_when(&self, call: &TriggerCall<'_>, when_sql: &str) -> Result<bool, SqlSurfaceError> {
        self.evals.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 評価に渡る NEW は、前のトリガが書き換えた最新の行（連鎖の検証用に文字列を観測する）。
        if when_sql == "sees_ab" {
            return Ok(matches!(call.new_row.get(1), Some(Value::Text(t)) if t == "vab"));
        }
        match when_sql {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(SqlSurfaceError::feature_not_supported("boom")),
        }
    }
}

#[test]
fn before_when_true_fires_false_skips_to_next_and_error_propagates() {
    handle!(h);
    let inv = WhenInvoker::new();
    let rt = TriggerRuntime::with_invoker(Arc::new(inv.clone()));
    let triggers = [
        trig("a", Some("true")),
        trig("b", Some("false")),
        trig("c", None),
    ];
    let out = fire_before_row_insert(&rt, &h, "t", &schema(), &triggers, row())
        .expect("fire")
        .expect("row");
    // b は不発火（関数を呼ばず次へ。NEW はそのまま渡る）。a と c だけが関数を呼ぶ。
    assert_eq!(out[1], Value::Text("vac".to_string()));
    assert_eq!(inv.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(inv.evals.load(std::sync::atomic::Ordering::SeqCst), 2);
    // 評価エラーはそのまま伝わる（黙って不発火にしない）。
    let err = fire_before_row_insert(&rt, &h, "t", &schema(), &[trig("a", Some("err"))], row())
        .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
}

// BEFORE の連鎖では、WHEN は前のトリガが書き換えた最新の NEW で評価される（D7）。
#[test]
fn before_when_sees_the_rewritten_new() {
    handle!(h);
    let inv = WhenInvoker::new();
    let rt = TriggerRuntime::with_invoker(Arc::new(inv.clone()));
    let triggers = [trig("a", None), trig("b", None), trig("c", Some("sees_ab"))];
    let out = fire_before_row_insert(&rt, &h, "t", &schema(), &triggers, row())
        .expect("fire")
        .expect("row");
    assert_eq!(out[1], Value::Text("vabc".to_string()));
}

// 評価を結線していない invoker（既定の `eval_when`）は WHEN 付きで固定文言の `0A000`（fail-closed）。
#[test]
fn invoker_without_when_support_is_0a000() {
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
fn after_when_true_fires_and_false_does_not() {
    handle!(h);
    let inv = WhenInvoker::new();
    let rt = TriggerRuntime::with_invoker(Arc::new(inv.clone()));
    fire_after_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[
            after_trig("a", Some("true")),
            after_trig("b", Some("false")),
            after_trig("c", None),
        ],
        &row(),
    )
    .expect("fire");
    assert_eq!(inv.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let err = fire_after_row_insert(
        &rt,
        &h,
        "t",
        &schema(),
        &[after_trig("a", Some("err"))],
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
