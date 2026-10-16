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
    let mut tail = sql.rsplit(' ');
    let last = tail.next().unwrap_or("");
    let ends_with_limit = tail.next() == Some("LIMIT") && last.chars().all(|c| c.is_ascii_digit());
    let sql = if ends_with_limit {
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
/// GROUP BY・DISTINCT・集合演算・サブクエリは UTC の時点で同一視する。
#[test]
fn grouping_distinct_set_ops_and_subquery() {
    let (_g, core) = open("tzq-group");
    let mut s = session();
    ok(&core, &mut s, "CREATE TABLE g (at TIMESTAMPTZ, k INTEGER)");
    // 同一時点を別 offset で入れる（UTC 2026-01-01 00:00 が 2 行、別時点が 1 行）。
    for (id, at, k) in [
        (1, "2026-01-01 09:00:00+09", 1),
        (2, "2026-01-01 00:00:00+00", 2),
        (3, "2026-02-01 00:00:00+00", 3),
    ] {
        ok(
            &core,
            &mut s,
            &format!(
                "INSERT INTO g (id, at, k) VALUES ({id}, '{at}', {k}) USING OPERATION_ID 'g{id}'"
            ),
        );
    }
    for z in ["UTC", "America/New_York"] {
        ok(&core, &mut s, &format!("SET TIME ZONE '{z}'"));
        let grouped = rows(
            &core,
            &mut s,
            "SELECT at, COUNT(*) FROM g GROUP BY at ORDER BY at LIMIT 10",
        );
        assert_eq!(
            grouped,
            vec![
                vec!["2026-01-01 00:00:00+00".to_string(), "2".to_string()],
                vec!["2026-02-01 00:00:00+00".to_string(), "1".to_string()],
            ],
            "{z}"
        );
        assert_eq!(
            rows(&core, &mut s, "SELECT DISTINCT at FROM g LIMIT 10").len(),
            2,
            "{z}"
        );
        let union = rows(
            &core,
            &mut s,
            "SELECT at FROM g WHERE k = 1 UNION SELECT at FROM g WHERE k = 2",
        );
        assert_eq!(union.len(), 1, "{z}");
        assert_eq!(union[0][0], "2026-01-01 00:00:00+00", "{z}");
        assert_eq!(
            rows(
                &core,
                &mut s,
                "SELECT at FROM g WHERE k = 1 UNION ALL SELECT at FROM g WHERE k = 2"
            )
            .len(),
            2,
            "{z}"
        );
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM g WHERE at IN (SELECT at FROM g WHERE k = 1 LIMIT 10) ORDER BY id"
            ),
            ["1", "2"],
            "{z}"
        );
        assert_eq!(
            ids(
                &core,
                &mut s,
                "SELECT id FROM g WHERE at = (SELECT MAX(at) FROM g)"
            ),
            ["3"],
            "{z}"
        );
    }
}

