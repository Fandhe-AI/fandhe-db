//! `TIMESTAMPTZ` 列（`ColumnType::TimestampTz`。TABLE-33・Issue #1574）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され
//! 再オープン後も値ごと復元されること、(2) 各種 offset 表記・IANA 名・TZ なし入力が同じ UTC
//! マイクロ秒へ正規化されること、(3) INSERT・UPDATE SET・UPSERT・COPY FROM・列 DEFAULT の
//! 値束縛が書式違反を `22007`、範囲外を `22008`、未知の TZ 名を `22023` で拒否し副作用を残さない
//! こと、(4) 未解禁の機能（述語・並べ替え・集計・索引・PK/UNIQUE・型変更・配列・`TIMESTAMPTZ(p)`・
//! 型付きリテラル）が fail-closed に拒否されること、(5) テナント境界が不変であること、を固定する。
//! 述語・集計・索引・AT TIME ZONE は #1576、セッション TimeZone は #1575、OID 1184 と NoSQL は
//! #1577 の担当。

use engine::catalog::ColumnType;
use engine::core::{CopyPlan, EngineCore};
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

fn ctx() -> PolicyContext {
    ctx_for("alice")
}

fn open(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for(tenant), &mut s, sql)
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    exec_as(core, "alice", sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

/// `SELECT id, <col> ...` の結果を `(id, UTC マイクロ秒または None)` で返す。
fn micros(core: &EngineCore, sql: &str) -> Vec<(u64, Option<i64>)> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| {
            let v = match r.cells.get(1) {
                Some(Cell::TimestampTz(m)) => Some(*m),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (r.id, v)
        })
        .collect()
}

fn text_of(m: i64) -> String {
    engine::datetime::format_timestamptz(m, &engine::datetime::SessionTimeZone::utc())
}

fn insert(
    core: &EngineCore,
    id: u64,
    literal: &str,
    op: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    exec(
        core,
        &format!("INSERT INTO t (id, v) VALUES ({id}, {literal}) USING OPERATION_ID '{op}'"),
    )
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

fn find(rows: &[(u64, Option<i64>)], id: u64) -> Option<i64> {
    rows.iter().find(|r| r.0 == id).and_then(|r| r.1)
}

#[test]
fn timestamptz_and_with_time_zone_are_persisted_as_the_same_type() {
    let (path, _g, core) = open("tz-persist");
    ok(
        &core,
        "CREATE TABLE t (a TIMESTAMPTZ, b timestamp WITH TIME ZONE, c TIMESTAMP, d INTEGER)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN e TIMESTAMPTZ");
    ok(
        &core,
        "ALTER TABLE t ADD COLUMN f TIMESTAMPTZ DEFAULT '2026-01-01 00:00:00+09'",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::TimestampTz);
    assert_eq!(column_type(&path, "t", "b"), ColumnType::TimestampTz);
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Timestamp);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Integer);
    assert_eq!(column_type(&path, "t", "e"), ColumnType::TimestampTz);
    assert_eq!(column_type(&path, "t", "f"), ColumnType::TimestampTz);
}

/// 受け入れ基準: ±オフセットの表記が UTC に正規化され、同じ格納値になる。
#[test]
fn offset_notations_normalize_to_the_same_utc_value() {
    let (_p, _g, core) = open("tz-normalize");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    let literals = [
        "'2026-01-01 12:00:00+09'",
        "'2026-01-01 03:00:00Z'",
        "'2026-01-01 03:00:00'",
        "'2026-01-01 03:00:00+00:00'",
        "'2026-01-01 08:30:00+0530'",
        "'2025-12-31 22:00:00-05'",
        "'2026-01-01T12:00:00+09:00:00'",
        "'2026-01-01 12:00:00 Asia/Tokyo'",
        "'2026-01-01 03:00:00 UTC'",
    ];
    for (i, lit) in literals.iter().enumerate() {
        insert(&core, 1 + i as u64, lit, &format!("n{i}"))
            .unwrap_or_else(|e| panic!("{lit}: {e:?}"));
    }
    let got = micros(&core, "SELECT id, v FROM t LIMIT 20");
    assert_eq!(got.len(), literals.len());
    let first = got[0].1.expect("non-null");
    for (id, v) in &got {
        assert_eq!(
            *v,
            Some(first),
            "id {id} must normalize to the same UTC value"
        );
    }
    assert_eq!(text_of(first), "2026-01-01 03:00:00+00");
}

