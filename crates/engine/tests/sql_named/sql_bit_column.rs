//! `BIT(n)` 列（`ColumnType::Bit`。TABLE-48・Issue #2021）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され再オープン後も
//! 値ごと復元されること、(2) 文字列リテラル `'1010'` が「入力 → 格納 → 出力 → 再入力」で往復一致する
//! こと、(3) INSERT・UPDATE SET・UPSERT・COPY FROM・列 DEFAULT の束縛が文字種違反を `22P02`、ビット数不一致を
//! `22026` で拒否し副作用を残さないこと、(4) 型宣言の境界（`BIT(0)`・上限超過・`BIT VARYING`・`VARBIT`・配列）が
//! 拒否されること、(5) 未解禁の機能（比較・並べ替え・集計・PK/UNIQUE・索引・CAST・`<~>`／`<%>`）が
//! fail-closed に拒否されること、(6) テナント境界が不変であることを固定する。距離カーネルへの接続は
//! 後続 Issue の担当。

use engine::bit_string::BitString;
use engine::catalog::ColumnType;
use engine::core::{CopyPlan, EngineCore};
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

/// `SELECT id, <col> ...` の結果を `(id, 文字列または None)` で返す。
fn values(core: &EngineCore, sql: &str) -> Vec<(i64, Option<String>)> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| {
            let id = match r.cells.first() {
                Some(Cell::SignedInteger(n)) => *n,
                other => panic!("unexpected id cell {other:?}"),
            };
            let v = match r.cells.get(1) {
                Some(Cell::Bit(v)) => Some(v.format_text()),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (id, v)
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
fn bit_type_is_persisted_in_the_catalog() {
    let (path, _g, core) = open("bit-persist");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a BIT(4), b bit, c BIT(16), d INTEGER)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN e BIT(9)");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Bit(4));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Bit(1));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bit(16));
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Integer);
    assert_eq!(column_type(&path, "t", "e"), ColumnType::Bit(9));
}

/// 受け入れ基準: 書き込んだ値を読むと同じ値が返り、出力を再入力しても一致する。
#[test]
fn values_roundtrip_through_storage_and_output() {
    let (_p, _g, core) = open("bit-roundtrip");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(9))");
    let cases = ["101010101", "000000000", "111111111", "100000001"];
    for (i, lit) in cases.iter().enumerate() {
        insert(&core, 1 + i as u64, &format!("'{lit}'"), &format!("n{i}"))
            .unwrap_or_else(|e| panic!("{lit}: {e:?}"));
    }
    let got = values(&core, "SELECT id, v FROM t LIMIT 50");
    assert_eq!(got.len(), cases.len());
    for (i, want) in cases.iter().enumerate() {
        let v = got
            .iter()
            .find(|r| r.0 == 1 + i as i64)
            .and_then(|r| r.1.clone())
            .expect("non-null");
        assert_eq!(&v, want);
        assert!(BitString::parse_text(&v, 9).is_ok(), "reparse {v}");
    }
    // 出力を再入力しても同じ値になる。
    let first = got.first().and_then(|r| r.1.clone()).expect("first");
    insert(&core, 100, &format!("'{first}'"), "again").expect("reinsert");
    let all = values(&core, "SELECT id, v FROM t LIMIT 50");
    assert_eq!(
        all.iter().find(|r| r.0 == 100).and_then(|r| r.1.clone()),
        Some(first)
    );
}

#[test]
fn insert_classifies_digit_and_length_errors_without_side_effects() {
    let (_p, _g, core) = open("bit-errors");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    let cases = [
        // 文字種違反は 22P02。
        ("'10a1'", "22P02"),
        ("'12'", "22P02"),
        ("' 101'", "22P02"),
        // ビット数不一致は 22026。
        ("'101'", "22026"),
        ("'10101'", "22026"),
        ("''", "22026"),
        // 数値・真偽値リテラルは型不一致（22000）。
        ("1010", "22000"),
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
    let (_p, _g, core) = open("bit-update");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    insert(&core, 1, "'1010'", "i1").expect("insert");
    let before = values(&core, "SELECT id, v FROM t LIMIT 5");
    for (sql, want) in [
        (
            "UPDATE t SET v = '101' WHERE id = 1 USING OPERATION_ID 'u1'",
            "22026",
        ),
        (
            "UPDATE t SET v = '10x0' WHERE id = 1 USING OPERATION_ID 'u2'",
            "22P02",
        ),
        (
            "INSERT INTO t (id, v) VALUES (1, '0101') ON CONFLICT (id) DO UPDATE SET v = 'zz' USING OPERATION_ID 'x1'",
            "22P02",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    assert_eq!(values(&core, "SELECT id, v FROM t LIMIT 5"), before);
    ok(
        &core,
        "UPDATE t SET v = '0110' WHERE id = 1 USING OPERATION_ID 'u4'",
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '1111') ON CONFLICT (id) DO UPDATE SET v = '0001' USING OPERATION_ID 'x2'",
    );
    assert_eq!(
        values(&core, "SELECT id, v FROM t LIMIT 5"),
        vec![(1, Some("0001".to_string()))]
    );
}

#[test]
fn column_default_is_validated_and_applied() {
    let (_p, _g, core) = open("bit-default");
    assert_eq!(
        code(
            &core,
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4) DEFAULT 'zz')"
        ),
        "22P02"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4) DEFAULT '101')"
        ),
        "22026"
    );
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4) DEFAULT '1001', w INTEGER)",
    );
    ok(
        &core,
        "INSERT INTO t (id, w) VALUES (1, 5) USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        values(&core, "SELECT id, v FROM t LIMIT 5"),
        vec![(1, Some("1001".to_string()))]
    );
    // 既存行は ADD COLUMN の DEFAULT を読み出し時に補完する。
    ok(&core, "ALTER TABLE t ADD COLUMN y BIT(3) DEFAULT '101'");
    assert_eq!(
        values(&core, "SELECT id, y FROM t LIMIT 5"),
        vec![(1, Some("101".to_string()))]
    );
    assert_eq!(
        code(&core, "ALTER TABLE t ADD COLUMN z BIT(3) DEFAULT 'nope'"),
        "22P02"
    );
}

