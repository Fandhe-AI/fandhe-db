//! NoSQL（HTTP）表層の書き込み入口（`execute_bound_*_in_session`）でのトリガ発火の結合テスト
//! （TABLE-45・Issue #3282）。
//!
//! 入口は wire-server の HTTP ハンドラ（`http::query::{insert,update,delete}`）が呼ぶ。ここでは
//! その入口を直接呼び、SQL 表層と同じ実行器・同じ発火計画に載ること（BEFORE の書き換え・
//! `RETURN NULL`・AFTER の呼び出し順・影響行数・`operation_id` 再送・NOT NULL の後回し）を、
//! 同じ注入 invoker で SQL 表層と並べて固定する。未結線の形（RETURNING・分割実行・生の行 API）は
//! `0A000` のままであることも固定する。設計記録は `docs/design/trigger-execution-model.md`。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{
    add_trigger, core_with, count, ctx, read_rows, run, trigger_def, Behavior,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{InsertLiteral, SetValue, ValidatedUpdate, WherePredicate};
use crate::sql::mode::SessionState;
use crate::sql::parser::{bind_update, BoundDelete, BoundInsert};
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

const INS: u8 = TriggerEvents::INSERT;
const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;

type Log = Arc<Mutex<Vec<String>>>;

fn dml_trigger(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut def = trigger_def(name, timing, level);
    def.events = TriggerEvents::from_bits(events).expect("events");
    def
}

fn n_of(row: &[Value]) -> i64 {
    match row.get(2) {
        Some(Value::BigInt(n)) => *n,
        _ => -1,
    }
}

/// 呼び出しを `TG_WHEN TG_OP name old->new` の形で記録し、行は変えない。
fn recording(log: &Log) -> Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        let old = c.old_row.map_or("-".to_string(), |r| n_of(r).to_string());
        let new = if c.new_row.is_empty() {
            "-".to_string()
        } else {
            n_of(c.new_row).to_string()
        };
        log.lock().expect("log").push(format!(
            "{} {} {} {}->{}",
            c.tg_when(),
            c.tg_op(),
            c.trigger_name,
            old,
            new
        ));
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

fn op(s: &str) -> OperationId {
    OperationId::parse(s).expect("op")
}

fn nosql_insert(
    core: &EngineCore,
    id: u64,
    n: i64,
    operation_id: &OperationId,
) -> Result<u64, crate::sql::allowlist::SqlSurfaceError> {
    core.execute_bound_insert_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        "trg",
        1,
        Some(operation_id),
        |_| {
            Ok(vec![BoundInsert {
                table: "trg".to_string(),
                id,
                values: vec![
                    Value::Vector(vec![1.0, 0.0]),
                    Value::Text("x".to_string()),
                    Value::BigInt(n),
                ],
                operation_id: Some(operation_id.clone()),
                volatile_default_columns: Vec::new(),
                id_omitted: false,
            }])
        },
    )
    .map(|o| o.rows_affected)
}

fn nosql_update_by_id(
    core: &EngineCore,
    id: u64,
    column: &str,
    literal: InsertLiteral,
    operation_id: &OperationId,
) -> Result<u64, crate::sql::allowlist::SqlSurfaceError> {
    let operation_id = operation_id.clone();
    let column = column.to_string();
    core.execute_bound_update_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        "trg",
        &[column.as_str()],
        Some(&operation_id),
        |schema| {
            let validated = ValidatedUpdate {
                table_name: "trg".to_string(),
                assignments: vec![(column.clone(), SetValue::Literal(literal))],
                id_literal: id.to_string(),
                operation_id: Some(operation_id.clone()),
                returning: None,
            };
            bind_update(&validated, schema)
        },
    )
    .map(|o| o.rows_affected)
}

fn nosql_delete_by_id(
    core: &EngineCore,
    id: u64,
    operation_id: &OperationId,
) -> Result<u64, crate::sql::allowlist::SqlSurfaceError> {
    core.execute_bound_delete_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        &BoundDelete {
            table: "trg".to_string(),
            id,
            operation_id: Some(operation_id.clone()),
        },
    )
    .map(|o| o.rows_affected)
}

