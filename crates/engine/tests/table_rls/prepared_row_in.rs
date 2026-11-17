//! WHERE の行値の `[NOT] IN` の要素の成分に置いた `$n`（拡張クエリ。対象ビヘイビア: SQL-9・WIRE-12。
//! 親 Issue #3298・本 Issue #3300）の結合テスト。
//!
//! 役割: Parse・Describe・Bind・Execute が、同じ値をリテラルで書いた簡易クエリと「同じ行集合、または
//! 同じ SQLSTATE」になること、型付きダミーが相手列の型で有効なこと、序数がずれないこと、未対応形が
//! fail-closed で拒否されること、述語形 DML が自テナントの行に限られることを固定する。
//! 行値の IN は構文段で `RowCompare` へ脱糖されるため、評価・RLS は行値比較と同じ経路を通る。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::legacy_table;
use crate::temp_db;
use legacy_table::LegacyExec as _;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, &'static str> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
        .map_err(|e| e.wire_code())
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    if let Err(c) = run(core, tenant, sql) {
        panic!("{sql} failed: {c}");
    }
}

fn ids_of(o: SqlOutcome) -> Vec<u64> {
    let SqlOutcome::Query(r) = o else {
        panic!("not a query");
    };
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

/// Prepared 経路（Parse → Describe → Bind → Execute）の結果。Parse／Describe／Bind／Execute の
/// どこで失敗しても SQLSTATE で返す。
fn prepared_ids(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
    values: &[Option<Vec<u8>>],
) -> Result<Vec<u64>, &'static str> {
    let prepared = core.parse_sql_prepared(sql).map_err(|e| e.wire_code())?;
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .map_err(|e| e.wire_code())?;
    let bound = core
        .bind_prepared(&prepared, values)
        .map_err(|e| e.wire_code())?;
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx(tenant), &mut s, &bound)
        .map(ids_of)
        .map_err(|e| e.wire_code())
}

fn literal_ids(core: &EngineCore, tenant: &str, sql: &str) -> Result<Vec<u64>, &'static str> {
    run(core, tenant, sql).map(ids_of)
}

fn code_of_parse(core: &EngineCore, sql: &str) -> &'static str {
    match core.parse_sql_prepared(sql) {
        Ok(_) => panic!("{sql} must be rejected at Parse"),
        Err(e) => e.wire_code(),
    }
}

const CREATE_ENUM: &str = "CREATE TYPE mood AS ENUM ('m1', 'm2', 'm3', 'm4')";
const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INTEGER, bg BIGINT, \
     r REAL, f DOUBLE PRECISION, s TEXT, v VARCHAR(20), n NUMERIC(10,2), d DATE, \
     ts TIMESTAMP, tz TIMESTAMPTZ, tm TIME, iv INTERVAL, u UUID, bb BYTEA, m mood, bo BOOLEAN, \
     j JSONB)";

/// alice: id1〜4（列ごとに昇順の値）、bob: id5（id2 と同じ値）。
fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(&core, "alice", CREATE_ENUM);
    ok(&core, "alice", CREATE);
    for (tenant, id, i) in [
        ("alice", 1, 1),
        ("alice", 2, 2),
        ("alice", 3, 3),
        ("alice", 4, 4),
        ("bob", 5, 2),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO t (id, embedding, a, bg, r, f, s, v, n, d, ts, tz, tm, iv, u, bb, m, bo, j) \
                 VALUES ({id}, '[0.1,0.2]', {i}, {i}0, {i}.5, {i}.25, 's{i}', 'v{i}', {i}.50, \
                 '2024-01-0{i}', '2024-01-0{i} 10:00:00', '2024-01-0{i} 10:00:00+00', '1{i}:00:00', \
                 '{i} hours', '00000000-0000-0000-0000-00000000000{i}', '\\x0{i}', 'm{i}', {bo}, '{{}}') \
                 USING OPERATION_ID 'op{id}'",
                bo = if i % 2 == 0 { "true" } else { "false" },
            ),
        );
    }
    (path, guard, core)
}

