//! `GROUP BY` の列数・グループ数上限の起動時設定（Issue #1550。ポインタ:
//! `docs/spec/04-behavior/` SQL-46）の結合テスト。
//!
//! `EngineCore::with_group_by_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 設定値ちょうどは成功し、超過は従来と同じ SQLSTATE `54000` で拒否される
//! - 列数の判定はデータに依存しない（空テーブルでも拒否される）
//! - 同じ設定値が `SELECT DISTINCT`・JOIN・集合演算の集計枝・
//!   ビュー・`DECLARE CURSOR` の各経路に効く
//! - グループ数は RLS 適用後の可視行だけで数える（他テナントの値は数えない）
//! - 既定値はハード上限と同値で、未注入と同一に振る舞う

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::{SqlSurfaceError, MAX_GROUP_BY_COLUMNS};
use engine::sql::group_by::MAX_GROUPS;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// テキスト列 `c1..c9` を持つ表（9 列 = ハード上限 + 1 の拒否確認用）。
const WIDE: &str = "wide";
/// `wide` と同じ id で結合する表。
const DIM: &str = "dim";

fn wide_schema(name: &str) -> TableSchema {
    let mut cols = vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)];
    cols.extend((1..=9).map(|i| ColumnDef::new(format!("c{i}"), ColumnType::Text, false)));
    TableSchema::new(name, cols)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(storage: &Storage, table: &str, tenant: &str, id: u64, c1: &str) {
    let mut values = vec![Value::Vector(vec![id as f32, 0.0])];
    values.push(Value::Text(c1.to_string()));
    values.extend((2..=9).map(|_| Value::Text("k".to_string())));
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        Visibility::Public,
        &values,
        &OperationId::parse(&format!("seed-{table}-{tenant}-{id}")).expect("op id"),
    )
    .expect("insert row");
}

/// `values` を `alice` の行（id は 1 始まり）として `wide`・`dim` に投入した core。
fn core_with(path: &std::path::Path, limits: Option<GroupByLimits>, values: &[&str]) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    for t in [WIDE, DIM] {
        storage.create_table(&wide_schema(t)).expect("create table");
    }
    for (i, v) in values.iter().enumerate() {
        insert(&storage, WIDE, "alice", i as u64 + 1, v);
        insert(&storage, DIM, "alice", i as u64 + 1, v);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_group_by_limits(l),
        None => core,
    }
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    run(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn cols(n: usize) -> String {
    (1..=n)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn default_limits_equal_hard_limits_and_match_uninjected_core() {
    let d = GroupByLimits::default();
    assert_eq!(d.max_group_by_columns(), MAX_GROUP_BY_COLUMNS);
    assert_eq!(d.max_groups(), MAX_GROUPS);

    let path = unique_db_path("group-by-limits-default");
    let _g = CleanupGuard(path.clone());
    let core = core_with(&path, None, &["x", "y"]);
    assert_eq!(core.group_by_limits(), GroupByLimits::default());
    let n = MAX_GROUP_BY_COLUMNS;
    ok(
        &core,
        &format!(
            "SELECT {}, COUNT(*) FROM {WIDE} GROUP BY {}",
            cols(n),
            cols(n)
        ),
    );
    assert_eq!(
        code(
            &core,
            &format!(
                "SELECT {}, COUNT(*) FROM {WIDE} GROUP BY {}",
                cols(n + 1),
                cols(n + 1)
            )
        ),
        "54000"
    );
}

#[test]
fn hard_limit_setting_behaves_like_default() {
    let path = unique_db_path("group-by-limits-hard");
    let _g = CleanupGuard(path.clone());
    let core = core_with(
        &path,
        Some(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, MAX_GROUPS)),
        &["x"],
    );
    let n = MAX_GROUP_BY_COLUMNS;
    ok(
        &core,
        &format!(
            "SELECT {}, COUNT(*) FROM {WIDE} GROUP BY {}",
            cols(n),
            cols(n)
        ),
    );
}

#[test]
fn column_count_boundary_follows_setting_even_on_empty_table() {
    let path = unique_db_path("group-by-limits-cols");
    let _g = CleanupGuard(path.clone());
    // 空テーブル: 判定がデータに依存しないこと。
    let core = core_with(&path, Some(GroupByLimits::new(2, MAX_GROUPS)), &[]);
    ok(
        &core,
        &format!(
            "SELECT {}, COUNT(*) FROM {WIDE} GROUP BY {}",
            cols(2),
            cols(2)
        ),
    );
    assert_eq!(
        code(
            &core,
            &format!(
                "SELECT {}, COUNT(*) FROM {WIDE} GROUP BY {}",
                cols(3),
                cols(3)
            )
        ),
        "54000"
    );
}

#[test]
fn group_count_boundary_follows_setting() {
    let path = unique_db_path("group-by-limits-groups");
    let _g = CleanupGuard(path.clone());
    let core = core_with(
        &path,
        Some(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, 3)),
        &["a", "b", "c"],
    );
    ok(
        &core,
        &format!("SELECT c1, COUNT(*) FROM {WIDE} GROUP BY c1"),
    );
    // 4 つ目の異なり値でちょうど超過する。
    let path2 = unique_db_path("group-by-limits-groups-over");
    let _g2 = CleanupGuard(path2.clone());
    let core2 = core_with(
        &path2,
        Some(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, 3)),
        &["a", "b", "c", "d"],
    );
    assert_eq!(
        code(
            &core2,
            &format!("SELECT c1, COUNT(*) FROM {WIDE} GROUP BY c1")
        ),
        "54000"
    );
}

