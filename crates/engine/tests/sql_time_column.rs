//! `TIME` 列（`ColumnType::Time`。TABLE-32・Issue #1569）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され
//! 再オープン後も値ごと復元されること、(2) INSERT・UPDATE SET・UPSERT・COPY FROM・列
//! DEFAULT の値束縛が書式違反を `22007`、範囲外を `22008` で拒否し副作用を残さないこと、
//! (3) 境界値 `00:00:00`・`23:59:59.999999`・`24:00:00` が往復で一致すること、(4) 未解禁の
//! 機能（述語・集計・索引・PK/UNIQUE・型変更・配列・`TIME(p)`・`TIME WITH TIME ZONE`）が
//! fail-closed に拒否されること、を固定する。未解禁の機能の解禁は兄弟 Issue #1570 の担当で、
//! 解禁時は (4) の期待を反転させる。

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

/// `SELECT id, <col> ...` の結果を `(id, 整形済み TIME テキストまたは None)` で返す
/// （wire-server が出力するのと同じ `format_time` を通す）。
fn times(core: &EngineCore, sql: &str) -> Vec<(u64, Option<String>)> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| {
            let text = match r.cells.get(1) {
                Some(Cell::Time(m)) => Some(engine::datetime::format_time(*m)),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (r.id, text)
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

#[test]
fn time_and_without_time_zone_are_persisted_as_the_same_type() {
    let (path, _g, core) = open("tm-persist");
    ok(
        &core,
        "CREATE TABLE t (a TIME, b time WITHOUT TIME ZONE, c INTEGER)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN d TIME");
    ok(&core, "ALTER TABLE t ADD COLUMN e TIME DEFAULT '12:34:56'");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Time);
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Time);
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Integer);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Time);
    assert_eq!(column_type(&path, "t", "e"), ColumnType::Time);
}

/// 受け入れ基準: 書き込んだ値を読み戻すと一致する（境界値を含む）。
#[test]
fn boundary_values_roundtrip_through_insert_and_select() {
    let (_p, _g, core) = open("tm-roundtrip");
    ok(&core, "CREATE TABLE t (v TIME)");
    let cases = [
        ("'00:00:00'", "00:00:00"),
        ("'23:59:59.999999'", "23:59:59.999999"),
        ("'24:00:00'", "24:00:00"),
        ("'12:30'", "12:30:00"),
        ("'01:02:03.5'", "01:02:03.5"),
        ("'01:02:03.120000'", "01:02:03.12"),
    ];
    for (i, (lit, _)) in cases.iter().enumerate() {
        insert(&core, 1 + i as u64, lit, &format!("ok{i}"))
            .unwrap_or_else(|e| panic!("{lit}: {e:?}"));
    }
    insert(&core, 100, "NULL", "okn").expect("null");
    let got = times(&core, "SELECT id, v FROM t LIMIT 20");
    for (i, (_, want)) in cases.iter().enumerate() {
        let row = got
            .iter()
            .find(|(id, _)| *id == 1 + i as u64)
            .unwrap_or_else(|| panic!("missing row {i}"));
        assert_eq!(row.1.as_deref(), Some(*want));
    }
    assert_eq!(
        got.iter().find(|(id, _)| *id == 100).map(|r| r.1.clone()),
        Some(None),
        "NULL must stay NULL"
    );
}

#[test]
fn fractional_seconds_beyond_six_digits_are_rounded_before_range_check() {
    let (_p, _g, core) = open("tm-round");
    ok(&core, "CREATE TABLE t (v TIME)");
    for (i, (lit, want)) in [
        ("'12:00:00.1234565'", "12:00:00.123457"),
        ("'12:00:00.1234564'", "12:00:00.123456"),
        ("'23:59:59.9999995'", "24:00:00"),
        ("'24:00:00.0000004'", "24:00:00"),
    ]
    .iter()
    .enumerate()
    {
        let id = 1 + i as u64;
        insert(&core, id, lit, &format!("r{i}")).unwrap_or_else(|e| panic!("{lit}: {e:?}"));
        let got = times(&core, "SELECT id, v FROM t LIMIT 20");
        let row = got.iter().find(|(rid, _)| *rid == id).expect("row");
        assert_eq!(row.1.as_deref(), Some(*want), "for {lit}");
    }
}

