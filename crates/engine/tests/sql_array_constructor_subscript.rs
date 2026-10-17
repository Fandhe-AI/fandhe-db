//! 配列コンストラクタ `ARRAY[...]` と添字 `arr[i]`／スライス `arr[i:j]`（対象ビヘイビア: TABLE-35。
//! Issue #1602）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価 → 行コーデック）
//! を production 経路として検証する。要素型の統一、添字・スライスの範囲外／NULL の規則、
//! `INSERT ... VALUES`／`UPDATE ... SET`／`WHERE` での利用、未対応形の fail-closed な拒否、
//! 再オープン後の復元、テナント境界を固定する。

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

/// 失敗を前提にした SQL の `wire_code`。成功してしまった場合は SQL を添えて落とす。
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

/// 先頭行・先頭列のセル。
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
    big BIGINT[8], dbl DOUBLE PRECISION[8], flags BOOLEAN[8], ds DATE[8], tss TIMESTAMP[8], \
    us UUID[8], bs BYTEA[8], jb JSONB[8], ip INTEGER, nn INTEGER[8] NOT NULL DEFAULT '{}')";

fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, tags, nums, ip) VALUES \
         (1, '[0.1,0.2]', '{a,b,c}', '{10,20,30}', 2) USING OPERATION_ID 'a1'",
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

#[test]
fn constructor_unifies_element_types() {
    let (path, _g) = setup("arr-ctor-types");
    let core = core_at(&path);
    assert_eq!(arr(&core, "SELECT ARRAY[1, 2]"), ints(&[Some(1), Some(2)]));
    assert_eq!(
        arr(&core, "SELECT ARRAY[1, 9999999999]"),
        V::BigInt(vec![Some(1), Some(9_999_999_999)])
    );
    assert_eq!(
        arr(&core, "SELECT ARRAY[1, 2.5::float8]"),
        V::Double(vec![Some(1.0), Some(2.5)])
    );
    assert_eq!(
        arr(&core, "SELECT ARRAY['a', 'b']"),
        V::Text(vec![Some("a".into()), Some("b".into())])
    );
    assert_eq!(arr(&core, "SELECT ARRAY[NULL]"), V::Text(vec![None]));
    assert_eq!(arr(&core, "SELECT ARRAY[1, NULL]"), ints(&[Some(1), None]));
    assert_eq!(
        arr(&core, "SELECT ARRAY[1, '2']"),
        ints(&[Some(1), Some(2)])
    );
    assert_eq!(
        arr(&core, "SELECT ARRAY['t'::boolean, 'f'::boolean, NULL]"),
        V::Bool(vec![Some(true), Some(false), None])
    );
    assert_eq!(arr(&core, "SELECT ARRAY[]::int[]"), ints(&[]));
    assert_eq!(
        arr(&core, "SELECT CAST(ARRAY[] AS text[])"),
        V::Text(vec![])
    );
    // 列参照・式の要素。
    assert_eq!(
        arr(
            &core,
            "SELECT ARRAY[ip, ip + 1, 7] FROM t WHERE id = 1 LIMIT 10"
        ),
        ints(&[Some(2), Some(3), Some(7)])
    );
}

#[test]
fn constructor_rejects_unmatched_and_unsupported_forms() {
    let (path, _g) = setup("arr-ctor-errors");
    let core = core_at(&path);
    assert_eq!(code(&core, "SELECT ARRAY[1, 'x']"), "22P02");
    assert_eq!(code(&core, "SELECT ARRAY[1, 'a'::text]"), "42804");
    assert_eq!(code(&core, "SELECT ARRAY[]"), "42804");
    assert_eq!(code(&core, "SELECT ARRAY[1.5]"), "0A000");
    assert_eq!(code(&core, "SELECT ARRAY[ARRAY[1]]"), "0A000");
    assert_eq!(code(&core, "SELECT ARRAY[[1]]"), "0A000");
    assert_eq!(code(&core, "SELECT ARRAY(SELECT 1)"), "0A000");
    assert_eq!(code(&core, "SELECT ARRAY[1,]"), "42601");
    let many = (0..1025)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(code(&core, &format!("SELECT ARRAY[{many}]")), "54000");
}

