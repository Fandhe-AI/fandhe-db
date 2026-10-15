//! 現在時刻関数（`now()`・`transaction_timestamp()`・`statement_timestamp()`・
//! `clock_timestamp()`・`CURRENT_TIMESTAMP`・`CURRENT_DATE`・`LOCALTIMESTAMP`）の結合テスト
//! （Issue #1586、対象ビヘイビア: SQL-37。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! `EngineCore::execute_sql_in_session`／`execute_sql_in_txn`（production 経路そのもの）で、
//! トランザクション内固定・文ごと確定・呼び出しごと評価の 3 区分と、時計を確定しない書き込み系
//! 文脈（INSERT／UPDATE／DELETE／CHECK／DEFAULT／VIEW 本文）の fail-closed 拒否を検証する。
//! 時刻の絶対値には依存せず、同値・単調性・型・エラーコードだけを見る（脆いテストにしない）。

use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "events";

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql37-current-time");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("at", ColumnType::Timestamp, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for (id, at) in [(1, "2000-01-01 00:00:00"), (2, "2999-01-01 00:00:00")] {
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, at) VALUES \
                 ({id}, '[0.1,0.2]', 'ja', '{at}') USING OPERATION_ID 'op-{id}'"
            ),
        )
        .expect("insert");
    }
    (core, guard)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn query(outcome: SqlOutcome) -> engine::sql::exec::QueryResult {
    match outcome {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

fn ts(cell: &Cell) -> i64 {
    match cell {
        Cell::Timestamp(v) => *v,
        other => panic!("expected Cell::Timestamp, got {other:?}"),
    }
}

/// autocommit で 1 文実行し、先頭行の全セルを返す。
fn first_row(core: &EngineCore, sql: &str) -> Vec<Cell> {
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let r = query(
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}")),
    );
    r.rows[0].cells.clone()
}

fn code(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

/// DDL 権限を許可したセッションで 1 文を実行し、拒否コードを返す。
fn ddl_code(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

fn sleep_a_bit() {
    std::thread::sleep(Duration::from_millis(15));
}

fn read_in_txn<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'a>,
    sql: &str,
) -> Vec<Cell> {
    let ctx = ctx_for("tenant-a");
    query(
        core.execute_sql_in_txn(&ctx, session, txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}")),
    )
    .rows[0]
        .cells
        .clone()
}

const NOW_ALL: &str = "SELECT now(), transaction_timestamp(), CURRENT_TIMESTAMP, \
     LOCALTIMESTAMP, statement_timestamp(), clock_timestamp() FROM events WHERE id = 1 LIMIT 10";

#[test]
fn txn_start_functions_are_fixed_across_statements_in_a_transaction() {
    let (core, _g) = new_core();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let ctx = ctx_for("tenant-a");
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let first = read_in_txn(&core, &mut session, &mut txn, NOW_ALL);
    sleep_a_bit();
    let second = read_in_txn(&core, &mut session, &mut txn, NOW_ALL);
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    // now()・transaction_timestamp()・CURRENT_TIMESTAMP・LOCALTIMESTAMP は txn 内で固定。
    let fixed = ts(&first[0]);
    for i in 0..4 {
        assert_eq!(ts(&first[i]), fixed, "column {i} (1st stmt)");
        assert_eq!(ts(&second[i]), fixed, "column {i} (2nd stmt)");
    }
    // statement_timestamp() は文ごとに確定する（2 文目は 1 文目より後）。
    assert!(ts(&second[4]) > ts(&first[4]));
    // 1 文内では statement_timestamp() が txn 開始以降、clock_timestamp() は文の開始以降。
    assert!(ts(&first[4]) >= fixed);
    assert!(ts(&first[5]) >= ts(&first[4]));
    assert!(ts(&second[5]) > ts(&first[5]));
}

#[test]
fn autocommit_statement_time_is_fixed_within_a_statement_and_advances_between_statements() {
    let (core, _g) = new_core();
    let a = first_row(&core, NOW_ALL);
    // autocommit では txn 開始時刻 = 文の開始時刻。
    assert_eq!(ts(&a[0]), ts(&a[4]));
    for i in 1..4 {
        assert_eq!(ts(&a[i]), ts(&a[0]));
    }
    sleep_a_bit();
    let b = first_row(&core, NOW_ALL);
    assert!(ts(&b[0]) > ts(&a[0]));
    assert!(ts(&b[4]) > ts(&a[4]));
}

#[test]
fn clock_timestamp_is_evaluated_per_call_and_non_decreasing_across_rows() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id, clock_timestamp(), now() FROM events WHERE id > 0 LIMIT 10",
        )
        .expect("select"),
    );
    assert!(r.rows.len() >= 2);
    let clocks: Vec<i64> = r.rows.iter().map(|row| ts(&row.cells[1])).collect();
    assert!(clocks.windows(2).all(|w| w[0] <= w[1]));
    let nows: Vec<i64> = r.rows.iter().map(|row| ts(&row.cells[2])).collect();
    assert!(nows.windows(2).all(|w| w[0] == w[1]));
    // 行をまたいでも now() は文の時刻、clock_timestamp() はそれ以降。
    assert!(clocks[0] >= nows[0]);
}

