//! `TIMESTAMPTZ[]`／`TIMESTAMPTZ[n]` 列（`ArrayElemType::TimestampTz`。TABLE-14・TABLE-35・
//! TABLE-33・Issue #3233）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、CREATE／INSERT／SELECT・添字・
//! スライス・述語・ARRAY 構築子・`array_agg`・UNIQUE・範囲外（`22008`）・書式不正（`22007`）・
//! 未知の TZ 名（`22023`）・テナント境界・再オープン後の読み戻しを固定する。入力の TZ 解釈が
//! スカラー `TIMESTAMPTZ` 列と同じであること、`TIMESTAMP[]` と別の要素型であること（型の区別）、
//! テキスト出力がセッション TimeZone に従うことも検証する。

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

/// 2026-01-01 01:00:00 UTC（`10:00:00+09`）。
const T0100: i64 = 1_767_229_200_000_000;
/// 2026-01-01 10:00:00 UTC。
const T1000: i64 = 1_767_261_600_000_000;
/// 2026-01-01 00:00:00 UTC。
const T0000: i64 = 1_767_225_600_000_000;

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

fn arr(core: &EngineCore, sql: &str) -> V {
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
        "CREATE TABLE a (id BIGINT PRIMARY KEY, t TIMESTAMPTZ[], m TIMESTAMPTZ[3], ts TIMESTAMP[], s TIMESTAMPTZ)",
    );
    ok(
        &core,
        "INSERT INTO a (id, t, m, ts, s) VALUES (1, '{\"2026-01-01 10:00:00+09\",NULL,\"2026-01-01 10:00:00Z\"}', '{\"2026-01-01 00:00:00Z\",\"2026-01-01 10:00:00+09\"}', '{2020-01-01 10:00:00}', '2026-01-01 10:00:00Z') USING OPERATION_ID 'i1'",
    );
    ok(
        &core,
        "INSERT INTO a (id, t, s) VALUES (2, '{\"2026-01-01 00:00:00\"}', '2026-01-01 00:00:00Z') USING OPERATION_ID 'i2'",
    );
    (path, guard, core)
}

#[test]
fn create_insert_select_roundtrip_normalizes_to_utc() {
    let (_p, _g, core) = setup("tza-basic");
    assert_eq!(
        arr(&core, "SELECT t FROM a WHERE id = 1 LIMIT 1"),
        V::TimestampTz(vec![Some(T0100), None, Some(T1000)])
    );
    assert_eq!(
        arr(&core, "SELECT m FROM a WHERE id = 1 LIMIT 1"),
        V::TimestampTz(vec![Some(T0000), Some(T0100)])
    );
    // offset 無しはセッション既定（UTC）で解釈する。
    assert_eq!(
        arr(&core, "SELECT t FROM a WHERE id = 2 LIMIT 1"),
        V::TimestampTz(vec![Some(T0000)])
    );
    // 2 次元（述語の対象外にするため別表）。
    ok(
        &core,
        "CREATE TABLE md (id BIGINT PRIMARY KEY, t TIMESTAMPTZ[])",
    );
    ok(
        &core,
        "INSERT INTO md (id, t) VALUES (3, '{{\"2026-01-01 00:00:00Z\",\"2026-01-01 01:00:00Z\"},{\"2026-01-01 02:00:00Z\",\"2026-01-01 03:00:00Z\"}}') USING OPERATION_ID 'i3'",
    );
    match arr(&core, "SELECT t FROM md WHERE id = 3 LIMIT 1") {
        V::Multi(m) => assert_eq!(m.dims(), &[2, 2]),
        other => panic!("expected a multi-dimensional array, got {other:?}"),
    }
}

