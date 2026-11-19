//! `LIKE`／`ILIKE`／`NOT LIKE`／`NOT ILIKE` の `ANY`／`SOME`／`ALL` と `~~` 系演算子（対象ビヘイビア:
//! SQL-9。関連: SQL-54・TABLE-35。Issue #3258）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を production
//! 経路として検証する。三値論理（空配列・NULL 配列・NULL 要素・NULL 左辺・後置 `NOT` と前置 `NOT` の
//! 区別）、`~~` 系の単独形・量化形、上限と未対応形の fail-closed な拒否、テナント境界
//! （SELECT／UPDATE／DELETE・行ポリシー）を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
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

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session_legacy(&ctx(tenant), &mut s, sql)
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

fn matching(core: &EngineCore, predicate: &str) -> Vec<u64> {
    ids(core, "alice", predicate)
}

const NONE: Vec<u64> = Vec::new();

const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, s TEXT, arr TEXT[8], \
    vs VARCHAR(20)[8], cs CHAR(5)[8], ints INTEGER[8], n INTEGER, \
    iu TEXT COLLATE \"und-x-icu\", cc CHAR(5))";

/// alice の行（`s`／`arr`）:
/// id1 `apple`／`{a%,b%}`・id2 `Banana`／`{b%,NULL}`・id3 NULL／`{a%}`・id4 `cherry`／NULL・
/// id5 `avocado`／`{}`。bob: id6 `apple`／`{a%}`。
fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    let rows: [(u64, &str, &str); 5] = [
        (1, "'apple'", "'{a%,b%}'"),
        (2, "'Banana'", "'{b%,NULL}'"),
        (3, "NULL", "'{a%}'"),
        (4, "'cherry'", "NULL"),
        (5, "'avocado'", "'{}'"),
    ];
    for (id, s, arr) in rows {
        ok(
            &core,
            "alice",
            &format!(
                "INSERT INTO t (id, embedding, s, arr, vs, cs, ints, n, iu, cc) VALUES \
                 ({id}, '[0.1,0.2]', {s}, {arr}, {arr}, '{{x}}', '{{1}}', 1, 'abc', 'abc') \
                 USING OPERATION_ID 'a{id}'"
            ),
        );
    }
    ok(
        &core,
        "bob",
        "INSERT INTO t (id, embedding, s, arr) VALUES (6, '[0.1,0.2]', 'apple', '{a%}') \
         USING OPERATION_ID 'b6'",
    );
    (path, guard)
}

/// SQL-9・Issue #3258: `ANY`／`SOME`／`ALL` の三値論理。`NOT (...)` を併用して NULL と偽を区別する。
#[test]
fn like_any_all_follow_three_valued_logic() {
    let (path, _g) = setup("like-quant-tvl");
    let core = core_at(&path);
    // ANY: id1 は真、id2・id3・id4 は NULL、id5（空配列）は偽。
    assert_eq!(matching(&core, "s LIKE ANY(arr)"), vec![1]);
    assert_eq!(matching(&core, "s LIKE SOME(arr)"), vec![1]);
    assert_eq!(matching(&core, "NOT (s LIKE ANY(arr))"), vec![5]);
    // ALL: id1・id2 は偽、id3・id4 は NULL、id5（空配列）は左辺を問わず真。
    assert_eq!(matching(&core, "s LIKE ALL(arr)"), vec![5]);
    assert_eq!(matching(&core, "NOT (s LIKE ALL(arr))"), vec![1, 2]);
    // ILIKE は ASCII を畳み込む（id2 の `Banana` が `b%` に一致する）。
    assert_eq!(matching(&core, "s ILIKE ANY(arr)"), vec![1, 2]);
    // 後置 NOT は「一致しない要素が 1 つでもある」。前置 NOT（量化子の反転）とは別物。
    assert_eq!(matching(&core, "s NOT LIKE ANY(arr)"), vec![1, 2]);
    assert_eq!(matching(&core, "s NOT LIKE ALL(arr)"), vec![5]);
    assert_eq!(matching(&core, "s NOT ILIKE ANY(arr)"), vec![1]);
    assert_eq!(matching(&core, "NOT (s NOT LIKE ANY(arr))"), vec![5]);
    // 左辺が定数: `'apple' NOT LIKE ANY(...)` は真、前置 NOT で包むと偽。
    assert_eq!(
        matching(&core, "'apple' NOT LIKE ANY(ARRAY['a%','b%'])"),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        matching(&core, "NOT ('apple' LIKE ANY(ARRAY['a%','b%']))"),
        NONE
    );
}

