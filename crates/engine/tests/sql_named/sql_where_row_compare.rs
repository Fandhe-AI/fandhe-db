//! WHERE の行値比較 `(e1, e2, ...) <op> (f1, f2, ...)` の構文段（対象ビヘイビア: SQL-9。
//! 親 Issue #3254・本 Issue #3255）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛）を production 経路として、
//! 構文段が受理した形が評価未対応の暫定ゲート（`0A000`。評価は #3256）で止まること、成分数不一致・
//! 構文違反が `42601` のままであること、副作用が無いこと、テナント境界を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
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

/// 失敗を前提にした SQL の `(wire_code, メッセージ)`。成功してしまった場合は SQL を添えて落とす。
fn fail(core: &EngineCore, tenant: &str, sql: &str) -> (&'static str, String) {
    match run(core, tenant, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => (e.wire_code(), e.to_string()),
    }
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    fail(core, "alice", sql).0
}

const CREATE: &str = "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INTEGER, b INTEGER, s TEXT)";

/// alice: id1 (1,1)・id2 (3,5)・id3 (3,NULL)、bob: id4 (3,5)。
fn setup(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = core_at(&path);
    ok(&core, "alice", CREATE);
    for (tenant, id, a, b) in [
        ("alice", 1, "1", "1"),
        ("alice", 2, "3", "5"),
        ("alice", 3, "3", "NULL"),
        ("bob", 4, "3", "5"),
    ] {
        ok(
            &core,
            tenant,
            &format!(
                "INSERT INTO t (id, embedding, a, b, s) VALUES \
                 ({id}, '[0.1,0.2]', {a}, {b}, 'x') USING OPERATION_ID 'op{id}'"
            ),
        );
    }
    (path, guard)
}

/// 構文段が受理する形は、評価が #3256 で入るまで束縛段の暫定ゲートで `0A000` になる
/// （従来は構文段で `42601`）。tracked: #3256
#[test]
fn row_compare_is_accepted_by_syntax_and_gated_at_bind() {
    let (path, _g) = setup("row-cmp-gate");
    let core = core_at(&path);
    for sql in [
        "SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) = (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) <> (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) != (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) <= (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) > (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) >= (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a + 1, lower(s)) = (2, 'x') LIMIT 10",
        "SELECT id FROM t WHERE ((a, b) < (3, 5)) LIMIT 10",
        "SELECT id FROM t WHERE NOT (a, b) < (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE a = 1 AND (a, b) < (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE a = 1 OR (a, b) < (3, 5) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10;",
        "SELECT count(*) FROM t WHERE (a, b) < (3, 5)",
        "SELECT DISTINCT a FROM t WHERE (a, b) < (3, 5) LIMIT 10",
        "EXPLAIN SELECT id FROM t WHERE (a, b) < (3, 5) LIMIT 10",
    ] {
        let (c, msg) = fail(&core, "alice", sql);
        assert_eq!(c, "0A000", "{sql}: {msg}");
        assert!(msg.contains("row value comparison"), "{sql}: {msg}");
    }
}

