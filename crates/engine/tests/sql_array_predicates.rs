//! 配列の `ANY`／`ALL`・配列どうしの比較・包含 `@>`／`<@`・重なり `&&`（対象ビヘイビア: TABLE-35。
//! Issue #1603）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を
//! production 経路として検証する。三値論理（NULL 要素・NULL 配列・空配列・`NOT` 越し）、
//! 要素型ごとの比較、未対応形の fail-closed な拒否、テナント境界（SELECT／UPDATE／DELETE）を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
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

/// alice から見て述語に一致する行の id（昇順）。
fn matching(core: &EngineCore, predicate: &str) -> Vec<u64> {
    ids(core, "alice", predicate)
}

const CREATE: &str =
    "CREATE TABLE t (embedding VECTOR(2) NOT NULL, tags TEXT[8], nums INTEGER[8], \
    big BIGINT[8], dbl DOUBLE PRECISION[8], flags BOOLEAN[8], ds DATE[8], tss TIMESTAMP[8], \
    us UUID[8], bs BYTEA[8], jb JSONB[8], ip INTEGER, nn INTEGER[8] NOT NULL DEFAULT '{}')";

/// alice: id1 `nums={10,20,30}`・id2 `nums={5,NULL,7}`・id3 `nums=NULL`、bob: id4（id1 と同じ配列）。
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

const NONE: Vec<u64> = Vec::new();

/// TABLE-35・Issue #1603: 量化比較の三値論理（NULL 要素・NULL 配列・空配列・`NOT` 越し）。
#[test]
fn quantified_any_all_follow_three_valued_logic() {
    let (path, _g) = setup("arr-pred-quant");
    let core = core_at(&path);
    assert_eq!(matching(&core, "10 = ANY(nums)"), vec![1]);
    assert_eq!(matching(&core, "10 = SOME(nums)"), vec![1]);
    assert_eq!(matching(&core, "5 = ANY(nums)"), vec![2]);
    // 真になる要素が無く NULL 要素がある行（id2）は NULL。偽ではないので NOT しても出ない。
    assert_eq!(matching(&core, "NOT 10 = ANY(nums)"), NONE);
    assert_eq!(matching(&core, "10 <> ALL(nums)"), NONE);
    assert_eq!(matching(&core, "NOT 5 = ANY(nums)"), vec![1]);
    assert_eq!(matching(&core, "5 <> ALL(nums)"), vec![1]);
    assert_eq!(matching(&core, "NOT 5 <> ALL(nums)"), vec![2]);
    // 列を左辺にした比較。ip: id1=2, id2=3, id3=1。
    assert_eq!(matching(&core, "ip < ANY(nums)"), vec![1, 2]);
    assert_eq!(matching(&core, "ip >= ALL(nums)"), NONE);
    assert_eq!(matching(&core, "ip <= ALL(nums)"), vec![1]);
    // 配列式・リテラル・スライス。他テナント（id4）は出ない。
    assert_eq!(matching(&core, "id = ANY(ARRAY[1, 3])"), vec![1, 3]);
    assert_eq!(matching(&core, "id <> ALL(ARRAY[1, 3])"), vec![2]);
    assert_eq!(matching(&core, "NOT id = ANY(ARRAY[1, 3])"), vec![2]);
    assert_eq!(matching(&core, "ip = ANY('{2,3}')"), vec![1, 2]);
    assert_eq!(matching(&core, "10 = ANY(nums[1:1])"), vec![1]);
    assert_eq!(matching(&core, "10 = ANY(nums[2:3])"), NONE);
    assert_eq!(matching(&core, "'a' = ANY(tags)"), vec![1]);
    assert_eq!(matching(&core, "'zz' = ANY(tags)"), NONE);
    assert_eq!(matching(&core, "NOT 'a' = ANY(tags)"), NONE);
    assert_eq!(matching(&core, "'x' = ANY(tags) AND ip = 3"), vec![2]);
    assert_eq!(matching(&core, "ip = 1 OR 10 = ANY(nums)"), vec![1, 3]);
    // 空配列: ANY は偽、ALL は真（NOT しても同じ規則）。
    assert_eq!(matching(&core, "1 = ANY(nn)"), NONE);
    assert_eq!(matching(&core, "1 = ALL(nn)"), vec![1, 2, 3]);
    assert_eq!(matching(&core, "NOT 1 = ANY(nn)"), vec![1, 2, 3]);
    assert_eq!(matching(&core, "NOT 1 = ALL(nn)"), NONE);
    // 配列が NULL の行（id3）は NULL。
    assert_eq!(matching(&core, "ip = ANY(nums)"), NONE);
}

