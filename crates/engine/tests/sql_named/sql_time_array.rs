//! `TIME[]`／`TIME[n]` 列（`ArrayElemType::Time`。TABLE-14・TABLE-35・Issue #3232）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、CREATE／INSERT／SELECT・添字・
//! スライス・述語・ARRAY 構築子・`array_agg`・UNIQUE・範囲外（`22008`）・書式不正（`22007`）・
//! テナント境界・再オープン後の読み戻しを固定する。`TIMESTAMP[]` と別の要素型であること
//! （型の区別）も検証する。

use engine::catalog::{ArrayElemType, ColumnType};
use engine::core::EngineCore;
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

const H10: i64 = 36_000_000_000;
const H24: i64 = 86_400_000_000;

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

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut s, sql)
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

fn times(core: &EngineCore, sql: &str) -> V {
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
        "CREATE TABLE a (id BIGINT PRIMARY KEY, t TIME[], m TIME[3], ts TIMESTAMP[], s TIME)",
    );
    ok(
        &core,
        "INSERT INTO a (id, t, m, ts, s) VALUES (1, '{10:00:00,NULL,24:00:00}', '{00:00:00,12:34:56.789}', '{2020-01-01 10:00:00}', '10:00:00') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO a (id, t, s) VALUES (2, '{23:59:59.999999}', '01:02:03') USING OPERATION_ID 'i2'",
    );
    (path, guard, core)
}

#[test]
fn create_insert_select_roundtrip_with_boundaries() {
    let (_p, _g, core) = setup("tia-basic");
    assert_eq!(
        times(&core, "SELECT t FROM a WHERE id = 1 LIMIT 1"),
        V::Time(vec![Some(H10), None, Some(H24)])
    );
    assert_eq!(
        times(&core, "SELECT m FROM a WHERE id = 1 LIMIT 1"),
        V::Time(vec![Some(0), Some(45_296_789_000)])
    );
    assert_eq!(
        times(&core, "SELECT t FROM a WHERE id = 2 LIMIT 1"),
        V::Time(vec![Some(H24 - 1)])
    );
    // 2 次元（述語の対象外にするため別表）。
    ok(&core, "CREATE TABLE md (id BIGINT PRIMARY KEY, t TIME[])");
    ok(
        &core,
        "INSERT INTO md (id, t) VALUES (3, '{{10:00,11:00},{12:00,13:00}}') USING OPERATION_ID 'i3'",
    );
    match times(&core, "SELECT t FROM md WHERE id = 3 LIMIT 1") {
        V::Multi(m) => assert_eq!(m.dims(), &[2, 2]),
        other => panic!("expected a multi-dimensional array, got {other:?}"),
    }
}

#[test]
fn subscript_slice_and_result_types() {
    let (_p, _g, core) = setup("tia-subscript");
    let r = select(
        &core,
        "alice",
        "SELECT t[1], t[1:2], t FROM a WHERE id = 1 LIMIT 1",
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
    assert_eq!(tys[0], Some(ColumnType::Time));
    for ty in &tys[1..] {
        match ty {
            Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::Time),
            other => panic!("expected time[], got {other:?}"),
        }
    }
    assert_eq!(
        times(&core, "SELECT t[1:2] FROM a WHERE id = 1 LIMIT 1"),
        V::Time(vec![Some(H10), None])
    );
    // 範囲外の添字は NULL。
    assert_eq!(
        first_cell(&core, "SELECT t[9] FROM a WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
}

#[test]
fn predicates_work_on_time_arrays() {
    let (_p, _g, core) = setup("tia-pred");
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE t = '{10:00:00,NULL,24:00:00}' LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE m IS NULL LIMIT 10"),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE s = ANY(t) LIMIT 10"),
        1
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE s = ANY('{10:00:00}') LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE t @> '{10:00}' LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE t && '{24:00}' LIMIT 10"
        ),
        1
    );
    // 異なる要素型の配列どうしは PG の anyarray と同じく型不一致（42883）。
    assert_eq!(
        code(&core, "SELECT id FROM a WHERE t = ts LIMIT 10"),
        "42883"
    );
}

#[test]
fn array_constructor_and_array_agg_yield_time_arrays() {
    let (_p, _g, core) = setup("tia-ctor");
    assert_eq!(
        times(
            &core,
            "SELECT ARRAY[CAST('10:00' AS time), CAST('11:30' AS time)] FROM a WHERE id = 1 LIMIT 1"
        ),
        V::Time(vec![Some(H10), Some(41_400_000_000)])
    );
    // time と date の混在は型不一致。
    assert_eq!(
        code(
            &core,
            "SELECT ARRAY[CAST('10:00' AS time), CAST('2020-01-01' AS date)] FROM a LIMIT 1"
        ),
        "42804"
    );
    match times(&core, "SELECT array_agg(s) FROM a WHERE id < 3") {
        V::Time(mut v) => {
            v.sort();
            assert_eq!(v, vec![Some(3_723_000_000), Some(H10)]);
        }
        other => panic!("expected time[], got {other:?}"),
    }
}

#[test]
fn out_of_range_malformed_and_oversized_are_rejected_without_side_effects() {
    let (_p, _g, core) = setup("tia-reject");
    for (lit, want) in [
        ("'{24:00:01}'", "22008"),
        ("'{x}'", "22007"),
        ("'{10:00,11:00,12:00,13:00}'", "54000"),
    ] {
        let sql = format!("INSERT INTO a (id, m) VALUES (9, {lit}) USING OPERATION_ID 'bad'");
        assert_eq!(code(&core, &sql), want, "{sql}");
    }
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE id = 9 LIMIT 10"),
        0
    );
}

#[test]
fn unique_time_array_rejects_duplicates() {
    let (_p, _g, core) = setup("tia-uniq");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, t TIME[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, t) VALUES (1, '{10:00,11:00}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "INSERT INTO u (id, t) VALUES (2, '{11:00,10:00}') USING OPERATION_ID 'u2'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, t) VALUES (3, '{10:00:00,11:00:00}') USING OPERATION_ID 'u3'"
        ),
        "23505"
    );
}

#[test]
fn unsupported_time_array_spellings_stay_feature_not_supported() {
    let (_p, _g, core) = setup("tia-unsupported");
    for sql in [
        "CREATE TABLE x (v TIME(3)[])",
        "CREATE TABLE x (v TIMETZ[])",
        "CREATE TABLE x (v TIME WITH TIME ZONE[])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
}

#[test]
fn other_tenants_see_no_rows_and_values_survive_reopen() {
    let (path, _g, core) = setup("tia-tenant");
    assert_eq!(count(&core, "bob", "SELECT id FROM a LIMIT 10"), 0);
    drop(core);
    let core = core_at(&path);
    assert_eq!(
        times(&core, "SELECT t FROM a WHERE id = 1 LIMIT 1"),
        V::Time(vec![Some(H10), None, Some(H24)])
    );
}