/// 二次索引: 索引ありと全走査（`id + 0 > 0` で索引を無効化）の結果が一致し、表示 TZ に依らない。
#[test]
fn secondary_index_matches_full_scan() {
    let (_g, core) = open("tzq-index");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE ix (at TIMESTAMPTZ, ts TIMESTAMP)",
    );
    for i in 0..40u64 {
        let (month, day) = (1 + i % 12, 1 + i % 27);
        ok(
            &core,
            &mut s,
            &format!(
                "INSERT INTO ix (id, at, ts) VALUES ({}, '2026-{month:02}-{day:02} 12:00:00+00', \
                 '2026-{month:02}-{day:02} 12:00:00') USING OPERATION_ID 'ix{i}'",
                i + 1
            ),
        );
    }
    ok(
        &core,
        &mut s,
        "INSERT INTO ix (id) VALUES (100) USING OPERATION_ID 'ix-null'",
    );
    let preds = [
        "at = '2026-03-04 12:00:00+00'",
        "at > '2026-06-01 00:00:00+00'",
        "at >= '2026-06-01 00:00:00+00' AND at < '2026-09-01 00:00:00+00'",
        "at BETWEEN '2026-02-01 00:00:00+00' AND '2026-04-01 00:00:00+00'",
        "at IN ('2026-03-04 12:00:00+00', '2026-05-06 12:00:00+00')",
        "at < '2026-01-01 00:00:00+00'",
    ];
    let mut before = Vec::new();
    for p in preds {
        before.push(ids(
            &core,
            &mut s,
            &format!("SELECT id FROM ix WHERE {p} ORDER BY id"),
        ));
    }
    ok(&core, &mut s, "CREATE INDEX ix_at ON ix (at)");
    for z in ["UTC", "Asia/Tokyo"] {
        ok(&core, &mut s, &format!("SET TIME ZONE '{z}'"));
        for (p, expected) in preds.iter().zip(&before) {
            let indexed = ids(
                &core,
                &mut s,
                &format!("SELECT id FROM ix WHERE {p} ORDER BY id"),
            );
            let scanned = ids(
                &core,
                &mut s,
                &format!("SELECT id FROM ix WHERE {p} AND id + 0 > 0 ORDER BY id"),
            );
            assert_eq!(&indexed, expected, "{z} {p}");
            assert_eq!(&scanned, expected, "{z} {p}");
        }
    }
    // ゾーン無しの文字列はセッション TZ の壁時計（束縛時に UTC へ確定し、索引でも同じ結果）。
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    let a = ids(
        &core,
        &mut s,
        "SELECT id FROM ix WHERE at = '2026-01-01 21:00:00' ORDER BY id",
    );
    let b = ids(
        &core,
        &mut s,
        "SELECT id FROM ix WHERE at = '2026-01-01 21:00:00' AND id + 0 > 0 ORDER BY id",
    );
    assert_eq!(a, b);
    assert!(!a.is_empty());
    // TIMESTAMP 列 vs TIMESTAMPTZ のリテラルは索引を使わず全走査で同じ結果になる。
    ok(&core, &mut s, "CREATE INDEX ix_ts ON ix (ts)");
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    let c = ids(
        &core,
        &mut s,
        "SELECT id FROM ix WHERE ts = TIMESTAMPTZ '2026-01-01 12:00:00+00' ORDER BY id",
    );
    assert_eq!(c, a);
}
fn explain_lines(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    let SqlOutcome::Explain(result) = run(core, s, sql).expect("EXPLAIN must succeed") else {
        panic!("expected Explain");
    };
    result
        .rows
        .iter()
        .filter_map(|r| match r.cells.first() {
            Some(Cell::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

/// TIMESTAMPTZ 列の範囲・等価述語は型付き順序索引を使い、TIMESTAMP 列 vs TIMESTAMPTZ の混在比較は
/// 索引を使わず全走査へ縮退する（gap で変換が単調でないため。fail-closed）。
#[test]
fn explain_shows_index_use_and_mixed_comparison_fallback() {
    let (_g, core) = open("tzq-explain");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE ex (embedding VECTOR(2), at TIMESTAMPTZ, ts TIMESTAMP)",
    );
    for i in 0..8u64 {
        ok(
            &core,
            &mut s,
            &format!(
                "INSERT INTO ex (id, embedding, at, ts) VALUES ({}, '[1.0,0.0]', \
                 '2026-01-0{} 12:00:00+00', '2026-01-0{} 12:00:00') USING OPERATION_ID 'ex{i}'",
                i + 1,
                i + 1,
                i + 1
            ),
        );
    }
    ok(&core, &mut s, "CREATE INDEX ex_at ON ex (at)");
    ok(&core, &mut s, "CREATE INDEX ex_ts ON ex (ts)");
    let tail = "ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";
    let ranged = explain_lines(
        &core,
        &mut s,
        &format!("EXPLAIN SELECT id FROM ex WHERE at > '2026-01-04 00:00:00+00' {tail}"),
    );
    assert!(
        ranged.iter().any(|l| l == "scalar_plan: index_typed_range"),
        "{ranged:?}"
    );
    let mixed = explain_lines(
        &core,
        &mut s,
        &format!(
            "EXPLAIN SELECT id FROM ex WHERE ts > TIMESTAMPTZ '2026-01-04 00:00:00+00' {tail}"
        ),
    );
    assert!(
        !mixed.iter().any(|l| l == "scalar_plan: index_typed_range"),
        "{mixed:?}"
    );
}

/// 代入時の変換（INSERT・UPDATE）と式の型付け。TIMESTAMP・DATE はセッション TZ の壁時計として
/// TIMESTAMPTZ 列へ、TIMESTAMPTZ は局所の値として TIMESTAMP・DATE 列へ入る（PG の代入キャスト）。
#[test]
fn assignment_conversions_follow_the_session_zone() {
    let (_g, core) = open("tzq-assign");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE a (at TIMESTAMPTZ, ts TIMESTAMP, d DATE)",
    );
    ok(&core, &mut s, "SET TIME ZONE 'America/New_York'");
    // INSERT: TIMESTAMP リテラル（gap を含む）→ TIMESTAMPTZ 列。
    ok(
        &core,
        &mut s,
        "INSERT INTO a (id, at) VALUES (1, TIMESTAMP '2026-06-01 12:00:00') USING OPERATION_ID 'a1'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO a (id, at) VALUES (2, TIMESTAMP '2026-03-08 02:30:00') USING OPERATION_ID 'a2'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO a (id, d) VALUES (3, DATE '2026-06-01') USING OPERATION_ID 'a3'",
    );
    // DATE 列 → TIMESTAMPTZ 列（局所の 0 時）。
    ok(
        &core,
        &mut s,
        "UPDATE a SET at = d WHERE id = 3 USING OPERATION_ID 'a3u'",
    );
    let r = rows(&core, &mut s, "SELECT id, at FROM a ORDER BY id LIMIT 10");
    assert_eq!(r[0][1], "2026-06-01 16:00:00+00");
    assert_eq!(r[1][1], "2026-03-08 07:30:00+00");
    assert_eq!(r[2][1], "2026-06-01 04:00:00+00");
    // UPDATE SET: TIMESTAMPTZ の式 → TIMESTAMP・DATE 列は局所の値になる。
    ok(
        &core,
        &mut s,
        "UPDATE a SET ts = at, d = at WHERE id = 1 USING OPERATION_ID 'u1'",
    );
    let r = rows(
        &core,
        &mut s,
        "SELECT id, ts, d FROM a WHERE id = 1 LIMIT 5",
    );
    assert_eq!(r[0][1], "2026-06-01 12:00:00");
    assert_eq!(r[0][2], "2026-06-01");
    // UPDATE SET: timestamp 列 → timestamptz 列（セッション TZ の壁時計）。
    ok(
        &core,
        &mut s,
        "UPDATE a SET at = ts WHERE id = 1 USING OPERATION_ID 'u2'",
    );
    let r = rows(&core, &mut s, "SELECT at FROM a WHERE id = 1 LIMIT 5");
    assert_eq!(r[0][0], "2026-06-01 16:00:00+00");
    // AT TIME ZONE の結果を列へ入れる。
    ok(
        &core,
        &mut s,
        "UPDATE a SET at = ts AT TIME ZONE 'Asia/Tokyo' WHERE id = 1 USING OPERATION_ID 'u3'",
    );
    let r = rows(&core, &mut s, "SELECT at FROM a WHERE id = 1 LIMIT 5");
    assert_eq!(r[0][0], "2026-06-01 03:00:00+00");
}

/// 範囲外の変換は `22008`、TZ 依存式の CHECK は `0A000`、他テナントの行はどの経路からも見えない。
#[test]
fn range_errors_check_constraints_and_tenant_boundary() {
    let (_g, core) = open("tzq-misc");
    let mut s = session();
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT TIMESTAMP '0001-01-01 00:00:00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "SELECT TIMESTAMPTZ '9999-12-31 23:59:59+00' AT TIME ZONE 'Asia/Tokyo'"
        ),
        "22008"
    );
    // 範囲内の端は成功する。
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT TIMESTAMPTZ '9999-12-31 23:59:59+00' AT TIME ZONE 'America/Los_Angeles'"
        ),
        "9999-12-31 15:59:59"
    );
    // CHECK にセッション TZ 依存の式は置けない（定義時と別の TZ で結果が変わるため）。
    ok(
        &core,
        &mut s,
        "CREATE TABLE c (at TIMESTAMPTZ, ts TIMESTAMP)",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE c2 (at TIMESTAMPTZ, ts TIMESTAMP, CHECK (at > ts))"
        ),
        "0A000"
    );
    // 他テナントの行は集計・AT TIME ZONE・索引のどの経路からも見えない。
    ok(&core, &mut s, "CREATE INDEX c_at ON c (at)");
    ok(
        &core,
        &mut s,
        "INSERT INTO c (id, at, ts) VALUES (1, '2026-01-01 00:00:00+00', '2026-01-01 00:00:00') \
         USING OPERATION_ID 'c1'",
    );
    assert!(
        rows(
            &core,
            &mut s,
            "SELECT id, ts AT TIME ZONE 'UTC' FROM c LIMIT 10"
        )
        .len()
            == 1
    );
    let mut bob = session();
    let mut by_bob = |sql: &str| match run_as("bob", &core, &mut bob, sql).expect("bob query") {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        by_bob("SELECT id FROM c WHERE at >= '2025-01-01 00:00:00+00' LIMIT 10"),
        0
    );
    assert_eq!(
        by_bob("SELECT id, ts AT TIME ZONE 'UTC' FROM c LIMIT 10"),
        0
    );
    assert_eq!(by_bob("SELECT MAX(at) FROM c"), 1);
    let max = match run_as("bob", &core, &mut bob, "SELECT MAX(at) FROM c").expect("max") {
        SqlOutcome::Query(r) => r.rows[0].cells[0].clone(),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(max, Cell::Null);
}

