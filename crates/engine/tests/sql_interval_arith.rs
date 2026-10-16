//! 日付時刻と `INTERVAL` の加減算・乗除・差（TABLE-34・Issue #1581）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 月末・うるう年・DST 境界の
//! 結果が PostgreSQL 17 と一致すること、(2) INTERVAL 列と型付きリテラルが式の中で使えること
//! （投影・WHERE・UPDATE SET・INSERT の定数畳み込み・CHECK）、(3) 範囲外が `22008`・0 除算が
//! `22012`、未対応の形が `42883`／`0A000` で fail-closed に拒否されること、(4) テナント境界と
//! エラー文言への値の非反射、を固定する。INTERVAL 式の比較・集計・ORDER BY は #1580 の担当。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

#[test]
fn month_end_and_leap_year_match_postgres() {
    let (_g, core) = open("ivar-calendar");
    let mut s = session();
    for (sql, want) in [
        (
            "SELECT TIMESTAMP '2024-01-31 00:00:00' + INTERVAL '1 mon'",
            "ts:2024-02-29 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '2023-01-31 00:00:00' + INTERVAL '1 mon'",
            "ts:2023-02-28 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '2024-02-29 00:00:00' + INTERVAL '1 year'",
            "ts:2025-02-28 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '2024-03-31 00:00:00' - INTERVAL '1 mon'",
            "ts:2024-02-29 00:00:00",
        ),
        // 月 → 日の順に加算する。
        (
            "SELECT TIMESTAMP '2024-01-31 00:00:00' + INTERVAL '1 mon 1 day'",
            "ts:2024-03-01 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '2024-01-31 00:00:00' + INTERVAL '1 mon' + INTERVAL '1 mon'",
            "ts:2024-03-29 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '1900-02-28 00:00:00' + INTERVAL '1 day'",
            "ts:1900-03-01 00:00:00",
        ),
        (
            "SELECT TIMESTAMP '2000-02-28 00:00:00' + INTERVAL '1 day'",
            "ts:2000-02-29 00:00:00",
        ),
        // DATE ± INTERVAL は TIMESTAMP を返す。
        (
            "SELECT DATE '2024-01-31' + INTERVAL '1 mon'",
            "ts:2024-02-29 00:00:00",
        ),
        (
            "SELECT INTERVAL '12 hours' + DATE '2024-01-31'",
            "ts:2024-01-31 12:00:00",
        ),
        (
            "SELECT DATE '2024-03-31' - INTERVAL '1 mon'",
            "ts:2024-02-29 00:00:00",
        ),
        (
            "SELECT INTERVAL '1 day' + TIMESTAMP '2024-02-28 12:30:00'",
            "ts:2024-02-29 12:30:00",
        ),
    ] {
        assert_eq!(one(&core, &mut s, sql), want, "{sql}");
    }
}

#[test]
fn timestamp_difference_is_justified_to_days() {
    let (_g, core) = open("ivar-diff");
    let mut s = session();
    for (sql, want) in [
        (
            "SELECT TIMESTAMP '2024-03-01 00:00:00' - TIMESTAMP '2024-02-01 00:00:00'",
            "iv:29 days",
        ),
        (
            "SELECT TIMESTAMP '2024-01-02 06:00:00' - TIMESTAMP '2024-01-01 00:00:00'",
            "iv:1 day 06:00:00",
        ),
        (
            "SELECT TIMESTAMP '2024-01-01 00:00:00' - TIMESTAMP '2024-01-02 06:00:00'",
            "iv:-1 days -06:00:00",
        ),
        (
            "SELECT TIMESTAMP '9999-12-31 00:00:00' - TIMESTAMP '0001-01-01 00:00:00'",
            "iv:3652058 days",
        ),
        (
            "SELECT TIMESTAMP '2024-01-02 00:00:00' - DATE '2024-01-01'",
            "iv:1 day",
        ),
        (
            "SELECT DATE '2024-01-02' - TIMESTAMP '2024-01-01 12:00:00'",
            "iv:12:00:00",
        ),
        // DATE - DATE は従来どおり日数（INTEGER 相当）。
        ("SELECT DATE '2024-03-01' - DATE '2024-02-01'", "29"),
    ] {
        assert_eq!(one(&core, &mut s, sql), want, "{sql}");
    }
}

