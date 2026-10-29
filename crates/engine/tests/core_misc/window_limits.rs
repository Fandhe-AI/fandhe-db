//! 窓キー数上限の起動時設定（Issue #1900。ポインタ: `docs/spec/04-behavior/` SQL-46・SQL-64）
//! の結合テスト。
//!
//! `EngineCore::with_window_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 未注入（既定）はハード上限 8 と同値で、8 キーは成功し 9 キーは `54000`（文言も従来どおり）
//! - 下げた設定値でも、設定値ちょうどは成功し +1 は同じ `54000`（`PARTITION BY`／`ORDER BY` とも）
//! - 名前付きウィンドウ参照・CTE 本文・派生表の内側の窓にも効く
//! - 判定は文の形だけで決まり、データ（空表）に依存しない
//! - 参照されない `WINDOW` 定義の検証はハード上限のまま（設計 D6。ビュー本文は窓を受理しない）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::window_limits::WindowLimits;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DOCS: &str = "docs";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 窓キーに使うテキスト列 `c0`..`c9` を持つ空の表を作る（判定はデータに依存しない）。
fn seed(storage: &Storage) {
    let mut cols = vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)];
    for i in 0..10 {
        cols.push(ColumnDef::new(format!("c{i}"), ColumnType::Text, true));
    }
    storage
        .create_table(&TableSchema::new(DOCS, cols))
        .expect("create table");
}

fn open_core(path: &std::path::Path, limits: Option<WindowLimits>) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_window_limits(l),
        None => core,
    }
}

fn seeded_core(path: &std::path::Path, limits: Option<WindowLimits>) -> EngineCore {
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

/// `c0, c1, ... c{n-1}`。
fn keys(n: usize) -> String {
    (0..n)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn partition_body(n: usize) -> String {
    format!(
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY {}) FROM {DOCS}",
        keys(n)
    )
}

fn partition_by(n: usize) -> String {
    format!("{} LIMIT 10", partition_body(n))
}

fn order_by(n: usize) -> String {
    format!(
        "SELECT id, ROW_NUMBER() OVER (ORDER BY {}) FROM {DOCS} LIMIT 10",
        keys(n)
    )
}

fn named_window(n: usize) -> String {
    format!(
        "SELECT id, ROW_NUMBER() OVER w FROM {DOCS} WINDOW w AS (PARTITION BY {}) LIMIT 10",
        keys(n)
    )
}

#[test]
fn default_limit_equals_hard_limit_and_keeps_messages() {
    assert_eq!(WindowLimits::default().max_window_keys(), 8);
    let path = unique_db_path("window-limits-default");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, None);
    assert_eq!(core.window_limits(), WindowLimits::default());

    ok(&core, &partition_by(8));
    assert!(too_large(&core, &partition_by(9)).contains("too many PARTITION BY columns"));
    ok(&core, &order_by(8));
    assert!(too_large(&core, &order_by(9)).contains("too many ORDER BY keys"));
}

#[test]
fn lowered_limit_applies_exactly_to_partition_by_and_order_by() {
    let path = unique_db_path("window-limits-lowered");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(WindowLimits::new(3)));
    assert_eq!(core.window_limits().max_window_keys(), 3);

    ok(&core, &partition_by(3));
    assert!(too_large(&core, &partition_by(4)).contains("too many PARTITION BY columns"));
    ok(&core, &order_by(3));
    assert!(too_large(&core, &order_by(4)).contains("too many ORDER BY keys"));
}

#[test]
fn builder_order_does_not_matter() {
    let path = unique_db_path("window-limits-order");
    let _g = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        seed(&storage);
    }
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_window_limits(WindowLimits::new(2))
        .with_nesting_limits(engine::nesting_limits::NestingLimits::default());
    ok(&core, &partition_by(2));
    too_large(&core, &partition_by(3));
}

#[test]
fn limit_reaches_named_window_cte_and_derived_table() {
    let path = unique_db_path("window-limits-paths");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(WindowLimits::new(2)));

    // 名前付きウィンドウ（`OVER w` 参照）。
    ok(&core, &named_window(2));
    too_large(&core, &named_window(3));
    // CTE 本文。
    ok(
        &core,
        &format!(
            "WITH c AS ({}) SELECT id FROM c LIMIT 10",
            partition_body(2)
        ),
    );
    too_large(
        &core,
        &format!(
            "WITH c AS ({}) SELECT id FROM c LIMIT 10",
            partition_body(3)
        ),
    );
    // 派生表。
    ok(
        &core,
        &format!("SELECT id FROM ({}) AS t LIMIT 10", partition_body(2)),
    );
    too_large(
        &core,
        &format!("SELECT id FROM ({}) AS t LIMIT 10", partition_body(3)),
    );
}

#[test]
fn unreferenced_window_definition_keeps_hard_limit() {
    // 参照されない `WINDOW w AS (...)` 定義の検証は `Parser::new` の既定（ハード上限）のまま
    // （設計 D6）。参照された定義は展開後に本パーサーが設定値で判定する（上のテスト）。
    let path = unique_db_path("window-limits-unreferenced");
    let _g = CleanupGuard(path.clone());
    let core = seeded_core(&path, Some(WindowLimits::new(2)));
    ok(
        &core,
        &format!(
            "SELECT id FROM {DOCS} WINDOW w AS (PARTITION BY {}) LIMIT 10",
            keys(3)
        ),
    );
    too_large(
        &core,
        &format!(
            "SELECT id FROM {DOCS} WINDOW w AS (PARTITION BY {}) LIMIT 10",
            keys(9)
        ),
    );
}