#[test]
fn session_time_zone_drives_input_and_text_output() {
    let (_p, _g, core) = setup("tza-session");
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = ctx("alice");
    core.execute_sql_in_session(&ctx, &mut s, "SET TIME ZONE 'Asia/Tokyo'")
        .expect("set time zone");
    // offset 無しの要素はセッション TimeZone（東京）で解釈する（スカラー列と同じ）。
    core.execute_sql_in_session(
        &ctx,
        &mut s,
        "INSERT INTO a (id, t) VALUES (5, '{\"2026-01-01 09:00:00\"}') USING OPERATION_ID 'tz5'",
    )
    .expect("insert");
    let cell = |s: &mut SessionState, sql: &str| match core
        .execute_sql_in_session(&ctx, s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    {
        SqlOutcome::Query(r) => r.rows[0].cells[0].clone(),
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(
        cell(&mut s, "SELECT t FROM a WHERE id = 5 LIMIT 1"),
        Cell::Array(V::TimestampTz(vec![Some(T0000)]))
    );
    // CAST(.. AS text)・array_to_string はセッション TimeZone で整形する。
    assert_eq!(
        cell(&mut s, "SELECT CAST(t AS text) FROM a WHERE id = 5 LIMIT 1"),
        Cell::Text("{\"2026-01-01 09:00:00+09\"}".into())
    );
    assert_eq!(
        cell(
            &mut s,
            "SELECT array_to_string(t, ',') FROM a WHERE id = 5 LIMIT 1"
        ),
        Cell::Text("2026-01-01 09:00:00+09".into())
    );
    core.execute_sql_in_session(&ctx, &mut s, "SET TIME ZONE 'UTC'")
        .expect("reset time zone");
    assert_eq!(
        cell(&mut s, "SELECT CAST(t AS text) FROM a WHERE id = 5 LIMIT 1"),
        Cell::Text("{\"2026-01-01 00:00:00+00\"}".into())
    );
}

#[test]
fn subscript_slice_and_result_types() {
    let (_p, _g, core) = setup("tza-subscript");
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
    assert_eq!(tys[0], Some(ColumnType::TimestampTz));
    for ty in &tys[1..] {
        match ty {
            Some(ColumnType::Array(a)) => assert_eq!(a.elem(), ArrayElemType::TimestampTz),
            other => panic!("expected timestamptz[], got {other:?}"),
        }
    }
    assert_eq!(
        arr(&core, "SELECT t[1:2] FROM a WHERE id = 1 LIMIT 1"),
        V::TimestampTz(vec![Some(T0100), None])
    );
    // 範囲外の添字は NULL。
    assert_eq!(
        first_cell(&core, "SELECT t[9] FROM a WHERE id = 1 LIMIT 1"),
        Cell::Null
    );
}

#[test]
fn predicates_work_on_timestamptz_arrays() {
    let (_p, _g, core) = setup("tza-pred");
    // 異なる offset で書いた同じ瞬間は等しい。
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE t = '{\"2026-01-01 01:00:00Z\",NULL,\"2026-01-01 19:00:00+09\"}' LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE m IS NULL LIMIT 10"),
        1
    );
    assert_eq!(
        count(&core, "alice", "SELECT id FROM a WHERE s = ANY(t) LIMIT 10"),
        // id=1 は s=T1000 が t の要素、id=2 は s=T0000 が t の要素。
        2
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE s = ANY('{\"2026-01-01 19:00:00+09\"}') LIMIT 10"
        ),
        1
    );
    assert_eq!(
        count(
            &core,
            "alice",
            "SELECT id FROM a WHERE t @> '{\"2026-01-01 10:00:00+09\"}' LIMIT 10"
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
fn array_constructor_and_array_agg_yield_timestamptz_arrays() {
    let (_p, _g, core) = setup("tza-ctor");
    assert_eq!(
        arr(
            &core,
            "SELECT ARRAY[CAST('2026-01-01 10:00:00+09' AS timestamptz), CAST('2026-01-01 10:00:00Z' AS timestamptz)] FROM a WHERE id = 1 LIMIT 1"
        ),
        V::TimestampTz(vec![Some(T0100), Some(T1000)])
    );
    // timestamp と timestamptz の混在は型不一致。
    assert_eq!(
        code(
            &core,
            "SELECT ARRAY[CAST('2026-01-01 00:00:00' AS timestamp), CAST('2026-01-01 00:00:00Z' AS timestamptz)] FROM a LIMIT 1"
        ),
        "42804"
    );
    match arr(&core, "SELECT array_agg(s) FROM a WHERE id < 3") {
        V::TimestampTz(mut v) => {
            v.sort();
            assert_eq!(v, vec![Some(T0000), Some(T1000)]);
        }
        other => panic!("expected timestamptz[], got {other:?}"),
    }
}

#[test]
fn out_of_range_malformed_and_oversized_are_rejected_without_side_effects() {
    let (_p, _g, core) = setup("tza-reject");
    for (lit, want) in [
        ("'{\"2026-13-01 00:00:00Z\"}'", "22008"),
        ("'{x}'", "22007"),
        ("'{\"2026-01-01 00:00:00 No/Such_Zone\"}'", "22023"),
        (
            "'{\"2026-01-01 00:00:00Z\",\"2026-01-01 00:00:00Z\",\"2026-01-01 00:00:00Z\",\"2026-01-01 00:00:00Z\"}'",
            "54000",
        ),
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
fn unique_timestamptz_array_compares_instants_not_spellings() {
    let (_p, _g, core) = setup("tza-uniq");
    ok(
        &core,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, t TIMESTAMPTZ[] UNIQUE)",
    );
    ok(
        &core,
        "INSERT INTO u (id, t) VALUES (1, '{\"2026-01-01 00:00:00Z\",\"2026-01-01 01:00:00Z\"}') USING OPERATION_ID 'u1'",
    );
    ok(
        &core,
        "INSERT INTO u (id, t) VALUES (2, '{\"2026-01-01 01:00:00Z\",\"2026-01-01 00:00:00Z\"}') USING OPERATION_ID 'u2'",
    );
    // 同じ瞬間を別の offset で書いても重複（キーは UTC マイクロ秒）。
    assert_eq!(
        code(
            &core,
            "INSERT INTO u (id, t) VALUES (3, '{\"2026-01-01 09:00:00+09\",\"2026-01-01 10:00:00+09\"}') USING OPERATION_ID 'u3'"
        ),
        "23505"
    );
}

#[test]
fn unsupported_timestamptz_array_spellings_stay_feature_not_supported() {
    let (_p, _g, core) = setup("tza-unsupported");
    // スカラー `TIMESTAMPTZ(p)` が未対応のため、精度付きの配列も従来どおり拒否する。
    assert_eq!(code(&core, "CREATE TABLE x (v TIMESTAMPTZ(3)[])"), "0A000");
}

#[test]
fn other_tenants_see_no_rows_and_values_survive_reopen() {
    let (path, _g, core) = setup("tza-tenant");
    assert_eq!(count(&core, "bob", "SELECT id FROM a LIMIT 10"), 0);
    drop(core);
    let core = core_at(&path);
    assert_eq!(
        arr(&core, "SELECT t FROM a WHERE id = 1 LIMIT 1"),
        V::TimestampTz(vec![Some(T0100), None, Some(T1000)])
    );
}
