//! JOIN のリレーション数・入力行数・出力行数上限の起動時設定（Issue #1551。ポインタ:
//! `docs/spec/04-behavior/` SQL-46）の結合テスト。
//!
//! `EngineCore::with_join_limits` で設定値を注入し、`execute_sql_in_session`
//! （wire-server の simple／extended プロトコルと NoSQL 表層が共有する実行入口）を
//! production 経路として検証する。確認する契約:
//!
//! - 設定値ちょうどは成功し、超過は従来と同じ SQLSTATE `54000` で拒否される
//! - リレーション数の判定はデータに依存しない（空テーブルでも拒否される）
//! - 入力行数は RLS 適用後の可視行だけで数える（他テナントの Private 行は数えない）
//! - 出力行数は `LIMIT` の値によらず、外部結合の NULL 補完行も数える
//! - 既定値はハード上限と同値で、未注入と同一に振る舞う

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::{JoinSizeLimits, MAX_JOIN_INPUT_ROWS, MAX_JOIN_OUTPUT_ROWS};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::relation::MAX_TABLE_REFS;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("c1", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(storage: &Storage, table: &str, tenant: &str, id: u64, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        table,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(format!("v{id}")),
        ],
        &OperationId::parse(&format!("seed-{table}-{tenant}-{id}")).expect("op id"),
    )
    .expect("insert row");
}

/// `t1..t{tables}` を作り、各表へ `alice` の行 id `1..=rows` を投入した storage。
fn seeded(path: &std::path::Path, tables: usize, rows: u64) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    for t in 1..=tables {
        let name = format!("t{t}");
        storage.create_table(&schema(&name)).expect("create table");
        for id in 1..=rows {
            insert(&storage, &name, "alice", id, Visibility::Public);
        }
    }
    storage
}

fn core_of(storage: Storage, limits: Option<JoinSizeLimits>) -> EngineCore {
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    match limits {
        Some(l) => core.with_join_limits(l),
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

/// `n` 方向の内部結合（`t1 JOIN t2 ON ... JOIN t3 ON ...`）。
fn join_n(n: usize) -> String {
    let mut sql = "SELECT t1.c1 FROM t1".to_string();
    for k in 2..=n {
        sql.push_str(&format!(" JOIN t{k} ON t1.id = t{k}.id"));
    }
    sql.push_str(" LIMIT 1000");
    sql
}

const J2: &str = "SELECT t1.c1 FROM t1 JOIN t2 ON t1.id = t2.id LIMIT 1000";

#[test]
fn default_limits_equal_hard_limits_and_match_uninjected_core() {
    let d = JoinSizeLimits::default();
    assert_eq!(d.max_relations(), MAX_TABLE_REFS);
    assert_eq!(d.max_input_rows(), MAX_JOIN_INPUT_ROWS);
    assert_eq!(d.max_output_rows(), MAX_JOIN_OUTPUT_ROWS);
    assert_eq!(
        (MAX_JOIN_INPUT_ROWS, MAX_JOIN_OUTPUT_ROWS),
        (100_000, 100_000)
    );

    let path = unique_db_path("join-limits-default");
    let _g = CleanupGuard(path.clone());
    let core = core_of(seeded(&path, MAX_TABLE_REFS, 1), None);
    assert_eq!(core.join_limits(), JoinSizeLimits::default());
    ok(&core, &join_n(MAX_TABLE_REFS));
}

#[test]
fn relation_count_boundary_is_data_independent() {
    for (limit, over) in [(2usize, 3usize), (3, 4)] {
        let path = unique_db_path(&format!("join-limits-rel-{limit}"));
        let _g = CleanupGuard(path.clone());
        // 空テーブルでも拒否される（データ非依存）。
        let core = core_of(
            seeded(&path, over, 0),
            Some(JoinSizeLimits::new(limit, 100, 100)),
        );
        ok(&core, &join_n(limit));
        assert_eq!(code(&core, &join_n(over)), "54000");
    }
}

#[test]
fn input_rows_boundary_and_where_pushdown() {
    let path = unique_db_path("join-limits-input");
    let _g = CleanupGuard(path.clone());
    let core = core_of(seeded(&path, 2, 3), Some(JoinSizeLimits::new(2, 3, 100)));
    ok(&core, J2);
    drop(core);

    let storage = Storage::open(&path).expect("reopen");
    insert(&storage, "t1", "alice", 4, Visibility::Public);
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 3, 100)));
    // 可視 4 行 > 3 で拒否。
    assert_eq!(code(&core, J2), "54000");
    // WHERE で絞った後の行数で数える。
    ok(
        &core,
        "SELECT t1.c1 FROM t1 JOIN t2 ON t1.id = t2.id WHERE t1.c1 = 'v1' LIMIT 10",
    );
}

#[test]
fn other_tenants_rows_are_not_counted_as_input() {
    let path = unique_db_path("join-limits-rls");
    let _g = CleanupGuard(path.clone());
    let storage = seeded(&path, 2, 2);
    // bob の Private 行は alice に不可視。
    for id in 10..13 {
        insert(&storage, "t1", "bob", id, Visibility::Private);
    }
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 2, 100)));
    ok(&core, J2);
    drop(core);

    // 陽性対照: 同じ行数を alice 自身に置くと拒否される。
    let storage = Storage::open(&path).expect("reopen");
    insert(&storage, "t1", "alice", 10, Visibility::Public);
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 2, 100)));
    assert_eq!(code(&core, J2), "54000");
}

#[test]
fn output_rows_boundary_ignores_limit_and_counts_outer_padding() {
    let path = unique_db_path("join-limits-output");
    let _g = CleanupGuard(path.clone());
    let core = core_of(seeded(&path, 2, 3), Some(JoinSizeLimits::new(2, 100, 3)));
    ok(&core, J2);
    // LIMIT が小さくても結合カーディナリティで判定する側は、4 件目の一致で拒否される。
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    insert(&storage, "t1", "alice", 4, Visibility::Public);
    insert(&storage, "t2", "alice", 4, Visibility::Public);
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 100, 3)));
    assert_eq!(
        code(
            &core,
            "SELECT t1.c1 FROM t1 JOIN t2 ON t1.id = t2.id LIMIT 1"
        ),
        "54000"
    );
    // 外部結合の NULL 補完行も数える（t1 に一致の無い行を増やす）。
    let path2 = unique_db_path("join-limits-output-outer");
    let _g2 = CleanupGuard(path2.clone());
    let storage = seeded(&path2, 2, 0);
    for id in 1..=4 {
        insert(&storage, "t1", "alice", id, Visibility::Public);
    }
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 100, 3)));
    assert_eq!(
        code(
            &core,
            "SELECT t1.c1 FROM t1 LEFT JOIN t2 ON t1.id = t2.id LIMIT 10"
        ),
        "54000"
    );
}

#[test]
fn boundary_follows_the_configured_value() {
    let path = unique_db_path("join-limits-follow");
    let _g = CleanupGuard(path.clone());
    let storage = seeded(&path, 2, 4);
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 100, 3)));
    assert_eq!(code(&core, J2), "54000");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = core_of(storage, Some(JoinSizeLimits::new(2, 100, 4)));
    ok(&core, J2);
}
