//! `AT TIME ZONE` と TIMESTAMP／TIMESTAMPTZ／DATE の変換、TIMESTAMPTZ の述語・ORDER BY・集計・
//! 二次索引（TABLE-33・Issue #1576）の結合テスト。
//!
//! 期待値は PG 17 の実測（docker `postgres:17`）。DST の gap／fold は America/New_York の
//! 2026-03-08 02:30（存在しない時刻）と 2026-11-01 01:30（2 回ある時刻）で固定する。
//! 比較・並べ替え・集計は UTC の時点で行い、`SET TIME ZONE` を変えても結果の行集合と UTC 値が
//! 変わらず表示だけが変わることを確認する。UNIQUE／PK／FK・JOIN キー・SUM／AVG・明示 CAST は
//! 担当 Issue が別のため fail-closed のまま（拒否コードが変わらないこと）を確認する。

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

/// 失敗する SQL の wire_code と表示文言（エラー文言に入力値が含まれないことの検査用）。
fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> (String, String) {
    let e = run(core, s, sql).expect_err(&format!("{sql} must fail"));
    (e.wire_code().to_string(), e.to_string())
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    err(core, s, sql).0
}

/// セルを比較しやすいテキストへ写す。TIMESTAMPTZ は UTC の明示 offset で出す
/// （セッション TZ に依存しない。表示 TZ の検査は `shown` が担当する）。
fn render(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Float(v) => format!("{v}"),
        Cell::Bool(b) => b.to_string(),
        Cell::Text(t) => t.clone(),
        Cell::Date(d) => engine::datetime::format_date(*d),
        Cell::Timestamp(t) => engine::datetime::format_timestamp(*t),
        Cell::TimestampTz(t) => {
            engine::datetime::format_timestamptz(*t, &engine::datetime::SessionTimeZone::utc())
        }
        other => format!("{other:?}"),
    }
}

/// SELECT の全行を文字列セルの表へ。
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

/// 1 行 1 列の値。
fn one(core: &EngineCore, s: &mut SessionState, sql: &str) -> String {
    let r = rows(core, s, sql);
    assert_eq!(r.len(), 1, "{sql}");
    assert_eq!(r[0].len(), 1, "{sql}");
    r[0][0].clone()
}

#[test]
fn at_time_zone_both_directions_and_dst() {
    let (_g, core) = open("tzq-at");
    let mut s = session();
    let q = |s: &mut SessionState, sql: &str| one(&core, s, sql);
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMP '2026-01-01 00:00:00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "2025-12-31 15:00:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMPTZ '2026-01-01 00:00:00+00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "2026-01-01 09:00:00"
    );
    // DST: gap は遷移前の offset（-05）、fold は遷移後の offset（-05）で解釈する（PG 17 の実測）。
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMP '2026-03-08 02:30:00' AT TIME ZONE 'America/New_York'"
        ),
        "2026-03-08 07:30:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMP '2026-11-01 01:30:00' AT TIME ZONE 'America/New_York'"
        ),
        "2026-11-01 06:30:00+00"
    );
    // 南半球（Australia/Sydney。2026-10-04 02:00 に +10 → +11、2026-04-05 03:00 に +11 → +10）。
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMP '2026-10-04 02:30:00' AT TIME ZONE 'Australia/Sydney'"
        ),
        "2026-10-03 16:30:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMP '2026-04-05 02:30:00' AT TIME ZONE 'Australia/Sydney'"
        ),
        "2026-04-04 16:30:00+00"
    );
    // 関数形と AT LOCAL（セッション TZ）。
    assert_eq!(
        q(
            &mut s,
            "SELECT timezone('Asia/Tokyo', TIMESTAMP '2026-01-01 00:00:00')"
        ),
        "2025-12-31 15:00:00+00"
    );
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(
        q(&mut s, "SELECT TIMESTAMP '2026-01-01 00:00:00' AT LOCAL"),
        "2025-12-31 15:00:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT TIMESTAMPTZ '2026-01-01 00:00:00+00' AT LOCAL"
        ),
        "2026-01-01 09:00:00"
    );
    // DATE は局所の 0 時の timestamptz を経由して timestamp になる。
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(
        q(&mut s, "SELECT DATE '2026-01-01' AT TIME ZONE 'Asia/Tokyo'"),
        "2026-01-01 09:00:00"
    );
    // NULL の zone は NULL（strict）。
    ok(&core, &mut s, "CREATE TABLE z (name TEXT, t TIMESTAMP)");
    ok(&core, &mut s, "INSERT INTO z (id, name, t) VALUES (1, 'Asia/Tokyo', '2026-01-01 00:00:00') USING OPERATION_ID 'z1'");
    ok(
        &core,
        &mut s,
        "INSERT INTO z (id, t) VALUES (2, '2026-01-01 00:00:00') USING OPERATION_ID 'z2'",
    );
    let r = rows(
        &core,
        &mut s,
        "SELECT id, t AT TIME ZONE name FROM z ORDER BY id LIMIT 10",
    );
    assert_eq!(r[0], ["1", "2025-12-31 15:00:00+00"]);
    assert_eq!(r[1], ["2", "NULL"]);
}

