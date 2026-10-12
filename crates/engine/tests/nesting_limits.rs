//! サブクエリ深さ・実行回数・CTE 定義数・CTE 連鎖深さの起動時設定（Issue #1552。
//! ポインタ: `docs/spec/04-behavior/` SQL-46）の結合テスト。
//!
//! `EngineCore::with_nesting_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 未注入（既定）はハード上限と同値で、上限ちょうどは成功し +1 は `54000`
//! - 下げた設定値でも、設定値ちょうどは成功し +1 は従来と同じ `54000`
//!   （エラー文には設定値が出る）
//! - 実行回数は WHERE の `IN`／`EXISTS` と投影位置のスカラーサブクエリの双方に効く
//! - `DECLARE CURSOR ... FOR WITH ...` にも CTE の設定値が効く
//! - 格納済みビュー本文の再パースはハード上限のままで、上限を下げた再起動後も
//!   既存ビューを参照できる（設計 D6）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::nesting_limits::NestingLimits;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            DOCS,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    for id in 1..=2u64 {
        engine::tenant::insert_typed_row(
            storage,
            DOCS,
            &ctx("alice"),
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &OperationId::parse(&format!("seed-{id}")).expect("op id"),
        )
        .expect("insert row");
    }
}

fn open_core(path: &std::path::Path, limits: Option<NestingLimits>) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_nesting_limits(l),
        None => core,
    }
}

/// 表を作成・投入した core（`limits` が `None` なら未注入）。
fn seeded_core(path: &std::path::Path, limits: Option<NestingLimits>) -> EngineCore {
    {
        let storage = Storage::open(path).expect("open storage");
        seed(&storage);
    }
    open_core(path, limits)
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}"));
}

/// `54000` で拒否されることを確認し、エラー文を返す。
fn too_large(core: &EngineCore, sql: &str) -> String {
    match run(core, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => {
            assert_eq!(e.wire_code(), "54000", "{sql}: {e}");
            assert!(matches!(e, SqlSurfaceError::PayloadTooLarge { .. }));
            e.to_string()
        }
    }
}

/// サブクエリの入れ子が `depth` 段の `IN` 文。
fn nested_in(depth: usize) -> String {
    let mut inner = format!("SELECT lang FROM {DOCS} LIMIT 100");
    for _ in 1..depth {
        inner = format!("SELECT lang FROM {DOCS} WHERE lang IN ({inner}) LIMIT 100");
    }
    format!("SELECT id FROM {DOCS} WHERE lang IN ({inner}) LIMIT 100")
}

/// 内側クエリを `n` 回実行する文（深さ 1 の `IN` を AND 連結）。
fn many_in(n: usize) -> String {
    let preds: Vec<String> = (0..n)
        .map(|_| format!("lang IN (SELECT lang FROM {DOCS} LIMIT 100)"))
        .collect();
    format!(
        "SELECT id FROM {DOCS} WHERE {} LIMIT 100",
        preds.join(" AND ")
    )
}

/// 内側クエリを `n` 回実行する文（投影位置のスカラーサブクエリ）。
fn many_scalar(n: usize) -> String {
    let items: Vec<String> = (0..n)
        .map(|i| format!("(SELECT lang FROM {DOCS} LIMIT 1) AS s{i}"))
        .collect();
    format!("SELECT id, {} FROM {DOCS} LIMIT 100", items.join(", "))
}

/// CTE 定義が `n` 件（どれも基底表を直接読む。連鎖なし）の文。
fn cte_defs(n: usize) -> String {
    let defs: Vec<String> = (0..n)
        .map(|i| format!("c{i} AS (SELECT id FROM {DOCS})"))
        .collect();
    format!("WITH {} SELECT id FROM c0 LIMIT 10", defs.join(", "))
}

/// `c0 -> c1 -> ... ` と `len` 件連鎖する文（`len` 件目を主クエリが読む）。
fn cte_chain(len: usize) -> String {
    let mut defs = vec![format!("c0 AS (SELECT id FROM {DOCS})")];
    for i in 1..len {
        defs.push(format!("c{i} AS (SELECT id FROM c{})", i - 1));
    }
    format!(
        "WITH {} SELECT id FROM c{} LIMIT 10",
        defs.join(", "),
        len - 1
    )
}

#[test]
fn default_limits_equal_hard_limits_and_boundaries_hold() {
    let d = NestingLimits::default();
    assert_eq!(
        (
            d.max_subquery_depth(),
            d.max_subquery_executions(),
            d.max_cte_definitions(),
            d.max_cte_nesting_depth()
        ),
        (4, 16, 16, 4)
    );

    let path = unique_db_path("nesting-limits-default");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, None);
    assert_eq!(core.nesting_limits(), NestingLimits::default());

    ok(&core, &nested_in(4));
    too_large(&core, &nested_in(5));
    ok(&core, &many_in(16));
    too_large(&core, &many_in(17));
    ok(&core, &cte_defs(16));
    too_large(&core, &cte_defs(17));
    ok(&core, &cte_chain(4));
    too_large(&core, &cte_chain(5));
}

#[test]
fn lowered_limits_apply_exactly_and_report_the_configured_value() {
    let path = unique_db_path("nesting-limits-lowered");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(2, 3, 2, 1)));

    ok(&core, &nested_in(2));
    assert!(too_large(&core, &nested_in(3)).contains("limit 2"));

    ok(&core, &many_in(3));
    assert!(too_large(&core, &many_in(4)).contains("limit 3"));

    ok(&core, &cte_defs(2));
    too_large(&core, &cte_defs(3));

    ok(&core, &cte_chain(1));
    too_large(&core, &cte_chain(2));
}

#[test]
fn execution_budget_applies_to_exists_and_projection_subqueries() {
    let path = unique_db_path("nesting-limits-exec-paths");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(4, 2, 16, 4)));

    let exists = |n: usize| {
        let preds: Vec<String> = (0..n)
            .map(|_| format!("EXISTS (SELECT lang FROM {DOCS} LIMIT 1)"))
            .collect();
        format!(
            "SELECT id FROM {DOCS} WHERE {} LIMIT 100",
            preds.join(" AND ")
        )
    };
    ok(&core, &exists(2));
    too_large(&core, &exists(3));

    ok(&core, &many_scalar(2));
    too_large(&core, &many_scalar(3));
}

#[test]
fn declare_cursor_for_with_honors_cte_limits() {
    let path = unique_db_path("nesting-limits-cursor");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(NestingLimits::new(4, 16, 2, 1)));

    let declare = |inner: String| format!("DECLARE cur CURSOR FOR {inner}");
    too_large(&core, &declare(cte_defs(3)));
    too_large(&core, &declare(cte_chain(2)));
}

#[test]
fn stored_view_bodies_are_revalidated_with_hard_limits() {
    let path = unique_db_path("nesting-limits-view");
    let _g = CleanupGuard(path.clone());
    {
        // 既定（ハード上限）の core でビューを作る。本文は 3 定義・2 段連鎖。
        let core = seeded_core(&path, None);
        ok(
            &core,
            "CREATE VIEW v AS WITH c0 AS (SELECT id FROM docs), c1 AS (SELECT id FROM c0), \
             c2 AS (SELECT id FROM c1) SELECT id FROM c2 LIMIT 5",
        );
    }
    // 上限を下げて再起動しても、既存ビューは参照できる（セッション SQL の CTE には効く）。
    let core = open_core(&path, Some(NestingLimits::new(1, 1, 1, 1)));
    ok(&core, "SELECT id FROM v LIMIT 10");
    too_large(&core, &cte_defs(2));
}