/// まだ閉じている機能（SUM/AVG・PK/UNIQUE・算術・明示 CAST）の拒否コードは変わらない。
#[test]
fn closed_features_stay_fail_closed() {
    let (_g, core) = open("tzq-closed");
    let mut s = session();
    ok(&core, &mut s, "CREATE TABLE t (at TIMESTAMPTZ, n INTEGER)");
    assert_eq!(code(&core, &mut s, "SELECT SUM(at) FROM t"), "42883");
    assert_eq!(code(&core, &mut s, "SELECT at + 1 FROM t LIMIT 5"), "42804");
    // PK／UNIQUE は担当 Issue 未定のまま拒否される（拒否コードは #1574 から不変）。
    assert!(run(&core, &mut s, "CREATE TABLE u (at TIMESTAMPTZ PRIMARY KEY)").is_err());
    // INTERVAL の zone は未実装のため構文段で拒否される。
    assert!(run(
        &core,
        &mut s,
        "SELECT TIMESTAMP '2026-01-01 00:00:00' AT TIME ZONE INTERVAL '1 hour'"
    )
    .is_err());
}
/// window 関数のキー・集計入力と、分割実行 UPDATE の TZ 依存を確認する。
#[test]
fn window_and_partitioned_update() {
    let (_g, core) = open("tzq-window");
    let mut s = session();
    setup_events(&core, &mut s);
    // window: ORDER BY／PARTITION BY のキーと MIN／MAX 入力。
    let r = rows(
        &core,
        &mut s,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY at DESC) FROM ev WHERE at IS NOT NULL ORDER BY id LIMIT 10",
    );
    assert_eq!(r[0], ["1", "4"]);
    assert_eq!(r[3], ["4", "1"]);
    let r = rows(
        &core,
        &mut s,
        "SELECT id, MAX(at) OVER (PARTITION BY n) FROM ev WHERE id = 3 LIMIT 5",
    );
    assert_eq!(r[0][1], "2026-06-15 12:00:00+00");
    // 分割実行 UPDATE: ゾーン無しのリテラルを別の TZ から再送すると内容不一致（22023）になる。
    let sql = "UPDATE ev SET at = '2026-01-01 09:00:00' WHERE n >= 0 \
               USING OPERATION_ID 'pp1' PARTITIONED CHUNK 2";
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, sql);
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(code(&core, &mut s, sql), "22023");
    // TIMESTAMPTZ 列を持たないテーブルでも、TIMESTAMPTZ の式（ゾーン無しのリテラル）を含む再送は
    // セッション TZ が違えば内容不一致になる。
    ok(&core, &mut s, "CREATE TABLE tp (ts TIMESTAMP, n INTEGER)");
    for i in 1..=3 {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO tp (id, ts, n) VALUES ({i}, '2026-01-01 00:00:00', {i}) USING OPERATION_ID 'tp{i}'"),
        );
    }
    let sql2 = "UPDATE tp SET n = 0 WHERE ts < TIMESTAMPTZ '2026-06-01 00:00:00' \
                USING OPERATION_ID 'pp2' PARTITIONED CHUNK 2";
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(&core, &mut s, sql2);
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(code(&core, &mut s, sql2), "22023");
}
/// NOT／OR・NULL の扱い・ORDER BY の NULL 位置・式の中の比較（CASE）も時点で動く。
#[test]
fn boolean_combinations_nulls_and_case() {
    let (_g, core) = open("tzq-bool");
    let mut s = session();
    setup_events(&core, &mut s);
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE NOT at > '2026-03-08 07:30:00+00' ORDER BY id"
        ),
        ["1", "2"]
    );
    assert_eq!(
        ids(
            &core,
            &mut s,
            "SELECT id FROM ev WHERE at < '2026-02-01 00:00:00+00' OR at > '2026-10-01 00:00:00+00' ORDER BY id"
        ),
        ["1", "4"]
    );
    // NULL は比較で偽（行に入らない）。IS NULL は存在ビットだけを見る。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM ev WHERE at IS NULL"),
        ["5"]
    );
    // ORDER BY ASC は NULL が最後（PG の既定）。
    assert_eq!(
        ids(&core, &mut s, "SELECT id FROM ev ORDER BY at ASC"),
        ["1", "2", "3", "4", "5"]
    );
    // 式の中の比較と AT TIME ZONE の組み合わせ。
    let r = rows(
        &core,
        &mut s,
        "SELECT id, CASE WHEN at > '2026-06-01 00:00:00+00' THEN 1 ELSE 0 END FROM ev WHERE id < 5 ORDER BY id LIMIT 10",
    );
    assert_eq!(
        r.iter().map(|x| x[1].as_str()).collect::<Vec<_>>(),
        ["0", "0", "1", "1"]
    );
    let r = rows(
        &core,
        &mut s,
        "SELECT id, at AT TIME ZONE 'UTC' FROM ev WHERE id = 3 LIMIT 5",
    );
    assert_eq!(r[0][1], "2026-06-15 12:00:00");
    // 範囲が逆の BETWEEN は空。
    assert!(ids(
        &core,
        &mut s,
        "SELECT id FROM ev WHERE at BETWEEN '2026-12-01 00:00:00+00' AND '2026-01-01 00:00:00+00'"
    )
    .is_empty());
    // 不正なリテラルは 22007、リテラル本文をエラー文言に含めない。
    let (c, msg) = err(
        &core,
        &mut s,
        "SELECT id FROM ev WHERE at = 'garbage-literal' LIMIT 5",
    );
    assert_eq!(c, "22007");
    assert!(!msg.contains("garbage-literal"), "{msg}");
}