/// TABLE-35・Issue #1603: 配列どうしの比較（順序・NULL 要素・宣言的レーンとのパリティ）。
#[test]
fn array_comparisons_follow_postgres_ordering() {
    let (path, _g) = setup("arr-pred-cmp");
    let core = core_at(&path);
    assert_eq!(matching(&core, "nums = ARRAY[10, 20, 30]"), vec![1]);
    // NULL 要素どうしは等しい。宣言的レーン（文字列リテラル）と式レーンで同じ行を返す。
    assert_eq!(matching(&core, "nums = '{5,NULL,7}'"), vec![2]);
    assert_eq!(matching(&core, "nums = ARRAY[5, NULL, 7]"), vec![2]);
    assert_eq!(matching(&core, "'{5,NULL,7}' = nums"), vec![2]);
    assert_eq!(matching(&core, "nums <> ARRAY[10, 20, 30]"), vec![2]);
    assert_eq!(matching(&core, "nums != ARRAY[10, 20, 30]"), vec![2]);
    assert_eq!(matching(&core, "NOT nums = ARRAY[10, 20, 30]"), vec![2]);
    assert_eq!(matching(&core, "NOT nums <> ARRAY[10, 20, 30]"), vec![1]);
    // 順序: 先頭から比較し、短い方が小さく、NULL 要素は最大。
    assert_eq!(matching(&core, "nums < ARRAY[10, 20, 31]"), vec![1, 2]);
    assert_eq!(matching(&core, "nums > ARRAY[10, 20]"), vec![1]);
    assert_eq!(matching(&core, "nums >= ARRAY[5, NULL, 7]"), vec![1, 2]);
    assert_eq!(matching(&core, "nums <= ARRAY[5, 9]"), NONE);
    assert_eq!(matching(&core, "nums > ARRAY[5, 9, 9]"), vec![1, 2]);
    assert_eq!(matching(&core, "NOT nums < ARRAY[10, 20, 31]"), NONE);
    assert_eq!(matching(&core, "tags = ARRAY['a', 'b', 'c']"), vec![1]);
    // 配列が NULL の行（id3）はどの比較でも出ない。
    assert_eq!(matching(&core, "nums <> ARRAY[1] AND id = 3"), NONE);
    assert_eq!(matching(&core, "NOT nums = ARRAY[1] AND id = 3"), NONE);
    // `<>` の脱糖の副作用: スカラーの `<>` も受理される。
    assert_eq!(matching(&core, "id + 1 <> 3"), vec![1, 3]);
    assert_eq!(matching(&core, "id <> 2"), vec![1, 3]);
}

