//! BEFORE ROW INSERT トリガの発火の結合テスト（TABLE-45・Issue #1723）。
//!
//! トリガ関数の保存形式・PL/pgSQL 結線（Issue #2228）が未整備のため、SQL の `CREATE TRIGGER` は成功経路へ
//! 到達できない。本ファイルは (1) 定義をカタログへ直接注入し、(2) 呼び出し口を差し替えて、発火側の
//! 意味論（NEW の書き換え・連鎖・NULL 返却・制約検査との順序・台帳・経路ごとのガード・テナント境界）を
//! `EngineCore` に SQL を流す水準で固定する。SQL だけで通す E2E は #2228 の後で確かめ直す。
//! 設計記録は `docs/design/trigger-before-insert.md`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::EngineCore;
use crate::catalog::function::{
    FunctionAttributes, FunctionDef, FunctionLanguage, FunctionParam, FunctionType,
};
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::{TriggerCall, TriggerFunctionInvoker, TriggerOutcome};

pub(super) type Behavior =
    Box<dyn Fn(&TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> + Send + Sync>;

/// 振る舞いをクロージャで注入する呼び出し口（呼び出し回数を数える）。
pub(super) struct FnInvoker {
    pub(super) behavior: Behavior,
    pub(super) calls: Arc<AtomicUsize>,
}

impl TriggerFunctionInvoker for FnInvoker {
    fn invoke(&self, call: &TriggerCall<'_>) -> Result<TriggerOutcome, SqlSurfaceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.behavior)(call)
    }
}

pub(super) fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 列: embedding VECTOR(2) NOT NULL・a TEXT・n BIGINT NOT NULL（既定値なし）。
pub(super) fn trg_schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("a", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::BigInt, false),
        ],
    )
}

fn open_storage(dir: &std::path::Path) -> Storage {
    let storage = Storage::open(dir.join("db.redb")).expect("open storage");
    storage
        .create_table(&trg_schema("trg"))
        .expect("create trg");
    storage
        .create_table(
            &trg_schema("alloc").with_row_identity(crate::catalog::RowIdentity::Allocated),
        )
        .expect("create alloc");
    storage
        .create_table(&trg_schema("plain"))
        .expect("create plain");
    storage
}

pub(super) fn func(key: &str) -> FunctionDef {
    FunctionDef {
        id: 1,
        function_key: key.to_string(),
        params: Vec::<FunctionParam>::new(),
        return_type: Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty")),
        language: FunctionLanguage::Sql,
        body_source: "SELECT 1".to_string(),
        attrs: FunctionAttributes::default(),
    }
}

pub(super) fn trigger_def(name: &str, timing: TriggerTiming, level: TriggerLevel) -> TriggerDef {
    TriggerDef {
        id: 0,
        name: name.to_string(),
        timing,
        events: TriggerEvents::from_bits(TriggerEvents::INSERT).expect("events"),
        level,
        update_of: Vec::new(),
        function_key: "f".to_string(),
        tg_args: Vec::new(),
        when_sql: None,
    }
}

pub(super) fn add_trigger(storage: &Storage, scope: &str, table: &str, def: &TriggerDef) {
    // 関数は同じ scope の引数なし関数。2 つ目以降のトリガは既存の関数を再利用する。
    let _ = storage.create_function(scope, &func("f"));
    storage
        .create_trigger_for_test(scope, table, def)
        .expect("create trigger");
}

pub(super) fn before_row(name: &str) -> TriggerDef {
    trigger_def(name, TriggerTiming::Before, TriggerLevel::Row)
}

pub(super) fn core_with(
    dir: &std::path::Path,
    behavior: Behavior,
) -> (EngineCore, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = EngineCore::from_storage(
        open_storage(dir),
        Box::new(crate::kernel::CpuScalarProvider),
    )
    .with_trigger_invoker_for_test(Arc::new(FnInvoker {
        behavior,
        calls: Arc::clone(&calls),
    }));
    (core, calls)
}

pub(super) fn run(
    core: &EngineCore,
    c: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(c, &mut s, sql)
}

pub(super) fn rows_affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Insert(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        _ => panic!("expected insert outcome"),
    }
}

pub(super) fn count(core: &EngineCore, c: &PolicyContext, table: &str) -> u64 {
    let SqlOutcome::Query(q) =
        run(core, c, &format!("SELECT COUNT(*) FROM {table}")).expect("count")
    else {
        panic!("expected query");
    };
    match q.rows[0].cells[0] {
        Cell::Integer(v) => v,
        ref o => panic!("unexpected {o:?}"),
    }
}