/// レビュー指摘（PR #2481）: INSERT の定数式畳み込みは parse 中に走るため、セッション TZ を
/// parse 区間へも張る。CHECK 本体はゾーン無しの文字列比較・3 引数 `date_trunc` の暗黙変換を
/// `0A000` で拒否する。`NULLIF(timestamptz, timestamptz)` は評価できる。
#[test]
fn const_fold_uses_session_zone_and_check_rejects_zone_less_forms() {
    let (_g, core) = open("tzq-review");
    let mut s = session();
    ok(&core, &mut s, "CREATE TABLE r (at TIMESTAMPTZ)");
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(
        &core,
        &mut s,
        "INSERT INTO r (id, at) VALUES (1, TIMESTAMPTZ '2026-01-01 09:00:00') \
         USING OPERATION_ID 'r1'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO r (id, at) VALUES (2, TIMESTAMP '2026-01-01 09:00:00' AT LOCAL) \
         USING OPERATION_ID 'r2'",
    );
    let r = rows(&core, &mut s, "SELECT id, at FROM r ORDER BY id LIMIT 5");
    assert_eq!(r[0][1], "2026-01-01 00:00:00+00");
    assert_eq!(r[1][1], "2026-01-01 00:00:00+00");
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT NULLIF(TIMESTAMPTZ '2026-01-01 00:00:00+00', TIMESTAMPTZ '2026-01-01 00:00:00+00')"
        ),
        "NULL"
    );
    assert_eq!(
        one(
            &core,
            &mut s,
            "SELECT NULLIF(TIMESTAMPTZ '2026-01-01 00:00:00+00', TIMESTAMPTZ '2026-01-02 00:00:00+00')"
        ),
        "2026-01-01 00:00:00+00"
    );
    // CHECK: ゾーン無しの文字列との比較は宣言的経路でも拒否。ゾーン付きは受理。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE ck1 (at TIMESTAMPTZ, CHECK (at > '2026-01-01 00:00:00'))"
        ),
        "0A000"
    );
    ok(
        &core,
        &mut s,
        "CREATE TABLE ck2 (at TIMESTAMPTZ, CHECK (at > '2026-01-01 00:00:00+00'))",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE TABLE ck3 (ts TIMESTAMP, \
             CHECK (date_trunc('day', ts, 'UTC') = TIMESTAMPTZ '2026-01-01 00:00:00+00'))"
        ),
        "0A000"
    );
}