/// TABLE-35・Issue #1603: `@>`・`<@`・`&&`（重複・空配列・NULL 要素・投影位置）。
#[test]
fn containment_and_overlap_operators() {
    let (path, _g) = setup("arr-pred-contain");
    let core = core_at(&path);
    assert_eq!(matching(&core, "nums @> ARRAY[10, 30]"), vec![1]);
    assert_eq!(matching(&core, "nums @> ARRAY[30, 10, 10]"), vec![1]);
    assert_eq!(matching(&core, "nums @> ARRAY[10, 99]"), NONE);
    assert_eq!(matching(&core, "nums @> '{}'"), vec![1, 2]);
    assert_eq!(matching(&core, "nums @> ARRAY[5]"), vec![2]);
    // NULL 要素はどの要素とも一致しない。
    assert_eq!(matching(&core, "nums @> ARRAY[NULL]::int[]"), NONE);
    assert_eq!(matching(&core, "nums <@ ARRAY[10, 20, 30, 40]"), vec![1]);
    assert_eq!(matching(&core, "ARRAY[10] <@ nums"), vec![1]);
    assert_eq!(matching(&core, "'{10}' <@ nums"), vec![1]);
    assert_eq!(matching(&core, "nums && ARRAY[30, 99]"), vec![1]);
    assert_eq!(matching(&core, "nums && ARRAY[7]"), vec![2]);
    assert_eq!(matching(&core, "nums && '{}'"), NONE);
    assert_eq!(matching(&core, "nums && ARRAY[NULL]::int[]"), NONE);
    assert_eq!(matching(&core, "tags && ARRAY['z']"), vec![2]);
    assert_eq!(
        matching(&core, "tags @> ARRAY['a'] AND nums && ARRAY[20]"),
        vec![1]
    );
    assert_eq!(
        matching(&core, "nums && ARRAY[20] OR nums && ARRAY[7]"),
        vec![1, 2]
    );
    // 投影位置でも bool を返す（配列が NULL なら NULL）。
    assert_eq!(
        first_cell(
            &core,
            "SELECT nums @> ARRAY[10] FROM t WHERE id = 1 LIMIT 5"
        ),
        Cell::Bool(true)
    );
    assert_eq!(
        first_cell(
            &core,
            "SELECT nums && ARRAY[99] FROM t WHERE id = 1 LIMIT 5"
        ),
        Cell::Bool(false)
    );
    assert_eq!(
        first_cell(
            &core,
            "SELECT nums @> ARRAY[10] FROM t WHERE id = 3 LIMIT 5"
        ),
        Cell::Null
    );
}

/// TABLE-35・Issue #1603: 要素型ごとの比較（bigint の正確な比較・各要素型の代表・型不一致の拒否）。
#[test]
fn element_types_are_compared_exactly() {
    let (path, _g) = setup("arr-pred-types");
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, embedding, big, dbl) VALUES \
         (6, '[0.1,0.2]', '{9007199254740993,1}', '{1.5,2.5}') USING OPERATION_ID 'a6'",
    );
    // bigint は f64 に落とさず正確に比較する。
    assert_eq!(matching(&core, "big @> '{9007199254740993}'"), vec![6]);
    assert_eq!(matching(&core, "big @> '{9007199254740992}'"), NONE);
    assert_eq!(matching(&core, "big && '{9007199254740992}'"), NONE);
    assert_eq!(matching(&core, "dbl @> '{2.5}'"), vec![6]);
    assert_eq!(matching(&core, "dbl > ARRAY[1.5::float8]"), vec![6]);
    assert_eq!(matching(&core, "2.5 = ANY(dbl)"), vec![6]);
    // 他の要素型（定数側で検証する。全行で真）。
    let all = vec![1, 2, 3, 6];
    for p in [
        "'2024-01-02'::date = ANY(ARRAY['2024-01-01'::date, '2024-01-02'::date])",
        "ARRAY['2024-01-01 10:00:00'::timestamp] = ARRAY['2024-01-01 10:00:00'::timestamp]",
        "'11111111-1111-1111-1111-111111111111'::uuid = ANY(ARRAY['11111111-1111-1111-1111-111111111111'::uuid])",
        "ARRAY['\\x01'::bytea] @> ARRAY['\\x01'::bytea]",
        "ARRAY['t'::boolean, 'f'::boolean] = ARRAY['t'::boolean, 'f'::boolean]",
        "ARRAY['f'::boolean] < ARRAY['t'::boolean]",
        "ARRAY['{\"a\":1}'::jsonb] @> ARRAY['{\"a\":1}'::jsonb]",
        "'{\"a\":1}'::jsonb = ANY(ARRAY['{\"a\":1}'::jsonb])",
    ] {
        assert_eq!(matching(&core, p), all, "{p}");
    }
    assert_eq!(
        matching(&core, "ARRAY['t'::boolean] = ARRAY['f'::boolean]"),
        NONE
    );
    // 型が合わない組は 42883、配列でない右辺は 42804、非対応の要素型は 0A000。
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nums = tags LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nums @> tags LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nums && big LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nums = 1 LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE ip && ip LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE ip = ANY(tags) LIMIT 5"),
        "42883"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE ip = ANY(5) LIMIT 5"),
        "42804"
    );
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE ARRAY['{}'::json] = ARRAY['{}'::json] LIMIT 5"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE ARRAY['{}'::jsonb] < ARRAY['{}'::jsonb] LIMIT 5"
        ),
        "0A000"
    );
}