/// `SELECT id, a, n` の結果を (id, a, n) の列で返す（id 昇順）。
pub(super) fn read_rows(
    core: &EngineCore,
    c: &PolicyContext,
    table: &str,
) -> Vec<(u64, String, i64)> {
    let SqlOutcome::Query(q) =
        run(core, c, &format!("SELECT id, a, n FROM {table} LIMIT 100")).expect("select")
    else {
        panic!("expected query");
    };
    let mut out: Vec<(u64, String, i64)> = q
        .rows
        .iter()
        .map(|r| {
            let id = match r.cells[0] {
                Cell::Integer(v) => v,
                ref o => panic!("id {o:?}"),
            };
            let a = match &r.cells[1] {
                Cell::Text(t) => t.clone(),
                o => panic!("a {o:?}"),
            };
            let n = match r.cells[2] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n {o:?}"),
            };
            (id, a, n)
        })
        .collect();
    out.sort();
    out
}

pub(super) fn text_of(row: &[Value], idx: usize) -> String {
    match row.get(idx) {
        Some(Value::Text(t)) => t.clone(),
        other => panic!("unexpected {other:?}"),
    }
}

/// `n` が NULL なら 7 を入れる補完トリガ。
fn fill_n() -> Behavior {
    Box::new(|call| {
        let mut row = call.new_row.to_vec();
        if matches!(row.get(2), Some(Value::Null)) {
            row[2] = Value::BigInt(7);
        }
        Ok(TriggerOutcome::Proceed(row))
    })
}

pub(super) const INSERT_A1: &str =
    "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x')";

// 対象ビヘイビア: TABLE-45（ポインタ）。BEFORE トリガが省略した NOT NULL 列を補える。
#[test]
fn before_trigger_fills_omitted_not_null_column() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), fill_n());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("fill"));

    let out = run(&core, &a, INSERT_A1).expect("insert");
    assert_eq!(rows_affected(out), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(read_rows(&core, &a, "trg"), vec![(1, "x".to_string(), 7)]);

    // 対照: トリガの無い表は従来どおり `23502`。
    let err = run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x')",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "23502");
}

// BEFORE が補わなかった非 nullable 列は従来どおり `23502`（`22000` ではない）で、行も残らない。
#[test]
fn unfilled_not_null_column_is_23502_without_side_effects() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("noop"));
    let err = run(&core, &a, INSERT_A1).unwrap_err();
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 連鎖: 名前のバイト順に呼ばれ、後のトリガは前のトリガが返した NEW を受け取る。
#[test]
fn triggers_chain_in_name_order_passing_new() {
    let dir = tempdir();
    let (core, calls) = core_with(
        dir.path(),
        Box::new(|c| {
            let mut row = c.new_row.to_vec();
            row[1] = Value::Text(format!("{}|{}", text_of(&row, 1), c.trigger_name));
            row[2] = Value::BigInt(1);
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    // 作成順を名前順と逆にして、カタログ順（名前のバイト順）で呼ばれることを確かめる。
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b_second"));
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("a_first"));
    run(&core, &a, INSERT_A1).expect("insert");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        read_rows(&core, &a, "trg"),
        vec![(1, "x|a_first|b_second".to_string(), 1)]
    );
}

// NULL 返却: その行は書かれず後続のトリガも呼ばれない。`rows_affected` は書いた行数。
#[test]
fn null_return_skips_row_and_later_triggers() {
    let dir = tempdir();
    let (core, calls) = core_with(
        dir.path(),
        Box::new(|c| {
            if text_of(c.new_row, 1) == "skip" {
                return Ok(TriggerOutcome::Skip);
            }
            let mut row = c.new_row.to_vec();
            row[2] = Value::BigInt(1);
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("a_first"));
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b_second"));
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'keep'), \
         (2, '[2.0,0.0]', 'skip'), (3, '[3.0,0.0]', 'keep')",
    )
    .expect("insert");
    assert_eq!(rows_affected(out), 2);
    // keep 行は 2 トリガ × 2 行、skip 行は最初のトリガだけ。
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    let rows = read_rows(&core, &a, "trg");
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![1, 3]);
}

// 全行スキップでも台帳は記録され、同じ operation_id の再送は `23505`（トリガは呼ばれない）。
#[test]
fn all_rows_skipped_still_records_ledger() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), Box::new(|_| Ok(TriggerOutcome::Skip)));
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("skip_all"));
    let sql = "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x') \
               USING OPERATION_ID 'op-skip'";
    let out = run(&core, &a, sql).expect("first");
    assert_eq!(rows_affected(out), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let err = run(&core, &a, sql).unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 台帳の照合は書き換え前の値で行う。BEFORE が値を書き換えても、同じ文の再送は `22023` ではなく `23505`。
#[test]
fn ledger_hash_uses_pre_trigger_values() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), fill_n());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("fill"));
    let sql = "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x') \
               USING OPERATION_ID 'op-hash'";
    run(&core, &a, sql).expect("first");
    let err = run(&core, &a, sql).unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&core, &a, "trg"), 1);
}

