//! 配列関数（`array_length`／`cardinality`／`array_append`／`array_prepend`／`array_cat`／
//! `array_position`／`array_remove`／`array_to_string`。対象ビヘイビア: TABLE-35。Issue #1604）の
//! 結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を
//! production 経路として検証する。各関数の NULL 入力・空配列に対する結果（PostgreSQL 17 と
//! 一致させる固定値）、型解決、配列列での利用（投影・WHERE・UPDATE）、上限・fail-closed な
//! 拒否、テナント境界を固定する。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue as V;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn select(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("{sql} failed: {e:?}"))
}

fn first_cell(core: &EngineCore, sql: &str) -> Cell {
    let r = select(core, "alice", sql);
    r.rows
        .first()
        .and_then(|row| row.cells.first())
        .cloned()
        .unwrap_or_else(|| panic!("{sql} returned no cell"))
}

fn arr(core: &EngineCore, sql: &str) -> V {
    match first_cell(core, sql) {
        Cell::Array(a) => a,
        other => panic!("{sql}: expected an array, got {other:?}"),
    }
}

fn ints(v: &[Option<i32>]) -> V {
    V::Integer(v.to_vec())
}

fn texts(v: &[Option<&str>]) -> V {
    V::Text(v.iter().map(|s| s.map(str::to_string)).collect())
}

fn ids(core: &EngineCore, tenant: &str, predicate: &str) -> Vec<u64> {
    let sql = format!("SELECT id FROM t WHERE {predicate} LIMIT 100");
    let mut v: Vec<u64> = select(core, tenant, &sql)
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    v.sort_unstable();
    v
}

const CREATE: &str =
    "CREATE TABLE t (embedding VECTOR(2) NOT NULL, tags TEXT[8], nums INTEGER[8], \
    big BIGINT[8], jb JSONB[8], js JSON[8], ip INTEGER)";

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, tags, nums, big, ip) VALUES \
         (1, '[0.1,0.2]', '{a,b,c}', '{10,20,30}', '{9007199254740993,5}', 2) \
         USING OPERATION_ID 'a1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, tags, nums, ip) VALUES \
         (2, '[0.1,0.2]', '{x,NULL,z}', '{5,NULL,7}', 3) USING OPERATION_ID 'a2'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, ip) VALUES (3, '[0.1,0.2]', 1) USING OPERATION_ID 'a3'",
    );
    ok(
        &core,
        "bob",
        "INSERT INTO t (id, embedding, tags, nums) VALUES \
         (4, '[0.1,0.2]', '{a,b,c}', '{10,20,30}') USING OPERATION_ID 'b1'",
    );
    (path, guard)
}

fn int(n: i64) -> Cell {
    Cell::SignedInteger(n)
}

#[test]
fn length_and_cardinality_follow_pg_null_and_empty_rules() {
    let (path, _g) = setup("afn-length");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e}"));
    assert_eq!(q("array_length(ARRAY[1,2,3], 1)"), int(3));
    assert_eq!(q("array_length(ARRAY[1,2,3], 2)"), Cell::Null);
    assert_eq!(q("array_length(ARRAY[1,2,3], 0)"), Cell::Null);
    assert_eq!(q("array_length(ARRAY[1,2,3], -1)"), Cell::Null);
    assert_eq!(q("array_length(ARRAY[1,2,3], NULL)"), Cell::Null);
    assert_eq!(q("array_length(ARRAY[]::int[], 1)"), Cell::Null);
    assert_eq!(q("array_length(ARRAY[NULL, NULL]::int[], 1)"), int(2));
    assert_eq!(q("cardinality(ARRAY[1,2,3])"), int(3));
    assert_eq!(q("cardinality(ARRAY[]::int[])"), int(0));
    assert_eq!(q("cardinality(NULL::int[])"), Cell::Null);
    assert_eq!(q("cardinality(ARRAY[NULL]::text[])"), int(1));
    // 列の NULL 配列・要素数。
    let c =
        |e: &str, id: u64| first_cell(&core, &format!("SELECT {e} FROM t WHERE id = {id} LIMIT 1"));
    assert_eq!(c("cardinality(nums)", 1), int(3));
    assert_eq!(c("cardinality(nums)", 3), Cell::Null);
    assert_eq!(c("array_length(tags, 1)", 2), int(3));
    assert_eq!(c("array_length(tags, 1)", 3), Cell::Null);
}