#[test]
fn boundary_values_roundtrip_and_one_microsecond_outside_is_rejected() {
    let (_p, _g, core) = open("tz-bounds");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    insert(&core, 1, "'0001-01-01 00:00:00+00'", "b1").expect("min");
    insert(&core, 2, "'9999-12-31 23:59:59.999999+00'", "b2").expect("max");
    let got = micros(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(
        find(&got, 1).map(text_of).as_deref(),
        Some("0001-01-01 00:00:00+00")
    );
    assert_eq!(
        find(&got, 2).map(text_of).as_deref(),
        Some("9999-12-31 23:59:59.999999+00")
    );
    // UTC に直して 1 µs でも範囲外なら 22008。
    for (id, lit) in [
        (3, "'0001-01-01 00:00:00+00:00:01'"),
        (4, "'9999-12-31 23:59:59.999999-01'"),
        (5, "'0001-01-01 00:00:00+01'"),
    ] {
        assert_eq!(
            insert(&core, id, lit, &format!("b{id}"))
                .expect_err(lit)
                .wire_code(),
            "22008",
            "{lit}"
        );
    }
    assert_eq!(micros(&core, "SELECT id, v FROM t LIMIT 10").len(), 2);
}

#[test]
fn insert_classifies_format_range_and_zone_errors() {
    let (_p, _g, core) = open("tz-errors");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    let cases = [
        // 範囲外は 22008。
        ("'2026-02-30 00:00:00Z'", "22008"),
        ("'2026-01-01 24:00:00Z'", "22008"),
        ("'2026-01-01 00:00:00+16'", "22008"),
        ("'2026-01-01 00:00:00+09:60'", "22008"),
        // 書式違反は 22007。
        ("'abc'", "22007"),
        ("''", "22007"),
        ("'2026-01-01'", "22007"),
        ("'2026-01-01 00:00'", "22007"),
        ("'2026-01-01 00:00:00.1234567Z'", "22007"),
        ("'2026-01-01 00:00:00+9'", "22007"),
        ("'2026-01-01 00:00:00+09:0'", "22007"),
        ("'2026-01-01 00:00:00 '", "22007"),
        // 未知の TZ 名は 22023。
        ("'2026-01-01 00:00:00 Mars/Base'", "22023"),
        ("'2026-01-01 00:00:00 Etc/Unknown'", "22023"),
        ("'2026-01-01 00:00:00 ../etc/passwd'", "22023"),
        // 数値・真偽値リテラルは型不一致（22000）。
        ("12", "22000"),
        ("true", "22000"),
    ];
    for (i, (lit, want)) in cases.iter().enumerate() {
        assert_eq!(
            insert(&core, 1 + i as u64, lit, &format!("e{i}"))
                .expect_err(lit)
                .wire_code(),
            *want,
            "for {lit}"
        );
    }
    assert!(
        micros(&core, "SELECT id, v FROM t LIMIT 30").is_empty(),
        "rejected rows must not be written"
    );
}

#[test]
fn update_set_and_upsert_bind_and_reject_without_side_effects() {
    let (_p, _g, core) = open("tz-update");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    insert(&core, 1, "'2026-01-01 00:00:00Z'", "i1").expect("insert");
    let before = micros(&core, "SELECT id, v FROM t LIMIT 5");
    for (sql, want) in [
        (
            "UPDATE t SET v = '2026-13-01 00:00:00Z' WHERE id = 1 USING OPERATION_ID 'u1'",
            "22008",
        ),
        (
            "UPDATE t SET v = 'xx' WHERE id = 1 USING OPERATION_ID 'u2'",
            "22007",
        ),
        (
            "UPDATE t SET v = '2026-01-01 00:00:00 Nowhere/Land' WHERE id = 1 USING OPERATION_ID 'u3'",
            "22023",
        ),
        (
            "INSERT INTO t (id, v) VALUES (1, '2026-13-01 00:00:00Z') ON CONFLICT (id) DO UPDATE SET v = '2026-01-02 00:00:00Z' USING OPERATION_ID 'x1'",
            "22008",
        ),
        (
            "INSERT INTO t (id, v) VALUES (1, '2026-01-02 00:00:00Z') ON CONFLICT (id) DO UPDATE SET v = 'zz' USING OPERATION_ID 'x2'",
            "22007",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    assert_eq!(micros(&core, "SELECT id, v FROM t LIMIT 5"), before);
    ok(
        &core,
        "UPDATE t SET v = '2026-01-01 09:00:00+09' WHERE id = 1 USING OPERATION_ID 'u4'",
    );
    assert_eq!(micros(&core, "SELECT id, v FROM t LIMIT 5"), before);
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '2026-01-02 00:00:00Z') ON CONFLICT (id) DO UPDATE SET v = '2026-01-03 00:00:00+01' USING OPERATION_ID 'x3'",
    );
    let got = micros(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(text_of(got[0].1.expect("v")), "2026-01-02 23:00:00+00");
}

#[test]
fn column_default_is_validated_and_applied() {
    let (_p, _g, core) = open("tz-default");
    assert_eq!(
        code(
            &core,
            "CREATE TABLE a (v TIMESTAMPTZ DEFAULT '2026-13-01 00:00:00Z')"
        ),
        "22008"
    );
    assert_eq!(
        code(&core, "CREATE TABLE a (v TIMESTAMPTZ DEFAULT 'zz')"),
        "22007"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE a (v TIMESTAMPTZ DEFAULT '2026-01-01 00:00:00 Nowhere/Land')"
        ),
        "22023"
    );
    ok(
        &core,
        "CREATE TABLE t (v TIMESTAMPTZ DEFAULT '2026-01-01 12:00:00+09', w INTEGER)",
    );
    ok(
        &core,
        "INSERT INTO t (id, w) VALUES (1, 5) USING OPERATION_ID 'd1'",
    );
    let got = micros(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(text_of(got[0].1.expect("v")), "2026-01-01 03:00:00+00");
    ok(
        &core,
        "ALTER TABLE t ADD COLUMN z TIMESTAMPTZ DEFAULT '2026-01-01 03:00:01Z'",
    );
    let r = core
        .execute_sql(&ctx(), "SELECT id, z FROM t LIMIT 5")
        .expect("select");
    match r.rows[0].cells.get(1) {
        Some(Cell::TimestampTz(m)) => assert_eq!(text_of(*m), "2026-01-01 03:00:01+00"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ADD COLUMN y TIMESTAMPTZ DEFAULT 'nope'"
        ),
        "22007"
    );
}

#[test]
fn returning_projects_the_utc_value() {
    let (_p, _g, core) = open("tz-returning");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    let outcome = exec(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '2026-01-01 12:00:00.25+09') RETURNING v USING OPERATION_ID 'r1'",
    )
    .expect("returning insert");
    match outcome {
        SqlOutcome::Returning(o) => match o.result.rows[0].cells[0] {
            Cell::TimestampTz(m) => assert_eq!(text_of(m), "2026-01-01 03:00:00.25+00"),
            ref other => panic!("unexpected {other:?}"),
        },
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

#[test]
fn copy_from_binds_timestamptz_fields() {
    let (_p, _g, core) = open("tz-copy");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    let run = |data: &[u8], op: &str| {
        let policy = ctx();
        let session = SessionState::default();
        let plan = core.begin_copy(
            &policy,
            &session,
            &format!("COPY t (id, v) FROM STDIN USING OPERATION_ID '{op}'"),
        )?;
        let mut cs = match plan {
            CopyPlan::From(s) => s,
            CopyPlan::To(..) => panic!("expected COPY FROM plan"),
        };
        cs.feed(data)?;
        let batch = cs.finish()?;
        core.commit_copy_in(&policy, batch)
    };
    run(
        b"1\t2026-01-01 12:00:00+09\n2\t2026-01-01 03:00:00Z\n3\t2026-01-01 03:00:00\n4\t\\N\n",
        "c1",
    )
    .expect("copy ok");
    let got = micros(&core, "SELECT id, v FROM t LIMIT 20");
    assert_eq!(got.len(), 4);
    assert_eq!(find(&got, 1), find(&got, 2));
    assert_eq!(find(&got, 2), find(&got, 3));
    assert_eq!(find(&got, 4), None);
    assert_eq!(
        run(b"9\t2026-13-01 00:00:00Z\n", "c2")
            .expect_err("range")
            .wire_code(),
        "22008"
    );
    assert_eq!(
        run(b"9\tnope\n", "c3").expect_err("format").wire_code(),
        "22007"
    );
    assert_eq!(
        run(b"9\t2026-01-01 00:00:00 Nowhere/Land\n", "c4")
            .expect_err("zone")
            .wire_code(),
        "22023"
    );
    assert_eq!(
        micros(&core, "SELECT id, v FROM t LIMIT 20").len(),
        4,
        "rejected COPY must not write rows"
    );
}

#[test]
fn values_and_catalog_survive_reopen() {
    let (path, guard, core) = open("tz-reopen");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    insert(&core, 1, "'9999-12-31 23:59:59.999999Z'", "p1").expect("insert");
    insert(&core, 2, "'0001-01-01 09:00:00+09'", "p2").expect("insert");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = micros(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(got.len(), 2);
    assert_eq!(
        find(&got, 1).map(text_of).as_deref(),
        Some("9999-12-31 23:59:59.999999+00")
    );
    assert_eq!(
        find(&got, 2).map(text_of).as_deref(),
        Some("0001-01-01 00:00:00+00")
    );
    drop(guard);
}

/// テナント境界は不変: 他テナントから行が見えず、拒否エラーに値を含めない。
#[test]
fn other_tenants_cannot_see_timestamptz_rows() {
    let (_p, _g, core) = open("tz-tenant");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ)");
    insert(&core, 1, "'2031-05-06 07:08:09+09'", "t1").expect("insert");
    let other = core
        .execute_sql(&ctx_for("bob"), "SELECT id, v FROM t LIMIT 5")
        .expect("select as bob");
    assert!(other.rows.is_empty(), "bob must not see alice's rows");
    let err = exec_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (2, '2031-05-06 07:08:09 Nowhere/Land') USING OPERATION_ID 'b1'",
    )
    .expect_err("bad");
    assert!(!err.to_string().contains("Nowhere"));
    assert!(!err.to_string().contains("2031"));
}

/// 未解禁の機能は TIMESTAMP 等の既存の腕へ黙って流さず fail-closed に拒否する
/// （述語・集計・索引・式は #1576、PK/UNIQUE・型変更は担当 Issue 未定）。
#[test]
fn features_not_yet_enabled_are_rejected_fail_closed() {
    let (_p, _g, core) = open("tz-failclosed");
    ok(&core, "CREATE TABLE t (v TIMESTAMPTZ, n INTEGER)");
    insert(&core, 1, "'2026-01-01 03:00:00Z'", "f1").expect("insert");
    for sql in [
        "SELECT id FROM t WHERE v = '2026-01-01 03:00:00Z' LIMIT 5",
        "SELECT id FROM t WHERE v > '2026-01-01 03:00:00Z' LIMIT 5",
        "SELECT id, v FROM t ORDER BY v LIMIT 5",
        "SELECT DISTINCT v FROM t LIMIT 5",
        "SELECT v, COUNT(*) FROM t GROUP BY v",
        "SELECT MIN(v) FROM t",
        "SELECT MAX(v) FROM t",
        "SELECT COUNT(v) FROM t",
        "SELECT COUNT(DISTINCT v) FROM t",
        "SELECT v + 1 FROM t LIMIT 5",
        "SELECT EXTRACT(YEAR FROM v) FROM t LIMIT 5",
        "SELECT TIMESTAMPTZ '2026-01-01 00:00:00Z' FROM t LIMIT 5",
        "CREATE INDEX ix ON t (v)",
        "CREATE TABLE a (v TIMESTAMPTZ PRIMARY KEY)",
        "CREATE TABLE a (v TIMESTAMPTZ UNIQUE)",
        "ALTER TABLE t ALTER COLUMN v TYPE TIMESTAMP",
        "ALTER TABLE t ALTER COLUMN n TYPE TIMESTAMPTZ",
    ] {
        assert!(exec(&core, sql).is_err(), "{sql} must be rejected");
    }
    // NULL 判定は型に依らず存在ビットだけを見るため TIMESTAMPTZ 列でも正しく動く。
    insert(&core, 2, "NULL", "f2").expect("insert null");
    let is_null = core
        .execute_sql(&ctx(), "SELECT id FROM t WHERE v IS NULL LIMIT 5")
        .expect("is null");
    assert_eq!(is_null.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    assert_eq!(micros(&core, "SELECT id, v FROM t LIMIT 5").len(), 2);
}

/// `TIMESTAMPTZ(p)`・`TIMESTAMPTZ[]` は構文段で `0A000`。
#[test]
fn unsupported_timestamptz_type_spellings_are_rejected() {
    let (_p, _g, core) = open("tz-spellings");
    for sql in [
        "CREATE TABLE a (v TIMESTAMPTZ(3))",
        "CREATE TABLE a (v TIMESTAMPTZ[])",
        "CREATE TABLE a (v TIMESTAMPTZ[3])",
        "CREATE TABLE a (v TIMESTAMP WITH TIME ZONE[])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(&core, "CREATE TABLE b (v TIMESTAMPTZ)");
    assert_eq!(
        code(&core, "ALTER TABLE b ADD COLUMN w TIMESTAMPTZ(6)"),
        "0A000"
    );
}

#[test]
fn timestamptz_type_name_is_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("tz-enum");
    assert!(
        exec(&core, "CREATE TYPE timestamptz AS ENUM ('a')").is_err(),
        "timestamptz must be reserved"
    );
}
