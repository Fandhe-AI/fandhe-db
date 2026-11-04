//! `SPARSEVEC(N)` 列（`ColumnType::SparseVec`。TABLE-47 (1)(2)(5)・Issue #2025）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され再オープン後も
//! 値ごと復元されること、(2) INSERT／UPDATE SET／UPSERT／RETURNING の往復一致（入力順非依存・ゼロ除去）、
//! (3) 形式不正は `22P02`・位置範囲外／重複／非有限／次元不一致は `22000`・上限超過は `54000` で副作用なく
//! 拒否されること、(4) 列本数の制約（VECTOR・HALFVEC・INT8VEC と合わせて 0〜1 本）、(5) 未解禁の機能
//! （距離計算・索引・制約・DDL 変更・COPY・CAST・並べ替え・集計）が fail-closed に拒否されること、
//! (6) テナント境界が不変であることを固定する。距離計算・kNN は兄弟 Issue（#2026）の担当。

use engine::catalog::ColumnType;
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

/// `SELECT id, <col> ...` の結果を `(id, テキスト表現または None)` で返す（SPARSEVEC は `Cell::SparseVec`）。
fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<(i64, Option<String>)> {
    let SqlOutcome::Query(q) =
        exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    else {
        panic!("{sql} must return a query result");
    };
    q.rows
        .iter()
        .map(|r| {
            let id = match r.cells.first() {
                Some(Cell::SignedInteger(n)) => *n,
                Some(Cell::Integer(n)) => i64::try_from(*n).expect("id"),
                other => panic!("unexpected id cell {other:?}"),
            };
            let v = match r.cells.get(1) {
                Some(Cell::SparseVec(v)) => Some(v.format_text()),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (id, v)
        })
        .collect()
}

fn rows(core: &EngineCore, sql: &str) -> Vec<(i64, Option<String>)> {
    rows_as(core, "alice", sql)
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

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// TABLE-47 (1): 型がカタログへ永続化され、再オープン後も値が復元される。
#[test]
fn sparsevec_type_is_persisted_in_the_catalog() {
    let (path, _g, core) = open("sv-persist");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a sparsevec(5), b TEXT NOT NULL)",
    );
    ok(
        &core,
        "INSERT INTO t (id, a, b) VALUES (1, '{3:2,1:1}/5', 'x')",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::SparseVec(5));
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        rows(&core, "SELECT id, a FROM t"),
        vec![(1, some("{1:1,3:2}/5"))]
    );
}

/// TABLE-47 (1): 入力順に依らず昇順で保存され、ゼロ要素は保存されず、テキストが往復で一致する。
#[test]
fn text_roundtrips_and_zero_elements_are_dropped() {
    let (_p, _g, core) = open("sv-roundtrip");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(1000000000))",
    );
    for (id, lit, want) in [
        (1, "{3:2,1:1}/1000000000", "{1:1,3:2}/1000000000"),
        (2, "{}/1000000000", "{}/1000000000"),
        (3, "{5:0,6:-0,7:0.5}/1000000000", "{7:0.5}/1000000000"),
        (
            4,
            "{1000000000:-1.25}/1000000000",
            "{1000000000:-1.25}/1000000000",
        ),
        (5, " { 2 : 3 } / 1000000000 ", "{2:3}/1000000000"),
    ] {
        ok(
            &core,
            &format!("INSERT INTO t (id, v) VALUES ({id}, '{lit}')"),
        );
        let got = rows(&core, &format!("SELECT id, v FROM t WHERE id = {id}"));
        assert_eq!(got, vec![(id, some(want))], "{lit}");
        // 出力を再投入しても同じ値になる。
        ok(&core, &format!("UPDATE t SET v = '{want}' WHERE id = {id}"));
        assert_eq!(
            rows(&core, &format!("SELECT id, v FROM t WHERE id = {id}")),
            vec![(id, some(want))]
        );
    }
}