#[test]
fn append_and_prepend_handle_null_array_null_element_and_empty() {
    let (path, _g) = setup("afn-append");
    let core = core_at(&path);
    let q = |e: &str| arr(&core, &format!("SELECT {e}"));
    assert_eq!(
        q("array_append(ARRAY[1,2], 3)"),
        ints(&[Some(1), Some(2), Some(3)])
    );
    assert_eq!(
        q("array_prepend(0, ARRAY[1,2])"),
        ints(&[Some(0), Some(1), Some(2)])
    );
    assert_eq!(q("array_append(NULL::int[], 1)"), ints(&[Some(1)]));
    assert_eq!(q("array_append(NULL, 1)"), ints(&[Some(1)]));
    assert_eq!(q("array_prepend(1, NULL)"), ints(&[Some(1)]));
    assert_eq!(q("array_append(ARRAY[1], NULL)"), ints(&[Some(1), None]));
    assert_eq!(q("array_prepend(NULL, ARRAY[1])"), ints(&[None, Some(1)]));
    assert_eq!(q("array_append(ARRAY[]::int[], 1)"), ints(&[Some(1)]));
    assert_eq!(q("array_prepend(1, ARRAY[]::int[])"), ints(&[Some(1)]));
    assert_eq!(
        q("array_append(ARRAY['a'], 'b')"),
        texts(&[Some("a"), Some("b")])
    );
    // 型なし文字列リテラルは要素型の入力として解釈される。
    assert_eq!(q("array_append(ARRAY[1], '2')"), ints(&[Some(1), Some(2)]));
    // 共通型へ拡幅される。
    assert_eq!(
        q("array_append(ARRAY[1,2], 5000000000)"),
        V::BigInt(vec![Some(1), Some(2), Some(5_000_000_000)])
    );
}

#[test]
fn cat_follows_pg_null_and_empty_rules() {
    let (path, _g) = setup("afn-cat");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e}"));
    let a = |e: &str| arr(&core, &format!("SELECT {e}"));
    assert_eq!(
        a("array_cat(ARRAY[1], ARRAY[2,3])"),
        ints(&[Some(1), Some(2), Some(3)])
    );
    assert_eq!(a("array_cat(NULL, ARRAY[1])"), ints(&[Some(1)]));
    assert_eq!(a("array_cat(ARRAY[1], NULL)"), ints(&[Some(1)]));
    assert_eq!(a("array_cat(ARRAY[]::int[], ARRAY[]::int[])"), ints(&[]));
    assert_eq!(a("array_cat(ARRAY[]::int[], ARRAY[1])"), ints(&[Some(1)]));
    assert_eq!(a("array_cat(ARRAY[1], ARRAY[]::int[])"), ints(&[Some(1)]));
    assert_eq!(q("array_cat(NULL::int[], NULL::int[])"), Cell::Null);
    // 異なる整数幅は共通型へ。
    assert_eq!(
        a("array_cat(ARRAY[1], ARRAY[5000000000])"),
        V::BigInt(vec![Some(1), Some(5_000_000_000)])
    );
}

#[test]
fn position_follows_pg_null_and_start_rules() {
    let (path, _g) = setup("afn-position");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e}"));
    assert_eq!(q("array_position(ARRAY[1,2,3,2], 2)"), int(2));
    assert_eq!(q("array_position(ARRAY[1,2,3,2], 2, 3)"), int(4));
    assert_eq!(q("array_position(ARRAY[1,2,3,2], 2, -5)"), int(2));
    assert_eq!(q("array_position(ARRAY[1,2,3,2], 2, 5)"), Cell::Null);
    assert_eq!(q("array_position(ARRAY[1,2,3], 9)"), Cell::Null);
    assert_eq!(q("array_position(ARRAY[]::int[], 1)"), Cell::Null);
    assert_eq!(q("array_position(NULL::int[], 1)"), Cell::Null);
    assert_eq!(q("array_position(NULL, 1)"), Cell::Null);
    // NULL 要素の検索は IS NOT DISTINCT FROM。
    assert_eq!(q("array_position(ARRAY[1,NULL,3], NULL)"), int(2));
    assert_eq!(q("array_position(ARRAY[1,2,3], NULL)"), Cell::Null);
    assert_eq!(q("array_position(ARRAY['a','b'], 'b')"), int(2));
    assert_eq!(q("array_position(ARRAY['a','b'], 'B')"), Cell::Null);
    // 開始位置が NULL は 22004。配列が NULL なら先に NULL。
    let c = code(&core, "SELECT array_position(ARRAY[1,2], 2, NULL)");
    assert_eq!(c, "22004");
    assert_eq!(q("array_position(NULL::int[], 1, NULL)"), Cell::Null);
}