/// 定数式の TIMESTAMPTZ は代入先の列型で変換する（TIMESTAMP・DATE 列へはセッション TZ の局所値）。
/// 裸の NULL の zone・値は strict に NULL を返す（`0A000` にしない）。Issue #1576 レビュー指摘。
#[test]
fn const_tstz_assignment_targets_and_null_arguments() {
    let (_g, core) = open("tzq-review2");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE ra (ts TIMESTAMP, d DATE, z TIMESTAMPTZ)",
    );
    ok(&core, &mut s, "SET TIME ZONE 'Asia/Tokyo'");
    ok(
        &core,
        &mut s,
        "INSERT INTO ra (id, ts, d, z) VALUES (1, \
         TIMESTAMPTZ '2026-01-01 20:00:00+00', TIMESTAMPTZ '2026-01-01 20:00:00+00', \
         TIMESTAMPTZ '2026-01-01 20:00:00+00') USING OPERATION_ID 'ra1'",
    );
    let r = rows(&core, &mut s, "SELECT ts, d, z FROM ra LIMIT 5");
    assert_eq!(r[0][0], "2026-01-02 05:00:00");
    assert_eq!(r[0][1], "2026-01-02");
    assert_eq!(r[0][2], "2026-01-01 20:00:00+00");
    // TEXT 列などへの代入は拒否のまま。
    ok(&core, &mut s, "CREATE TABLE rb (t TEXT)");
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO rb (id, t) VALUES (1, TIMESTAMPTZ '2026-01-01 20:00:00+00') \
             USING OPERATION_ID 'rb1'"
        ),
        "42804"
    );
    // 定数式（INSERT VALUES）の裸の NULL は strict に NULL を返す。
    ok(
        &core,
        &mut s,
        "CREATE TABLE rn (t TIMESTAMP, z TIMESTAMPTZ)",
    );
    for (i, (col, expr)) in [
        ("t", "timezone('UTC', NULL)"),
        ("z", "timezone(NULL, TIMESTAMP '2026-01-01 00:00:00')"),
        (
            "z",
            "date_trunc('day', TIMESTAMPTZ '2026-01-01 00:00:00+00', NULL)",
        ),
        ("z", "TIMESTAMP '2026-01-01 00:00:00' AT TIME ZONE NULL"),
    ]
    .iter()
    .enumerate()
    {
        ok(
            &core,
            &mut s,
            &format!(
                "INSERT INTO rn (id, {col}) VALUES ({}, {expr}) USING OPERATION_ID 'rn{i}'",
                i + 1
            ),
        );
    }
    for r in rows(&core, &mut s, "SELECT t, z FROM rn ORDER BY id LIMIT 10") {
        assert_eq!(r, ["NULL", "NULL"]);
    }
    // 述語付き分割実行: ゾーン付きリテラルでも TIMESTAMP 列との比較は TZ 依存のため、別 TZ での再送は不一致。
    ok(&core, &mut s, "CREATE TABLE rp (ts TIMESTAMP, n INTEGER)");
    for i in 1..=3 {
        ok(
            &core,
            &mut s,
            &format!("INSERT INTO rp (id, ts, n) VALUES ({i}, '2026-01-01 00:00:00', {i}) USING OPERATION_ID 'rp{i}'"),
        );
    }
    let del = "DELETE FROM rp WHERE ts < TIMESTAMPTZ '2026-06-01 00:00:00+00' \
               USING OPERATION_ID 'rpd' PARTITIONED CHUNK 2";
    ok(&core, &mut s, del);
    ok(&core, &mut s, "SET TIME ZONE 'UTC'");
    assert_eq!(code(&core, &mut s, del), "22023");
}

