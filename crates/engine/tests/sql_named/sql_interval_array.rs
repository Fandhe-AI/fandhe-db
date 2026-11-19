//! `INTERVAL[]`／`INTERVAL[n]` 列（`ArrayElemType::Interval`。TABLE-14・TABLE-34・TABLE-35・
//! Issue #3234）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、CREATE／INSERT／SELECT・添字・
//! スライス・述語（正規化キーによる等価・包含・量化比較）・ARRAY 構築子・`array_agg`・UNIQUE・
//! 書式不正（`22007`）・フィールドあふれ（`22015`）・無限大（`0A000`）・集合演算の拒否・
//! テナント境界・再オープン後の読み戻しを固定する。

use engine::catalog::{ArrayElemType, ColumnType};
use engine::core::EngineCore;
use engine::datetime::Interval;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue as V;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const fn iv(months: i32, days: i32, micros: i64) -> Interval {
    Interval {
        months,
        days,
        micros,
    }
}

const HOUR: i64 = 3_600_000_000;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
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

fn select(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn first_cell(core: &EngineCore, sql: &str) -> Cell {
    select(core, "alice", sql)
        .rows
        .first()
        .and_then(|r| r.cells.first())
        .cloned()
        .unwrap_or_else(|| panic!("{sql} returned no cell"))
}

fn ivs(core: &EngineCore, sql: &str) -> V {
    match first_cell(core, sql) {
        Cell::Array(a) => a,
        other => panic!("{sql}: expected an array, got {other:?}"),
    }
}

fn count(core: &EngineCore, tenant: &str, sql: &str) -> usize {
    select(core, tenant, sql).rows.len()
}

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "CREATE TABLE a (id BIGINT PRIMARY KEY, v INTERVAL[], m INTERVAL[3], s INTERVAL)",
    );
    ok(
        &core,
        "INSERT INTO a (id, v, m, s) VALUES (1, '{\"1 day\",NULL,\"2 hours\"}', '{\"1 mon\"}', '1 day') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO a (id, v, s) VALUES (2, '{\"30 days\"}', '5 days') USING OPERATION_ID 'i2'",
    );
    (path, guard, core)
}

#[test]
fn create_insert_select_roundtrip_keeps_raw_components() {
    let (_p, _g, core) = setup("iva-basic");
    assert_eq!(
        ivs(&core, "SELECT v FROM a WHERE id = 1 LIMIT 1"),
        V::Interval(vec![Some(iv(0, 1, 0)), None, Some(iv(0, 0, 2 * HOUR))])
    );
    // 行バイトは生の三要素（`1 mon` と `30 days` は別の値のまま往復する）。
    assert_eq!(
        ivs(&core, "SELECT m FROM a WHERE id = 1 LIMIT 1"),
        V::Interval(vec![Some(iv(1, 0, 0))])
    );
    assert_eq!(
        ivs(&core, "SELECT v FROM a WHERE id = 2 LIMIT 1"),
        V::Interval(vec![Some(iv(0, 30, 0))])
    );
    // 2 次元（述語の対象外にするため別表）。
    ok(
        &core,
        "CREATE TABLE md (id BIGINT PRIMARY KEY, v INTERVAL[])",
    );
    ok(
        &core,
        "INSERT INTO md (id, v) VALUES (3, '{{\"1 day\",\"2 days\"},{\"3 days\",\"4 days\"}}') USING OPERATION_ID 'i3'",
    );
    match ivs(&core, "SELECT v FROM md WHERE id = 3 LIMIT 1") {
        V::Multi(m) => assert_eq!(m.dims(), &[2, 2]),
        other => panic!("expected a multi-dimensional array, got {other:?}"),
    }
}

#[test]
fn array_literal_and_constructor_forms_are_accepted() {
    let (_p, _g, core) = setup("iva-ctor");
    ok(
        &core,
        "INSERT INTO a (id, v) VALUES (4, ARRAY[INTERVAL '1 day', NULL]) USING OPERATION_ID 'i4'",
    );
    assert_eq!(
        ivs(&core, "SELECT v FROM a WHERE id = 4 LIMIT 1"),
        V::Interval(vec![Some(iv(0, 1, 0)), None])
    );
    assert_eq!(
        ivs(
            &core,
            "SELECT ARRAY[CAST('1 day' AS interval), CAST('2 hours' AS interval)] FROM a WHERE id = 1 LIMIT 1"
        ),
        V::Interval(vec![Some(iv(0, 1, 0)), Some(iv(0, 0, 2 * HOUR))])
    );
    // interval と他型の混在は型不一致。
    assert_eq!(
        code(
            &core,
            "SELECT ARRAY[CAST('1 day' AS interval), CAST('10:00' AS time)] FROM a LIMIT 1"
        ),
        "42804"
    );
}