// 返却行の構造違い（列数）は `42804`・副作用ゼロ。
#[test]
fn malformed_returned_row_is_42804() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|_| Ok(TriggerOutcome::Proceed(vec![Value::Null]))),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("bad"));
    let err = run(&core, &a, INSERT_A1).unwrap_err();
    assert_eq!(err.wire_code(), "42804");
    assert_eq!(count(&core, &a, "trg"), 0);
}

// トリガが返したエラーは SQLSTATE を保って透過し、autocommit では行も台帳も残らない。
#[test]
fn trigger_error_is_transparent_and_atomic() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|_| {
            Err(SqlSurfaceError::InvalidInput {
                detail: "rejected".to_string(),
            })
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("boom"));
    let sql = "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x') \
               USING OPERATION_ID 'op-boom'";
    let err = run(&core, &a, sql).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 一意性の違反は従来の SQLSTATE（`23505`）。
#[test]
fn duplicate_id_is_23505() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), fill_n());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("fill"));
    run(&core, &a, INSERT_A1).expect("first");
    let err = run(&core, &a, INSERT_A1).unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count(&core, &a, "trg"), 1);
}

// 呼び出し口が未結線（既定）なら、トリガのある表への INSERT は固定文言の `0A000`・副作用ゼロ。
#[test]
fn unwired_invoker_is_0a000() {
    let dir = tempdir();
    let core = EngineCore::from_storage(
        open_storage(dir.path()),
        Box::new(crate::kernel::CpuScalarProvider),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("t"));
    let err = run(&core, &a, INSERT_A1).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 文トリガ（INSERT）がある表への SQL INSERT は、結線するまで黙って発火させず `0A000`（AFTER ROW は Issue #1724 で結線済み）。
#[test]
fn statement_triggers_are_0a000() {
    for (timing, level) in [
        (TriggerTiming::After, TriggerLevel::Statement),
        (TriggerTiming::Before, TriggerLevel::Statement),
    ] {
        let dir = tempdir();
        let (core, calls) = core_with(dir.path(), fill_n());
        let a = ctx("tenant-a");
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &trigger_def("t", timing, level),
        );
        let err = run(&core, &a, INSERT_A1).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(count(&core, &a, "trg"), 0);
    }
}

// 発火に未対応の経路（RETURNING・生の型付き挿入・UPSERT）は、INSERT トリガのある表で `0A000`。
// 台帳は消費されない（同じ operation_id を、トリガを消した後の経路で使える）。
#[test]
fn unwired_paths_are_guarded_without_consuming_ledger() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), fill_n());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("fill"));

    let err = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         RETURNING id USING OPERATION_ID 'op-ret'",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");

    let err = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-ups'",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");

    let op = crate::recovery::required_op_id::OperationId::parse("op-raw").expect("op");
    let err = crate::tenant::insert_typed_row(
        &core.storage,
        "trg",
        &a,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("x".into()),
            Value::BigInt(1),
        ],
        &op,
    )
    .unwrap_err();
    assert_eq!(
        crate::error_format::ClassifiedError::error_class(&err),
        crate::error_format::ErrorClass::FeatureNotSupported
    );

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 0);
    // 台帳が消費されていないので、同じ operation_id を別の（発火する）経路で使える。
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a) VALUES (1, '[1.0,0.0]', 'x') \
         USING OPERATION_ID 'op-ret'",
    )
    .expect("operation id is not consumed by the guarded path");
}

// テナント境界: テナント A のトリガは、同じ共有表へのテナント B の INSERT では呼ばれず、B の書き込みが
// A のトリガの存在で拒否されることもない。
#[test]
fn trigger_fires_only_for_the_defining_tenant() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), fill_n());
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("fill"));

    // B: トリガなし。省略した NOT NULL 列は従来どおり `23502`（呼び出し 0 回）。
    let err = run(&core, &b, INSERT_A1).unwrap_err();
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    run(
        &core,
        &b,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'b', 5)",
    )
    .expect("tenant b insert");
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // A: 発火する。
    run(&core, &a, INSERT_A1).expect("tenant a insert");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(read_rows(&core, &a, "trg"), vec![(1, "x".to_string(), 7)]);
    assert_eq!(read_rows(&core, &b, "trg"), vec![(1, "b".to_string(), 5)]);
}

