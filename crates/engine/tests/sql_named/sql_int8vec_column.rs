//! `INT8VEC(N)` 列（`ColumnType::Int8Vec`。TABLE-49 (1)(3)(4)・Issue #2018）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され再オープン後も
//! 値ごと復元されること、(2) INSERT／UPDATE SET／UPSERT／RETURNING の往復一致（境界 −128・127・0・`-0`）、
//! (3) 範囲外は `22003`・整数でない表記は `22P02`・次元不一致は `22000`・長さ超過は `54000` で副作用なく
//! 拒否されること、(4) 列本数の制約（VECTOR・HALFVEC と合わせて 0〜1 本）、(5) 未解禁の機能（距離計算・
//! 索引・制約・DDL 変更・COPY・CAST・並べ替え・集計）が fail-closed に拒否されること、
//! (6) テナント境界が不変であることを固定する。距離計算は兄弟 Issue（#2019）の担当。

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

/// `SELECT id, <col> ...` の結果を `(id, 成分または None)` で返す（INT8VEC は f32 へ広げた `Cell::Vector`）。
fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<(i64, Option<Vec<f32>>)> {
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
                Some(Cell::Vector(v)) => Some(v.clone()),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (id, v)
        })
        .collect()
}

fn rows(core: &EngineCore, sql: &str) -> Vec<(i64, Option<Vec<f32>>)> {
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

/// wire のテキスト表現（`[a,b,...]`。f32 の Display は整数値を `1`・`-2` と出す）。
fn text_of(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

#[test]
fn int8vec_type_is_persisted_in_the_catalog() {
    let (path, _g, core) = open("iv-persist");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a int8vec(3), b TEXT NOT NULL)",
    );
    ok(
        &core,
        "INSERT INTO t (id, a, b) VALUES (1, '[1,-2,3]', 'x')",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Int8Vec(3));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Text);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let got = rows(&core, "SELECT id, a FROM t");
    assert_eq!(got, vec![(1, Some(vec![1.0, -2.0, 3.0]))]);
    let v = got[0].1.clone().expect("value");
    assert_eq!(text_of(&v), "[1,-2,3]");
}

#[test]
fn boundary_values_roundtrip_exactly() {
    let (_p, _g, core) = open("iv-roundtrip");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(5))",
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '[-128,127,0,-0,+5]')",
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, ' [ 1 , -2 ,3,4,5 ] ')",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t ORDER BY id"),
        vec![
            (1, Some(vec![-128.0, 127.0, 0.0, 0.0, 5.0])),
            (2, Some(vec![1.0, -2.0, 3.0, 4.0, 5.0])),
        ]
    );
}

#[test]
fn update_and_upsert_roundtrip() {
    let (_p, _g, core) = open("iv-update");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), n TEXT)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '[1,2]', 'a')");
    ok(&core, "UPDATE t SET v = '[-128,127]' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![-128.0, 127.0]))]
    );
    // 他列だけの UPDATE は INT8VEC を壊さない。
    ok(&core, "UPDATE t SET n = 'b' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![-128.0, 127.0]))]
    );
    ok(
        &core,
        "INSERT INTO t (id, v, n) VALUES (1, '[9,9]', 'c') ON CONFLICT (id) DO UPDATE SET v = '[4,-5]'",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![4.0, -5.0]))]
    );
    // 次元不一致・範囲外・非整数の SET は拒否され、値は変わらない。
    assert_eq!(
        code(&core, "UPDATE t SET v = '[1,2,3]' WHERE id = 1"),
        "22000"
    );
    assert_eq!(
        code(&core, "UPDATE t SET v = '[1,128]' WHERE id = 1"),
        "22003"
    );
    assert_eq!(
        code(&core, "UPDATE t SET v = '[1,1.5]' WHERE id = 1"),
        "22P02"
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![4.0, -5.0]))]
    );
}

#[test]
fn returning_and_null_handling() {
    let (_p, _g, core) = open("iv-null");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), w TEXT NOT NULL)",
    );
    let out = exec(
        &core,
        "INSERT INTO t (id, v, w) VALUES (1, '[-1,2]', 'x') RETURNING v",
    )
    .expect("returning");
    let SqlOutcome::Returning(o) = out else {
        panic!("expected SqlOutcome::Returning");
    };
    assert_eq!(
        o.result.rows.first().and_then(|r| r.cells.first()),
        Some(&Cell::Vector(vec![-1.0, 2.0]))
    );
    ok(&core, "INSERT INTO t (id, w) VALUES (2, 'x')");
    ok(&core, "INSERT INTO t (id, v, w) VALUES (3, NULL, 'x')");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 2"),
        vec![(2, None)]
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 3"),
        vec![(3, None)]
    );
    assert_eq!(code(&core, "INSERT INTO t (id) VALUES (4)"), "23502");
    assert_eq!(
        code(&core, "INSERT INTO t (id, w) VALUES (5, NULL)"),
        "23502"
    );
}