/// 列ごとの `(列名, 種別, 行 2 の値の text 表記)`。種別は `q`＝クォート付きリテラル、`n`＝裸のリテラル。
const COLUMNS: [(&str, char, &str); 16] = [
    ("a", 'n', "2"),
    ("bg", 'n', "20"),
    ("r", 'n', "2.5"),
    ("f", 'n', "2.25"),
    ("s", 'q', "s2"),
    ("v", 'q', "v2"),
    ("n", 'q', "2.50"),
    ("d", 'q', "2024-01-02"),
    ("ts", 'q', "2024-01-02 10:00:00"),
    ("tz", 'q', "2024-01-02 10:00:00+00"),
    ("tm", 'q', "12:00:00"),
    ("iv", 'q', "2 hours"),
    ("u", 'q', "00000000-0000-0000-0000-000000000002"),
    ("m", 'q', "m2"),
    ("bo", 'n', "true"),
    ("id", 'n', "2"),
];

fn lit(kind: char, v: &str) -> String {
    if kind == 'q' {
        format!("'{}'", v.replace('\'', "''"))
    } else {
        v.to_string()
    }
}

/// 全対応型 × `IN`／`NOT IN` で、Prepared の結果がリテラル形と一致する。
#[test]
fn prepared_row_in_matches_literal_form_across_types() {
    let (_p, _g, core) = setup("prep-row-in-literal");
    let mut nonempty = 0usize;
    for (col, kind, v2) in COLUMNS {
        let v3 = match col {
            "a" | "bg" | "id" => "3".to_string(),
            _ => v2.to_string(),
        };
        for neg in ["", "NOT "] {
            let sql_p = format!(
                "SELECT id FROM t WHERE ({col}, a) {neg}IN (($1, $2), ($3, $4)) ORDER BY id LIMIT 10"
            );
            let sql_l = format!(
                "SELECT id FROM t WHERE ({col}, a) {neg}IN (({}, 2), ({}, 3)) ORDER BY id LIMIT 10",
                lit(kind, v2),
                lit(kind, &v3)
            );
            let vals = [some(v2), some("2"), some(&v3), some("3")];
            for tenant in ["alice", "bob"] {
                let got = prepared_ids(&core, tenant, &sql_p, &vals);
                let want = literal_ids(&core, tenant, &sql_l);
                assert_eq!(got, want, "{tenant}: {sql_p}");
                nonempty += usize::from(matches!(&got, Ok(v) if !v.is_empty()));
            }
        }
    }
    assert!(nonempty >= 10, "only {nonempty} cases returned rows");
}

/// 代表的な形（要素 1 個・リテラル混在・前置 NOT・AND 結合）で期待の行が返る。
#[test]
fn prepared_row_in_returns_expected_rows() {
    let (_p, _g, core) = setup("prep-row-in-rows");
    let q = |sql: &str, vals: &[Option<Vec<u8>>]| prepared_ids(&core, "alice", sql, vals);
    assert_eq!(
        q(
            "SELECT id FROM t WHERE (a, s) IN (($1, $2)) LIMIT 10",
            &[some("2"), some("s2")]
        ),
        Ok(vec![2])
    );
    assert_eq!(
        q(
            "SELECT id FROM t WHERE (a, s) IN (($1, $2), (3, $3)) LIMIT 10",
            &[some("1"), some("s1"), some("s3")]
        ),
        Ok(vec![1, 3])
    );
    assert_eq!(
        q(
            "SELECT id FROM t WHERE (a, s) NOT IN (($1, $2)) LIMIT 10",
            &[some("2"), some("s2")]
        ),
        Ok(vec![1, 3, 4])
    );
    assert_eq!(
        q(
            "SELECT id FROM t WHERE NOT (a, s) IN (($1, $2)) LIMIT 10",
            &[some("2"), some("s2")]
        ),
        Ok(vec![1, 3, 4])
    );
    assert_eq!(
        q(
            "SELECT id FROM t WHERE bo = true AND (a, bg) IN (($1, $2), ($3, $4)) LIMIT 10",
            &[some("2"), some("20"), some("3"), some("30")]
        ),
        Ok(vec![2])
    );
}