#[test]
fn subscript_and_slice_follow_pg_rules() {
    let (path, _g) = setup("arr-subscript");
    let core = core_at(&path);
    let q = |e: &str| first_cell(&core, &format!("SELECT {e} FROM t WHERE id = 1 LIMIT 10"));
    assert_eq!(q("tags[1]"), Cell::Text("a".into()));
    assert_eq!(q("tags[3]"), Cell::Text("c".into()));
    assert_eq!(q("tags[0]"), Cell::Null);
    assert_eq!(q("tags[4]"), Cell::Null);
    assert_eq!(q("tags[-1]"), Cell::Null);
    assert_eq!(q("tags[NULL]"), Cell::Null);
    assert_eq!(q("tags['2']"), Cell::Text("b".into()));
    assert_eq!(q("nums[ip]"), Cell::SignedInteger(20));
    assert_eq!(q("nums[1] / 4"), Cell::SignedInteger(2));
    assert_eq!(q("(ARRAY[10,20,30])[2]"), Cell::SignedInteger(20));
    assert_eq!(q("nums[2]::text"), Cell::Text("20".into()));
    assert_eq!(q("CAST(nums[2] AS text)"), Cell::Text("20".into()));
    // スライス。
    let sl = |e: &str| arr(&core, &format!("SELECT {e} FROM t WHERE id = 1 LIMIT 10"));
    assert_eq!(sl("nums[2:3]"), ints(&[Some(20), Some(30)]));
    assert_eq!(sl("nums[:2]"), ints(&[Some(10), Some(20)]));
    assert_eq!(sl("nums[2:]"), ints(&[Some(20), Some(30)]));
    assert_eq!(sl("nums[:]"), ints(&[Some(10), Some(20), Some(30)]));
    assert_eq!(sl("nums[0:10]"), ints(&[Some(10), Some(20), Some(30)]));
    assert_eq!(sl("nums[5:10]"), ints(&[]));
    assert_eq!(sl("nums[3:2]"), ints(&[]));
    assert_eq!(q("nums[NULL:2]"), Cell::Null);
    assert_eq!(q("(nums[2:3])[2]"), Cell::SignedInteger(30));
    // NULL 要素・NULL 列。
    let null_elem = first_cell(&core, "SELECT tags[2] FROM t WHERE id = 2 LIMIT 10");
    assert_eq!(null_elem, Cell::Null);
    assert_eq!(
        first_cell(&core, "SELECT tags[1] FROM t WHERE id = 3 LIMIT 10"),
        Cell::Null
    );
}

#[test]
fn subscript_rejects_invalid_bases_and_indexes() {
    let (path, _g) = setup("arr-subscript-errors");
    let core = core_at(&path);
    assert_eq!(
        code(&core, "SELECT tags['t'::boolean] FROM t LIMIT 10"),
        "42804"
    );
    assert_eq!(code(&core, "SELECT tags[1.5] FROM t LIMIT 10"), "0A000");
    assert_eq!(code(&core, "SELECT ip[1] FROM t LIMIT 10"), "42804");
    assert_eq!(code(&core, "SELECT ARRAY[1,2][1]"), "42601");
    assert_eq!(code(&core, "SELECT tags[1][1] FROM t LIMIT 10"), "0A000");
    assert_eq!(code(&core, "SELECT tags[1:2][1] FROM t LIMIT 10"), "0A000");
    assert_eq!(code(&core, "SELECT tags[:] FROM t WHERE"), "42601");
    assert_eq!(code(&core, "SELECT tags[1 FROM t LIMIT 10"), "42601");
}