#[test]
fn insert_classifies_format_range_and_type_errors() {
    let (_p, _g, core) = open("tm-errors");
    ok(&core, "CREATE TABLE t (v TIME)");
    let cases = [
        // 範囲外は 22008。
        ("'24:00:00.000001'", "22008"),
        ("'25:00'", "22008"),
        ("'12:60'", "22008"),
        ("'12:00:60'", "22008"),
        // 書式違反は 22007。
        ("'abc'", "22007"),
        ("''", "22007"),
        ("'12:00:00+09'", "22007"),
        ("' 12:00'", "22007"),
        ("'1:2:3'", "22007"),
        ("'12:00 PM'", "22007"),
        (
            "'12:00:00.1234567890123456789012345678901234567890'",
            "22007",
        ),
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
        times(&core, "SELECT id, v FROM t LIMIT 20").is_empty(),
        "rejected rows must not be written"
    );
}

#[test]
fn update_set_and_upsert_bind_and_reject_without_side_effects() {
    let (_p, _g, core) = open("tm-update");
    ok(&core, "CREATE TABLE t (v TIME)");
    insert(&core, 1, "'01:00:00'", "i1").expect("insert");
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = '25:00' WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET v = 'xx' WHERE id = 1 USING OPERATION_ID 'u2'"
        ),
        "22007"
    );
    assert_eq!(
        times(&core, "SELECT id, v FROM t LIMIT 5")[0].1.as_deref(),
        Some("01:00:00")
    );
    ok(
        &core,
        "UPDATE t SET v = '23:59:59.999999' WHERE id = 1 USING OPERATION_ID 'u3'",
    );
    assert_eq!(
        times(&core, "SELECT id, v FROM t LIMIT 5")[0].1.as_deref(),
        Some("23:59:59.999999")
    );
    // UPSERT: 挿入側・更新側の双方を検査する。
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v) VALUES (1, '99:00') ON CONFLICT (id) DO UPDATE SET v = '02:00' USING OPERATION_ID 'x1'"
        ),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, v) VALUES (1, '02:00') ON CONFLICT (id) DO UPDATE SET v = '99:00' USING OPERATION_ID 'x2'"
        ),
        "22008"
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '02:00') ON CONFLICT (id) DO UPDATE SET v = '03:04:05' USING OPERATION_ID 'x3'",
    );
    assert_eq!(
        times(&core, "SELECT id, v FROM t LIMIT 5")[0].1.as_deref(),
        Some("03:04:05")
    );
}

#[test]
fn column_default_is_validated_and_applied() {
    let (_p, _g, core) = open("tm-default");
    assert_eq!(
        code(&core, "CREATE TABLE a (v TIME DEFAULT '25:00')"),
        "22008"
    );
    assert_eq!(code(&core, "CREATE TABLE a (v TIME DEFAULT 'zz')"), "22007");
    ok(
        &core,
        "CREATE TABLE t (v TIME DEFAULT '24:00:00', w INTEGER)",
    );
    ok(
        &core,
        "INSERT INTO t (id, w) VALUES (1, 5) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        times(&core, "SELECT id, v FROM t LIMIT 5")[0].1.as_deref(),
        Some("24:00:00")
    );
    // 既存行へ補完される ADD COLUMN ... DEFAULT。
    ok(&core, "ALTER TABLE t ADD COLUMN z TIME DEFAULT '00:00:01'");
    let r = core
        .execute_sql(&ctx(), "SELECT id, z FROM t LIMIT 5")
        .expect("select");
    assert!(matches!(
        r.rows[0].cells.get(1),
        Some(Cell::Time(1_000_000))
    ));
    assert_eq!(
        code(&core, "ALTER TABLE t ADD COLUMN y TIME DEFAULT '99:00'"),
        "22008"
    );
}