#[test]
fn returning_projects_the_bit_string() {
    let (_p, _g, core) = open("bit-returning");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(6))");
    let outcome = exec(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '110011') RETURNING v USING OPERATION_ID 'r1'",
    )
    .expect("returning insert");
    match outcome {
        SqlOutcome::Returning(o) => match &o.result.rows[0].cells[0] {
            Cell::Bit(v) => assert_eq!(v.format_text(), "110011"),
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
}

#[test]
fn copy_from_binds_bit_fields() {
    let (_p, _g, core) = open("bit-copy");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
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
    run(b"1\t1010\n2\t0001\n3\t\\N\n", "c1").expect("copy ok");
    let got = values(&core, "SELECT id, v FROM t LIMIT 20");
    assert_eq!(got.len(), 3);
    assert_eq!(
        got.iter().find(|r| r.0 == 1).and_then(|r| r.1.clone()),
        Some("1010".to_string())
    );
    assert_eq!(
        got.iter().find(|r| r.0 == 3).and_then(|r| r.1.clone()),
        None
    );
    assert_eq!(
        run(b"9\t101\n", "c2").expect_err("length").wire_code(),
        "22026"
    );
    assert_eq!(
        run(b"9\t10x1\n", "c3").expect_err("digit").wire_code(),
        "22P02"
    );
    assert_eq!(
        values(&core, "SELECT id, v FROM t LIMIT 20").len(),
        3,
        "rejected COPY must not write rows"
    );
}

#[test]
fn values_and_catalog_survive_reopen() {
    let (path, guard, core) = open("bit-reopen");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(11))");
    insert(&core, 1, "'10110011101'", "p1").expect("insert");
    insert(&core, 2, "'00000000001'", "p2").expect("insert");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = values(&core, "SELECT id, v FROM t LIMIT 5");
    assert_eq!(got.len(), 2);
    assert_eq!(
        got.iter().find(|r| r.0 == 1).and_then(|r| r.1.clone()),
        Some("10110011101".to_string())
    );
    assert_eq!(
        got.iter().find(|r| r.0 == 2).and_then(|r| r.1.clone()),
        Some("00000000001".to_string())
    );
    drop(guard);
}

/// テナント境界は不変: 他テナントから行が見えず、拒否エラーに入力値を含めない。
#[test]
fn other_tenants_cannot_see_bit_rows() {
    let (_p, _g, core) = open("bit-tenant");
    ok(&core, "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4))");
    insert(&core, 1, "'1010'", "t1").expect("insert");
    let other = core
        .execute_sql(&ctx_for("bob"), "SELECT id, v FROM t LIMIT 5")
        .expect("select as bob");
    assert!(other.rows.is_empty(), "bob must not see alice's rows");
    for lit in ["'1secretword'", "'10'"] {
        let err = exec_as(
            &core,
            "bob",
            &format!("INSERT INTO t (id, v) VALUES (2, {lit}) USING OPERATION_ID 'b1'"),
        )
        .expect_err("bad");
        assert!(!err.to_string().contains("secretword"));
        assert!(!err.to_string().contains(lit.trim_matches('\'')));
    }
}