#[test]
fn zone_argument_forms_and_errors() {
    let (_g, core) = open("tzq-zone");
    let mut s = session();
    let at = |s: &mut SessionState, z: &str| {
        one(
            &core,
            s,
            &format!("SELECT TIMESTAMP '2026-01-01 00:00:00' AT TIME ZONE '{z}'"),
        )
    };
    // POSIX 解釈で符号が反転する（SET TIME ZONE の `'+9'` とは逆）。
    for z in ["+9", "9", "+09:00", "09:00"] {
        assert_eq!(at(&mut s, z), "2026-01-01 09:00:00+00", "{z}");
    }
    assert_eq!(at(&mut s, "ASIA/TOKYO"), "2025-12-31 15:00:00+00");
    assert_eq!(at(&mut s, "UTC"), "2026-01-01 00:00:00+00");
    // 不正な zone は 22023 で、文言に入力値を含めない。
    for z in ["Foo/Bar", "Etc/Unknown", "CET", "+0900", ""] {
        let (c, msg) = err(
            &core,
            &mut s,
            &format!("SELECT TIMESTAMP '2026-01-01 00:00:00' AT TIME ZONE '{z}'"),
        );
        assert_eq!(c, "22023", "{z}");
        assert!(!z.is_empty() && !msg.contains(z) || z.is_empty(), "{msg}");
    }
    // 未実装の形は fail-closed。
    assert_eq!(
        code(&core, &mut s, "SELECT TIME '10:00:00' AT TIME ZONE 'UTC'"),
        "0A000"
    );
}

fn setup_events(core: &EngineCore, s: &mut SessionState) {
    ok(
        core,
        s,
        "CREATE TABLE ev (at TIMESTAMPTZ, ts TIMESTAMP, d DATE, n INTEGER)",
    );
    // 時点（UTC）: 1=2026-01-01 00:00Z, 2=2026-03-08 07:30Z（NY の gap 後）, 3=2026-06-15 12:00Z,
    // 4=2026-11-01 06:30Z（NY の fold 後）, 5=NULL。
    let data = [
        (
            1,
            "'2026-01-01 00:00:00+00'",
            "'2026-01-01 00:00:00'",
            "'2026-01-01'",
            1,
        ),
        (
            2,
            "'2026-03-08 07:30:00+00'",
            "'2026-03-08 07:30:00'",
            "'2026-03-08'",
            2,
        ),
        (
            3,
            "'2026-06-15 12:00:00+00'",
            "'2026-06-15 12:00:00'",
            "'2026-06-15'",
            3,
        ),
        (
            4,
            "'2026-11-01 06:30:00+00'",
            "'2026-11-01 06:30:00'",
            "'2026-11-01'",
            4,
        ),
    ];
    for (id, at, ts, d, n) in data {
        ok(
            core,
            s,
            &format!(
                "INSERT INTO ev (id, at, ts, d, n) VALUES ({id}, {at}, {ts}, {d}, {n}) \
                 USING OPERATION_ID 'ev{id}'"
            ),
        );
    }
    ok(
        core,
        s,
        "INSERT INTO ev (id, n) VALUES (5, 5) USING OPERATION_ID 'ev5'",
    );
}

fn ids(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    let sql = if sql.contains("LIMIT") {
        sql.to_string()
    } else {
        format!("{sql} LIMIT 200")
    };
    rows(core, s, &sql)
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

/// 述語・並べ替え・集計は UTC の時点で行われ、`SET TIME ZONE` を変えても結果が変わらない。
#[test]
fn predicates_ordering_and_aggregates_are_instant_based() {
    let (_g, core) = open("tzq-pred");
    let mut s = session();
    setup_events(&core, &mut s);
    let zones = ["UTC", "Asia/Tokyo", "America/New_York"];
    for z in zones {
        ok(&core, &mut s, &format!("SET TIME ZONE '{z}'"));
        // 明示 offset の文字列リテラル（TZ 非依存）。
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM ev WHERE at >= '2026-03-08 07:30:00+00' ORDER BY id"
            ),
            ["2", "3", "4"],
            "{z}"
        );
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM ev WHERE at = TIMESTAMPTZ '2026-06-15 21:00:00+09' ORDER BY id"
            ),
            ["3"],
            "{z}"
        );
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM ev WHERE at BETWEEN '2026-02-01 00:00:00+00' AND '2026-07-01 00:00:00+00' ORDER BY id"
            ),
            ["2", "3"],
            "{z}"
        );
        assert_eq!(
            ids(&core, &mut s, "SELECT id FROM ev WHERE at IN ('2026-01-01 09:00:00+09', '2026-11-01 06:30:00+00') ORDER BY id"),
            ["1", "4"],
            "{z}"
        );
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM ev WHERE at IS NOT NULL ORDER BY at DESC"
            ),
            ["4", "3", "2", "1"],
            "{z}"
        );
        let agg = rows(
            &core,
            &mut s,
            "SELECT MIN(at), MAX(at), COUNT(at), COUNT(DISTINCT at) FROM ev",
        );
        assert_eq!(
            agg,
            vec![vec![
                "2026-01-01 00:00:00+00".to_string(),
                "2026-11-01 06:30:00+00".to_string(),
                "4".to_string(),
                "4".to_string()
            ]],
            "{z}"
        );
    }
    // SUM/AVG は未定義（42883）。
    assert_eq!(code(&core, &mut s, "SELECT SUM(at) FROM ev"), "42883");
    assert_eq!(code(&core, &mut s, "SELECT AVG(at) FROM ev"), "42883");
}