/// Describe 用の型付きダミーが全対応型で通る。
#[test]
fn describe_accepts_typed_dummy_for_row_in_on_every_column_type() {
    let (_p, _g, core) = setup("prep-row-in-describe");
    let session = SessionState::default();
    for (col, _, _) in COLUMNS {
        let sql = format!("SELECT id FROM t WHERE ({col}, a) IN (($1, $2)) LIMIT 10");
        let prepared = core
            .parse_sql_prepared(&sql)
            .unwrap_or_else(|e| panic!("{sql}: parse {}", e.wire_code()));
        core.describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
    }
}

/// 型を持たない列（JSONB）の相手は Parse で固定の `0A000`（値を含まない）。
#[test]
fn unsupported_column_types_in_row_in_are_rejected_with_0a000() {
    let (_p, _g, core) = setup("prep-row-in-unsupported");
    let sql = "SELECT id FROM t WHERE (j, a) IN (($1, $2)) LIMIT 10";
    let err = core.parse_sql_prepared(sql).expect_err("must be rejected");
    assert_eq!(err.wire_code(), "0A000", "{sql}");
    assert!(!err.to_string().contains('$'), "{err}");
}

/// intentional: docs/design/wire-extended-query-param-binding.md
/// 拒否が残る形（fail-closed）。左辺成分の `$n`、相手がリテラル・式・修飾列の位置、式の内側、
/// 要素が行でない形・スカラー `IN`、サブクエリ形、EXPLAIN 前置、集合演算、成分数の不一致。
#[test]
fn unsupported_row_in_shapes_stay_fail_closed() {
    let (_p, _g, core) = setup("prep-row-in-reject");
    for sql in [
        "SELECT id FROM t WHERE ($1, $2) IN ((a, bg)) LIMIT 10",
        "SELECT id FROM t WHERE (a, 7) IN (($1, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, a + 1) IN (($1, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, t.a) IN (($1, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN (($1 + 1, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN (($1::int, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN ($1, $2) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN (($1, $2, $3)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN ((SELECT a, bg FROM t)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN () LIMIT 10",
        "SELECT id FROM t WHERE a IN ($1, $2) LIMIT 10",
        "EXPLAIN SELECT id FROM t WHERE (a, bg) IN (($1, $2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) IN (($1, $2)) UNION SELECT id FROM t WHERE a = 1 LIMIT 10",
    ] {
        let c = code_of_parse(&core, sql);
        assert!(["42601", "0A000"].contains(&c), "{sql}: {c}");
    }
}

/// intentional: docs/design/wire-extended-query-param-binding.md
/// NULL のバインドは、リテラル `NULL` 成分と同じ SQLSTATE になる（行値の NULL 成分の評価は対象外）。
#[test]
fn null_bind_matches_literal_null_component_in_row_in() {
    let (_p, _g, core) = setup("prep-row-in-null");
    let got = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) IN (($1, $2)) LIMIT 10",
        &[None, some("5")],
    );
    let want = literal_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, bg) IN ((NULL, 5)) LIMIT 10",
    );
    assert_eq!(got, want);
}

/// 不正な値は Parse 後の Bind／Execute でリテラル形と同じ SQLSTATE になり、メッセージに値本文を含めない。
#[test]
fn invalid_values_fail_like_the_literal_form_in_row_in() {
    let (_p, _g, core) = setup("prep-row-in-invalid");
    let secret = "zz-secret-value";
    for (col, kind) in [("a", 'q'), ("d", 'q'), ("u", 'q'), ("m", 'q')] {
        let sql = format!("SELECT id FROM t WHERE ({col}, a) IN (($1, $2)) LIMIT 10");
        let got = prepared_ids(&core, "alice", &sql, &[some(secret), some("2")]);
        let lit_sql = format!(
            "SELECT id FROM t WHERE ({col}, a) IN (({}, 2)) LIMIT 10",
            lit(kind, secret)
        );
        let want = literal_ids(&core, "alice", &lit_sql);
        if col != "m" {
            assert!(got.is_err(), "{col}: {got:?}");
        }
        assert_eq!(got, want, "{col}");
    }
}