fn eq_a(value: &str) -> WherePredicate {
    WherePredicate::Equality {
        column: "a".to_string(),
        value: value.to_string(),
    }
}

fn seed(core: &EngineCore) {
    run(
        core,
        &ctx("tenant-a"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
         (2, '[2.0,0.0]', 'y', 20), (3, '[3.0,0.0]', 'z', 30)",
    )
    .expect("seed");
}

// 単一行 insert は BEFORE・AFTER を呼び、再送ではトリガを呼ばない（22023 も未呼び出し）。
#[test]
fn nosql_insert_fires_and_replay_skips_triggers() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, calls) = core_with(dir.path(), recording(&log));
    for (name, timing) in [("b", TriggerTiming::Before), ("c", TriggerTiming::After)] {
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &dml_trigger(name, timing, TriggerLevel::Row, INS),
        );
    }
    let o = op("nosql-ins-1");
    assert_eq!(nosql_insert(&core, 1, 5, &o).expect("insert"), 1);
    assert_eq!(
        take(&log),
        vec!["BEFORE INSERT b -->5", "AFTER INSERT c -->5"]
    );
    // 同一内容の再送は台帳が検出し（23505）、トリガは呼ばれない。
    let err = nosql_insert(&core, 1, 5, &o).expect_err("replay");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let err = nosql_insert(&core, 1, 6, &o).expect_err("conflict");
    assert_eq!(err.wire_code(), "22023");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(count(&core, &ctx("tenant-a"), "trg"), 1);
}

// BEFORE が NULL を返した行は保存されない（複数行 `rows[]`。影響行数にも数えない）。
#[test]
fn nosql_insert_batch_skips_row_when_before_returns_null() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| {
            if n_of(c.new_row) == 2 {
                Ok(TriggerOutcome::Skip)
            } else {
                Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
            }
        }),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, INS),
    );
    let o = op("nosql-ins-batch");
    let row = |id: u64, n: i64| BoundInsert {
        table: "trg".to_string(),
        id,
        values: vec![
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("x".to_string()),
            Value::BigInt(n),
        ],
        operation_id: Some(o.clone()),
        volatile_default_columns: Vec::new(),
        id_omitted: false,
    };
    let out = core
        .execute_bound_insert_in_session(
            &ctx("tenant-a"),
            &SessionState::default(),
            "trg",
            2,
            Some(&o),
            |_| Ok(vec![row(1, 1), row(2, 2)]),
        )
        .expect("batch");
    assert_eq!(out.rows_affected, 1);
    assert_eq!(
        read_rows(&core, &ctx("tenant-a"), "trg"),
        vec![(1, "x".to_string(), 1)]
    );
}

// 単一行 update・delete（id 指定）は BEFORE の書き換えと AFTER の OLD／NEW を SQL 表層と同じに扱う。
#[test]
fn nosql_single_row_update_and_delete_fire() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    seed(&core);
    for (name, timing) in [("b", TriggerTiming::Before), ("c", TriggerTiming::After)] {
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &dml_trigger(name, timing, TriggerLevel::Row, UPD | DEL),
        );
    }
    let _ = take(&log);
    let n = InsertLiteral::Number("11".to_string());
    assert_eq!(
        nosql_update_by_id(&core, 1, "n", n, &op("u1")).expect("update"),
        1
    );
    assert_eq!(
        take(&log),
        vec!["BEFORE UPDATE b 10->11", "AFTER UPDATE c 10->11"]
    );
    assert_eq!(nosql_delete_by_id(&core, 2, &op("d1")).expect("delete"), 1);
    assert_eq!(
        take(&log),
        vec!["BEFORE DELETE b 20->-", "AFTER DELETE c 20->-"]
    );
    // 不在 id は 0 件でトリガ未呼び出し。
    assert_eq!(nosql_delete_by_id(&core, 99, &op("d2")).expect("none"), 0);
    assert!(take(&log).is_empty());
}