/// 述語形 UPDATE／DELETE も 0A000 で止まり、行は変わらない（副作用なし）。
#[test]
fn row_compare_in_predicate_dml_is_gated_without_side_effects() {
    let (path, _g) = setup("row-cmp-dml");
    let core = core_at(&path);
    for sql in [
        "UPDATE t SET s = 'y' WHERE (a, b) < (3, 5) USING OPERATION_ID 'u1'",
        "DELETE FROM t WHERE (a, b) = (3, 5) USING OPERATION_ID 'd1'",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE s = 'x' LIMIT 10")
        .expect("select");
    assert_eq!(r.rows.len(), 3, "rows must be unchanged");
}

/// 成分数の不一致・形の誤りは `42601`、成分数の上限超過は `54000`。
#[test]
fn row_compare_malformed_forms_fail_closed() {
    let (path, _g) = setup("row-cmp-bad");
    let core = core_at(&path);
    for sql in [
        "SELECT id FROM t WHERE (a, b) < (1, 2, 3) LIMIT 10",
        "SELECT id FROM t WHERE (a, b, s) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < 3 LIMIT 10",
        "SELECT id FROM t WHERE a < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < ROW(1, 2) LIMIT 10",
        "SELECT id FROM t WHERE ROW(a, b) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE ((a, b), s) < ((1, 2), 3) LIMIT 10",
        "SELECT id FROM t WHERE (a, ) < (1, 2) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) IN ((1, 2)) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) IS NULL LIMIT 10",
        "SELECT id FROM t WHERE (a, b) BETWEEN (1, 2) AND (3, 4) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) = ANY (ARRAY[1]) LIMIT 10",
        "SELECT id FROM t WHERE (a, b) < (1, 2) + 1 LIMIT 10",
        "SELECT (a, b) FROM t LIMIT 10",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            "SELECT id FROM t WHERE (a, b) = (SELECT 1, 2) LIMIT 10"
        ),
        "0A000"
    );
    // 上限ちょうどは構文段を通る（束縛ゲートで 0A000）。+1 は 54000。
    let row = |n: usize| (0..n).map(|_| "a").collect::<Vec<_>>().join(", ");
    let at_limit = format!("SELECT id FROM t WHERE ({0}) = ({0}) LIMIT 10", row(64));
    assert_eq!(code(&core, &at_limit), "0A000");
    let over = format!("SELECT id FROM t WHERE ({0}) = ({0}) LIMIT 10", row(65));
    assert_eq!(code(&core, &over), "54000");
}

/// 成分 1 個の括弧は従来どおり式（行値にならない）。
#[test]
fn single_component_parens_stay_scalar() {
    let (path, _g) = setup("row-cmp-scalar");
    let core = core_at(&path);
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE (a) < 3 LIMIT 10")
        .expect("scalar parens");
    assert_eq!(r.rows.len(), 1);
}

/// 永続化される定義文脈（POLICY・VIEW・部分 INDEX・CHECK）は定義時に拒否され、何も残らない。
/// POLICY・部分 INDEX は束縛段の暫定ゲート／索引述語の検査で `0A000`、VIEW 本文・CHECK 本体は
/// 行値を認識せず `42601`。tracked: #3256
#[test]
fn row_compare_is_rejected_in_definition_contexts() {
    let (path, _g) = setup("row-cmp-defs");
    let core = core_at(&path);
    for (sql, want) in [
        (
            "CREATE POLICY p ON t FOR SELECT USING ((a, b) < (3, 5))",
            "0A000",
        ),
        (
            "CREATE VIEW v AS SELECT id FROM t WHERE (a, b) < (3, 5)",
            "42601",
        ),
        ("CREATE INDEX i ON t (a) WHERE (a, b) < (3, 5)", "0A000"),
        (
            "ALTER TABLE t ADD CONSTRAINT c CHECK ((a, b) < (3, 5))",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
    // 拒否された定義は残らず、平文の SELECT は従来どおり通る（RLS 述語が増えていない）。
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id FROM t WHERE s = 'x' LIMIT 10")
        .expect("select after rejected definitions");
    assert_eq!(r.rows.len(), 3);
}

/// テナント境界: 他テナントの行を含む表へ投げても 0A000 で、エラー文言に他テナントの情報は出ない。
#[test]
fn row_compare_error_does_not_leak_other_tenants() {
    let (path, _g) = setup("row-cmp-tenant");
    let core = core_at(&path);
    let (c, msg) = fail(
        &core,
        "alice",
        "SELECT id FROM t WHERE (a, b) = (3, 5) LIMIT 10",
    );
    assert_eq!(c, "0A000");
    assert!(!msg.contains("bob"), "{msg}");
    let (c, _) = fail(
        &core,
        "bob",
        "SELECT id FROM t WHERE (a, b) = (3, 5) LIMIT 10",
    );
    assert_eq!(c, "0A000");
}

/// `$n` を行値の成分に置く形は Parse の時点で拒否される（fail-closed。誤った型・個数を報告して
/// 成功させない）。型推論・Describe への結線は #3257 の担当。tracked: #3257
#[test]
fn row_compare_with_placeholders_fails_closed_at_parse() {
    let (path, _g) = setup("row-cmp-param");
    let core = core_at(&path);
    let err = core
        .parse_sql_prepared("SELECT id FROM t WHERE (a, b) < ($1, $2) LIMIT 10")
        .expect_err("placeholders in a row value must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