#[test]
fn invalid_values_are_rejected_with_expected_sqlstate_and_no_side_effects() {
    let (_p, _g, core) = open("iv-invalid");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    let cases: &[(&str, &str)] = &[
        // 範囲外（桁数過大を含む）は 22003。
        ("'[128,1]'", "22003"),
        ("'[1,-129]'", "22003"),
        ("'[99999999999999999999,1]'", "22003"),
        // 整数でない表記・形式不正は 22P02。
        ("'[1.5,1]'", "22P02"),
        ("'[1.0,1]'", "22P02"),
        ("'[1e2,1]'", "22P02"),
        ("'[abc,1]'", "22P02"),
        ("'[0x10,1]'", "22P02"),
        ("'[1,,2]'", "22P02"),
        ("'[1,]'", "22P02"),
        ("'[-,1]'", "22P02"),
        ("'1,2'", "22P02"),
        ("'[1,2'", "22P02"),
        ("'abc'", "22P02"),
        // 次元不一致は 22000。
        ("'[1]'", "22000"),
        ("'[1,2,3]'", "22000"),
        ("5", "22000"),
    ];
    for (lit, expected) in cases {
        assert_eq!(
            code(&core, &format!("INSERT INTO t (id, v) VALUES (1, {lit})")),
            *expected,
            "{lit}"
        );
    }
    // 長さ上限（64 KiB）超過は 54000。
    let long = format!("'[{}]'", "1,".repeat(40_000));
    assert_eq!(
        code(&core, &format!("INSERT INTO t (id, v) VALUES (1, {long})")),
        "54000"
    );
    assert!(rows(&core, "SELECT id, v FROM t").is_empty());
}

#[test]
fn column_count_constraint_is_shared_with_vector_family() {
    let (_p, _g, core) = open("iv-count");
    for sql in [
        "CREATE TABLE a (id BIGINT PRIMARY KEY, x VECTOR(2), y INT8VEC(2))",
        "CREATE TABLE b (id BIGINT PRIMARY KEY, x HALFVEC(2), y INT8VEC(2))",
        "CREATE TABLE b2 (id BIGINT PRIMARY KEY, x INT8VEC(2), y VECTOR(2))",
        "CREATE TABLE c (id BIGINT PRIMARY KEY, x INT8VEC(2), y INT8VEC(2))",
        "CREATE TABLE d (id BIGINT PRIMARY KEY, x INT8VEC(0))",
        "CREATE TABLE e (id BIGINT PRIMARY KEY, x INT8VEC)",
        "CREATE TABLE f (id BIGINT PRIMARY KEY, x INT8VEC(2)[])",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

/// `INT8` は BIGINT の別名のままで、`INT8VEC` とは別語として解決される。
#[test]
fn int8_alias_still_means_bigint() {
    let (path, _g, core) = open("iv-alias");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INT8, v INT8VEC(2))",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "n"), ColumnType::BigInt);
    assert_eq!(column_type(&path, "t", "v"), ColumnType::Int8Vec(2));
}

#[test]
fn unsupported_features_fail_closed() {
    let (_p, _g, core) = open("iv-closed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2), n INTEGER)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '[1,2]', 1)");
    // 列変更。
    ok(&core, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    assert_eq!(
        code(&core, "ALTER TABLE u ADD COLUMN w INT8VEC(2)"),
        "0A000"
    );
    ok(&core, "ALTER TABLE u ADD COLUMN n INTEGER");
    assert_eq!(
        code(&core, "ALTER TABLE u ALTER COLUMN n TYPE INT8VEC(2)"),
        "42804"
    );
    assert_eq!(code(&core, "ALTER TABLE t DROP COLUMN v"), "42601");
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN v TYPE TEXT"),
        "42804"
    );
    // 検索・式・並べ替え・集計・索引・制約・実体化は拒否する（距離計算は兄弟 Issue #2019 の担当）。
    let cases: &[(&str, &str)] = &[
        ("SELECT id FROM t ORDER BY v <-> '[1,2]' LIMIT 1", "0A000"),
        ("SELECT id FROM t ORDER BY v", "22000"),
        ("SELECT id FROM t WHERE v = '[1,2]'", "22000"),
        ("SELECT DISTINCT v FROM t", "22000"),
        ("SELECT v, count(*) FROM t GROUP BY v", "22000"),
        ("SELECT max(v) FROM t", "42883"),
        ("SELECT count(v) FROM t", "42883"),
        ("SELECT v::text FROM t", "22000"),
        ("SELECT '[1,2]'::int8vec(2)", "0A000"),
        ("SELECT v FROM t UNION SELECT v FROM t", "0A000"),
        ("SELECT v || 'x' FROM t", "22000"),
        ("CREATE INDEX i ON t (v)", "0A000"),
        ("CREATE INDEX i ON t USING hnsw (v)", "0A000"),
        ("CREATE INDEX i ON t USING gin (v)", "0A000"),
        ("CREATE TABLE c AS SELECT * FROM t", "0A000"),
        ("CREATE TABLE p (v INT8VEC(2) PRIMARY KEY)", "42601"),
        (
            "CREATE TABLE q (id BIGINT PRIMARY KEY, v INT8VEC(2) UNIQUE)",
            "42601",
        ),
        (
            "CREATE TABLE r (id BIGINT PRIMARY KEY, v INT8VEC(2) DEFAULT '[1,2]')",
            "0A000",
        ),
        (
            "CREATE TABLE r2 (id BIGINT PRIMARY KEY, v INT8VEC(2) CHECK (v IS NOT NULL))",
            "42601",
        ),
        ("UPDATE t SET v = v WHERE id = 1", "0A000"),
        ("INSERT INTO t (id, v) SELECT id + 10, v FROM t", "0A000"),
    ];
    for (sql, expected) in cases {
        assert_eq!(code(&core, sql), *expected, "{sql}");
    }
    // 副作用なし。
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![1.0, 2.0]))]
    );
}