#[test]
fn result_column_names_and_types() {
    let (path, _g) = setup("arr-meta");
    let core = core_at(&path);
    let r = select(
        &core,
        "alice",
        "SELECT tags[1], nums[1], ARRAY[ip] , nums[1:2] FROM t WHERE id = 1 LIMIT 10",
    );
    let names: Vec<String> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } => name.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["tags", "nums", "array", "nums"]);
    let tys: Vec<Option<ColumnType>> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(tys[0], Some(ColumnType::Text));
    assert_eq!(tys[1], Some(ColumnType::Integer));
    assert!(matches!(tys[2], Some(ColumnType::Array(_))));
}
#[test]
fn insert_values_stores_constructor_and_survives_reopen() {
    let path = unique_db_path("arr-values");
    let _g = CleanupGuard(path.clone());
    {
        let core = core_at(&path);
        ok(&core, "alice", CREATE);
        ok(
            &core,
            "alice",
            "INSERT INTO t (id, embedding, tags, nums, big, dbl, flags, ds, tss, ip) VALUES \
             (1, '[0.1,0.2]', ARRAY['a', 'b c', 'NULL', 'x,y', 'q\"r', '', NULL], \
              ARRAY[1, 2, NULL], ARRAY[1, 9999999999], ARRAY[1, 2], \
              '{t,NULL}', ARRAY[DATE '2026-01-02', NULL], \
              ARRAY[TIMESTAMP '2026-01-02 03:04:05'], 1 + 1) USING OPERATION_ID 'v1'",
        );
    }
    let core = core_at(&path);
    let r = select(
        &core,
        "alice",
        "SELECT tags, nums, big, dbl, flags, ds, tss, ip FROM t WHERE id = 1 LIMIT 1",
    );
    let cells = &r.rows[0].cells;
    let get = |i: usize| match &cells[i] {
        Cell::Array(a) => a.clone(),
        other => panic!("expected array at {i}, got {other:?}"),
    };
    assert_eq!(
        get(0),
        V::Text(vec![
            Some("a".into()),
            Some("b c".into()),
            Some("NULL".into()),
            Some("x,y".into()),
            Some("q\"r".into()),
            Some("".into()),
            None
        ])
    );
    assert_eq!(get(1), ints(&[Some(1), Some(2), None]));
    assert_eq!(get(2), V::BigInt(vec![Some(1), Some(9_999_999_999)]));
    assert_eq!(get(3), V::Double(vec![Some(1.0), Some(2.0)]));
    assert_eq!(get(4), V::Bool(vec![Some(true), None]));
    assert_eq!(get(5), V::Date(vec![Some(20_455), None]));
    assert!(matches!(get(6), V::Timestamp(ref v) if v.len() == 1));
    // 列の最大要素数（宣言 8）を超える配列は拒否する。
    let many = (0..9).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "INSERT INTO t (id, embedding, nums, ip) VALUES (9, '[0.1,0.2]', ARRAY[{many}], 1) USING OPERATION_ID 'v9'"
    );
    assert_eq!(code(&core, &sql), "54000");
    // 列参照を含む要素は定数式ではない。
    let sql = "INSERT INTO t (id, embedding, nums, ip) VALUES (9, '[0.1,0.2]', ARRAY[ip], 1) USING OPERATION_ID 'v8'";
    assert_eq!(code(&core, sql), "42703");
}
#[test]
fn update_set_assigns_array_expressions() {
    let (path, _g) = setup("arr-set");
    let core = core_at(&path);
    let nums = |id: u64| {
        let sql = format!("SELECT nums FROM t WHERE id = {id} LIMIT 1");
        arr(&core, &sql)
    };
    ok(
        &core,
        "alice",
        "UPDATE t SET nums = ARRAY[7, 8, 9] WHERE id = 3 USING OPERATION_ID 'u1'",
    );
    assert_eq!(nums(3), ints(&[Some(7), Some(8), Some(9)]));
    // 更新前の行値を参照するスライス。
    ok(
        &core,
        "alice",
        "UPDATE t SET nums = nums[2:3] WHERE id = 1 USING OPERATION_ID 'u2'",
    );
    assert_eq!(nums(1), ints(&[Some(20), Some(30)]));
    // 要素型の変換（int4 → bigint 列、int4 → text 列）。
    ok(
        &core,
        "alice",
        "UPDATE t SET big = ARRAY[1, 2], tags = ARRAY[1, 2] WHERE id = 3 USING OPERATION_ID 'u3'",
    );
    assert_eq!(
        arr(&core, "SELECT big FROM t WHERE id = 3 LIMIT 1"),
        V::BigInt(vec![Some(1), Some(2)])
    );
    assert_eq!(
        arr(&core, "SELECT tags FROM t WHERE id = 3 LIMIT 1"),
        V::Text(vec![Some("1".into()), Some("2".into())])
    );
    // 空配列。
    ok(
        &core,
        "alice",
        "UPDATE t SET nums = ARRAY[]::int[] WHERE id = 3 USING OPERATION_ID 'u4'",
    );
    assert_eq!(nums(3), ints(&[]));
    // 型が合わない代入・上限超過・NOT NULL。
    let upd =
        |set: &str, op: &str| format!("UPDATE t SET {set} WHERE id = 3 USING OPERATION_ID '{op}'");
    assert_eq!(code(&core, &upd("nums = ARRAY['a']", "e1")), "42804");
    assert_eq!(code(&core, &upd("flags = ARRAY[1]", "e2")), "42804");
    let many = (0..9).map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    assert_eq!(
        code(&core, &upd(&format!("nums = ARRAY[{many}]"), "e4")),
        "54000"
    );
    // 添字への代入は未対応。
    let sql = upd("nums[1] = 5", "e5");
    assert!(matches!(code(&core, &sql), "42601" | "0A000"));
    // 他テナントの行は更新されない。
    ok(
        &core,
        "alice",
        "UPDATE t SET nums = ARRAY[0] WHERE id = 4 USING OPERATION_ID 'u9'",
    );
    assert_eq!(
        select(&core, "bob", "SELECT nums FROM t WHERE id = 4 LIMIT 1").rows[0].cells[0],
        Cell::Array(ints(&[Some(10), Some(20), Some(30)]))
    );
}

