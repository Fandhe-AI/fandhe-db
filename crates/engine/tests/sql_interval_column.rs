//! `INTERVAL` 列（`ColumnType::Interval`。TABLE-34・Issue #1579）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され
//! 再オープン後も値ごと復元されること、(2) 代表的な書式が「入力 → 格納 → 出力（postgres 形式）
//! → 再入力」で往復一致すること、(3) INSERT・UPDATE SET・UPSERT・COPY FROM・列 DEFAULT の値束縛が
//! 書式違反を `22007`、フィールド値の範囲外を `22015`、合成後の範囲外を `22008`、`infinity` を
//! `0A000` で拒否し副作用を残さないこと、(4) 未解禁の機能（式・PK/UNIQUE・型変更・集合演算・
//! 配列・`INTERVAL(p)`・フィールド制限など）が fail-closed に拒否されること、(5) テナント境界が
//! 不変であること、を固定する。比較・集計・索引は #1580（`sql_interval_query.rs`）、式・算術は
//! #1581、EXTRACT は #1582、OID 1186・バイナリ・NoSQL は #1583 の担当。

use engine::catalog::ColumnType;
use engine::core::{CopyPlan, EngineCore};
use engine::datetime::{format_interval, parse_interval, Interval};
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

/// `SELECT id, <col> ...` の結果を `(id, 値または None)` で返す。
fn values(core: &EngineCore, sql: &str) -> Vec<(u64, Option<Interval>)> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| {
            let v = match r.cells.get(1) {
                Some(Cell::Interval(v)) => Some(*v),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (r.id, v)
        })
        .collect()
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

fn find(rows: &[(u64, Option<Interval>)], id: u64) -> Option<Interval> {
    rows.iter().find(|r| r.0 == id).and_then(|r| r.1)
}

#[test]
fn interval_type_is_persisted_in_the_catalog() {
    let (path, _g, core) = open("iv-persist");
    ok(&core, "CREATE TABLE t (a INTERVAL, b interval, c INTEGER)");
    ok(&core, "ALTER TABLE t ADD COLUMN d INTERVAL");
    ok(
        &core,
        "ALTER TABLE t ADD COLUMN e INTERVAL DEFAULT '1 day 02:00:00'",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Interval);
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Interval);
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Integer);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Interval);
    assert_eq!(column_type(&path, "t", "e"), ColumnType::Interval);
}

/// 受け入れ基準: 代表的な書式で「入力 → 格納 → 出力 → 再入力」が一致する。
#[test]
fn representative_formats_roundtrip_through_storage_and_output() {
    let (_p, _g, core) = open("iv-roundtrip");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    let cases = [
        ("'1 day 2 hours'", "1 day 02:00:00"),
        ("'@ 1 day ago'", "-1 days"),
        ("'P1Y2M3DT4H5M6S'", "1 year 2 mons 3 days 04:05:06"),
        ("'1-2'", "1 year 2 mons"),
        ("'1 2:03:04'", "1 day 02:03:04"),
        ("'-1 days +02:03:00'", "-1 days +02:03:00"),
        ("'1.5 months'", "1 mon 15 days"),
        ("'0'", "00:00:00"),
        ("'100 hours'", "100:00:00"),
        ("'  1 DAY  '", "1 day"),
    ];
    for (i, (lit, _)) in cases.iter().enumerate() {
        insert(&core, 1 + i as u64, lit, &format!("n{i}"))
            .unwrap_or_else(|e| panic!("{lit}: {e:?}"));
    }
    let got = values(&core, "SELECT id, v FROM t LIMIT 50");
    assert_eq!(got.len(), cases.len());
    for (i, (_, want)) in cases.iter().enumerate() {
        let v = find(&got, 1 + i as u64).expect("non-null");
        assert_eq!(format_interval(&v), *want);
        assert_eq!(parse_interval(want), Ok(v), "reparse {want}");
    }
}

#[test]
fn insert_classifies_format_overflow_range_and_infinity_errors() {
    let (_p, _g, core) = open("iv-errors");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    let cases = [
        // 書式違反は 22007。
        ("'abc'", "22007"),
        ("''", "22007"),
        ("'1 fortnight'", "22007"),
        ("'ago 1 day'", "22007"),
        ("'1 day @'", "22007"),
        ("'1 日'", "22007"),
        // フィールド値の範囲外は 22015。
        ("'2147483648 days'", "22015"),
        ("'9223372036854775807 hours'", "22015"),
        // 合成後の範囲外は 22008。
        ("'178956971 years'", "22008"),
        // infinity は未対応（0A000）。
        ("'infinity'", "0A000"),
        ("'-infinity'", "0A000"),
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
        values(&core, "SELECT id, v FROM t LIMIT 30").is_empty(),
        "rejected rows must not be written"
    );
}