/// SQL-9・Issue #3258: 右辺は配列式・`ARRAY[...]`・型なしリテラル。左辺は括弧付き・関数呼び出しも可。
#[test]
fn like_any_all_operand_shapes() {
    let (path, _g) = setup("like-quant-shapes");
    let core = core_at(&path);
    assert_eq!(
        matching(&core, "s LIKE ANY(ARRAY['a%','c%'])"),
        vec![1, 4, 5]
    );
    assert_eq!(matching(&core, "s LIKE ANY('{a%,c%}')"), vec![1, 4, 5]);
    assert_eq!(matching(&core, "s LIKE ALL(ARRAY['a%','%o'])"), vec![5]);
    assert_eq!(matching(&core, "lower(s) LIKE ANY(ARRAY['b%'])"), vec![2]);
    assert_eq!(matching(&core, "(lower(s)) LIKE ANY(ARRAY['b%'])"), vec![2]);
    assert_eq!(matching(&core, "(s) LIKE ANY(ARRAY['b%'])"), NONE);
    // VARCHAR(n)[] 要素。
    assert_eq!(matching(&core, "s LIKE ANY(vs)"), vec![1]);
    // 他の述語との組み合わせ・名前が `any` の列は壊れない。
    assert_eq!(
        matching(&core, "s LIKE ANY(ARRAY['a%']) AND n = 1"),
        vec![1, 5]
    );
    assert_eq!(
        matching(&core, "s LIKE ANY(ARRAY['a%']) OR s LIKE ANY(ARRAY['c%'])"),
        vec![1, 4, 5]
    );
    // CHAR(n) の左辺は TEXT への変換値（`lower(cc)` の式左辺の単独 LIKE と同じ結果）。
    for pat in ["abc", "abc ", "ab%", "a_c", "%c"] {
        assert_eq!(
            matching(&core, &format!("cc LIKE ANY(ARRAY['{pat}'])")),
            matching(&core, &format!("lower(cc) LIKE '{pat}'")),
            "{pat}"
        );
    }
}

/// SQL-9・Issue #3258: `~~` 系は LIKE 系の単独形・量化形と同じ結果になる。
#[test]
fn tilde_tilde_operators_match_like_family() {
    let (path, _g) = setup("like-quant-tilde");
    let core = core_at(&path);
    assert_eq!(matching(&core, "s ~~ 'a%'"), matching(&core, "s LIKE 'a%'"));
    assert_eq!(matching(&core, "s ~~ 'a%'"), vec![1, 5]);
    assert_eq!(matching(&core, "s !~~ 'a%'"), vec![2, 4]);
    assert_eq!(matching(&core, "s ~~* 'b%'"), vec![2]);
    assert_eq!(matching(&core, "s !~~* 'b%'"), vec![1, 4, 5]);
    assert_eq!(
        matching(&core, "s ~~ ANY(arr)"),
        matching(&core, "s LIKE ANY(arr)")
    );
    assert_eq!(matching(&core, "s ~~* ANY(arr)"), vec![1, 2]);
    assert_eq!(matching(&core, "s !~~ ALL(arr)"), vec![5]);
    assert_eq!(matching(&core, "s !~~* ANY(arr)"), vec![1]);
    // 正規表現演算子は従来どおり。
    assert_eq!(matching(&core, "s ~ '^a'"), vec![1, 5]);
}