/// 未解禁の機能は他型の腕へ黙って流さず fail-closed に拒否する。
#[test]
fn features_not_yet_enabled_are_rejected_fail_closed() {
    let (_p, _g, core) = open("bit-failclosed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v BIT(4), n INTEGER)",
    );
    insert(&core, 1, "'1010'", "f1").expect("insert");
    for sql in [
        "SELECT id FROM t WHERE v = '1010' LIMIT 5",
        "SELECT id FROM t WHERE v < '1010' LIMIT 5",
        "SELECT id FROM t ORDER BY v LIMIT 5",
        "SELECT id FROM t ORDER BY v <~> '1010' LIMIT 5",
        "SELECT id FROM t ORDER BY v <%> '1010' LIMIT 5",
        "SELECT id FROM t WHERE v <~> '1010' LIMIT 5",
        "SELECT v FROM t GROUP BY v",
        "SELECT DISTINCT v FROM t LIMIT 5",
        "SELECT COUNT(DISTINCT v) FROM t",
        "SELECT MAX(v) FROM t",
        "SELECT v + 1 FROM t LIMIT 5",
        "SELECT v::TEXT FROM t LIMIT 5",
        "SELECT CAST('1010' AS BIT(4)) FROM t LIMIT 5",
        "SELECT v FROM t UNION SELECT v FROM t",
        "SELECT v FROM t UNION ALL SELECT v FROM t",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY v) FROM t LIMIT 5",
        "CREATE TABLE a (id BIGINT NOT NULL UNIQUE, v BIT(4) PRIMARY KEY)",
        "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4) UNIQUE)",
        "CREATE INDEX i ON t (v)",
        "ALTER TABLE t ALTER COLUMN n TYPE BIT(4)",
        "ALTER TABLE t ALTER COLUMN v TYPE TEXT",
        "ALTER TABLE t ALTER COLUMN v TYPE BIT(8)",
    ] {
        assert!(exec(&core, sql).is_err(), "{sql} must be rejected");
    }
    // `<~>`・`<%>` はどの文法位置でも受理せず構文段（42601）で拒否する。
    for sql in [
        "SELECT id FROM t ORDER BY v <~> '1010' LIMIT 5",
        "SELECT id FROM t ORDER BY v <%> '1010' LIMIT 5",
        "SELECT id FROM t WHERE v <~> '1010' LIMIT 5",
        "SELECT v <%> v FROM t LIMIT 5",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    // 拒否された文は副作用を残さない。
    assert_eq!(values(&core, "SELECT id, v FROM t LIMIT 5").len(), 1);
    // NULL 判定は型に依らず存在ビットだけを見るため BIT 列でも正しく動く。
    insert(&core, 2, "NULL", "f2").expect("insert null");
    let is_null = core
        .execute_sql(&ctx(), "SELECT id FROM t WHERE v IS NULL LIMIT 5")
        .expect("is null");
    assert_eq!(is_null.rows.len(), 1);
    assert_eq!(values(&core, "SELECT id, v FROM t LIMIT 5").len(), 2);
}

/// 型宣言の境界: `BIT(0)`・上限超過は `22023`、`BIT VARYING`・`VARBIT`・配列は `0A000`、
/// 先頭ゼロ・小数・符号・閉じ括弧の欠落は `42601`。
#[test]
fn type_declaration_boundaries() {
    let (_p, _g, core) = open("bit-spellings");
    for (sql, want) in [
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(0))", "22023"),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(83886081))",
            "22023",
        ),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(99999999999999999999))",
            "22023",
        ),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT VARYING)",
            "0A000",
        ),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT VARYING(4))",
            "0A000",
        ),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v VARBIT)", "0A000"),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v VARBIT(4))",
            "0A000",
        ),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4)[])",
            "0A000",
        ),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT[])", "0A000"),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(04))", "42601"),
        (
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(1.5))",
            "42601",
        ),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(-1))", "42601"),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT(4)", "42601"),
        ("CREATE TABLE a (id BIGINT PRIMARY KEY, v BIT())", "42601"),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    ok(
        &core,
        "CREATE TABLE ok_max (id BIGINT PRIMARY KEY, v BIT(83886080))",
    );
    ok(&core, "CREATE TABLE b (id BIGINT PRIMARY KEY, v INTEGER)");
    assert_eq!(code(&core, "ALTER TABLE b ADD COLUMN w BIT(0)"), "22023");
}

/// `BIT(N)` は VECTOR 列の本数に数えない（VECTOR 列 1 本と BIT 列を併せ持てる）。
#[test]
fn bit_column_does_not_count_as_a_vector_column() {
    let (_p, _g, core) = open("bit-vector");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, e VECTOR(3), b BIT(8))",
    );
    ok(
        &core,
        "INSERT INTO t (id, e, b) VALUES (1, '[1,2,3]', '10101010') USING OPERATION_ID 'v1'",
    );
    assert_eq!(
        values(&core, "SELECT id, b FROM t LIMIT 5"),
        vec![(1, Some("10101010".to_string()))]
    );
}

#[test]
fn bit_type_name_is_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("bit-enum");
    for name in ["bit", "varbit"] {
        assert!(
            exec(&core, &format!("CREATE TYPE {name} AS ENUM ('a')")).is_err(),
            "{name} must be reserved"
        );
    }
}