#[test]
fn where_uses_subscripts_and_array_equality() {
    let (path, _g) = setup("arr-where");
    let core = core_at(&path);
    assert_eq!(ids(&core, "alice", "tags[1] = 'a'"), vec![1]);
    assert_eq!(ids(&core, "alice", "nums[2] > 10"), vec![1]);
    assert_eq!(
        ids(&core, "alice", "nums[1] > 4 AND tags[3] = 'z'"),
        vec![2]
    );
    assert_eq!(ids(&core, "alice", "NOT tags[1] = 'a'"), vec![2]);
    assert_eq!(
        ids(&core, "alice", "tags[1] = 'a' OR tags[1] = 'x'"),
        vec![1, 2]
    );
    // 他テナント（id 4）は結果にも現れない。
    assert_eq!(ids(&core, "bob", "tags[1] = 'a'"), vec![4]);
    // 二次索引のある列でも添字の述語は全走査で評価される。
    ok(&core, "alice", "CREATE INDEX t_ip ON t (ip)");
    assert_eq!(ids(&core, "alice", "ip = 2 AND tags[1] = 'a'"), vec![1]);
}
#[test]
fn placeholders_in_constructor_and_subscript() {
    let (path, _g) = setup("arr-params");
    let core = core_at(&path);
    // 兄弟要素の型から推論できる位置（`ARRAY[$1, 2]` は int4）。
    let p = core
        .parse_sql_prepared("UPDATE t SET nums = ARRAY[$1, 2] WHERE id = 3 USING OPERATION_ID $2")
        .expect("parse");
    let binds = vec![Some(b"41".to_vec()), Some(b"p1".to_vec())];
    let parsed = core.bind_prepared(&p, &binds).expect("bind");
    let mut s = SessionState::default();
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &parsed)
        .expect("exec");
    assert_eq!(
        arr(&core, "SELECT nums FROM t WHERE id = 3 LIMIT 1"),
        ints(&[Some(41), Some(2)])
    );
    // 添字の `$n` は int4。
    let p = core
        .parse_sql_prepared("UPDATE t SET ip = nums[$1] WHERE id = 3 USING OPERATION_ID $2")
        .expect("parse");
    let binds = vec![Some(b"2".to_vec()), Some(b"p2".to_vec())];
    let parsed = core.bind_prepared(&p, &binds).expect("bind");
    core.execute_parsed_in_session(&ctx("alice"), &mut s, &parsed)
        .expect("exec");
    assert_eq!(
        first_cell(&core, "SELECT ip FROM t WHERE id = 3 LIMIT 1"),
        Cell::SignedInteger(2)
    );
    // 型を導けない `$n` だけの要素は不定（42P18）。
    let err = core
        .parse_sql_prepared("UPDATE t SET nums = ARRAY[$1] WHERE id = 3 USING OPERATION_ID $2")
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.wire_code(), "42P18");
}