/// ゾーン無しの文字列・TIMESTAMP・DATE との比較はセッション TZ の壁時計として解釈される。
#[test]
fn mixed_comparisons_follow_the_session_zone() {
    let (_g, core) = open("tzq-mixed");
    let mut s = session();
    setup_events(&core, &mut s);
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    // 2026-01-01 09:00 JST = 2026-01-01 00:00Z。
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE at = '2026-01-01 09:00:00'"
        ),
        ["1"]
    );
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE at = TIMESTAMP '2026-01-01 09:00:00'"
        ),
        ["1"]
    );
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE at >= DATE '2026-06-15' ORDER BY id"
        ),
        ["3", "4"]
    );
    // TIMESTAMP 列 vs TIMESTAMPTZ のリテラル: 列側をセッション TZ で timestamptz へ変換する。
    // ts=2026-01-01 00:00 は JST では前日 15:00Z。
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE ts = TIMESTAMPTZ '2025-12-31 15:00:00+00'"
        ),
        ["1"]
    );
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE ts = TIMESTAMPTZ '2026-01-01 00:00:00+00'"
        ),
        ["1"]
    );
    // 列どうし（timestamptz 列 vs timestamp 列）。UTC なら全行一致する。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM ev WHERE at = ts ORDER BY id"),
        ["1", "2", "3", "4"]
    );
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM ev WHERE at = ts ORDER BY id"),
        Vec::<String>::new()
    );
}

/// EXTRACT／date_part／date_trunc は評価時のセッション TZ で行う。
#[test]
fn extract_and_date_trunc_use_the_session_zone() {
    let (_g, core) = open("tzq-extract");
    let mut s = session();
    let q = |s: &mut SessionState, sql: &str| one(&core, s, sql);
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    let t = "TIMESTAMPTZ '2026-01-01 00:00:00+00'";
    assert_eq!(q(&mut s, &format!("SELECT EXTRACT(hour FROM {t})")), "9");
    assert_eq!(
        q(&mut s, &format!("SELECT EXTRACT(epoch FROM {t})")),
        "1767225600"
    );
    assert_eq!(
        q(&mut s, &format!("SELECT EXTRACT(timezone FROM {t})")),
        "32400"
    );
    assert_eq!(
        q(&mut s, &format!("SELECT EXTRACT(timezone_hour FROM {t})")),
        "9"
    );
    assert_eq!(
        q(&mut s, &format!("SELECT EXTRACT(timezone_minute FROM {t})")),
        "0"
    );
    assert_eq!(
        q(&mut s, &format!("SELECT date_trunc('day', {t})")),
        "2025-12-31 15:00:00+00"
    );
    // 第 3 引数の TZ。
    assert_eq!(
        q(
            &mut s,
            &format!("SELECT date_trunc('day', {t}, 'America/New_York')")
        ),
        "2025-12-31 05:00:00+00"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            &format!("SELECT date_trunc('day', {t}, 'Foo/Bar')")
        ),
        "22023"
    );
    // timestamp に対する timezone 系 field は 0A000。
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT EXTRACT(timezone FROM TIMESTAMP '2026-01-01 00:00:00')"
        ),
        "0A000"
    );
    // DST: day 以上は切り捨て後の局所時刻を gap／fold 規則で再解決し、hour 以下は元の offset を保つ。
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    assert_eq!(
        q(
            &mut s,
            "SELECT date_trunc('day', TIMESTAMPTZ '2026-03-08 16:00:00+00')"
        ),
        "2026-03-08 05:00:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT date_trunc('hour', TIMESTAMPTZ '2026-11-01 05:30:00+00')"
        ),
        "2026-11-01 05:00:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT date_trunc('hour', TIMESTAMPTZ '2026-11-01 06:30:00+00')"
        ),
        "2026-11-01 06:00:00+00"
    );
    assert_eq!(
        q(
            &mut s,
            "SELECT date_trunc('hour', TIMESTAMPTZ '2026-03-08 07:30:00+00')"
        ),
        "2026-03-08 07:00:00+00"
    );
}