#[test]
fn update_upsert_and_returning_roundtrip() {
    let (_p, _g, core) = open("sv-update");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(4), n TEXT)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '{1:1}/4', 'a')");
    ok(&core, "UPDATE t SET v = '{4:2,2:3}/4' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, some("{2:3,4:2}/4"))]
    );
    // 他列だけの UPDATE は SPARSEVEC を壊さない。
    ok(&core, "UPDATE t SET n = 'b' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, some("{2:3,4:2}/4"))]
    );
    ok(
        &core,
        "INSERT INTO t (id, v, n) VALUES (1, '{1:9}/4', 'c') ON CONFLICT (id) DO UPDATE SET v = '{3:7}/4'",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, some("{3:7}/4"))]
    );
    // 不正な SET は拒否され、値は変わらない。
    assert_eq!(
        code(&core, "UPDATE t SET v = '{1:1}/5' WHERE id = 1"),
        "22000"
    );
    assert_eq!(
        code(&core, "UPDATE t SET v = '{5:1}/4' WHERE id = 1"),
        "22000"
    );
    assert_eq!(
        code(&core, "UPDATE t SET v = '{1:x}/4' WHERE id = 1"),
        "22P02"
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, some("{3:7}/4"))]
    );
    let out = exec(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '{2:1,1:1}/4') RETURNING v",
    )
    .expect("returning");
    let SqlOutcome::Returning(o) = out else {
        panic!("expected SqlOutcome::Returning");
    };
    assert!(matches!(
        o.result.rows.first().and_then(|r| r.cells.first()),
        Some(Cell::SparseVec(v)) if v.format_text() == "{1:1,2:1}/4"
    ));
}

/// TABLE-47 (1)(2): 形式不正 22P02・値不正 22000・上限超過 54000。副作用なし。
#[test]
fn invalid_values_are_rejected_with_expected_sqlstate_and_no_side_effects() {
    let (_p, _g, core) = open("sv-invalid");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(5))",
    );
    let cases: &[(&str, &str)] = &[
        // 形式不正は 22P02。
        ("'abc'", "22P02"),
        ("''", "22P02"),
        ("'{1:1}'", "22P02"),
        ("'{1:1}/'", "22P02"),
        ("'{1:1}/x'", "22P02"),
        ("'1:1/5'", "22P02"),
        ("'{1}/5'", "22P02"),
        ("'{1:}/5'", "22P02"),
        ("'{:1}/5'", "22P02"),
        ("'{a:1}/5'", "22P02"),
        ("'{1:abc}/5'", "22P02"),
        ("'{1:1,}/5'", "22P02"),
        ("'{1.5:1}/5'", "22P02"),
        // 位置範囲外・重複・非有限・次元不一致は 22000。
        ("'{0:1}/5'", "22000"),
        ("'{6:1}/5'", "22000"),
        ("'{-1:1}/5'", "22000"),
        ("'{99999999999999999999:1}/5'", "22000"),
        ("'{1:1,1:2}/5'", "22000"),
        ("'{1:0,1:1}/5'", "22000"),
        ("'{1:nan}/5'", "22000"),
        ("'{1:inf}/5'", "22000"),
        ("'{1:1e999}/5'", "22000"),
        ("'{1:1}/4'", "22000"),
        ("'{1:1}/6'", "22000"),
        ("'{}/0'", "22000"),
        ("5", "22000"),
        // 次元の上限超過は 54000。
        ("'{}/1000000001'", "54000"),
        ("'{}/99999999999'", "54000"),
    ];
    for (lit, expected) in cases {
        assert_eq!(
            code(&core, &format!("INSERT INTO t (id, v) VALUES (1, {lit})")),
            *expected,
            "{lit}"
        );
    }
    // リテラル長の上限（512 KiB）超過は 54000。
    let long = format!("'{{{}}}/5'", "1:1,".repeat(200_000));
    assert_eq!(
        code(&core, &format!("INSERT INTO t (id, v) VALUES (1, {long})")),
        "54000"
    );
    assert!(rows(&core, "SELECT id, v FROM t").is_empty());
}

/// TABLE-47 (2): 非ゼロ数の既定上限 16,000 ちょうどは受理、+1 は 54000。ゼロ要素は数えない。
#[test]
fn non_zero_count_limit_is_enforced() {
    let (_p, _g, core) = open("sv-nnz");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(20000))",
    );
    let lit = |n: usize, zeros: usize| {
        let mut parts: Vec<String> = (1..=n).map(|i| format!("{i}:1")).collect();
        parts.extend((n + 1..=n + zeros).map(|i| format!("{i}:0")));
        format!("'{{{}}}/20000'", parts.join(","))
    };
    ok(
        &core,
        &format!("INSERT INTO t (id, v) VALUES (1, {})", lit(16_000, 100)),
    );
    assert_eq!(
        code(
            &core,
            &format!("INSERT INTO t (id, v) VALUES (2, {})", lit(16_001, 0))
        ),
        "54000"
    );
    let got = rows(&core, "SELECT id, v FROM t");
    assert_eq!(got.len(), 1);
    assert!(got[0]
        .1
        .as_deref()
        .is_some_and(|s| s.starts_with("{1:1,2:1,")));
}