/// TABLE-35・Issue #1603: jsonb 要素は値として等価比較し（`1` と `1.0` は等しい）、`<>`／`NOT =` は
/// 順序比較へ脱糖せず等値の否定で評価する。smallint の左辺も型なしリテラル配列と比較できる。
#[test]
fn jsonb_elements_use_value_equality_and_inequality_needs_no_ordering() {
    let (path, _g) = setup("arr-pred-jsonb-eq");
    let core = core_at(&path);
    let all = vec![1, 2, 3];
    for p in [
        "ARRAY['1'::jsonb] = ARRAY['1.0'::jsonb]",
        "ARRAY['1'::jsonb] @> ARRAY['1.0'::jsonb]",
        "ARRAY['1.0'::jsonb] <@ ARRAY['1'::jsonb, '2'::jsonb]",
        "ARRAY['1'::jsonb] && ARRAY['1.0'::jsonb]",
        "'1'::jsonb = ANY(ARRAY['1.0'::jsonb])",
        "'1'::jsonb = ALL(ARRAY['1.0'::jsonb])",
        "ARRAY['1'::jsonb] <> ARRAY['2'::jsonb]",
        "ARRAY['1'::jsonb] != ARRAY['2'::jsonb]",
        "NOT ARRAY['1'::jsonb] = ARRAY['2'::jsonb]",
        "NOT ARRAY['1'::jsonb] <> ARRAY['1.0'::jsonb]",
        "NOT (ARRAY['1'::jsonb] <> ARRAY['1.0'::jsonb])",
        "NOT (ARRAY['1'::jsonb] = ARRAY['2'::jsonb])",
        "CAST(1 AS smallint) = ANY('{1,2}')",
        "CAST(3 AS smallint) <> ALL('{1,2}')",
    ] {
        assert_eq!(matching(&core, p), all, "{p}");
    }
    for p in [
        "ARRAY['1'::jsonb] <> ARRAY['1.0'::jsonb]",
        "NOT ARRAY['1'::jsonb] = ARRAY['1.0'::jsonb]",
        "ARRAY['1'::jsonb] = ARRAY['2'::jsonb]",
        "NOT (ARRAY['1'::jsonb] = ARRAY['1.0'::jsonb])",
        "NOT (ARRAY['1'::jsonb] <> ARRAY['2'::jsonb])",
    ] {
        assert_eq!(matching(&core, p), NONE, "{p}");
    }
}