#[test]
fn remove_follows_pg_null_and_empty_rules() {
    let (path, _g) = setup("afn-remove");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e}"));
    let a = |e: &str| arr(&core, &format!("SELECT {e}"));
    assert_eq!(a("array_remove(ARRAY[1,2,1], 1)"), ints(&[Some(2)]));
    assert_eq!(a("array_remove(ARRAY[1,NULL,1], 1)"), ints(&[None]));
    assert_eq!(
        a("array_remove(ARRAY[1,NULL,2,NULL], NULL)"),
        ints(&[Some(1), Some(2)])
    );
    assert_eq!(a("array_remove(ARRAY[]::int[], 1)"), ints(&[]));
    assert_eq!(a("array_remove(ARRAY[1], 1)"), ints(&[]));
    assert_eq!(a("array_remove(ARRAY[1,2], 9)"), ints(&[Some(1), Some(2)]));
    assert_eq!(q("array_remove(NULL::int[], 1)"), Cell::Null);
    assert_eq!(q("array_remove(NULL, 1)"), Cell::Null);
}

#[test]
fn to_string_follows_pg_null_string_rules() {
    let (path, _g) = setup("afn-tostring");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e}"));
    let t = |s: &str| Cell::Text(s.to_string());
    assert_eq!(q("array_to_string(ARRAY['a','b','c'], ',')"), t("a,b,c"));
    assert_eq!(q("array_to_string(ARRAY['a',NULL,'b'], ',')"), t("a,b"));
    assert_eq!(
        q("array_to_string(ARRAY['a',NULL,'b'], ',', '*')"),
        t("a,*,b")
    );
    assert_eq!(
        q("array_to_string(ARRAY['a',NULL,'b'], ',', NULL)"),
        t("a,b")
    );
    assert_eq!(q("array_to_string(ARRAY[NULL]::text[], ',')"), t(""));
    assert_eq!(q("array_to_string(ARRAY[]::text[], ',')"), t(""));
    assert_eq!(q("array_to_string(ARRAY['a','b'], NULL)"), Cell::Null);
    assert_eq!(q("array_to_string(NULL::text[], ',')"), Cell::Null);
    assert_eq!(q("array_to_string(ARRAY[1,2,3], '-')"), t("1-2-3"));
    assert_eq!(q("array_to_string(ARRAY['a'], '')"), t("a"));
    assert_eq!(
        q("array_to_string(ARRAY['t'::boolean, 'f'::boolean], ',')"),
        t("t,f")
    );
    assert_eq!(
        q("array_to_string(ARRAY[TIMESTAMP '2026-01-02 03:04:05'], ',')"),
        t("2026-01-02 03:04:05")
    );
    assert_eq!(
        q("array_to_string(ARRAY['a b', 'NULL'], ';')"),
        t("a b;NULL")
    );
}