/// SQL 断片を含む値は 1 個のリテラルにとどまり、述語を書き換えない。
#[test]
fn values_with_sql_fragments_stay_a_single_literal_in_row_in() {
    let (_p, _g, core) = setup("prep-row-in-injection");
    let got = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (s, a) IN (($1, $2)) LIMIT 10",
        &[some("x') OR (1=1"), some("2")],
    );
    assert_eq!(got, Ok(vec![]));
    let got = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (s, a) NOT IN (($1, $2)) LIMIT 10",
        &[some("x'; DROP TABLE t; --"), some("2")],
    );
    assert_eq!(got, Ok(vec![1, 2, 3, 4]));
}

/// 兄弟の `col = $n` の序数（Describe 用ダミーフラグ）が行値の IN の `$n` でずれない。
#[test]
fn describe_ordinals_stay_aligned_with_row_in_params() {
    let (_p, _g, core) = setup("prep-row-in-ordinal");
    let session = SessionState::default();
    for sql in [
        "SELECT id FROM t WHERE (a, m) IN (($1, $2)) AND m = $3 LIMIT 10",
        "SELECT id FROM t WHERE m = $1 AND (a, bg) IN (($2, $3), ($4, $5)) LIMIT 10",
        "SELECT id FROM t WHERE (a, bg) NOT IN (($1, $2)) OR m = $3 LIMIT 10",
    ] {
        let prepared = core
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
        core.describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("{sql}: describe {}", e.wire_code()));
    }
    // 兄弟の等価述語の不正値は、行値の IN があっても検証される。
    let got = prepared_ids(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, m) IN (($1, $2)) AND m = $3 LIMIT 10",
        &[some("2"), some("m2"), some("not-a-mood")],
    );
    assert!(got.is_err(), "{got:?}");
}

/// 述語形 UPDATE／DELETE も同じ経路で動き、副作用は自テナントの該当行に限られる。
#[test]
fn predicate_dml_with_row_in_params_touches_only_own_matching_rows() {
    let (_p, _g, core) = setup("prep-row-in-dml");
    let upd = core
        .parse_sql_prepared(
            "UPDATE t SET s = 'changed' WHERE (a, bg) IN (($1, $2), ($3, $4)) USING OPERATION_ID $5",
        )
        .expect("parse update");
    assert_eq!(upd.param_count(), 5);
    let bound = core
        .bind_prepared(
            &upd,
            &[
                some("2"),
                some("20"),
                some("3"),
                some("30"),
                some("op-upd-1"),
            ],
        )
        .expect("bind");
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &bound)
        .expect("execute update");
    assert_eq!(
        literal_ids(
            &core,
            "alice",
            "SELECT id FROM t WHERE s = 'changed' LIMIT 10"
        ),
        Ok(vec![2, 3])
    );
    // bob の id5 は a=2 / bg=20 で述語に一致するが、他テナントの行は変わらない。
    assert_eq!(
        literal_ids(
            &core,
            "bob",
            "SELECT id FROM t WHERE s = 'changed' LIMIT 10"
        ),
        Ok(vec![])
    );

    let del = core
        .parse_sql_prepared("DELETE FROM t WHERE (a, bg) NOT IN (($1, $2)) USING OPERATION_ID $3")
        .expect("parse delete");
    let bound = core
        .bind_prepared(&del, &[some("2"), some("20"), some("op-del-1")])
        .expect("bind");
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &bound)
        .expect("execute delete");
    assert_eq!(
        literal_ids(&core, "alice", "SELECT id FROM t WHERE a >= 1 LIMIT 10"),
        Ok(vec![2])
    );
    assert_eq!(
        literal_ids(&core, "bob", "SELECT id FROM t WHERE a = 2 LIMIT 10"),
        Ok(vec![5])
    );
}

/// スキーマ修飾表の Parse・Describe が通り、修飾表を第 1 表に置いた JOIN は `42601`。
#[test]
fn schema_qualified_row_in_params_parse_and_join_is_rejected() {
    let (_p, _g, core) = setup("prep-row-in-qualified");
    let session = SessionState::default();
    let sql = "SELECT id FROM public.t WHERE (a, bg) IN (($1, $2)) LIMIT 10";
    let prepared = core.parse_sql_prepared(sql).expect("parse");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe");
    assert_eq!(
        code_of_parse(
            &core,
            "SELECT t.id FROM public.t JOIN t u ON t.id = u.id WHERE (t.a, t.bg) IN (($1, $2)) LIMIT 10"
        ),
        "42601"
    );
}