#[test]
fn column_count_constraint_is_shared_with_vector_family() {
    let (_p, _g, core) = open("sv-count");
    for sql in [
        "CREATE TABLE a (id BIGINT PRIMARY KEY, x VECTOR(2), y SPARSEVEC(2))",
        "CREATE TABLE b (id BIGINT PRIMARY KEY, x HALFVEC(2), y SPARSEVEC(2))",
        "CREATE TABLE b2 (id BIGINT PRIMARY KEY, x INT8VEC(2), y SPARSEVEC(2))",
        "CREATE TABLE b3 (id BIGINT PRIMARY KEY, x SPARSEVEC(2), y VECTOR(2))",
        "CREATE TABLE c (id BIGINT PRIMARY KEY, x SPARSEVEC(2), y SPARSEVEC(2))",
        "CREATE TABLE d (id BIGINT PRIMARY KEY, x SPARSEVEC(0))",
        "CREATE TABLE e (id BIGINT PRIMARY KEY, x SPARSEVEC)",
        "CREATE TABLE f (id BIGINT PRIMARY KEY, x SPARSEVEC(2)[])",
        "CREATE TABLE g (id BIGINT PRIMARY KEY, x SPARSEVEC(1000000001))",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    ok(
        &core,
        "CREATE TABLE ok1 (id BIGINT PRIMARY KEY, x SPARSEVEC(1000000000))",
    );
}

#[test]
fn unsupported_features_fail_closed() {
    let (_p, _g, core) = open("sv-closed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(2), n INTEGER)",
    );
    ok(
        &core,
        "INSERT INTO t (id, v, n) VALUES (1, '{1:1,2:2}/2', 1)",
    );
    ok(&core, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    assert_eq!(
        code(&core, "ALTER TABLE u ADD COLUMN w SPARSEVEC(2)"),
        "0A000"
    );
    ok(&core, "ALTER TABLE u ADD COLUMN n INTEGER");
    assert_eq!(
        code(&core, "ALTER TABLE u ALTER COLUMN n TYPE SPARSEVEC(2)"),
        "42804"
    );
    assert_eq!(code(&core, "ALTER TABLE t DROP COLUMN v"), "42601");
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN v TYPE TEXT"),
        "42804"
    );
    let cases: &[(&str, &str)] = &[
        ("SELECT id FROM t ORDER BY v <-> '{1:1}/2' LIMIT 1", "0A000"),
        ("SELECT '{1:1}/2'::sparsevec(2)", "0A000"),
        ("CREATE INDEX i ON t USING hnsw (v)", "0A000"),
        ("CREATE INDEX i ON t (v)", "0A000"),
        ("CREATE INDEX i ON t USING gin (v)", "0A000"),
        ("CREATE TABLE c AS SELECT * FROM t", "0A000"),
        ("CREATE TABLE p (v SPARSEVEC(2) PRIMARY KEY)", "42601"),
        (
            "CREATE TABLE q (id BIGINT PRIMARY KEY, v SPARSEVEC(2) UNIQUE)",
            "42601",
        ),
        ("UPDATE t SET v = v WHERE id = 1", "0A000"),
        ("INSERT INTO t (id, v) SELECT id + 10, v FROM t", "0A000"),
    ];
    for (sql, expected) in cases {
        assert_eq!(code(&core, sql), *expected, "{sql}");
    }
    // 比較・並べ替え・集計・集合演算・式は成功しない（fail-closed）。コードは問わず拒否されること。
    for sql in [
        "SELECT id FROM t ORDER BY v",
        "SELECT id FROM t WHERE v = '{1:1}/2'",
        "SELECT DISTINCT v FROM t",
        "SELECT v, count(*) FROM t GROUP BY v",
        "SELECT max(v) FROM t",
        "SELECT count(v) FROM t",
        "SELECT count(DISTINCT v) FROM t",
        "SELECT v::text FROM t",
        "SELECT v FROM t UNION SELECT v FROM t",
        "SELECT v || 'x' FROM t",
    ] {
        assert!(exec(&core, sql).is_err(), "{sql} must be rejected");
    }
    // 副作用なし。
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, some("{1:1,2:2}/2"))]
    );
}