#[test]
fn type_resolution_and_errors() {
    let (path, _g) = setup("afn-types");
    let core = core_at(&path);
    // 配列でない引数・共通型の無い組は 42883。
    assert_eq!(code(&core, "SELECT cardinality(1)"), "42883");
    assert_eq!(code(&core, "SELECT array_length(1, 1)"), "42883");
    assert_eq!(
        code(&core, "SELECT array_append(ARRAY[1], 'a'::text)"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT array_cat(ARRAY[1], ARRAY['a'::text])"),
        "42883"
    );
    assert_eq!(code(&core, "SELECT array_to_string(ARRAY[1], 1)"), "42883");
    // 型を決められない多相引数は 42804。
    assert_eq!(code(&core, "SELECT cardinality(NULL)"), "42804");
    assert_eq!(code(&core, "SELECT array_length(NULL, 1)"), "42804");
    assert_eq!(code(&core, "SELECT array_append(NULL, NULL)"), "42804");
    assert_eq!(code(&core, "SELECT array_cat(NULL, NULL)"), "42804");
    assert_eq!(code(&core, "SELECT cardinality('{1,2}')"), "42804");
    // アリティ。
    assert_eq!(code(&core, "SELECT cardinality()"), "42883");
    assert_eq!(code(&core, "SELECT array_length(ARRAY[1])"), "42883");
    assert_eq!(code(&core, "SELECT array_position(ARRAY[1])"), "42883");
    assert_eq!(code(&core, "SELECT array_to_string(ARRAY[1])"), "42883");
    assert_eq!(code(&core, "SELECT array_remove(ARRAY[1], 1, 1)"), "42883");
    // 非整数の次元は未対応（PG は 42883。既知差分）。
    assert_eq!(code(&core, "SELECT array_length(ARRAY[1], 1.5)"), "0A000");
    // json 要素は等価演算子が無い。
    assert_eq!(
        code(&core, "SELECT array_position(js, '1') FROM t LIMIT 1"),
        "42883"
    );
}

#[test]
fn result_column_types() {
    let (path, _g) = setup("afn-meta");
    let core = core_at(&path);
    let r = select(
        &core,
        "alice",
        "SELECT cardinality(nums), array_append(nums, 1), array_to_string(tags, ','), \
         array_position(tags, 'a'), array_append(nums, 5000000000) FROM t WHERE id = 1 LIMIT 1",
    );
    let tys: Vec<Option<ColumnType>> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(tys[0], Some(ColumnType::Integer));
    assert!(matches!(tys[1], Some(ColumnType::Array(_))));
    assert_eq!(tys[2], Some(ColumnType::Text));
    assert_eq!(tys[3], Some(ColumnType::Integer));
    match &tys[4] {
        Some(ColumnType::Array(at)) => {
            assert_eq!(at.elem(), engine::catalog::ArrayElemType::BigInt)
        }
        other => panic!("expected bigint array, got {other:?}"),
    }
}

#[test]
fn array_columns_in_projection_where_and_integer_context() {
    let (path, _g) = setup("afn-columns");
    let core = core_at(&path);
    let c =
        |e: &str, id: u64| first_cell(&core, &format!("SELECT {e} FROM t WHERE id = {id} LIMIT 1"));
    assert_eq!(c("array_position(tags, 'b')", 1), int(2));
    assert_eq!(c("nums[array_length(nums, 1)]", 1), int(30));
    assert_eq!(c("array_length(nums, 1) - 1", 1), int(2));
    assert_eq!(
        c("CAST(cardinality(nums) AS text)", 1),
        Cell::Text("3".into())
    );
    assert_eq!(
        c("array_to_string(tags, '/')", 1),
        Cell::Text("a/b/c".into())
    );
    assert_eq!(
        c("array_to_string(tags, '/', '?')", 2),
        Cell::Text("x/?/z".into())
    );
    assert_eq!(c("array_to_string(tags, '/')", 3), Cell::Null);
    assert_eq!(
        arr(
            &core,
            "SELECT array_append(nums, ip) FROM t WHERE id = 1 LIMIT 1"
        ),
        ints(&[Some(10), Some(20), Some(30), Some(2)])
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_append(nums, 1) FROM t WHERE id = 3 LIMIT 1"
        ),
        ints(&[Some(1)])
    );
    assert_eq!(ids(&core, "alice", "cardinality(nums) > 2"), vec![1, 2]);
    assert_eq!(
        ids(&core, "alice", "array_position(tags, 'b') = 2"),
        vec![1]
    );
    assert_eq!(
        ids(&core, "alice", "array_position(tags, NULL) = 2"),
        vec![2]
    );
    // テナント境界: bob の行は alice から見えない。
    assert_eq!(ids(&core, "bob", "cardinality(nums) > 2"), vec![4]);
}