#[test]
fn column_default_accepts_a_constructor() {
    let path = unique_db_path("arr-default");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE d (embedding VECTOR(2) NOT NULL, a INTEGER[4] DEFAULT ARRAY[1, 2])",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO d (id, embedding) VALUES (1, '[0.1,0.2]') USING OPERATION_ID 'd1'",
    );
    assert_eq!(
        arr(&core, "SELECT a FROM d WHERE id = 1 LIMIT 1"),
        ints(&[Some(1), Some(2)])
    );
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (path, _g) = setup("arr-closed");
    let core = core_at(&path);
    // CHECK 本体・UDF 本体・配列の演算は受理しない。
    assert_eq!(
        code(
            &core,
            "CREATE TABLE c (embedding VECTOR(2) NOT NULL, a INTEGER[3], CHECK (a[1] > 0))"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "CREATE TABLE c (embedding VECTOR(2) NOT NULL, a INTEGER[3], CHECK (a[1:2] = a))"
        ),
        "0A000"
    );
    assert_eq!(code(&core, "SELECT 1 + ARRAY[1] LIMIT 1"), "0A000");
    assert_eq!(code(&core, "SELECT a:b FROM t LIMIT 1"), "42601");
    assert_eq!(code(&core, "SELECT ARRAY[1,2][1]"), "42601");
    // WHERE の配列の等価述語は従来どおりリテラルのみ（式との比較は後続 Issue #1603）。
    assert!(!matches!(
        code(
            &core,
            "SELECT id FROM t WHERE nums = ARRAY[10, 20, 30] LIMIT 5"
        ),
        "00000"
    ));
}

#[test]
fn typecast_and_array_type_names_still_parse() {
    let (path, _g) = setup("arr-regress");
    let core = core_at(&path);
    assert_eq!(
        arr(&core, "SELECT '{1,2}'::int[]"),
        ints(&[Some(1), Some(2)])
    );
    assert_eq!(
        arr(&core, "SELECT CAST('{a,b}' AS text[])"),
        V::Text(vec![Some("a".into()), Some("b".into())])
    );
    assert_eq!(
        arr(&core, "SELECT ARRAY[1, 2]::bigint[]"),
        V::BigInt(vec![Some(1), Some(2)])
    );
    assert_eq!(
        first_cell(&core, "SELECT (ARRAY[1, 2]::bigint[])[2]"),
        Cell::SignedInteger(2)
    );
    // 引用された "array" は列参照のまま（未定義列）。
    assert_eq!(code(&core, "SELECT \"array\" FROM t LIMIT 1"), "42703");
}
/// 入れ子の `ARRAY[(ARRAY[...])[1]]`・`a[(a[...])]` の束縛が入れ子の深さに対して線形に収まること
/// （分類のための束縛を変換時に繰り返さない）。指数的に重複すると終わらないため時間で固定する。
#[test]
fn deeply_nested_constructors_and_subscripts_bind_in_linear_time() {
    let (path, _g) = setup("arr-nesting");
    let core = core_at(&path);
    let started = std::time::Instant::now();
    for depth in [8usize, 16, 24, 32, 48] {
        let mut ctor = "1".to_string();
        let mut index = "1".to_string();
        for _ in 0..depth {
            ctor = format!("(ARRAY[{ctor}, 2])[1]");
            index = format!("nums[{index}]");
        }
        for sql in [
            format!("SELECT {ctor} FROM t WHERE id = 1 LIMIT 1"),
            format!("SELECT {index} FROM t WHERE id = 1 LIMIT 1"),
        ] {
            match run(&core, "alice", &sql) {
                Ok(_) => {}
                Err(e) => assert!(
                    matches!(e.wire_code(), "54000" | "42601" | "0A000"),
                    "depth {depth}: unexpected {e:?}"
                ),
            }
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "nested array expressions took {:?}",
        started.elapsed()
    );
}