#[test]
fn subscript_slice_and_result_types() {
    let (_p, _g, core) = setup("iva-subscript");
    let r = select(
        &core,
        "alice",
        "SELECT v[1], v[1:2], v FROM a WHERE id = 1 LIMIT 1",
    );
    let tys: Vec<Option<ColumnType>> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tys[0], Some(ColumnType::Interval));
    for ty in &tys[1..] {
        match ty {
            Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::Interval),
            other => panic!("expected interval[], got {other:?}"),
        }
    }
    assert_eq!(
        ivs(&core, "SELECT v[1:2] FROM a WHERE id = 1 LIMIT 1"),
        V::Interval(vec![Some(iv(0, 1, 0)), None])
    );
    assert_eq!(
        first_cell(&core, "SELECT v[9] FROM a WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
}

#[test]
fn predicates_use_the_scalar_normalized_equality() {
    let (_p, _g, core) = setup("iva-pred");
    let n = |sql: &str| count(&core, "alice", sql);
    // 1 か月 = 30 日（スカラー INTERVAL と同じ正規化）。生の三要素は異なっても等しい。
    assert_eq!(n("SELECT id FROM a WHERE m = '{\"30 days\"}' LIMIT 10"), 1);
    assert_eq!(n("SELECT id FROM a WHERE v = '{\"1 mon\"}' LIMIT 10"), 1);
    assert_eq!(n("SELECT id FROM a WHERE v <> '{\"1 mon\"}' LIMIT 10"), 1);
    assert_eq!(
        n("SELECT id FROM a WHERE v = '{\"1 day\",NULL,\"2 hours\"}' LIMIT 10"),
        1
    );
    assert_eq!(n("SELECT id FROM a WHERE m IS NULL LIMIT 10"), 1);
    assert_eq!(
        n("SELECT id FROM a WHERE v @> '{\"24 hours\"}' LIMIT 10"),
        1
    );
    assert_eq!(
        n("SELECT id FROM a WHERE v && '{\"120 minutes\"}' LIMIT 10"),
        1
    );
    assert_eq!(n("SELECT id FROM a WHERE s = ANY(v) LIMIT 10"), 1);
    assert_eq!(
        n("SELECT id FROM a WHERE s = ANY('{\"1 day\"}') LIMIT 10"),
        1
    );
    assert_eq!(
        n("SELECT id FROM a WHERE s < ALL('{\"2 days\"}') LIMIT 10"),
        1
    );
}

#[test]
fn array_agg_of_interval_columns_yields_interval_arrays() {
    let (_p, _g, core) = setup("iva-agg");
    match ivs(&core, "SELECT array_agg(s) FROM a WHERE id < 3") {
        V::Interval(mut v) => {
            v.sort_by_key(|x| x.map(|i| i.days));
            assert_eq!(v, vec![Some(iv(0, 1, 0)), Some(iv(0, 5, 0))]);
        }
        other => panic!("expected interval[], got {other:?}"),
    }
    // INTERVAL の DISTINCT・ORDER BY はスカラーと同じく未対応（fail-closed）。
    assert_eq!(code(&core, "SELECT array_agg(DISTINCT s) FROM a"), "0A000");
}

#[test]
fn malformed_overflow_infinity_and_oversized_are_rejected_without_side_effects() {
    let (_p, _g, core) = setup("iva-reject");
    for (lit, want) in [
        ("'{x}'", "22007"),
        ("'{infinity}'", "0A000"),
        ("'{\"1 day\",\"2 days\",\"3 days\",\"4 days\"}'", "54000"),
    ] {
        let sql = format!("INSERT INTO a (id, m) VALUES (9, {lit}) USING OPERATION_ID 'bad'");
        assert_eq!(code(&core, &sql), want, "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "INSERT INTO a (id, m) VALUES (9, '{\"99999999999999999999 days\"}') USING OPERATION_ID 'bad2'"
        ),
        "22015"
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE id = 9 LIMIT 10"),
        0
    );
}

#[test]
fn unique_interval_array_uses_normalized_keys() {
    let (_p, _g, core) = setup("iva-uniq");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, v INTERVAL[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (1, '{\"1 mon\"}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (2, '{\"1 day\",\"2 days\"}') USING OPERATION_ID 'u2'",
    );
    // `1 mon` と `30 days` は等価（重複）。
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, v) VALUES (3, '{\"30 days\"}') USING OPERATION_ID 'u3'"
        ),
        "23505"
    );
    // 順序の違いは別の配列。
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (4, '{\"2 days\",\"1 day\"}') USING OPERATION_ID 'u4'",
    );
}

#[test]
fn unsupported_interval_array_spellings_stay_feature_not_supported() {
    let (_p, _g, core) = setup("iva-unsupported");
    for sql in [
        "CREATE TABLE x (v INTERVAL(3)[])",
        "CREATE TABLE x (v INTERVAL YEAR TO MONTH[])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(
        &core,
        "CREATE TABLE y (v INTERVAL[], w INTERVAL[3], z INTERVAL[][])",
    );
    ok(&core, "ALTER TABLE a ADD COLUMN extra INTERVAL[]");
    // 集合演算は scalar INTERVAL と同じく未対応（fail-closed）。
    assert_eq!(
        code(&core, "SELECT v FROM a UNION ALL SELECT v FROM a LIMIT 10"),
        "0A000"
    );
}

#[test]
fn other_tenants_see_no_rows_and_values_survive_reopen() {
    let (path, _g, core) = setup("iva-tenant");
    assert_eq!(count(&core, "bob", "SELECT id FROM a LIMIT 10"), 0);
    // 別テナントが同じ配列値を持っても UNIQUE 衝突しない。
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, v INTERVAL[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, v) VALUES (1, '{\"1 mon\"}') USING OPERATION_ID 'ua'",
    );
    exec_as(
        &core,
        "bob",
        "INSERT INTO u (id, v) VALUES (1, '{\"1 mon\"}') USING OPERATION_ID 'ub'",
    )
    .expect("another tenant may hold the same value");
    drop(core);
    let core = core_at(&path);
    assert_eq!(
        ivs(&core, "SELECT v FROM a WHERE id = 1 LIMIT 1"),
        V::Interval(vec![Some(iv(0, 1, 0)), None, Some(iv(0, 0, 2 * HOUR))])
    );
}