#[test]
fn interval_multiplication_division_and_addition() {
    let (_g, core) = open("ivar-muldiv");
    let mut s = session();
    for (sql, want) in [
        ("SELECT INTERVAL '1 day' * 1.5", "iv:1 day 12:00:00"),
        ("SELECT 2 * INTERVAL '1 day 02:00:00'", "iv:2 days 04:00:00"),
        ("SELECT INTERVAL '1 mon' * 0.5", "iv:15 days"),
        ("SELECT INTERVAL '1 mon' * 1.5", "iv:1 mon 15 days"),
        ("SELECT INTERVAL '1 mon' / 3", "iv:10 days"),
        ("SELECT INTERVAL '1 day' / 3", "iv:08:00:00"),
        (
            "SELECT INTERVAL '1 mon 2 days' + INTERVAL '1 day 01:00:00'",
            "iv:1 mon 3 days 01:00:00",
        ),
        (
            "SELECT INTERVAL '1 day' - INTERVAL '25 hours'",
            "iv:1 day -25:00:00",
        ),
    ] {
        assert_eq!(one(&core, &mut s, sql), want, "{sql}");
    }
}

#[test]
fn timestamptz_arithmetic_follows_session_time_zone_and_dst() {
    let (_g, core) = open("ivar-dst");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    for (sql, want) in [
        // 1 day は壁時計で進み、24 hours は経過時間で進む。
        (
            "SELECT TIMESTAMPTZ '2024-03-09 12:00:00-05' + INTERVAL '1 day'",
            "tz:2024-03-10 16:00:00+00",
        ),
        (
            "SELECT TIMESTAMPTZ '2024-03-09 12:00:00-05' + INTERVAL '24 hours'",
            "tz:2024-03-10 17:00:00+00",
        ),
        (
            "SELECT TIMESTAMPTZ '2024-11-02 12:00:00-04' + INTERVAL '1 day'",
            "tz:2024-11-03 17:00:00+00",
        ),
        // gap（存在しない壁時計）は直前の offset で解決する。
        (
            "SELECT TIMESTAMPTZ '2024-03-09 02:30:00-05' + INTERVAL '1 day'",
            "tz:2024-03-10 07:30:00+00",
        ),
        (
            "SELECT TIMESTAMPTZ '2024-03-11 12:00:00-04' - INTERVAL '1 day'",
            "tz:2024-03-10 16:00:00+00",
        ),
        // 月の結果が gap に入ると正規化（03:30 EDT）されてから日が加算される。
        (
            "SELECT TIMESTAMPTZ '2024-02-10 02:30:00-05' + INTERVAL '1 mon 1 day'",
            "tz:2024-03-11 07:30:00+00",
        ),
        // 時点の差は経過時間（TZ 非依存）。
        (
            "SELECT TIMESTAMPTZ '2024-03-11 00:00:00-04' - TIMESTAMPTZ '2024-03-10 00:00:00-05'",
            "iv:23:00:00",
        ),
    ] {
        assert_eq!(one(&core, &mut s, sql), want, "{sql}");
    }
    // 月の加算はセッション TZ の壁時計で行うため、TZ によって結果の時点が変わる。
    let sql = "SELECT TIMESTAMPTZ '2024-01-31 00:00:00+09' + INTERVAL '1 mon'";
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(one(&core, &mut s, sql), "tz:2024-02-28 15:00:00+00");
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(one(&core, &mut s, sql), "tz:2024-02-29 15:00:00+00");
}

fn seed(core: &EngineCore, s: &mut SessionState) {
    ok(
        core,
        s,
        "CREATE TABLE ev (ts TIMESTAMP, d DATE, v INTERVAL, tz TIMESTAMPTZ, n INTEGER)",
    );
    for (id, ts, d, v, n) in [
        (1, "2024-01-31 10:00:00", "2024-01-31", "'1 mon'", 2),
        (
            2,
            "2024-02-28 23:30:00",
            "2024-02-28",
            "'1 day 02:00:00'",
            4,
        ),
        (3, "2023-12-31 00:00:00", "2023-12-31", "NULL", 3),
    ] {
        ok(
            core,
            s,
            &format!(
                "INSERT INTO ev (id, ts, d, v, tz, n) VALUES ({id}, '{ts}', '{d}', {v}, \
                 '2024-03-09 12:00:00+00', {n}) USING OPERATION_ID 'seed{id}'"
            ),
        );
    }
}