/// SQL-9・Issue #3258: 上限・型・未対応形は固定のコードで fail-closed に拒否する。
// intentional: docs/design/like-wildcard-patterns.md
#[test]
fn like_any_all_rejections_are_fail_closed() {
    let (path, _g) = setup("like-quant-reject");
    let core = core_at(&path);
    let q = |sql: &str| format!("SELECT id FROM t WHERE {sql} LIMIT 5");
    // 要素数 1025（ARRAY 形・リテラル形）は 54000。
    let many: Vec<String> = (0..1025).map(|i| format!("'p{i}%'")).collect();
    assert_eq!(
        code(&core, &q(&format!("s LIKE ANY(ARRAY[{}])", many.join(",")))),
        "54000"
    );
    let literal = (0..1025)
        .map(|i| format!("p{i}%"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        code(&core, &q(&format!("s LIKE ANY('{{{literal}}}')"))),
        "54000"
    );
    // 1024 要素（リテラル形）はちょうど受理される。
    let at = (0..1024)
        .map(|i| format!("p{i}%"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        ids(&core, "alice", &format!("s LIKE ANY('{{{at}}}')")),
        NONE
    );
    // パターン長が 4096 を超える。
    let long = "a".repeat(4097);
    assert_eq!(
        code(&core, &q(&format!("s LIKE ANY(ARRAY['{long}'])"))),
        "54000"
    );
    // 末尾の孤立エスケープ。
    assert_eq!(code(&core, &q("s LIKE ANY(ARRAY['a\\'])")), "22025");
    // 型: 非 TEXT の左辺・非 TEXT 要素の配列は 42883、CHAR(n)[] は 0A000。
    assert_eq!(code(&core, &q("n LIKE ANY(arr)")), "42883");
    assert_eq!(code(&core, &q("s LIKE ANY(ARRAY[1,2])")), "42883");
    assert_eq!(code(&core, &q("s LIKE ANY(ints)")), "42883");
    assert_eq!(code(&core, &q("s LIKE ANY(cs)")), "0A000");
    // 配列でない右辺は 42804。
    assert_eq!(code(&core, &q("s LIKE ANY(n)")), "42804");
    // ICU 照合の列は黙って畳み込まない。
    assert_eq!(code(&core, &q("iu LIKE ANY(arr)")), "0A000");
    assert_eq!(code(&core, &q("iu ILIKE ANY(arr)")), "0A000");
    // サブクエリ形・ESCAPE 付き。
    assert_eq!(code(&core, &q("s LIKE ANY(SELECT arr FROM t)")), "0A000");
    assert_eq!(code(&core, &q("s LIKE ANY(arr) ESCAPE '!'")), "42601");
    // CHECK 本体。
    assert_eq!(
        code(
            &core,
            "CREATE TABLE c (embedding VECTOR(2) NOT NULL, s TEXT, CHECK (s LIKE ANY(ARRAY['a%'])))"
        ),
        "0A000"
    );
}

/// SQL-9・Issue #3258: 他テナントの行は見えず、UPDATE／DELETE の対象にもならない。
#[test]
fn like_any_all_respect_tenant_boundary() {
    let (path, _g) = setup("like-quant-tenant");
    let core = core_at(&path);
    assert_eq!(matching(&core, "s LIKE ANY(arr)"), vec![1]);
    assert_eq!(ids(&core, "bob", "s LIKE ANY(arr)"), vec![6]);
    assert_eq!(ids(&core, "bob", "s ~~ ANY(arr)"), vec![6]);
    ok(
        &core,
        "alice",
        "UPDATE t SET n = 99 WHERE s LIKE ANY(ARRAY['a%']) USING OPERATION_ID 'u1'",
    );
    assert_eq!(matching(&core, "n = 99"), vec![1, 5]);
    assert_eq!(ids(&core, "bob", "n = 99"), NONE);
    ok(
        &core,
        "alice",
        "DELETE FROM t WHERE s NOT LIKE ALL(ARRAY['a%']) USING OPERATION_ID 'd1'",
    );
    assert_eq!(matching(&core, "id > 0"), vec![1, 3, 5]);
    assert_eq!(ids(&core, "bob", "id > 0"), vec![6]);
}

/// SQL-9・Issue #3258: 行ポリシー式の LIKE 量化形は RLS として効く（同じ束縛経路）。
#[test]
fn like_any_in_row_policy_filters_rows() {
    let (path, _g) = setup("like-quant-policy");
    let core = core_at(&path);
    for ddl in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
        "CREATE POLICY only_a ON t AS PERMISSIVE FOR ALL USING (s LIKE ANY(ARRAY['a%']))",
    ] {
        ok(&core, "alice", ddl);
    }
    assert_eq!(matching(&core, "id > 0"), vec![1, 5]);
    assert_eq!(ids(&core, "bob", "id > 0"), vec![6]);
}

/// SQL-9・Issue #3258: 他の受理位置（ビュー本文）で単独の LIKE／ILIKE と整合する。
#[test]
fn like_any_in_view_positions() {
    let (path, _g) = setup("like-quant-positions");
    let core = core_at(&path);
    // 単純形ビュー: LIKE の量化形は WHERE と同じ結果で読める。
    ok(
        &core,
        "alice",
        "CREATE VIEW va AS SELECT id, s FROM t WHERE s LIKE ANY(ARRAY['a%'])",
    );
    let r = select(&core, "alice", "SELECT id FROM va LIMIT 10");
    let mut v: Vec<u64> = r.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    assert_eq!(v, vec![1, 5]);
    // 評価後射影形ビュー（LIMIT 本体）の外側 WHERE の `ILIKE ANY` は単独の ILIKE と同じく拒否。
    ok(
        &core,
        "alice",
        "CREATE VIEW vb AS SELECT id, s FROM t ORDER BY id LIMIT 10",
    );
    assert_eq!(
        code(
            &core,
            "SELECT id FROM vb WHERE s ILIKE ANY(ARRAY['a%']) LIMIT 10"
        ),
        code(&core, "SELECT id FROM vb WHERE s ILIKE 'a%' LIMIT 10"),
    );
    // 大小区別ありの量化形は既存の `= ANY` と同じ扱い（この表は ICU 照合の列を持つため、
    // 合成スキーマ上の式は照合の伝搬が未対応として同じコードで拒否される）。
    assert_eq!(
        code(
            &core,
            "SELECT id FROM vb WHERE s LIKE ANY(ARRAY['a%']) LIMIT 10"
        ),
        code(
            &core,
            "SELECT id FROM vb WHERE s = ANY(ARRAY['apple']) LIMIT 10"
        ),
    );
}