// 述語形 update・delete は 0A000 にならず発火し、同じ状態への SQL 表層と結果・呼び出し記録が一致する。
#[test]
fn nosql_predicate_paths_match_sql_surface() {
    let nosql_dir = tempdir();
    let sql_dir = tempdir();
    let nosql_log: Log = Arc::default();
    let sql_log: Log = Arc::default();
    let (nosql, _) = core_with(nosql_dir.path(), recording(&nosql_log));
    let (sql, _) = core_with(sql_dir.path(), recording(&sql_log));
    for core in [&nosql, &sql] {
        seed(core);
        for (name, timing) in [("b", TriggerTiming::Before), ("c", TriggerTiming::After)] {
            add_trigger(
                &core.storage,
                "tenant-a",
                "trg",
                &dml_trigger(name, timing, TriggerLevel::Row, UPD | DEL),
            );
        }
    }
    let _ = (take(&nosql_log), take(&sql_log));
    let a = ctx("tenant-a");

    let upd = nosql
        .execute_bound_predicate_update_in_session(
            &a,
            &SessionState::default(),
            "trg",
            Some(&op("pu1")),
            |_| {
                Ok((
                    vec![("n".to_string(), InsertLiteral::Number("1".to_string()))],
                    vec![eq_a("x")],
                ))
            },
        )
        .expect("predicate update fires");
    run(
        &sql,
        &a,
        "UPDATE trg SET n = 1 WHERE a = 'x' USING OPERATION_ID 'pu1'",
    )
    .expect("sql update");
    assert_eq!(upd.rows_affected, 1);
    assert_eq!(take(&nosql_log), take(&sql_log));

    let del = nosql
        .execute_bound_predicate_delete_in_session(
            &a,
            &SessionState::default(),
            "trg",
            Some(&op("pd1")),
            |_| Ok(vec![eq_a("y")]),
        )
        .expect("predicate delete fires");
    run(
        &sql,
        &a,
        "DELETE FROM trg WHERE a = 'y' USING OPERATION_ID 'pd1'",
    )
    .expect("sql delete");
    assert_eq!(del.rows_affected, 1);
    assert_eq!(take(&nosql_log), take(&sql_log));
    assert_eq!(read_rows(&nosql, &a, "trg"), read_rows(&sql, &a, "trg"));

    // 再送は台帳が検出し（23505）、トリガは呼ばれない。
    let err = nosql
        .execute_bound_predicate_delete_in_session(
            &a,
            &SessionState::default(),
            "trg",
            Some(&op("pd1")),
            |_| Ok(vec![eq_a("y")]),
        )
        .expect_err("replay");
    assert_eq!(err.wire_code(), "23505");
    assert!(take(&nosql_log).is_empty());
}

fn fill_null_n() -> Behavior {
    Box::new(|c| {
        let mut row = c.new_row.to_vec();
        if c.tg_when() == "BEFORE" && matches!(row.get(2), Some(Value::Null)) {
            if let Some(slot) = row.get_mut(2) {
                *slot = Value::BigInt(77);
            }
        }
        Ok(TriggerOutcome::Proceed(row))
    })
}

// NOT NULL 検査の後回しは SQL 表層と同じ。BEFORE があれば最終 NEW で判定し、AFTER のみなら束縛時に
// 23502（対象行の有無で応答が分かれない）。
#[test]
fn nosql_single_row_update_defers_not_null_like_sql() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), fill_null_n());
    seed(&core);
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, TriggerLevel::Row, UPD),
    );
    for id in [1u64, 999] {
        let err = nosql_update_by_id(&core, id, "n", InsertLiteral::Null, &op("nn-a"))
            .expect_err("after-only must reject at bind");
        assert_eq!(err.wire_code(), "23502");
    }
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, UPD),
    );
    assert_eq!(
        nosql_update_by_id(&core, 1, "n", InsertLiteral::Null, &op("nn-b")).expect("deferred"),
        1
    );
    assert_eq!(
        nosql_update_by_id(&core, 999, "n", InsertLiteral::Null, &op("nn-c")).expect("none"),
        0
    );
    assert!(read_rows(&core, &a, "trg").contains(&(1, "x".to_string(), 77)));
}

