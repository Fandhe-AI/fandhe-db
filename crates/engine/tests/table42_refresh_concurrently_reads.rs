//! `REFRESH MATERIALIZED VIEW`（CONCURRENTLY 有無）の実行中も読み取りが止まらないことの結合テスト
//! （TABLE-42 (4)・Issue #1666）。ポインタ: `docs/spec/04-behavior/data-model.md` の TABLE-42。
//!
//! feature `test-sync-points` 限定。REFRESH の commit 直前の同期点で更新スレッドを停止させ、
//! その間に別テナントの読み取りが即座に旧実体を返す（ブロックも 55000 もない）ことを、
//! sleep に依存せず `mpsc` で決定的に検証する。`table15_drop_table_concurrency.rs` の流儀に従う。
#![cfg(feature = "test-sync-points")]

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::test_sync::SyncPoint;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const WAIT: Duration = Duration::from_secs(30);

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn sql(core: &EngineCore, tenant: &str, text: &str) -> SqlOutcome {
    let mut s = session();
    core.execute_sql_in_session(&ctx(tenant), &mut s, text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn bodies(core: &EngineCore, tenant: &str) -> Vec<String> {
    match sql(core, tenant, "SELECT body FROM mv LIMIT 100") {
        SqlOutcome::Query(q) => {
            let mut v: Vec<String> = q
                .rows
                .iter()
                .map(|r| match &r.cells[0] {
                    Cell::Text(t) => t.clone(),
                    other => panic!("unexpected cell {other:?}"),
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn insert(storage: &Storage, tenant: &str, id: u64, body: &str) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx(tenant),
        id,
        Visibility::Private,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(body.to_string()),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("operation id"),
    )
    .expect("insert");
}

/// 同期点で更新スレッドを止め、その間の読み取りが旧実体を即座に返すことを検証する。
fn reads_during_refresh(refresh_sql: &str, label: &str) {
    let path = unique_db_path(label);
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    insert(&storage, "alice", 1, "a1");
    insert(&storage, "bob", 2, "b1");

    // hook は新しい同期点だけを扱い、他は素通しにする（評価側の読み取り同期点で止まらないため）。
    let (reached_tx, reached_rx) = mpsc::channel::<()>();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let resume_rx = Mutex::new(resume_rx);
    let reached_tx = Mutex::new(reached_tx);
    storage.set_sync_hook(Some(Arc::new(move |p| {
        if p == SyncPoint::RefreshMaterializedViewBeforeCommit {
            let _ = reached_tx.lock().expect("lock").send(());
            let _ = resume_rx.lock().expect("lock").recv_timeout(WAIT);
        }
    })));
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_materialization_contexts(vec![ctx("alice"), ctx("bob")]),
    );
    sql(
        &core,
        "alice",
        "CREATE MATERIALIZED VIEW mv AS SELECT body FROM docs",
    );
    sql(&core, "alice", "CREATE UNIQUE INDEX mv_body ON mv (body)");
    let before_alice = bodies(&core, "alice");
    let before_bob = bodies(&core, "bob");
    assert_eq!((before_alice.len(), before_bob.len()), (1, 1));
    // 基表の変更（REFRESH 前は見えない）。
    sql(
        &core,
        "alice",
        "INSERT INTO docs (id, embedding, body) VALUES (100, '[1.0, 0.0]', 'a-new') \
         USING OPERATION_ID 'op-new-100'",
    );
    assert_eq!(bodies(&core, "alice"), before_alice);

    let worker = {
        let core = Arc::clone(&core);
        let refresh_sql = refresh_sql.to_string();
        std::thread::spawn(move || {
            let mut s = session();
            core.execute_sql_in_session(&ctx("alice"), &mut s, &refresh_sql)
                .map(|_| ())
                .map_err(|e| e.wire_code().to_string())
        })
    };
    reached_rx
        .recv_timeout(WAIT)
        .expect("refresh must reach the pre-commit sync point");

    // 更新が commit 直前で止まっている間も、読み取りは旧実体を即座に返す。
    assert_eq!(bodies(&core, "alice"), before_alice);
    assert_eq!(bodies(&core, "bob"), before_bob);

    resume_tx.send(()).expect("resume");
    worker.join().expect("join").expect("refresh ok");
    assert!(bodies(&core, "alice").contains(&"a-new".to_string()));
    assert_eq!(bodies(&core, "bob"), before_bob);
}

#[test]
fn reads_are_not_blocked_during_refresh_concurrently() {
    reads_during_refresh("REFRESH MATERIALIZED VIEW CONCURRENTLY mv", "t42rc-conc");
}

#[test]
fn reads_are_not_blocked_during_plain_refresh() {
    reads_during_refresh("REFRESH MATERIALIZED VIEW mv", "t42rc-plain");
}
