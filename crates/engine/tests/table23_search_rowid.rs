//! 検索系の行参照を物理 rowid へ移した後も、従来方式の表で kNN の結果（`id` と同点順序）が
//! 変わらないことを公開 API から固定する結合テスト（Issue #2195。ビヘイビア: TABLE-23・
//! SEARCH-1。設計記録: `docs/design/rowid-primary-key-separation.md`）。
//!
//! 役割: 同一ベクトル（同点スコア）の行を意図的に含む決定的コーパスで、SQL の kNN と
//! Rust API の `search` が「スコア降順・同点は `id` 昇順」の固定順序を返すことを検査する。
//! 採番方式の表の拒否は `src/catalog/rowid_gate_tests.rs` が持つ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid tenant")
}

/// id 7・3・9・5 が同一ベクトル（クエリと同点）、id 20 だけが遠い。投入順は id 非昇順。
fn core_with_ties(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let c = ctx();
    for (id, v) in [
        (20u64, [0.0f32, 1.0]),
        (7, [1.0, 0.0]),
        (3, [1.0, 0.0]),
        (9, [1.0, 0.0]),
        (5, [1.0, 0.0]),
    ] {
        let op_id = OperationId::parse(&format!("seed-{id}")).expect("op id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &c,
            id,
            Visibility::Public,
            &[Value::Vector(v.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

#[test]
fn table23_sql_knn_ties_are_ordered_by_id_ascending() {
    let (core, _g) = core_with_ties("table23-search-sql");
    let mut session = SessionState::default();
    let out = core
        .execute_sql_in_session(
            &ctx(),
            &mut session,
            "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 5",
        )
        .expect("knn");
    let SqlOutcome::Query(result) = out else {
        panic!("expected rows");
    };
    let ids: Vec<u64> = result
        .rows
        .iter()
        .map(|r| match r.cells[0] {
            Cell::Integer(i) => i,
            ref other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![3, 5, 7, 9, 20]);
}

#[test]
fn table23_rust_api_search_ties_are_ordered_by_id_ascending() {
    let (core, _g) = core_with_ties("table23-search-api");
    let hits = core.search(&ctx(), "docs", &[1.0, 0.0], 5).expect("search");
    let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![3, 5, 7, 9, 20]);
    // 先頭 4 件は同点スコア（ビット列まで一致）。
    let first = hits[0].score.to_bits();
    assert!(hits[..4].iter().all(|h| h.score.to_bits() == first));
}