#[test]
fn returning_projects_the_time_value() {
    let (_p, _g, core) = open("tm-returning");
    ok(&core, "CREATE TABLE t (v TIME)");
    let outcome = exec(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '10:20:30.25') RETURNING v USING OPERATION_ID 'r1'",
    )
    .expect("returning insert");
    match outcome {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.result.rows.len(), 1);
            assert_eq!(o.result.rows[0].cells[0], Cell::Time(37_230_250_000));
        }
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

#[test]
fn copy_from_binds_time_fields() {
    let (_p, _g, core) = open("tm-copy");
    ok(&core, "CREATE TABLE t (v TIME)");
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
        b"1\t00:00:00\n2\t23:59:59.999999\n3\t24:00:00\n4\t\\N\n",
        "c1",
    )
    .expect("copy ok");
    let got = times(&core, "SELECT id, v FROM t LIMIT 20");
    assert_eq!(got.len(), 4);
    assert_eq!(
        got.iter().find(|r| r.0 == 2).and_then(|r| r.1.clone()),
        Some("23:59:59.999999".to_string())
    );
    assert_eq!(
        run(b"9\t25:00\n", "c2").expect_err("range").wire_code(),
        "22008"
    );
    assert_eq!(
        run(b"9\tnope\n", "c3").expect_err("format").wire_code(),
        "22007"
    );
    assert_eq!(
        times(&core, "SELECT id, v FROM t LIMIT 20").len(),
        4,
        "rejected COPY must not write rows"
    );
}

#[test]
fn values_and_catalog_survive_reopen() {
    let (path, guard, core) = open("tm-reopen");
    ok(&core, "CREATE TABLE t (v TIME)");
    insert(&core, 1, "'24:00:00'", "p1").expect("insert");
    insert(&core, 2, "'00:00:00'", "p2").expect("insert");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = times(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(got.len(), 2);
    assert_eq!(
        got.iter().find(|r| r.0 == 1).and_then(|r| r.1.clone()),
        Some("24:00:00".to_string())
    );
    assert_eq!(
        got.iter().find(|r| r.0 == 2).and_then(|r| r.1.clone()),
        Some("00:00:00".to_string())
    );
    drop(guard);
}

#[test]
fn other_tenants_cannot_see_time_rows() {
    let (_p, _g, core) = open("tm-tenant");
    ok(&core, "CREATE TABLE t (v TIME)");
    insert(&core, 1, "'05:06:07'", "t1").expect("insert");
    let other = core
        .execute_sql(&ctx_for("bob"), "SELECT id, v FROM t LIMIT 5")
        .expect("select as bob");
    assert!(other.rows.is_empty(), "bob must not see alice's rows");
    // 他テナントの書き込み拒否時のエラーに値を含めない。
    let err = exec_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (2, '25:00') USING OPERATION_ID 'b1'",
    )
    .expect_err("range");
    assert!(!err.to_string().contains("05:06:07"));
}

