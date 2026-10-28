//! `EXTRACT`／`date_part`／`date_trunc` の `INTERVAL` 入力と `justify_*`（TABLE-34・Issue #1582）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 各 field／unit の結果が
//! PostgreSQL 17 と一致すること、(2) INTERVAL 列・リテラルの両方で投影・WHERE・ORDER BY・集計・
//! CHECK が使えること、(3) 未対応の形が `0A000`／`22000`／`42883`／`42723` で fail-closed に
//! 拒否されること、(4) テナント境界とエラー文言への値の非反射、を固定する。
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run_as(
    tenant: &str,
    core: &EngineCore,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx_for(tenant), s, sql)
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as("alice", core, s, sql)
}

fn ok(core: &EngineCore, s: &mut SessionState, sql: &str) {
    run(core, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> (String, String) {
    let e = run(core, s, sql).expect_err(&format!("{sql} must fail"));
    (e.wire_code().to_string(), e.to_string())
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err(core, s, sql).0
}

/// セルを比較しやすいテキストへ写す。TIMESTAMPTZ は UTC の明示 offset で出す。
fn render(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Float(v) => format!("{v}"),
        Cell::Bool(b) => b.to_string(),
        Cell::Text(t) => t.clone(),
        Cell::Date(d) => format!("date:{}", engine::datetime::format_date(*d)),
        Cell::Timestamp(t) => format!("ts:{}", engine::datetime::format_timestamp(*t)),
        Cell::TimestampTz(t) => format!(
            "tz:{}",
            engine::datetime::format_timestamptz(*t, &engine::datetime::SessionTimeZone::utc())
        ),
        Cell::Interval(v) => format!("iv:{}", engine::datetime::format_interval(v)),
        other => format!("{other:?}"),
    }
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<Vec<String>> {
    match run(core, s, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}")) {
        SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| row.cells.iter().map(render).collect())
            .collect(),
        other => panic!("expected a query result for {sql}, got {other:?}"),
    }
}

fn one(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    let r = rows(core, s, sql);
    assert_eq!(r.len(), 1, "{sql}");
    assert_eq!(r[0].len(), 1, "{sql}");
    r[0][0].clone()
}

fn seed(core: &EngineCore, s: &mut SessionState) {
    ok(
        core,
        s,
        "CREATE TABLE iv (id BIGINT PRIMARY KEY, v INTERVAL, ts TIMESTAMP)",
    );
    for (id, v) in [
        (1, "'1 year 2 mons 3 days 04:05:06.789'"),
        (2, "'-1 day -25:30:00'"),
        (3, "NULL"),
    ] {
        ok(
            core,
            s,
            &format!(
                "INSERT INTO iv (id, v, ts) VALUES ({id}, {v}, '2024-01-31 10:00:00') \
                 USING OPERATION_ID 'seed{id}'"
            ),
        );
    }
}

const LIT: &str = "INTERVAL '1 year 2 mons 3 days 04:05:06.789'";

#[test]
fn date_part_fields_match_postgres() {
    let (_g, core) = open("ivpt-fields");
    let mut s = session();
    for (field, want) in [
        ("year", "1"),
        ("month", "2"),
        ("quarter", "1"),
        ("day", "3"),
        ("hour", "4"),
        ("minute", "5"),
        ("second", "6.789"),
        ("milliseconds", "6789"),
        ("microseconds", "6789000"),
        ("decade", "0"),
        ("century", "0"),
        ("millennium", "0"),
        ("epoch", "37015506.789000005"),
    ] {
        assert_eq!(
            one(
                &core,
                &mut s,
                &format!("SELECT date_part('{field}', {LIT})")
            ),
            want,
            "date_part {field}"
        );
        assert_eq!(
            one(
                &core,
                &mut s,
                &format!("SELECT EXTRACT({field} FROM {LIT})")
            ),
            want,
            "EXTRACT {field}"
        );
    }
    let mut f = |lit: &str, field: &str| {
        one(
            &core,
            &mut s,
            &format!("SELECT date_part('{field}', INTERVAL '{lit}')"),
        )
    };
    assert_eq!(f("-14 mons", "year"), "-1");
    assert_eq!(f("-14 mons", "month"), "-2");
    assert_eq!(f("-14 mons", "quarter"), "1");
    assert_eq!(f("36 hours", "day"), "0");
    assert_eq!(f("36 hours", "hour"), "36");
    assert_eq!(f("-1 day -25:30:00", "day"), "-1");
    assert_eq!(f("-1 day -25:30:00", "hour"), "-25");
    assert_eq!(f("-1 day -25:30:00", "minute"), "-30");
    assert_eq!(f("1 mon", "epoch"), "2592000");
    assert_eq!(f("1 year", "epoch"), "31557600");
    assert_eq!(f("-15 years", "decade"), "-1");
}