/// TABLE-35・Issue #1603: 未対応形は fail-closed（サブクエリ形・否定の押し下げ・CHECK・上限）。
#[test]
fn unsupported_forms_fail_closed() {
    let (path, _g) = setup("arr-pred-closed");
    let core = core_at(&path);
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE id = ANY(SELECT id FROM t) LIMIT 5"
        ),
        "0A000"
    );
    // 2^53 を超える整数リテラルは float 配列との比較で丸め誤一致を避けるため、
    // 範囲外 int8 と同じ fail-closed の拒否（22003）に倒す。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE 9007199254740993 = ANY(ARRAY[9007199254740992::float8]) LIMIT 5"
        ),
        "22003"
    );
    // 末尾ドット付き整数（`9007199254740993.`）も同じ exactness ガードで拒否する。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE 9007199254740993. = ANY(ARRAY[9007199254740992::float8]) LIMIT 5"
        ),
        "22003"
    );
    assert_eq!(code(&core, "SELECT ANY(nums) FROM t LIMIT 5"), "42601");
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE ALL(nums) = 1 LIMIT 5"),
        "42601"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nums & nums LIMIT 5"),
        "42601"
    );
    // 否定は jsonb の述語と同じく押し下げない（NULL を偽へ落とさない）。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE NOT (nums @> ARRAY[1]) LIMIT 5"
        ),
        "42601"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE NOT nums && ARRAY[1] LIMIT 5"),
        "42601"
    );
    assert_eq!(
        code(&core, "SELECT id FROM t WHERE nope = ANY(nums) LIMIT 5"),
        "42703"
    );
    // CHECK 本体では使えない。
    for check in ["1 = ANY(a)", "a @> a", "a && a", "a = a"] {
        assert_eq!(
            code(
                &core,
                &format!(
                    "CREATE TABLE c (embedding VECTOR(2) NOT NULL, a INTEGER[3], CHECK ({check}))"
                )
            ),
            "0A000",
            "{check}"
        );
    }
    // 要素数 1025 の配列式は構文段が 54000 にする。
    let many: Vec<String> = (0..1025).map(|i| i.to_string()).collect();
    assert_eq!(
        code(
            &core,
            &format!(
                "SELECT id FROM t WHERE id = ANY(ARRAY[{}]) LIMIT 5",
                many.join(",")
            )
        ),
        "54000"
    );
}

/// TABLE-35・Issue #1603: `any`／`some` という名前の列は従来どおり使える（文脈的キーワード）。
#[test]
fn contextual_keywords_remain_usable_as_column_names() {
    let (path, _g) = setup("arr-pred-kw");
    let core = core_at(&path);
    ok(
        &core,
        "alice",
        "CREATE TABLE k (embedding VECTOR(2) NOT NULL, any INTEGER, some INTEGER)",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO k (id, embedding, any, some) VALUES (1, '[0.1,0.2]', 1, 2) \
         USING OPERATION_ID 'k1'",
    );
    let r = select(
        &core,
        "alice",
        "SELECT id FROM k WHERE any = 1 AND some = 2 LIMIT 5",
    );
    assert_eq!(r.rows.len(), 1);
    let r = select(&core, "alice", "SELECT id FROM k WHERE any < some LIMIT 5");
    assert_eq!(r.rows.len(), 1);
}

/// TABLE-35・Issue #1603: 述語は RLS の述語と AND で評価され、UPDATE／DELETE でも他テナントの行に届かない。
#[test]
fn predicates_respect_tenant_boundary_in_dml() {
    let (path, _g) = setup("arr-pred-tenant");
    let core = core_at(&path);
    assert_eq!(matching(&core, "nums @> ARRAY[10]"), vec![1]);
    assert_eq!(ids(&core, "bob", "nums @> ARRAY[10]"), vec![4]);
    ok(
        &core,
        "alice",
        "UPDATE t SET ip = 99 WHERE nums @> ARRAY[10] USING OPERATION_ID 'u1'",
    );
    assert_eq!(matching(&core, "ip = 99"), vec![1]);
    assert_eq!(ids(&core, "bob", "ip = 99"), NONE);
    ok(
        &core,
        "alice",
        "DELETE FROM t WHERE 10 = ANY(nums) USING OPERATION_ID 'd1'",
    );
    assert_eq!(matching(&core, "id > 0"), vec![2, 3]);
    assert_eq!(ids(&core, "bob", "id > 0"), vec![4]);
    ok(
        &core,
        "bob",
        "DELETE FROM t WHERE nums && ARRAY[20] USING OPERATION_ID 'd2'",
    );
    assert_eq!(ids(&core, "bob", "id > 0"), NONE);
    assert_eq!(matching(&core, "id > 0"), vec![2, 3]);
}