#[test]
fn interval_columns_work_in_projection_and_where() {
    let (_g, core) = open("ivar-columns");
    let mut s = session();
    seed(&core, &mut s);
    let r = rows(
        &core,
        &mut s,
        "SELECT id, ts + v, d + v, v * 2, v + v, v / 2 FROM ev ORDER BY id LIMIT 10",
    );
    assert_eq!(
        r,
        vec![
            vec![
                "1",
                "ts:2024-02-29 10:00:00",
                "ts:2024-02-29 00:00:00",
                "iv:2 mons",
                "iv:2 mons",
                "iv:15 days",
            ],
            vec![
                "2",
                "ts:2024-03-01 01:30:00",
                "ts:2024-02-29 02:00:00",
                "iv:2 days 04:00:00",
                "iv:2 days 04:00:00",
                "iv:13:00:00",
            ],
            // NULL の INTERVAL 列は算術の結果も NULL。
            vec!["3", "NULL", "NULL", "NULL", "NULL", "NULL"],
        ]
    );
    // 列どうしの差・数値列との乗算。
    let r = rows(
        &core,
        &mut s,
        "SELECT id, ts - d, v * n FROM ev ORDER BY id LIMIT 10",
    );
    assert_eq!(r[0], vec!["1", "iv:10:00:00", "iv:2 mons"]);
    assert_eq!(r[1], vec!["2", "iv:23:30:00", "iv:4 days 08:00:00"]);
    assert_eq!(r[2], vec!["3", "iv:00:00:00", "NULL"]);
    // TIMESTAMPTZ 列 ± INTERVAL 列（セッション TZ は UTC）。
    let r = rows(
        &core,
        &mut s,
        "SELECT id, tz + v FROM ev WHERE id = 2 LIMIT 5",
    );
    assert_eq!(r[0], vec!["2", "tz:2024-03-10 14:00:00+00"]);
    // WHERE の式述語（NULL は非該当）。
    let r = rows(
        &core,
        &mut s,
        "SELECT id FROM ev WHERE ts + v > TIMESTAMP '2024-03-01 00:00:00' LIMIT 10",
    );
    assert_eq!(r, vec![vec!["2"]]);
}

#[test]
fn update_set_and_insert_values_accept_interval_expressions() {
    let (_g, core) = open("ivar-update");
    let mut s = session();
    seed(&core, &mut s);
    ok(
        &core,
        &mut s,
        "UPDATE ev SET ts = ts + v WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT ts FROM ev WHERE id = 1 LIMIT 5"),
        "ts:2024-02-29 10:00:00"
    );
    ok(
        &core,
        &mut s,
        "UPDATE ev SET v = v / 2 WHERE id = 2 USING OPERATION_ID 'u2'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT v FROM ev WHERE id = 2 LIMIT 5"),
        "iv:13:00:00"
    );
    // NULL の INTERVAL を使った代入は NULL（列が NULL 可）。
    ok(
        &core,
        &mut s,
        "UPDATE ev SET v = v * 2 WHERE id = 3 USING OPERATION_ID 'u3'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT v FROM ev WHERE id = 3 LIMIT 5"),
        "NULL"
    );
    // 定数畳み込み: INSERT の VALUES に INTERVAL 式を置く。
    ok(
        &core,
        &mut s,
        "INSERT INTO ev (id, v) VALUES (9, INTERVAL '1 day' * 2) USING OPERATION_ID 'i9'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT v FROM ev WHERE id = 9 LIMIT 5"),
        "iv:2 days"
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO ev (id, ts) VALUES (10, TIMESTAMP '2024-01-31 00:00:00' + INTERVAL '1 mon') \
         USING OPERATION_ID 'i10'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT ts FROM ev WHERE id = 10 LIMIT 5"),
        "ts:2024-02-29 00:00:00"
    );
    // NULL 定数の算術は INTERVAL の候補でも畳み込まれ、INTERVAL 列へ NULL が入る。
    ok(
        &core,
        &mut s,
        "INSERT INTO ev (id, v) VALUES (11, NULL + INTERVAL '1 day') USING OPERATION_ID 'i11'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO ev (id, v) VALUES (12, INTERVAL '1 day' * NULL) USING OPERATION_ID 'i12'",
    );
    for id in [11, 12] {
        assert_eq!(
            one(
                &core,
                &mut s,
                &format!("SELECT v FROM ev WHERE id = {id} LIMIT 5")
            ),
            "NULL"
        );
    }
    // 裸の NULL と INTERVAL の加減算は INTERVAL 同士の演算へ解決される（型が TIMESTAMP にならない）。
    for (id, e) in [
        (13, "COALESCE(NULL + INTERVAL '1 day', INTERVAL '0')"),
        (14, "COALESCE(INTERVAL '1 day' - NULL, INTERVAL '0')"),
    ] {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO ev (id, v) VALUES ({id}, {e}) USING OPERATION_ID 'i{id}'"),
        );
        assert_eq!(
            one(
                &core,
                &mut s,
                &format!("SELECT v FROM ev WHERE id = {id} LIMIT 5")
            ),
            "iv:00:00:00"
        );
    }
    // 範囲外の結果は副作用を残さない。
    assert_eq!(
        code(
            &core,
            &mut s,
            "UPDATE ev SET ts = TIMESTAMP '9999-12-31 00:00:00' + INTERVAL '2 days' \
             WHERE id = 1 USING OPERATION_ID 'u4'"
        ),
        "22008"
    );
    assert_eq!(
        one(&core, &mut s, "SELECT ts FROM ev WHERE id = 1 LIMIT 5"),
        "ts:2024-02-29 10:00:00"
    );
}