/// 4 グループ・上限 3 の core（各経路が同じ設定値で拒否されることの確認用）。
fn over_groups_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let g = CleanupGuard(path.clone());
    let core = core_with(
        &path,
        Some(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, 3)),
        &["a", "b", "c", "d"],
    );
    (core, g)
}

#[test]
fn distinct_inherits_group_limits() {
    let (core, _g) = over_groups_core("group-by-limits-distinct");
    assert_eq!(
        code(&core, &format!("SELECT DISTINCT c1 FROM {WIDE}")),
        "54000"
    );
}

#[test]
fn join_aggregate_follows_group_and_column_limits() {
    let (core, _g) = over_groups_core("group-by-limits-join");
    assert_eq!(
        code(
            &core,
            &format!("SELECT {WIDE}.c1, COUNT(*) FROM {WIDE} JOIN {DIM} ON {WIDE}.id = {DIM}.id GROUP BY {WIDE}.c1")
        ),
        "54000"
    );
    let path = unique_db_path("group-by-limits-join-cols");
    let _g2 = CleanupGuard(path.clone());
    let core = core_with(&path, Some(GroupByLimits::new(1, MAX_GROUPS)), &["a"]);
    ok(
        &core,
        &format!("SELECT {WIDE}.c1, COUNT(*) FROM {WIDE} JOIN {DIM} ON {WIDE}.id = {DIM}.id GROUP BY {WIDE}.c1"),
    );
    assert_eq!(
        code(
            &core,
            &format!("SELECT {WIDE}.c1, {WIDE}.c2, COUNT(*) FROM {WIDE} JOIN {DIM} ON {WIDE}.id = {DIM}.id GROUP BY {WIDE}.c1, {WIDE}.c2")
        ),
        "54000"
    );
}

#[test]
fn set_operation_aggregate_branch_follows_limits() {
    let (core, _g) = over_groups_core("group-by-limits-setop");
    assert_eq!(
        code(
            &core,
            &format!("SELECT c1, COUNT(*) FROM {WIDE} GROUP BY c1 UNION ALL SELECT c1, COUNT(*) FROM {DIM} GROUP BY c1")
        ),
        "54000"
    );
}

#[test]
fn view_and_cursor_follow_limits() {
    let (core, _g) = over_groups_core("group-by-limits-view");
    ok(
        &core,
        &format!("CREATE VIEW vb AS SELECT c1, c2 FROM {WIDE} LIMIT 100"),
    );
    assert_eq!(
        code(&core, "SELECT c1, COUNT(*) FROM vb GROUP BY c1"),
        "54000"
    );

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let caller = ctx("alice");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DECLARE c CURSOR FOR SELECT c1, COUNT(*) FROM {WIDE} GROUP BY c1"),
        )
        .expect_err("declare must fail over the group limit");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn other_tenants_groups_are_not_counted() {
    let path = unique_db_path("group-by-limits-rls");
    let _g = CleanupGuard(path.clone());
    let core = core_with(
        &path,
        Some(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, 2)),
        &["a", "b"],
    );
    // 他テナントの Public 行は可視だが Private 行は不可視。Private の追加グループは数えない。
    let storage_path = path.clone();
    drop(core);
    let storage = Storage::open(&storage_path).expect("reopen");
    engine::tenant::insert_typed_row(
        &storage,
        WIDE,
        &ctx("bob"),
        100,
        Visibility::Private,
        &{
            let mut v = vec![Value::Vector(vec![1.0, 0.0])];
            v.push(Value::Text("secret".to_string()));
            v.extend((2..=9).map(|_| Value::Text("k".to_string())));
            v
        },
        &OperationId::parse("seed-bob-private").expect("op id"),
    )
    .expect("insert bob private");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_group_by_limits(GroupByLimits::new(MAX_GROUP_BY_COLUMNS, 2));
    ok(
        &core,
        &format!("SELECT c1, COUNT(*) FROM {WIDE} GROUP BY c1"),
    );
}