#[test]
fn date_trunc_units_match_postgres() {
    let (_g, core) = open("ivpt-trunc");
    let mut s = session();
    let mut t = |unit: &str, lit: &str| {
        one(
            &core,
            &mut s,
            &format!("SELECT date_trunc('{unit}', INTERVAL '{lit}')"),
        )
    };
    assert_eq!(t("decade", "-15 years"), "iv:-10 years");
    assert_eq!(t("hour", "1 day 02:30:45.5"), "iv:1 day 02:00:00");
    assert_eq!(t("year", "14 mons 20 days"), "iv:1 year");
    assert_eq!(t("quarter", "1 year 5 mons"), "iv:1 year 3 mons");
    assert_eq!(t("milliseconds", "00:00:01.234567"), "iv:00:00:01.234");
    assert_eq!(t("microseconds", "00:00:01.234567"), "iv:00:00:01.234567");
    assert_eq!(t("second", "00:00:01.234567"), "iv:00:00:01");
    assert_eq!(t("minute", "1 day 02:30:45.5"), "iv:1 day 02:30:00");
    assert_eq!(t("day", "1 day 02:30:45.5"), "iv:1 day");
    assert_eq!(t("month", "1 mon 5 days 02:00:00"), "iv:1 mon");
    assert_eq!(t("century", "250 years 3 mons"), "iv:200 years");
    assert_eq!(t("millennium", "2500 years"), "iv:2000 years");
}

#[test]
fn justify_functions_match_postgres() {
    let (_g, core) = open("ivpt-justify");
    let mut s = session();
    let mut q = |f: &str, lit: &str| one(&core, &mut s, &format!("SELECT {f}(INTERVAL '{lit}')"));
    assert_eq!(q("justify_hours", "1 day 25:00:00"), "iv:2 days 01:00:00");
    assert_eq!(q("justify_days", "35 days"), "iv:1 mon 5 days");
    assert_eq!(q("justify_days", "-35 days"), "iv:-1 mons -5 days");
    assert_eq!(
        q("justify_interval", "1 mon -1 hour"),
        "iv:29 days 23:00:00"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT justify_hours(INTERVAL '2147483647 days 25:00:00')"
        ),
        "22008"
    );
}

#[test]
fn columns_work_in_projection_where_order_and_aggregate() {
    let (_g, core) = open("ivpt-cols");
    let mut s = session();
    seed(&core, &mut s);
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT id, date_trunc('hour', v) FROM iv ORDER BY id LIMIT 10"
        ),
        vec![
            vec![
                "1".to_string(),
                "iv:1 year 2 mons 3 days 04:00:00".to_string()
            ],
            vec!["2".to_string(), "iv:-1 days -25:00:00".to_string()],
            vec!["3".to_string(), "NULL".to_string()],
        ]
    );
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT id FROM iv WHERE EXTRACT(day FROM v) >= 1 LIMIT 10"
        ),
        vec![vec!["1".to_string()]]
    );
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT id FROM iv WHERE v IS NOT NULL ORDER BY date_part('hour', v) LIMIT 10"
        ),
        vec![vec!["2".to_string()], vec!["1".to_string()]]
    );
    assert_eq!(
        one(&core, &mut s, "SELECT SUM(date_part('minute', v)) FROM iv"),
        "-25"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT date_trunc('day', ts + INTERVAL '1 day 03:00') FROM iv WHERE id = 1 LIMIT 1"
        ),
        "ts:2024-02-01 00:00:00"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT date_trunc('month', v) + INTERVAL '1 day' FROM iv WHERE id = 1 LIMIT 1"
        ),
        "iv:1 year 2 mons 1 day"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT ts + date_trunc('hour', v) FROM iv \
             WHERE id = 1 LIMIT 1"
        ),
        "ts:2025-04-03 14:00:00"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT EXTRACT(epoch FROM TIMESTAMP '2024-01-02 00:00:00' \
             - TIMESTAMP '2024-01-01 00:00:00')"
        ),
        "86400"
    );
}