#[test]
fn passthrough_paths_return_values() {
    let (_p, _g, core) = open("sv-passthrough");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(3))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '{3:-7,2:2}/3')");
    ok(&core, "INSERT INTO t (id) VALUES (2)");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE v IS NULL"),
        vec![(2, None)]
    );
    ok(&core, "CREATE VIEW vw AS SELECT id, v FROM t");
    assert_eq!(
        rows(&core, "SELECT id, v FROM vw WHERE id = 1"),
        vec![(1, some("{2:2,3:-7}/3"))]
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '{1:1}/3') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 2"),
        vec![(2, some("{1:1}/3"))]
    );
}

/// テナント境界は不変で、エラー文言に入力値を含めない（TABLE-47 (5)）。
#[test]
fn tenant_boundary_is_unchanged() {
    let (_p, _g, core) = open("sv-tenant");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(3))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '{1:1}/3')");
    exec_as(&core, "bob", "INSERT INTO t (id, v) VALUES (2, '{3:4}/3')").expect("bob insert");
    assert_eq!(
        rows_as(&core, "alice", "SELECT id, v FROM t"),
        vec![(1, some("{1:1}/3"))]
    );
    assert_eq!(
        rows_as(&core, "bob", "SELECT id, v FROM t"),
        vec![(2, some("{3:4}/3"))]
    );
    let err = exec_as(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (3, '{1:secretvalue}/3')",
    )
    .expect_err("invalid");
    assert!(!format!("{err:?}").contains("secretvalue"));
}

#[test]
fn set_null_on_nullable_sparsevec_column() {
    let (_p, _g, core) = open("sv-setnull");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '{1:1}/2')");
    ok(&core, "UPDATE t SET v = NULL WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 1"),
        vec![(1, None)]
    );
    ok(
        &core,
        "CREATE TABLE n (id BIGINT PRIMARY KEY, r SPARSEVEC(2) NOT NULL)",
    );
    ok(&core, "INSERT INTO n (id, r) VALUES (1, '{2:4}/2')");
    assert_eq!(code(&core, "UPDATE n SET r = NULL WHERE id = 1"), "23502");
}

#[test]
fn sparsevec_is_rejected_in_materialized_view_regardless_of_rows() {
    let (_p, _g, core) = open("sv-matview");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h SPARSEVEC(2))",
    );
    let q = "CREATE MATERIALIZED VIEW mv AS SELECT h FROM t";
    assert_eq!(code(&core, q), "0A000");
    ok(&core, "INSERT INTO t (id, h) VALUES (1, '{1:1}/2')");
    assert_eq!(code(&core, q), "0A000");
}

#[test]
fn sparsevec_type_name_is_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("sv-enum");
    for name in ["sparsevec", "SPARSEVEC"] {
        assert!(
            exec(&core, &format!("CREATE TYPE {name} AS ENUM ('a')")).is_err(),
            "{name} must be reserved"
        );
    }
}

/// `COPY FROM` は値の有無によらず SPARSEVEC 列を 0A000 で拒否し、行を書かない。
#[test]
fn copy_from_rejects_sparsevec_even_for_null_and_empty() {
    use engine::core::CopyPlan;
    let (_p, _g, core) = open("sv-copy");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v SPARSEVEC(2))",
    );
    for (i, data) in [&b"1\t\\N\n"[..], b"2\t\n", b"3\t{1:1}/2\n"]
        .iter()
        .enumerate()
    {
        let policy = ctx_for("alice");
        let session = SessionState::default();
        let plan = core
            .begin_copy(
                &policy,
                &session,
                &format!("COPY t (id, v) FROM STDIN USING OPERATION_ID 'c{i}'"),
            )
            .and_then(|plan| {
                let mut cs = match plan {
                    CopyPlan::From(s) => s,
                    CopyPlan::To(..) => panic!("expected COPY FROM plan"),
                };
                cs.feed(data)?;
                cs.finish()
            });
        let err = plan.err().expect("COPY into SPARSEVEC must fail");
        assert_eq!(err.wire_code(), "0A000", "input {data:?}");
    }
    assert!(rows(&core, "SELECT id, v FROM t").is_empty());
}