#[test]
fn update_set_and_upsert_bind_and_reject_without_side_effects() {
    let (_p, _g, core) = open("iv-update");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    insert(&core, 1, "'1 day'", "i1").expect("insert");
    let before = values(&core, "SELECT id, v FROM t LIMIT 5");
    for (sql, want) in [
        (
            "UPDATE t SET v = '2147483648 days' WHERE id = 1 USING OPERATION_ID 'u1'",
            "22015",
        ),
        (
            "UPDATE t SET v = 'xx' WHERE id = 1 USING OPERATION_ID 'u2'",
            "22007",
        ),
        (
            "UPDATE t SET v = '178956971 years' WHERE id = 1 USING OPERATION_ID 'u3'",
            "22008",
        ),
        (
            "INSERT INTO t (id, v) VALUES (1, '2 days') ON CONFLICT (id) DO UPDATE SET v = 'zz' USING OPERATION_ID 'x1'",
            "22007",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    assert_eq!(values(&core, "SELECT id, v FROM t LIMIT 5"), before);
    ok(
        &core,
        "UPDATE t SET v = '24 hours' WHERE id = 1 USING OPERATION_ID 'u4'",
    );
    let got = values(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(
        got[0].1,
        Some(Interval {
            months: 0,
            days: 0,
            micros: 86_400_000_000
        }),
        "24 hours is stored as micros, not normalized into days"
    );
}

#[test]
fn column_default_is_validated_and_applied() {
    let (_p, _g, core) = open("iv-default");
    assert_eq!(
        code(&core, "CREATE TABLE a (v INTERVAL DEFAULT 'zz')"),
        "22007"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE a (v INTERVAL DEFAULT '2147483648 days')"
        ),
        "22015"
    );
    assert_eq!(
        code(&core, "CREATE TABLE a (v INTERVAL DEFAULT 'infinity')"),
        "0A000"
    );
    ok(
        &core,
        "CREATE TABLE t (v INTERVAL DEFAULT '1 day 02:00:00', w INTEGER)",
    );
    ok(
        &core,
        "INSERT INTO t (id, w) VALUES (1, 5) USING OPERATION_ID 'd1'",
    );
    let got = values(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(
        got[0].1.map(|v| format_interval(&v)).as_deref(),
        Some("1 day 02:00:00")
    );
    assert_eq!(
        code(&core, "ALTER TABLE t ADD COLUMN y INTERVAL DEFAULT 'nope'"),
        "22007"
    );
}

#[test]
fn returning_projects_the_postgres_style_value() {
    let (_p, _g, core) = open("iv-returning");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    let outcome = exec(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '1 year 2 months') RETURNING v USING OPERATION_ID 'r1'",
    )
    .expect("returning insert");
    match outcome {
        SqlOutcome::Returning(o) => match o.result.rows[0].cells[0] {
            Cell::Interval(v) => assert_eq!(format_interval(&v), "1 year 2 mons"),
            ref other => panic!("unexpected {other:?}"),
        },
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

#[test]
fn copy_from_binds_interval_fields() {
    let (_p, _g, core) = open("iv-copy");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
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
    run(b"1\t1 day 02:00:00\n2\tP1DT2H\n3\t\\N\n", "c1").expect("copy ok");
    let got = values(&core, "SELECT id, v FROM t LIMIT 20");
    assert_eq!(got.len(), 3);
    assert_eq!(find(&got, 1), find(&got, 2));
    assert_eq!(find(&got, 3), None);
    assert_eq!(
        run(b"9\t2147483648 days\n", "c2")
            .expect_err("overflow")
            .wire_code(),
        "22015"
    );
    assert_eq!(
        run(b"9\tnope\n", "c3").expect_err("format").wire_code(),
        "22007"
    );
    assert_eq!(
        values(&core, "SELECT id, v FROM t LIMIT 20").len(),
        3,
        "rejected COPY must not write rows"
    );
}

#[test]
fn values_and_catalog_survive_reopen() {
    let (path, guard, core) = open("iv-reopen");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    insert(&core, 1, "'-178956970 years -8 mons'", "p1").expect("insert");
    insert(&core, 2, "'2562047788:00:54.775807'", "p2").expect("insert");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = values(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(got.len(), 2);
    assert_eq!(find(&got, 1).map(|v| v.months), Some(i32::MIN));
    assert_eq!(find(&got, 2).map(|v| v.micros), Some(i64::MAX));
    drop(guard);
}

/// テナント境界は不変: 他テナントから行が見えず、拒否エラーに入力値を含めない。
#[test]
fn other_tenants_cannot_see_interval_rows() {
    let (_p, _g, core) = open("iv-tenant");
    ok(&core, "CREATE TABLE t (v INTERVAL)");
    insert(&core, 1, "'31 days 7 hours'", "t1").expect("insert");
    let other = core
        .execute_sql(&ctx_for("bob"), "SELECT id, v FROM t LIMIT 5")
        .expect("select as bob");
    assert!(other.rows.is_empty(), "bob must not see alice's rows");
    let err = exec_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (2, '31 secretword') USING OPERATION_ID 'b1'",
    )
    .expect_err("bad");
    assert!(!err.to_string().contains("secretword"));
}

/// 未解禁の機能は他型の腕へ黙って流さず fail-closed に拒否する（式・算術・型付きリテラルは
/// #1581。GROUP BY・DISTINCT・window・PK/UNIQUE・型変更は担当 Issue 未定）。述語・並べ替え・
/// 集計・索引は #1580 で解禁し `sql_interval_query.rs` が担う。
#[test]
fn features_not_yet_enabled_are_rejected_fail_closed() {
    let (_p, _g, core) = open("iv-failclosed");
    ok(&core, "CREATE TABLE t (v INTERVAL, n INTEGER)");
    insert(&core, 1, "'1 day'", "f1").expect("insert");
    for sql in [
        "SELECT id FROM t WHERE v IS NOT DISTINCT FROM '1 day' LIMIT 5",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) FROM t LIMIT 5",
        "UPDATE t SET v = v WHERE id = 1 USING OPERATION_ID 'fx1'",
        "SELECT v FROM t GROUP BY v",
        "SELECT v + 1 FROM t LIMIT 5",
        "SELECT v FROM t UNION SELECT v FROM t",
        "SELECT v FROM t UNION ALL SELECT v FROM t",
        "SELECT INTERVAL '1 day' FROM t LIMIT 5",
        "CREATE TABLE a (v INTERVAL PRIMARY KEY)",
        "CREATE TABLE a (v INTERVAL UNIQUE)",
        "ALTER TABLE t ALTER COLUMN v TYPE TEXT",
        "ALTER TABLE t ALTER COLUMN n TYPE INTERVAL",
    ] {
        assert!(exec(&core, sql).is_err(), "{sql} must be rejected");
    }
    // NULL 判定は型に依らず存在ビットだけを見るため INTERVAL 列でも正しく動く。
    insert(&core, 2, "NULL", "f2").expect("insert null");
    let is_null = core
        .execute_sql(&ctx(), "SELECT id FROM t WHERE v IS NULL LIMIT 5")
        .expect("is null");
    assert_eq!(is_null.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    assert_eq!(values(&core, "SELECT id, v FROM t LIMIT 5").len(), 2);
}

/// `INTERVAL(p)`・フィールド制限・`INTERVAL[]` は構文段で `0A000`。
#[test]
fn unsupported_interval_type_spellings_are_rejected() {
    let (_p, _g, core) = open("iv-spellings");
    for sql in [
        "CREATE TABLE a (v INTERVAL(3))",
        "CREATE TABLE a (v INTERVAL YEAR)",
        "CREATE TABLE a (v INTERVAL DAY TO SECOND)",
        "CREATE TABLE a (v INTERVAL[])",
        "CREATE TABLE a (v INTERVAL[3])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(&core, "CREATE TABLE b (v INTERVAL)");
    assert_eq!(
        code(&core, "ALTER TABLE b ADD COLUMN w INTERVAL(6)"),
        "0A000"
    );
}

#[test]
fn interval_type_name_is_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("iv-enum");
    assert!(
        exec(&core, "CREATE TYPE interval AS ENUM ('a')").is_err(),
        "interval must be reserved"
    );
}