#[test]
fn null_propagation_and_case_coalesce_branches() {
    let (_g, core) = open("ivar-null");
    let mut s = session();
    seed(&core, &mut s);
    let r = rows(
        &core,
        &mut s,
        "SELECT id, COALESCE(v, INTERVAL '1 day'), CASE WHEN n > 2 THEN v ELSE INTERVAL '0' END \
         FROM ev ORDER BY id LIMIT 10",
    );
    assert_eq!(r[0], vec!["1", "iv:1 mon", "iv:00:00:00"]);
    assert_eq!(r[1], vec!["2", "iv:1 day 02:00:00", "iv:1 day 02:00:00"]);
    assert_eq!(r[2], vec!["3", "iv:1 day", "NULL"]);
}

#[test]
fn literal_errors_and_unsupported_forms_fail_closed() {
    let (_g, core) = open("ivar-errors");
    let mut s = session();
    seed(&core, &mut s);
    // リテラルの書式・範囲・infinity・修飾子・精度。
    assert_eq!(code(&core, &mut s, "SELECT INTERVAL 'bogus'"), "22007");
    assert_eq!(
        code(&core, &mut s, "SELECT INTERVAL '9223372036854775808 days'"),
        "22015"
    );
    assert_eq!(code(&core, &mut s, "SELECT INTERVAL 'infinity'"), "0A000");
    assert_eq!(code(&core, &mut s, "SELECT INTERVAL '1' DAY"), "0A000");
    assert_eq!(code(&core, &mut s, "SELECT INTERVAL(3) '1 day'"), "0A000");
    // 範囲外・0 除算。
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT INTERVAL '2147483647 mons' + INTERVAL '1 mon'"
        ),
        "22008"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT INTERVAL '2147483647 days' * 2"),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT TIMESTAMP '9999-12-31 00:00:00' + INTERVAL '1 day'"
        ),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT TIMESTAMP '0001-01-01 00:00:00' - INTERVAL '1 day'"
        ),
        "22008"
    );
    assert_eq!(code(&core, &mut s, "SELECT INTERVAL '1 day' / 0"), "22012");
    // 演算子が存在しない組は 42883。
    for sql in [
        "SELECT INTERVAL '1 day' - TIMESTAMP '2024-01-01 00:00:00'",
        "SELECT INTERVAL '1 day' + 1",
        "SELECT 2 / INTERVAL '1 day'",
        "SELECT INTERVAL '1 day' * INTERVAL '1 day'",
        "SELECT TIME '10:00:00' + INTERVAL '1 hour'",
        "SELECT id FROM ev WHERE v * 2 = INTERVAL '2 days' LIMIT 5",
        "SELECT v + 1 FROM ev LIMIT 5",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42883", "{sql}");
    }
    // INTERVAL が絡まない既存の不一致は従来どおり 42804。
    assert_eq!(
        code(&core, &mut s, "SELECT TIMESTAMP '2024-01-01 00:00:00' + 1"),
        "42804"
    );
    // 関数引数・json 変換・集計・ORDER BY は未対応。
    assert_eq!(
        code(&core, &mut s, "SELECT length(INTERVAL '1 day')"),
        "42804"
    );
    assert_eq!(code(&core, &mut s, "SELECT SUM(v * 2) FROM ev"), "0A000");
    assert_eq!(
        code(&core, &mut s, "SELECT id FROM ev ORDER BY v * 2 LIMIT 5"),
        "42601"
    );
    // 列名 `interval` は引き続き使える。
    ok(&core, &mut s, "CREATE TABLE named (interval INTEGER)");
    ok(
        &core,
        &mut s,
        "INSERT INTO named (id, interval) VALUES (1, 5) USING OPERATION_ID 'nm1'",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT interval + 1 FROM named LIMIT 5"),
        "6"
    );
}