/// `n`（NOT NULL）を省略した NoSQL insert を、HTTP ハンドラと同じ `bind_insert_identity` で束縛して流す。
fn nosql_insert_omitting_n(
    core: &EngineCore,
    id: u64,
    operation_id: &OperationId,
) -> Result<u64, crate::sql::allowlist::SqlSurfaceError> {
    core.execute_bound_insert_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        "trg",
        1,
        Some(operation_id),
        |schema| {
            let validated = crate::sql::allowlist::ValidatedInsert {
                table_name: "trg".to_string(),
                columns: vec!["id".to_string(), "embedding".to_string(), "a".to_string()],
                columns_omitted: false,
                rows: vec![vec![
                    InsertLiteral::Number(id.to_string()),
                    InsertLiteral::Vector(vec![1.0, 0.0]),
                    InsertLiteral::String("x".to_string()),
                ]],
                operation_id: Some(operation_id.clone()),
                returning: None,
                on_conflict: None,
            };
            crate::sql::parser::bind_insert_identity(&validated, schema).map(|b| vec![b])
        },
    )
    .map(|o| o.rows_affected)
}

// NoSQL insert の NOT NULL 検査は SQL 行 INSERT と同じく書き込み段へ延期する（Issue #3282）。
// BEFORE ROW が省略列を補えば成功し、補われない・トリガ無しなら 23502 のまま（fail-closed）。
#[test]
fn nosql_insert_defers_not_null_so_before_row_can_fill() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), fill_null_n());
    let a = ctx("tenant-a");
    // トリガ無し: 書き込み段で 23502。
    let err = nosql_insert_omitting_n(&core, 1, &op("nn-i0")).expect_err("no trigger");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count(&core, &a, "trg"), 0);
    // AFTER のみ: 補えないので 23502。
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, TriggerLevel::Row, INS),
    );
    let err = nosql_insert_omitting_n(&core, 2, &op("nn-i1")).expect_err("after only");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(count(&core, &a, "trg"), 0);
    // BEFORE ROW が補う。
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, INS),
    );
    assert_eq!(
        nosql_insert_omitting_n(&core, 3, &op("nn-i2")).expect("filled"),
        1
    );
    assert!(read_rows(&core, &a, "trg").contains(&(3, "x".to_string(), 77)));
}

// 未結線の形（NoSQL の RETURNING）は従来どおり 0A000 のまま。トリガは呼ばれない。
// tracked: #3189
#[test]
fn nosql_returning_with_triggers_stays_unsupported() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), recording(&Arc::default()));
    seed(&core);
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::Before, TriggerLevel::Row, UPD),
    );
    let o = op("ret-1");
    let projection = crate::sql::allowlist::Projection::Columns(vec!["id".to_string()]);
    let err = core
        .execute_bound_update_returning_in_session(
            &ctx("tenant-a"),
            &SessionState::default(),
            "trg",
            Some(&o),
            &projection,
            |schema| {
                let validated = ValidatedUpdate {
                    table_name: "trg".to_string(),
                    assignments: vec![(
                        "n".to_string(),
                        SetValue::Literal(InsertLiteral::Number("1".to_string())),
                    )],
                    id_literal: "1".to_string(),
                    operation_id: Some(o.clone()),
                    returning: None,
                };
                bind_update(&validated, schema)
            },
        )
        .expect_err("returning stays unwired");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
// ---------------------------------------------------------------------
// 本番 invoker（注入なし）での越境・原子性（TABLE-45・RLS-10。Issue #3282）
// ---------------------------------------------------------------------

fn sql_as(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
        .map(|_| ())
}

/// テナント SCHEMA `s` の `tt (n INTEGER)` を作り、NoSQL 入口へ渡す内部キー `#<id>.tt` を返す。
/// HTTP の `table` は裸の識別子だけなので、SCHEMA 表への書き込みは内部キーを直接渡して検証する。
fn schema_table(core: &EngineCore, tenant: &str) -> String {
    sql_as(core, tenant, "CREATE SCHEMA s").expect("schema");
    sql_as(core, tenant, "CREATE TABLE s.tt (n INTEGER)").expect("table");
    // `audit` は共有 public 表（2 テナント目の作成は重複で失敗するため結果を無視する）。
    let _ = sql_as(core, tenant, "CREATE TABLE audit (n INTEGER)");
    let id = core
        .storage
        .schema_id(tenant, "s")
        .expect("lookup")
        .expect("schema id");
    format!("#{id}.tt")
}

