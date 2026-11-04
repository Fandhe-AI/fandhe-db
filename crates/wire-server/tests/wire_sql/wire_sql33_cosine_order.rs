//! `<=>` がコサイン距離（`1 - cos`）の昇順、`<#>` が負の内積の昇順として
//! PostgreSQL wire プロトコル v3 の簡易クエリ経路で観測できることを固定する
//! 層 A 結合テスト（Issue #1993。ポインタ: SQL-33・MS-10）。
//!
//! 単位ベクトルだけのフィクスチャではコサインと内積で順位が一致するため、
//! 本ファイルは非正規化ベクトル（ゼロノルム行を含む）で両者の順位が分かれる
//! ことを使い、「`<=>` が内積のまま」という退行を wire 越しに検出する。
//! 実行契約そのもの（距離計算・NaN の末尾配置）は engine 側テストが確定済みで、
//! 本ファイルは wire フレーミング越しの再確認に徹する（`wire_scan_offset.rs` と同方針）。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// alice（tenant-a）から可視な Public 行 4 件。クエリ `[1,0]` に対し、コサイン距離は
/// id3=0／id2≈0.005／id1≈0.293／id4=NaN（ゼロノルム）、負の内積は
/// id1=-10／id2=-1／id3=-0.5／id4=0。
fn new_core_unnormalized() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql33-cosine-order");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let rows: [(u64, [f32; 2]); 4] = [
        (1, [10.0, 10.0]),
        (2, [1.0, 0.1]),
        (3, [0.5, 0.0]),
        (4, [0.0, 0.0]),
    ];
    for (id, emb) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("sql33-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 接続し、`sql` の全 `DataRow` を `n` 行読んで各行のセル（テキスト）を返す。
fn fetch_rows(stream: &mut std::net::TcpStream, sql: &str, n: usize) -> Vec<Vec<Option<String>>> {
    send_simple_query(stream, sql);
    let _columns = read_row_description(stream);
    let rows: Vec<_> = (0..n).map(|_| read_data_row(stream)).collect();
    assert_eq!(read_command_complete(stream), format!("SELECT {n}"));
    read_ready_for_query(stream);
    rows
}

fn ids(rows: &[Vec<Option<String>>]) -> Vec<String> {
    rows.iter()
        .map(|r| r[0].clone().expect("id must not be NULL"))
        .collect()
}

/// `<=>` はコサイン距離の昇順（ゼロノルム行は末尾）、`<#>` は負の内積の昇順。
#[test]
fn cosine_and_inner_product_orders_differ_over_wire() {
    let (core, _guard) = new_core_unnormalized();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    let cosine = fetch_rows(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[1,0]' LIMIT 4",
        4,
    );
    assert_eq!(ids(&cosine), ["3", "2", "1", "4"]);

    let inner = fetch_rows(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <#> '[1,0]' LIMIT 4",
        4,
    );
    assert_eq!(ids(&inner), ["1", "2", "3", "4"]);
}

/// 投影した `<=>` 距離列が近い順に単調非減少で、最近傍の距離が 0 になる。
/// ゼロノルム行は有限値の列挙から外し、末尾に置かれることだけを確認する。
#[test]
fn projected_cosine_distance_is_non_decreasing_over_wire() {
    let (core, _guard) = new_core_unnormalized();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    let rows = fetch_rows(
        &mut stream,
        "SELECT id, embedding <=> '[1,0]' AS d FROM docs \
         ORDER BY embedding <=> '[1,0]' LIMIT 4",
        4,
    );
    assert_eq!(ids(&rows), ["3", "2", "1", "4"]);
    let finite: Vec<f64> = rows[..3]
        .iter()
        .map(|r| {
            r[1].as_deref()
                .expect("distance must not be NULL")
                .parse::<f64>()
                .expect("finite distance text")
        })
        .collect();
    assert!(
        finite[0].abs() < 1e-6,
        "nearest distance must be 0: {finite:?}"
    );
    assert!(
        finite.windows(2).all(|w| w[0] <= w[1]),
        "distances must be non-decreasing: {finite:?}"
    );
    assert!(finite.iter().all(|d| (0.0..=2.0).contains(d)), "{finite:?}");
}