// NEW に入るのは利用者の列だけ（テナントのキー・rowid・可視性・`id` 疑似列を含めない）。
#[test]
fn new_row_has_only_user_columns() {
    let dir = tempdir();
    let widths = Arc::new(AtomicUsize::new(0));
    let w = Arc::clone(&widths);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c| {
            w.store(c.new_row.len(), Ordering::SeqCst);
            let mut row = c.new_row.to_vec();
            row[2] = Value::BigInt(1);
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("w"));
    run(&core, &a, INSERT_A1).expect("insert");
    assert_eq!(widths.load(Ordering::SeqCst), 3);
}

// `INSERT ... SELECT` も BEFORE ROW トリガを発火し、書き換えた値で保存する。省略した NOT NULL 列は
// SELECT 経路では束縛時に拒否される（BEFORE で補えない。PostgreSQL との差分として設計記録に残す）。
#[test]
fn insert_select_fires_before_triggers() {
    let dir = tempdir();
    let (core, calls) = core_with(
        dir.path(),
        Box::new(|c| {
            let mut row = c.new_row.to_vec();
            row[1] = Value::Text(format!("{}!", text_of(&row, 1)));
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'p1', 1), \
         (2, '[2.0,0.0]', 'p2', 2)",
    )
    .expect("seed plain");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("bang"));
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) SELECT id, embedding, a, n FROM plain LIMIT 100 \
         USING OPERATION_ID 'op-sel'",
    )
    .expect("insert select");
    assert_eq!(rows_affected(out), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        read_rows(&core, &a, "trg"),
        vec![(1, "p1!".to_string(), 1), (2, "p2!".to_string(), 2)]
    );
}

// 採番方式の表で `operation_id` 省略かつ全行スキップでも、払い出し済みの rowid は欠番になる
// （Autocommit が `NoOp` で abort して採番状態を巻き戻し、次の INSERT が再利用しない）。
#[test]
fn all_rows_skipped_without_operation_id_leaves_rowid_gap() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| {
            if text_of(c.new_row, 1) == "skip" {
                return Ok(TriggerOutcome::Skip);
            }
            let mut row = c.new_row.to_vec();
            row[2] = Value::BigInt(1);
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "alloc", &before_row("t"));
    let out = run(
        &core,
        &a,
        "INSERT INTO alloc (embedding, a) VALUES ('[1.0,0.0]', 'skip')",
    )
    .expect("skipped insert");
    assert_eq!(rows_affected(out), 0);
    // skip 行が rowid 1 を払い出し済みなら、次の払い出しは 2（abort で巻き戻れば 1 になる）。
    let txn = core.storage.begin_write_txn().expect("txn");
    let next = crate::catalog::rowid_alloc::allocate_in_txn(&txn, "tenant-a", "alloc", 1)
        .expect("allocate");
    assert_eq!(next.start().raw_for_state(), 2, "skipped rowid was reused");
}
// 複数行 INSERT で SEQUENCE 値を払い出した後に全行を `NULL` 返却でスキップしても、払い出し済みの値は
// 欠番になる（`NoOp` の abort で SEQUENCE の永続状態が巻き戻り、次の nextval が同じ値を再利用しない）。
#[test]
fn multi_row_all_skipped_keeps_sequence_state() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), Box::new(|_| Ok(TriggerOutcome::Skip)));
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("skip"));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&a, &mut s, "CREATE SEQUENCE s")
        .expect("create sequence");
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES \
         (1, '[1.0,0.0]', 'x', nextval('s')), (2, '[1.0,0.0]', 'y', nextval('s'))",
    )
    .expect("skipped insert");
    assert_eq!(rows_affected(out), 0);
    run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'z', nextval('s'))",
    )
    .expect("plain insert");
    assert_eq!(
        read_rows(&core, &a, "plain"),
        vec![(1, "z".to_string(), 3)],
        "skipped sequence values were reused"
    );
}

// ファイル形 INSERT（`replace_typed_rows_by_text_key`）は発火に未対応のため、INSERT トリガのある表では
// 台帳・行に触れる前に `0A000` で拒否する（黙ってトリガを飛ばさない）。
#[test]
fn file_shaped_insert_is_guarded_for_trigger_tables() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), fill_n());
    let core = core.with_embedder(Box::new(
        crate::embedding::HashingEmbedder::new(8).expect("dim"),
    ));
    let a = ctx("tenant-a");
    core.storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(8), false),
                ColumnDef::new("path", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create docs");
    add_trigger(&core.storage, "tenant-a", "docs", &before_row("t"));
    let err = core
        .execute_insert_sql(
            &a,
            "INSERT INTO docs (path, body) VALUES ('a.txt', 'hello world') \
             USING OPERATION_ID 'op-file'",
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "0A000", "{err:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "docs"), 0);
}