fn insert_key(
    core: &EngineCore,
    tenant: &str,
    key: &str,
    id: u64,
    n: i32,
    o: &OperationId,
) -> Result<u64, crate::sql::allowlist::SqlSurfaceError> {
    core.execute_bound_insert_in_session(
        &ctx(tenant),
        &SessionState::default(),
        key,
        1,
        Some(o),
        |_| {
            Ok(vec![BoundInsert {
                table: key.to_string(),
                id,
                values: vec![Value::Integer(n)],
                operation_id: Some(o.clone()),
                volatile_default_columns: Vec::new(),
                id_omitted: false,
            }])
        },
    )
    .map(|r| r.rows_affected)
}

fn open_default_core(dir: &std::path::Path) -> EngineCore {
    let storage = crate::storage::Storage::open(dir.join("db.redb")).expect("open storage");
    EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider))
}

fn audit_count(core: &EngineCore, tenant: &str) -> usize {
    match core
        .execute_sql_in_session(
            &ctx(tenant),
            &mut SessionState::default(),
            "SELECT n FROM audit",
        )
        .expect("audit select")
    {
        crate::sql::SqlOutcome::Query(q) => q.rows.len(),
        _ => panic!("expected query"),
    }
}

// 本番 invoker で NoSQL 入口の書き込みが発火し、本体の SQL は書き込みテナントの行にだけ作用する。
// 他テナントの監査行は見えず・変わらず、本体のエラーで文全体（本表・監査）が巻き戻る。
#[test]
fn production_invoker_fires_from_nosql_entry_within_tenant_boundary() {
    let dir = tempdir();
    let core = open_default_core(dir.path());
    let key1 = schema_table(&core, "t1");
    let key2 = schema_table(&core, "t2");
    sql_as(
        &core,
        "t1",
        "CREATE FUNCTION aud() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (n) VALUES (NEW.n); RETURN NEW; END $$",
    )
    .expect("fn");
    sql_as(
        &core,
        "t1",
        "CREATE TRIGGER ta AFTER INSERT ON s.tt FOR EACH ROW EXECUTE FUNCTION aud()",
    )
    .expect("trigger");

    assert_eq!(
        insert_key(&core, "t1", &key1, 1, 7, &op("p1")).expect("insert"),
        1
    );
    assert_eq!(audit_count(&core, "t1"), 1);
    assert_eq!(audit_count(&core, "t2"), 0);

    // t2 の同名表にはトリガが無く、t1 のトリガの存在で応答は変わらない。
    assert_eq!(
        insert_key(&core, "t2", &key2, 1, 9, &op("p2")).expect("insert"),
        1
    );
    assert_eq!(audit_count(&core, "t1"), 1);
    assert_eq!(audit_count(&core, "t2"), 0);

    // 本体が許可リスト外の文を含むと `0A000` で文全体が巻き戻る（本表にも監査にも残らない）。
    sql_as(
        &core,
        "t1",
        "CREATE FUNCTION bad() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO audit (n) VALUES (50); INSERT INTO audit (n) SELECT 51; RETURN NEW; END $$",
    )
    .expect("fn bad");
    sql_as(
        &core,
        "t1",
        "CREATE TRIGGER tb BEFORE UPDATE ON s.tt FOR EACH ROW EXECUTE FUNCTION bad()",
    )
    .expect("trigger bad");
    let o = op("p3");
    let err = core
        .execute_bound_update_in_session(
            &ctx("t1"),
            &SessionState::default(),
            &key1,
            &["n"],
            Some(&o),
            |schema| {
                bind_update(
                    &ValidatedUpdate {
                        table_name: key1.clone(),
                        assignments: vec![(
                            "n".to_string(),
                            SetValue::Literal(InsertLiteral::Number("8".to_string())),
                        )],
                        id_literal: "1".to_string(),
                        operation_id: Some(o.clone()),
                        returning: None,
                    },
                    schema,
                )
            },
        )
        .expect_err("body rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(audit_count(&core, "t1"), 1);
}