#[test]
fn passthrough_paths_return_values() {
    let (_p, _g, core) = open("iv-passthrough");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[-7,2]')");
    ok(&core, "INSERT INTO t (id) VALUES (2)");
    // IS NULL は行の有無を正しく判定する。
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE v IS NULL"),
        vec![(2, None)]
    );
    // ビュー経由の素通し。
    ok(&core, "CREATE VIEW vw AS SELECT id, v FROM t");
    assert_eq!(
        rows(&core, "SELECT id, v FROM vw WHERE id = 1"),
        vec![(1, Some(vec![-7.0, 2.0]))]
    );
    // EXCLUDED の参照も運ぶ。
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '[1,3]') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 2"),
        vec![(2, Some(vec![1.0, 3.0]))]
    );
}

#[test]
fn tenant_boundary_is_unchanged() {
    let (_p, _g, core) = open("iv-tenant");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    exec_as(&core, "bob", "INSERT INTO t (id, v) VALUES (2, '[3,-4]')").expect("bob insert");
    assert_eq!(
        rows_as(&core, "alice", "SELECT id, v FROM t"),
        vec![(1, Some(vec![1.0, 2.0]))]
    );
    assert_eq!(
        rows_as(&core, "bob", "SELECT id, v FROM t"),
        vec![(2, Some(vec![3.0, -4.0]))]
    );
}

/// nullable INT8VEC 列への `SET v = NULL`（UPDATE／UPSERT）は成功し、NOT NULL 列では 23502 で拒否される。
#[test]
fn set_null_on_nullable_int8vec_column() {
    let (_p, _g, core) = open("iv-setnull");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    ok(&core, "UPDATE t SET v = NULL WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 1"),
        vec![(1, None)]
    );
    ok(&core, "UPDATE t SET v = '[5,6]' WHERE id = 1");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '[9,9]') ON CONFLICT (id) DO UPDATE SET v = NULL",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 1"),
        vec![(1, None)]
    );
    ok(
        &core,
        "CREATE TABLE n (id BIGINT PRIMARY KEY, r INT8VEC(2) NOT NULL)",
    );
    ok(&core, "INSERT INTO n (id, r) VALUES (1, '[3,4]')");
    assert_eq!(code(&core, "UPDATE n SET r = NULL WHERE id = 1"), "23502");
}

/// INT8VEC 列は行の内容によらず、マテリアライズドビューの出力に使えない（0A000）。
#[test]
fn int8vec_is_rejected_in_materialized_view_regardless_of_rows() {
    let (_p, _g, core) = open("iv-matview");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h INT8VEC(2))",
    );
    let q = "CREATE MATERIALIZED VIEW mv AS SELECT h FROM t";
    assert_eq!(code(&core, q), "0A000");
    ok(&core, "INSERT INTO t (id, h) VALUES (1, NULL)");
    assert_eq!(code(&core, q), "0A000");
    ok(&core, "UPDATE t SET h = '[1,2]' WHERE id = 1");
    assert_eq!(code(&core, q), "0A000");
}

/// `int8vec` は新規 ENUM 型名として予約される（大文字小文字不問）。
#[test]
fn int8vec_type_name_is_reserved_against_new_enum_names() {
    let (_p, _g, core) = open("iv-enum");
    for name in ["int8vec", "INT8VEC"] {
        assert!(
            exec(&core, &format!("CREATE TYPE {name} AS ENUM ('a')")).is_err(),
            "{name} must be reserved"
        );
    }
}

/// `COPY FROM` は値の有無（`\N`・空欄・値あり）によらず INT8VEC 列を 0A000 で拒否し、行を書かない。
#[test]
fn copy_from_rejects_int8vec_even_for_null_and_empty() {
    use engine::core::CopyPlan;
    let (_p, _g, core) = open("iv-copy");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INT8VEC(2))",
    );
    for (i, data) in [&b"1\t\\N\n"[..], b"2\t\n", b"3\t[1,2]\n"]
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
        let err = plan.err().expect("COPY into INT8VEC must fail");
        assert_eq!(err.wire_code(), "0A000", "input {data:?}");
    }
    assert!(rows(&core, "SELECT id, v FROM t").is_empty());
}