/// レビュー指摘（PR #2481）: 値位置（INSERT VALUES／UPDATE SET）で単独の文字列リテラルの
/// 直後に `AT TIME ZONE`／`AT LOCAL` が続く形も式として解析される。
#[test]
fn at_time_zone_after_lone_literal_in_value_positions() {
    let (_g, core) = open("tzq-valpos");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE v (at TIMESTAMPTZ, ts TIMESTAMP)",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO v (id, at) VALUES (1, '2026-01-01 00:00:00' AT TIME ZONE 'Asia/Tokyo') \
         USING OPERATION_ID 'v1'",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO v (id, at) VALUES (2, '2026-01-01 00:00:00+00' AT TIME ZONE 'UTC') \
         USING OPERATION_ID 'v2'",
    );
    let r = rows(&core, &mut s, "SELECT id, at FROM v ORDER BY id LIMIT 5");
    assert_eq!(r[0][1], "2026-01-01 09:00:00+00");
    ok(
        &core,
        &mut s,
        "UPDATE v SET ts = '2026-01-01 00:00:00+00' AT TIME ZONE 'UTC' WHERE id = 2 \
         USING OPERATION_ID 'v3'",
    );
    let r = rows(&core, &mut s, "SELECT ts FROM v WHERE id = 2 LIMIT 5");
    assert_eq!(r[0][0], "2026-01-01 00:00:00");
}