#[test]
fn bigint_elements_above_2_53_survive_without_f64_conversion() {
    let (path, _g) = setup("afn-bigint");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e} FROM t WHERE id = 1 LIMIT 1"));
    // 2^53 + 1 を含む配列でも、検索・除去・追加は要素を f64 へ変換せず（22003 にならず）扱う。
    // （検索値そのものは式評価の整数表現の範囲内に限られる既知の制約。）
    assert_eq!(q("array_position(big, 5)"), int(2));
    assert_eq!(q("array_position(big, 6)"), Cell::Null);
    assert_eq!(q("cardinality(big)"), int(2));
    assert_eq!(
        arr(
            &core,
            "SELECT array_remove(big, 5) FROM t WHERE id = 1 LIMIT 1"
        ),
        V::BigInt(vec![Some(9_007_199_254_740_993)])
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_append(big, 7) FROM t WHERE id = 1 LIMIT 1"
        ),
        V::BigInt(vec![Some(9_007_199_254_740_993), Some(5), Some(7)])
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_cat(big, big) FROM t WHERE id = 1 LIMIT 1"
        ),
        V::BigInt(vec![
            Some(9_007_199_254_740_993),
            Some(5),
            Some(9_007_199_254_740_993),
            Some(5)
        ])
    );
}

#[test]
fn update_set_with_array_functions_persists() {
    let path = unique_db_path("afn-update");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        ok(&core, "alice", CREATE);
        ok(
            &core,
            "alice",
            "INSERT INTO t (id, embedding, tags, nums, ip) VALUES \
             (1, '[0.1,0.2]', '{a,b}', '{1,2}', 1) USING OPERATION_ID 'i1'",
        );
        ok(
            &core,
            "alice",
            "UPDATE t SET tags = array_append(tags, 'z'), nums = array_remove(nums, 1) \
             WHERE id = 1 USING OPERATION_ID 'u1'",
        );
        ok(
            &core,
            "alice",
            "INSERT INTO t (id, embedding, nums, ip) VALUES \
             (2, '[0.1,0.2]', array_cat(ARRAY[1], ARRAY[2]), 1) USING OPERATION_ID 'i2'",
        );
    }
    let core = core_at(&path);
    assert_eq!(
        arr(&core, "SELECT tags FROM t WHERE id = 1 LIMIT 1"),
        texts(&[Some("a"), Some("b"), Some("z")])
    );
    assert_eq!(
        arr(&core, "SELECT nums FROM t WHERE id = 1 LIMIT 1"),
        ints(&[Some(2)])
    );
    assert_eq!(
        arr(&core, "SELECT nums FROM t WHERE id = 2 LIMIT 1"),
        ints(&[Some(1), Some(2)])
    );
}

#[test]
fn size_limits_fail_closed_with_54000() {
    let path = unique_db_path("afn-limits");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE w (embedding VECTOR(2) NOT NULL, a INTEGER[1024], s TEXT)",
    );
    let many = (0..1024)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    ok(
        &core,
        "alice",
        &format!(
            "INSERT INTO w (id, embedding, a) VALUES (1, '[0.1,0.2]', '{{{many}}}') \
             USING OPERATION_ID 'w1'"
        ),
    );
    assert_eq!(
        code(
            &core,
            "SELECT array_append(a, 1) FROM w WHERE id = 1 LIMIT 1"
        ),
        "54000"
    );
    assert_eq!(
        code(
            &core,
            "SELECT array_prepend(1, a) FROM w WHERE id = 1 LIMIT 1"
        ),
        "54000"
    );
    assert_eq!(
        code(&core, "SELECT array_cat(a, a) FROM w WHERE id = 1 LIMIT 1"),
        "54000"
    );
    // 1024 要素ちょうど（空配列との連結）は通る。
    assert_eq!(
        arr(
            &core,
            "SELECT array_cat(a, ARRAY[]::int[]) FROM w WHERE id = 1 LIMIT 1"
        ),
        V::Integer((0..1024).map(Some).collect())
    );
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (path, _g) = setup("afn-closed");
    let core = core_at(&path);
    assert_eq!(
        code(
            &core,
            "CREATE TABLE c (embedding VECTOR(2) NOT NULL, a INTEGER[3], CHECK (cardinality(a) > 0))"
        ),
        "0A000"
    );
    assert_eq!(
        code(&core, "CREATE FUNCTION f(x) AS cardinality(x)"),
        "0A000"
    );
}