#[test]
fn current_date_is_a_date_and_precision_form_rounds() {
    let (core, _g) = new_core();
    let row = first_row(
        &core,
        "SELECT CURRENT_DATE, CURRENT_TIMESTAMP(0), LOCALTIMESTAMP(3), now() \
         FROM events WHERE id = 1 LIMIT 10",
    );
    let now = ts(&row[3]);
    match &row[0] {
        Cell::Date(d) => assert_eq!(i64::from(*d), now.div_euclid(86_400_000_000)),
        other => panic!("expected Cell::Date, got {other:?}"),
    }
    assert_eq!(ts(&row[1]) % 1_000_000, 0);
    assert_eq!(ts(&row[2]) % 1_000, 0);
    // 範囲外の精度は 22023。
    assert_eq!(
        code(
            &core,
            "SELECT CURRENT_TIMESTAMP(7) FROM events WHERE id = 1 LIMIT 10"
        ),
        "22023"
    );
}

#[test]
fn from_less_select_and_result_types_and_default_names() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT now(), CURRENT_TIMESTAMP, CURRENT_DATE, LOCALTIMESTAMP",
        )
        .expect("from-less select"),
    );
    let names: Vec<&str> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } => name.as_str(),
            other => panic!("unexpected column meta {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        ["now", "current_timestamp", "current_date", "localtimestamp"]
    );
    assert!(matches!(r.rows[0].cells[0], Cell::Timestamp(_)));
    assert!(matches!(r.rows[0].cells[2], Cell::Date(_)));
}

#[test]
fn now_works_in_where_and_aggregate_and_order_by() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM events WHERE at < now() LIMIT 10",
        )
        .expect("where now()"),
    );
    assert_eq!(r.rows.len(), 1);
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM events WHERE at > CURRENT_TIMESTAMP LIMIT 10",
        )
        .expect("where CURRENT_TIMESTAMP"),
    );
    assert_eq!(r.rows.len(), 1);
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT COUNT(*) FROM events WHERE at < now()",
        )
        .expect("aggregate with now()"),
    );
    assert_eq!(r.rows.len(), 1);
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "SELECT id FROM events WHERE id > 0 ORDER BY at LIMIT 10",
    )
    .expect("order by");
}

#[test]
fn explain_accepts_now() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "EXPLAIN SELECT id FROM events WHERE at < now() LIMIT 10",
    )
    .expect("explain with now()");
}

#[test]
fn describe_reports_timestamp_and_date_types_for_time_functions() {
    let (core, _g) = new_core();
    let session = SessionState::default();
    let parsed = core.parse_sql("SELECT now(), CURRENT_DATE").expect("parse");
    let cols = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe must accept time functions")
        .expect("row description");
    assert_eq!(cols.len(), 2);
    assert!(matches!(
        &cols[0],
        ColumnMeta::Computed {
            ty: Some(ColumnType::Timestamp),
            ..
        }
    ));
    assert!(matches!(
        &cols[1],
        ColumnMeta::Computed {
            ty: Some(ColumnType::Date),
            ..
        }
    ));
}