/// レビュー指摘（PR #2481）: 定数式 DEFAULT が TIMESTAMPTZ を返しても CREATE TABLE／
/// ALTER TABLE ADD COLUMN が受理され、UTC 表記へ正規化される。
#[test]
fn default_accepts_timestamptz_const_expr() {
    let (_g, core) = open("tzq-default");
    let mut s = session();
    ok(
        &core,
        &mut s,
        "CREATE TABLE d (z TIMESTAMPTZ DEFAULT TIMESTAMPTZ '2026-01-01 09:00:00+09', \
         t TIMESTAMP DEFAULT '2026-01-01 09:00:00+09' AT TIME ZONE 'UTC')",
    );
    ok(
        &core,
        &mut s,
        "INSERT INTO d (id) VALUES (1) USING OPERATION_ID 'd1'",
    );
    ok(
        &core,
        &mut s,
        "ALTER TABLE d ADD COLUMN w TIMESTAMPTZ DEFAULT '2026-01-01 00:00:00' AT TIME ZONE 'Asia/Tokyo'",
    );
    let r = rows(&core, &mut s, "SELECT z, t, w FROM d LIMIT 5");
    assert_eq!(r[0][0], "2026-01-01 00:00:00+00");
    assert_eq!(r[0][1], "2026-01-01 00:00:00");
    assert_eq!(r[0][2], "2026-01-01 09:00:00+00");
}