/// 未解禁の機能は TIMESTAMP 等の既存の腕へ黙って流さず fail-closed に拒否する。
/// 解禁は #1570（述語・集計・索引・PK/UNIQUE・型変更）と #1571（wire・NoSQL）の担当で、
/// 解禁時に本テストの期待を反転させる。
#[test]
fn features_not_yet_enabled_are_rejected_fail_closed() {
    let (_p, _g, core) = open("tm-failclosed");
    ok(&core, "CREATE TABLE t (v TIME, n INTEGER)");
    insert(&core, 1, "'05:06:07'", "f1").expect("insert");
    for sql in [
        "SELECT id, v FROM t WHERE v = '05:06:07' LIMIT 5",
        "SELECT id, v FROM t WHERE v > '05:00:00' LIMIT 5",
        "SELECT id, v FROM t ORDER BY v LIMIT 5",
        "SELECT MIN(v) FROM t",
        "SELECT COUNT(v) FROM t",
        "SELECT v, COUNT(*) FROM t GROUP BY v",
        "SELECT DISTINCT v FROM t LIMIT 5",
        "SELECT v FROM t UNION SELECT v FROM t",
        "SELECT v + 1 FROM t LIMIT 5",
        "SELECT id FROM t WHERE v IN ('05:06:07') LIMIT 5",
        "CREATE INDEX ix ON t (v)",
        "CREATE TABLE a (v TIME PRIMARY KEY)",
        "CREATE TABLE a (v TIME UNIQUE)",
        "ALTER TABLE t ALTER COLUMN v TYPE TIMESTAMP",
        "ALTER TABLE t ALTER COLUMN n TYPE TIME",
    ] {
        assert!(exec(&core, sql).is_err(), "{sql} must be rejected");
    }
    // NULL 判定は型に依らず存在ビットだけを見るため TIME 列でも正しく動く。
    insert(&core, 2, "NULL", "f2").expect("insert null");
    let is_null = core
        .execute_sql(&ctx(), "SELECT id FROM t WHERE v IS NULL LIMIT 5")
        .expect("is null");
    assert_eq!(is_null.rows.iter().map(|r| r.id).collect::<Vec<_>>(), [2]);
    // 行は無傷のまま読める。
    assert_eq!(times(&core, "SELECT id, v FROM t LIMIT 5").len(), 2);
}

/// 集合演算は格納値・行数・演算種別に依らず TIME 列の列型で拒否する（空入力・全 NULL・
/// UNION ALL・INTERSECT・EXCEPT でも `0A000`。#1570 で解禁まで fail-closed）。
#[test]
fn set_operations_reject_time_columns_by_column_type() {
    let (_p, _g, core) = open("tm-setop-type");
    ok(&core, "CREATE TABLE t (v TIME, n INTEGER)");
    ok(&core, "CREATE TABLE u (v TIME, n INTEGER)");
    let check = |core: &EngineCore| {
        for op in ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"] {
            let sql = format!("SELECT v FROM t {op} SELECT v FROM u");
            assert_eq!(code(core, &sql), "0A000", "{sql} must be 0A000");
        }
    };
    // 空入力。
    check(&core);
    // 全 NULL。
    insert(&core, 1, "NULL", "f1").expect("insert null");
    check(&core);
    // 値あり。
    insert(&core, 2, "'05:06:07'", "f2").expect("insert");
    check(&core);
}

/// `TIME(p)`・`TIME WITH TIME ZONE`・`TIMETZ`・`time[]` は構文段で `0A000`。
#[test]
fn unsupported_time_type_spellings_are_rejected_with_feature_not_supported() {
    let (_p, _g, core) = open("tm-spellings");
    for sql in [
        "CREATE TABLE a (v TIME(3))",
        "CREATE TABLE a (v TIME WITH TIME ZONE)",
        "CREATE TABLE a (v time with time zone)",
        "CREATE TABLE a (v TIMETZ)",
        "CREATE TABLE a (v TIME[])",
        "CREATE TABLE a (v TIME[3])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(&core, "CREATE TABLE b (v TIME)");
    for sql in [
        "ALTER TABLE b ADD COLUMN w TIME(6)",
        "ALTER TABLE b ADD COLUMN w TIMETZ",
        "ALTER TABLE b ADD COLUMN w TIME WITH TIME ZONE",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
}

#[test]
fn time_type_names_are_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("tm-enum");
    for name in ["time", "timetz"] {
        assert!(
            exec(&core, &format!("CREATE TYPE {name} AS ENUM ('a')")).is_err(),
            "{name} must be reserved"
        );
    }
}