#[test]
fn error_messages_do_not_echo_values() {
    let (_g, core) = open("ivar-noecho");
    let mut s = session();
    let (c, msg) = err(
        &core,
        &mut s,
        "SELECT INTERVAL '2147483647 mons' + INTERVAL '2147483647 mons'",
    );
    assert_eq!(c, "22008");
    assert!(!msg.contains("2147483647"), "{msg}");
    let (_, msg) = err(&core, &mut s, "SELECT INTERVAL 'secretword'");
    assert!(!msg.contains("secretword"), "{msg}");
}

#[test]
fn check_constraint_with_interval_roundtrips_and_rejects_session_tz() {
    let (path, guard, core) = {
        let path = unique_db_path("ivar-check");
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        (
            path,
            guard,
            EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        )
    };
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (ts TIMESTAMP, tz TIMESTAMPTZ, \
         CHECK (ts + INTERVAL '1 day' > ts))",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO c (id, ts) VALUES (1, '2024-01-01 00:00:00') USING OPERATION_ID 'c1'",
    );
    // 9999-12-31 の翌日は範囲外なので CHECK 評価が 22008 になり、行は入らない。
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO c (id, ts) VALUES (2, '9999-12-31 00:00:00') USING OPERATION_ID 'c2'"
        ),
        "22008"
    );
    // セッション TZ に依存する式は CHECK に置けない。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE c2 (tz TIMESTAMPTZ, CHECK (tz + INTERVAL '1 day' > tz))"
        ),
        "0A000"
    );
    drop(core);
    // 再オープン後も CHECK 制約（INTERVAL リテラルの描画・再解析）が効く。
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = session();
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO c (id, ts) VALUES (3, '9999-12-31 00:00:00') USING OPERATION_ID 'c3'"
        ),
        "22008"
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO c (id, ts) VALUES (4, '2024-05-05 00:00:00') USING OPERATION_ID 'c4'",
    );
    drop(guard);
}

#[test]
fn other_tenants_rows_never_appear_in_arithmetic_results() {
    let (_g, core) = open("ivar-tenant");
    let mut s = session();
    seed(&core, &mut s);
    ok(
        &core,
        &mut s,
        "INSERT INTO ev (id, v) VALUES (20, '5 days') USING OPERATION_ID 'a20'",
    );
    let mut sb = session();
    let r = match run_as(
        "bob",
        &core,
        &mut sb,
        "SELECT id, v * 2 FROM ev ORDER BY id LIMIT 50",
    ) {
        Ok(SqlOutcome::Query(r)) => r.rows.len(),
        Ok(other) => panic!("unexpected outcome {other:?}"),
        // 他テナントには表自体が見えない実装でも、行は一切返らない。
        Err(_) => 0,
    };
    assert_eq!(r, 0);
}

/// TIMESTAMPTZ の月加算後の中間値が 9999 年を超えても最終値が範囲内なら成功する。
#[test]
fn timestamptz_intermediate_beyond_year_9999_succeeds() {
    let (_g, core) = open("ivar-tz-mid");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    let r = one(
        &core,
        &mut s,
        "SELECT TIMESTAMPTZ '2000-01-01 12:00:00-05' + INTERVAL '120000 mons -3652425 days'",
    );
    assert!(r.starts_with("tz:"), "{r}");
}

/// DST 期間（7 月）の日付で、範囲外の中間値を経由しても壁時計が 1 時間ずれない
/// （UTC→局所と局所→UTC で 400 年周期の同じ対応年を使う。Issue #1581）。
#[test]
fn timestamptz_out_of_range_intermediate_keeps_dst_offset() {
    let (_g, core) = open("ivar-tz-mid-dst");
    let mut s = session();
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT TIMESTAMPTZ '2000-07-01 12:00:00-04' + INTERVAL '120000 mons -3652425 days'",
        ),
        "tz:2000-07-01 16:00:00+00"
    );
}

/// 月加算後が天文学的年 -4713 でも、日加算後の最終値が範囲内なら成功する
/// （年の予備検査は -4713 を許容し、正確な下限は通日検査が判定する）。
#[test]
fn month_addition_landing_in_year_minus_4713_succeeds() {
    let (_g, core) = open("ivar-year-4713");
    let mut s = session();
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT TIMESTAMP '2000-01-01 00:00:00' + INTERVAL '-80545 mons 2451545 days'",
        ),
        "ts:2000-01-08 00:00:00"
    );
}