#[test]
fn write_contexts_reject_time_functions_fail_closed() {
    let (core, _g) = new_core();
    // INSERT VALUES は従来どおり 42601（書き込み側の台帳 v3・INSERT 系ハッシュが未実装）。
    for expr in [
        "now()",
        "CURRENT_TIMESTAMP",
        "clock_timestamp()",
        "CURRENT_DATE",
    ] {
        assert_eq!(
            code(
                &core,
                &format!(
                    "INSERT INTO {TABLE} (id, embedding, lang, at) VALUES \
                     (10, '[0.1,0.2]', 'ja', {expr}) USING OPERATION_ID 'op-x'"
                )
            ),
            "42601",
            "INSERT VALUES {expr}"
        );
    }
    // UPDATE SET は定数式経路で 42601、述語形 UPDATE／DELETE の WHERE は束縛段で 0A000。
    for (sql, expected) in [
        (
            "UPDATE events SET at = now() WHERE id = 1 USING OPERATION_ID 'op-u'",
            "42601",
        ),
        (
            "UPDATE events SET lang = 'x' WHERE at < now() USING OPERATION_ID 'op-u2'",
            "0A000",
        ),
        (
            "DELETE FROM events WHERE at < now() USING OPERATION_ID 'op-d'",
            "0A000",
        ),
    ] {
        assert_eq!(code(&core, sql), expected, "{sql}");
    }
    // 明示トランザクション内の書き込みも同じ（時計を渡すのは読み取り文だけ）。
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &ctx,
            &mut session,
            &mut txn,
            "DELETE FROM events WHERE at < now() USING OPERATION_ID 'op-d2'",
        )
        .expect_err("delete with now() in txn");
    assert_eq!(err.wire_code(), "0A000");
    // DEFAULT・CHECK・VIEW 本文は 42601（SQL-37・TABLE-16・TABLE-18）。
    for sql in [
        "CREATE TABLE t1 (embedding VECTOR(2), n TIMESTAMP DEFAULT now())",
        "CREATE TABLE t2 (embedding VECTOR(2), n TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
        "CREATE TABLE t3 (embedding VECTOR(2), n TIMESTAMP, CHECK (n < now()))",
        "CREATE TABLE t4 (embedding VECTOR(2), n TIMESTAMP, CHECK (n < CURRENT_TIMESTAMP))",
        "CREATE VIEW v1 AS SELECT id, now() FROM events LIMIT 10",
    ] {
        assert_eq!(ddl_code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn quoted_keyword_identifiers_stay_column_references() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE q (embedding VECTOR(2), \"current_date\" TEXT)",
    )
    .expect("create table with a quoted keyword column");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "INSERT INTO q (id, embedding, \"current_date\") VALUES (1, '[0.1,0.2]', 'x') \
         USING OPERATION_ID 'op-q'",
    )
    .expect("insert");
    let r = query(
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT \"current_date\" FROM q WHERE id = 1 LIMIT 10",
        )
        .expect("quoted column reference"),
    );
    assert!(
        matches!(&r.rows[0].cells[0], Cell::Text(t) if t == "x"),
        "{:?}",
        r.rows[0].cells
    );
}

#[test]
fn time_keywords_without_timezone_support_are_rejected() {
    let (core, _g) = new_core();
    for kw in ["CURRENT_TIME", "LOCALTIME"] {
        assert_eq!(
            code(
                &core,
                &format!("SELECT {kw} FROM events WHERE id = 1 LIMIT 10")
            ),
            "0A000"
        );
    }
    // 引数付きの関数形は arity 違反の 42883。
    assert_eq!(
        code(&core, "SELECT now(1) FROM events WHERE id = 1 LIMIT 10"),
        "42883"
    );
}

#[test]
fn udf_named_after_a_time_function_is_rejected_and_udf_bodies_inherit_the_clock() {
    let (core, _g) = new_core();
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for name in ["now", "clock_timestamp", "statement_timestamp"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}() AS 1"),
            )
            .expect_err("reserved");
        assert_eq!(err.wire_code(), "42723", "{name}");
    }
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f(x) AS x + 1")
        .expect("define f");
}
