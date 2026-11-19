//! `SMALLINT[]`／`INT2[n]` 列（`ArrayElemType::SmallInt`。TABLE-14・TABLE-35・Issue #3230）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、CREATE／INSERT／SELECT・添字・
//! スライス・述語・ARRAY 構築子・`array_agg`・UNIQUE・範囲外（`22003`）・テナント境界・
//! 再オープン後の読み戻しを固定する。`INTEGER[]` と別の要素型であること（型の区別）も検証する。

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

fn small(core: &EngineCore, sql: &str) -> V {
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
        "CREATE TABLE a (id BIGINT PRIMARY KEY, n SMALLINT[], m INT2[3], i INTEGER[], s SMALLINT)",
    );
    ok(
        &core,
        "INSERT INTO a (id, n, m, i, s) VALUES (1, '{1,NULL,-32768}', '{1,2}', '{1,2}', 7) USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO a (id, n, s) VALUES (2, '{32767}', 32767) USING OPERATION_ID 'i2'",
    );
    (path, guard, core)
}

#[test]
fn create_insert_select_roundtrip_with_boundaries() {
    let (_p, _g, core) = setup("sia-basic");
    assert_eq!(
        small(&core, "SELECT n FROM a WHERE id = 1 LIMIT 1"),
        V::SmallInt(vec![Some(1), None, Some(i16::MIN)])
    );
    assert_eq!(
        small(&core, "SELECT n FROM a WHERE id = 2 LIMIT 1"),
        V::SmallInt(vec![Some(i16::MAX)])
    );
    // 2 次元（述語の対象外にするため別表）。
    ok(
        &core,
        "CREATE TABLE md (id BIGINT PRIMARY KEY, n SMALLINT[])",
    );
    ok(
        &core,
        "INSERT INTO md (id, n) VALUES (3, '{{1,2},{3,4}}') USING OPERATION_ID 'i3'",
    );
    match small(&core, "SELECT n FROM md WHERE id = 3 LIMIT 1") {
        V::Multi(m) => assert_eq!(m.dims(), &[2, 2]),
        other => panic!("expected a multi-dimensional array, got {other:?}"),
    }
}

#[test]
fn subscript_slice_and_result_types() {
    let (_p, _g, core) = setup("sia-subscript");
    let r = select(
        &core,
        "alice",
        "SELECT n[1], n[1:2], n FROM a WHERE id = 1 LIMIT 1",
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
    assert_eq!(tys[0], Some(ColumnType::SmallInt));
    match &tys[1] {
        Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::SmallInt),
        other => panic!("expected smallint[] slice, got {other:?}"),
    }
    match &tys[2] {
        Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::SmallInt),
        other => panic!("expected smallint[], got {other:?}"),
    }
    assert_eq!(
        small(&core, "SELECT n[1:2] FROM a WHERE id = 1 LIMIT 1"),
        V::SmallInt(vec![Some(1), None])
    );
}

#[test]
fn predicates_work_on_smallint_arrays() {
    let (_p, _g, core) = setup("sia-pred");
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE n = '{1,NULL,-32768}' LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE m IS NULL LIMIT 10"),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE 1 = ANY(n) LIMIT 10"),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE s = ANY(n) LIMIT 10"),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE n @> '{1}' LIMIT 10"),
        1
    );
    // 異なる要素型の配列どうしは PG の anyarray と同じく型不一致（42883）。
    assert_eq!(
        code(&core, "SELECT id FROM a WHERE n = i LIMIT 10"),
        "42883"
    );
}

#[test]
fn array_constructor_and_array_agg_yield_smallint_arrays() {
    let (_p, _g, core) = setup("sia-ctor");
    assert_eq!(
        small(
            &core,
            "SELECT ARRAY[CAST(1 AS smallint), CAST(2 AS smallint)] FROM a WHERE id = 1 LIMIT 1"
        ),
        V::SmallInt(vec![Some(1), Some(2)])
    );
    match small(&core, "SELECT array_agg(s) FROM a WHERE id < 3") {
        V::SmallInt(mut v) => {
            v.sort();
            assert_eq!(v, vec![Some(7), Some(i16::MAX)]);
        }
        other => panic!("expected smallint[], got {other:?}"),
    }
    assert_eq!(
        small(
            &core,
            "SELECT CAST(i AS smallint[]) FROM a WHERE id = 1 LIMIT 1"
        ),
        V::SmallInt(vec![Some(1), Some(2)])
    );
}

#[test]
fn out_of_range_malformed_and_oversized_are_rejected() {
    let (_p, _g, core) = setup("sia-reject");
    for (lit, want) in [
        ("'{32768}'", "22003"),
        ("'{-32769}'", "22003"),
        ("'{1,70000}'", "22003"),
        ("'{a}'", "22P02"),
        ("'{1,2,3,4}'", "54000"),
    ] {
        let sql = format!("INSERT INTO a (id, m) VALUES (9, {lit}) USING OPERATION_ID 'bad'");
        assert_eq!(code(&core, &sql), want, "{sql}");
    }
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE id = 9 LIMIT 10"),
        0
    );
    assert_eq!(
        code(&core, "SELECT CAST('{70000}' AS smallint[]) FROM a LIMIT 1"),
        "22003"
    );
}

#[test]
fn unique_smallint_array_rejects_duplicates() {
    let (_p, _g, core) = setup("sia-uniq");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, n SMALLINT[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, n) VALUES (1, '{1,2}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "INSERT INTO u (id, n) VALUES (2, '{2,1}') USING OPERATION_ID 'u2'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, n) VALUES (3, '{1,2}') USING OPERATION_ID 'u3'"
        ),
        "23505"
    );
}

#[test]
fn other_tenants_see_no_rows_and_values_survive_reopen() {
    let (path, _g, core) = setup("sia-tenant");
    assert_eq!(count(&core, "bob", "SELECT id FROM a LIMIT 10"), 0);
    drop(core);
    let core = core_at(&path);
    assert_eq!(
        small(&core, "SELECT n FROM a WHERE id = 1 LIMIT 1"),
        V::SmallInt(vec![Some(1), None, Some(i16::MIN)])
    );
}
