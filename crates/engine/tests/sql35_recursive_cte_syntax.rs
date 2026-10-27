//! `WITH RECURSIVE` の構文受理と再帰構造の検証の結合テスト（段階 A。SQL-35、Issue #1869）。
//! ポインタ: `docs/spec/04-behavior/` の SQL-35・ERR-6・RLS-10 (b)。
//!
//! `sql29_cte.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql_in_session`）で、
//! 不正な再帰が `42P19`・構文不正が `42601`・機能ゲートが `0A000` になること、判定順序
//! （構文 → 再帰構造 → 機能ゲート）、再帰構造の判定が表の有無・テナントに依存しない
//! （存在オラクルを作らない）ことを固定する。構造検証を通過した形の反復評価は #1870
//! （`sql35_recursive_cte_eval.rs`）の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql35-recursive-cte");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// (wire_code, 文言) を返す。成功する文を渡した場合は panic する。
fn outcome(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(other) => panic!("must be rejected: {sql} -> {other:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

#[test]
fn recursive_forms_have_stable_codes() {
    let (core, _guard) = open_core();
    let cases: &[(&str, &str)] = &[
        // `UNION`（重複除去）は #1872 まで機能ゲート。
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
            "0A000",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT 1) SELECT n FROM t",
            "42P19",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT count(*) FROM t) SELECT n FROM t",
            "42P19",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM docs WHERE n IN (SELECT n FROM t)) SELECT n FROM t",
            "42P19",
        ),
        (
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) CYCLE n SET c USING p SELECT n FROM t",
            "0A000",
        ),
        ("WITH RECURSIVE t(n) (SELECT 1) SELECT n FROM t", "42601"),
    ];
    for (sql, expected) in cases {
        let (code, _) = outcome(&core, "alice", sql);
        assert_eq!(&code, expected, "{sql}");
    }
}

/// 構造検証を通過した再帰形は評価される（自己参照が同名の実表 `docs` へ解決されない）。
#[test]
fn valid_recursive_forms_are_evaluated() {
    let (core, _guard) = open_core();
    for sql in [
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
        "WITH RECURSIVE docs(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM docs WHERE n < 3) SELECT n FROM docs",
    ] {
        let mut session = SessionState::default();
        let result = core
            .execute_sql_in_session(&ctx("alice"), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(
            matches!(result, engine::sql::SqlOutcome::Query(_)),
            "{sql}"
        );
    }
}

/// 再帰構造の判定（`42P19`）は表の有無・テナントで code も文言も変わらない（存在オラクルを作らない）。
#[test]
fn structural_verdict_is_independent_of_catalog_and_tenant() {
    let (core, _guard) = open_core();
    let with_real = "WITH RECURSIVE r(id) AS (SELECT id FROM docs UNION ALL SELECT count(*) FROM r) SELECT id FROM r";
    let with_ghost = "WITH RECURSIVE r(id) AS (SELECT id FROM ghost UNION ALL SELECT count(*) FROM r) SELECT id FROM r";
    let a = outcome(&core, "alice", with_real);
    let b = outcome(&core, "bob", with_real);
    let c = outcome(&core, "alice", with_ghost);
    assert_eq!(a.0, "42P19");
    assert_eq!(a, b);
    assert_eq!(a, c);
}