#[test]
fn null_inputs_are_strict_and_typed() {
    let (_g, core) = open("ivpt-null");
    let mut s = session();
    seed(&core, &mut s);
    assert_eq!(
        one(&core, &mut s, "SELECT date_part(NULL, INTERVAL '1 day')"),
        "NULL"
    );
    assert_eq!(
        one(&core, &mut s, "SELECT date_trunc(NULL, INTERVAL '1 day')"),
        "NULL"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT date_part('day', v) FROM iv WHERE id = 3 LIMIT 1"
        ),
        "NULL"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT justify_days(v) FROM iv WHERE id = 3 LIMIT 1"
        ),
        "NULL"
    );
    // justify_*(NULL) は裸の NULL を受理し、結果型 INTERVAL の NULL を返す。
    for f in ["justify_hours", "justify_days", "justify_interval"] {
        assert_eq!(one(&core, &mut s, &format!("SELECT {f}(NULL)")), "NULL");
        assert_eq!(
            one(
                &core,
                &mut s,
                &format!("SELECT {f}(NULL) + INTERVAL '1 day'")
            ),
            "NULL"
        );
    }
    // date_trunc(NULL, interval) の結果型は INTERVAL なので INTERVAL と加算できる。
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT date_trunc(NULL, INTERVAL '1 day') + INTERVAL '1 day'"
        ),
        "NULL"
    );
}

#[test]
fn unsupported_forms_fail_closed() {
    let (_g, core) = open("ivpt-reject");
    let mut s = session();
    seed(&core, &mut s);
    for field in ["week", "dow", "isodow", "doy", "isoyear", "timezone"] {
        assert_eq!(
            code(
                &core,
                &mut s,
                &format!("SELECT EXTRACT({field} FROM {LIT})")
            ),
            "0A000",
            "{field}"
        );
    }
    assert_eq!(
        code(&core, &mut s, &format!("SELECT date_trunc('week', {LIT})")),
        "0A000"
    );
    assert_eq!(
        code(&core, &mut s, &format!("SELECT date_part('bogus', {LIT})")),
        "22000"
    );
    assert_eq!(
        code(&core, &mut s, &format!("SELECT date_trunc('bogus', {LIT})")),
        "22000"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            &format!("SELECT date_trunc('hour', {LIT}, 'UTC')")
        ),
        "42883"
    );
    assert_eq!(code(&core, &mut s, "SELECT justify_days(1)"), "42804");
    assert_eq!(
        code(&core, &mut s, "CREATE FUNCTION justify_days(x) AS x"),
        "42723"
    );
    // INTERVAL 式の比較・GROUP BY は #1580 の列参照限定のまま fail-closed。
    let c = code(
        &core,
        &mut s,
        "SELECT id FROM iv WHERE date_trunc('day', v) = INTERVAL '1 day' LIMIT 10",
    );
    assert!(c == "42883" || c == "0A000", "{c}");
    let c = code(
        &core,
        &mut s,
        "SELECT date_trunc('month', v) FROM iv GROUP BY date_trunc('month', v)",
    );
    assert!(c == "42883" || c == "0A000" || c == "42601", "{c}");
}

#[test]
fn error_messages_do_not_echo_values_and_tenants_are_isolated() {
    let (_g, core) = open("ivpt-tenant");
    let mut s = session();
    seed(&core, &mut s);
    let (_, msg) = err(
        &core,
        &mut s,
        "SELECT justify_hours(INTERVAL '2147483647 days 25:00:00')",
    );
    assert!(!msg.contains("2147483647"), "{msg}");
    // 別テナントの行は結果に混ざらない。
    let mut bob = session();
    run_as(
        "bob",
        &core,
        &mut bob,
        "CREATE TABLE iv (id BIGINT PRIMARY KEY, v INTERVAL, ts TIMESTAMP)",
    )
    .ok();
    let r = run_as(
        "bob",
        &core,
        &mut bob,
        "SELECT date_part('year', v) FROM iv LIMIT 10",
    );
    if let Ok(SqlOutcome::Query(q)) = r {
        assert!(q.rows.is_empty());
    }
}

#[test]
fn check_constraint_with_interval_part_roundtrips() {
    let path = unique_db_path("ivpt-check");
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (id BIGINT PRIMARY KEY, v INTERVAL, CHECK (date_part('day', v) >= 0))",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO c (id, v) VALUES (1, '2 days') USING OPERATION_ID 'c1'",
    );
    let first = code(
        &core,
        &mut s,
        "INSERT INTO c (id, v) VALUES (2, '-2 days') USING OPERATION_ID 'c2'",
    );
    assert_eq!(first, "23514");
    drop(core);
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    let mut s = session();
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO c (id, v) VALUES (3, '-3 days') USING OPERATION_ID 'c3'"
        ),
        "23514"
    );
    drop(guard);
}
