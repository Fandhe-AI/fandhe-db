//! 応答バッファ組み立て（Issue #481・`crate::response_buffer::ResponseBuffer`）
//! の結合テスト。実サーバー（`common::spawn_server_with_engine`）経由で
//! 大量行・大容量本文の `SELECT` を発行し、受信バイト列（`RowDescription`・
//! `DataRow`×N・`CommandComplete`・`ReadyForQuery`）が個別送出時と同じ内容で
//! 届くことを確認する。`crates/wire-server/src/response_buffer.rs` の単体
//! テストはバッファ組み立てロジック自体（フレーム境界分割・巻き戻し）を
//! 検証しており、本ファイルは wire 経由の end-to-end（実ソケットの読み取り
//! 境界に依存しないこと）を確認する立場を持つ。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// `docs`（`embedding VECTOR(2)` + `body TEXT`）へ `rows` 件を投入した
/// `EngineCore` を作る。各行の `body` は `body_len` バイトの決定的な文字列
/// （NUL・改行を含まない ASCII）とする。
fn new_core_with_rows(rows: u64, body_len: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-bulk-response");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    for id in 1..=rows {
        // すべて同一近傍方向（クエリと同一ベクトル）にすることで、
        // `ORDER BY <=> LIMIT rows` が全件を返すことを保証する（Top-k の
        // 順序に依存しないアサーションにするため id 集合の一致のみを見る）。
        let body: String = "x".repeat(body_len);
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![1.0, 0.0]), Value::Text(body)],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 1,000 行（`id, body`）の `SELECT ... LIMIT 1000` が `crossdb-bench.md` の
/// `bulk_knn_k1000` 相当の広域取得を模す。応答バッファ（Issue #481）を
/// 経由しても、行数・各行の内容・`CommandComplete`・`ReadyForQuery` が
/// これまでどおり届くことを固定する。
#[test]
fn wire_bulk_select_returns_all_rows_with_correct_content() {
    const ROWS: u64 = 1_000;
    let (core, _guard) = new_core_with_rows(ROWS, 32);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        &format!("SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT {ROWS}"),
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "body"]);

    let mut seen_ids = std::collections::BTreeSet::new();
    for _ in 0..ROWS {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 2);
        let id = row[0].clone().expect("id is not null");
        let body = row[1].clone().expect("body is not null");
        assert_eq!(
            body.len(),
            32,
            "body must round-trip at its original length"
        );
        seen_ids.insert(id);
    }
    assert_eq!(
        seen_ids.len(),
        ROWS as usize,
        "all {ROWS} distinct ids must be observed exactly once"
    );

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, format!("SELECT {ROWS}"));
    read_ready_for_query(&mut stream);
}

/// 応答合計が `crate::limits::MAX_RESPONSE_BUFFER_BYTES`（1 MiB）を跨ぐ
/// ケース（本文を大きくして合計約 2 MiB にする）でも、分割送出
/// （`ResponseBuffer::push_frame` のフラッシュ閾値）を経て全行が欠落なく
/// 届くことを確認する。`common` のクライアントヘルパーは `read_exact` で
/// フレーム単位に読むため、送出が 1 回か複数回かに関わらず内容は同一になる
/// ―― 本テストは「複数回の write に分割されても壊れない」ことを保証する。
#[test]
fn wire_bulk_select_spanning_response_buffer_cap_delivers_all_rows() {
    const ROWS: u64 = 50;
    const BODY_LEN: usize = 40_000; // 50 * 40,000 ≈ 2,000,000 bytes > 1 MiB cap
    let (core, _guard) = new_core_with_rows(ROWS, BODY_LEN);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        &format!("SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT {ROWS}"),
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "body"]);

    let mut seen_ids = std::collections::BTreeSet::new();
    for _ in 0..ROWS {
        let row = read_data_row(&mut stream);
        let id = row[0].clone().expect("id is not null");
        let body = row[1].clone().expect("body is not null");
        assert_eq!(body.len(), BODY_LEN);
        seen_ids.insert(id);
    }
    assert_eq!(seen_ids.len(), ROWS as usize);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, format!("SELECT {ROWS}"));
    read_ready_for_query(&mut stream);
}

/// `crossdb-bench.md` の `bulk_knn_k1000`（`id`+`body`・k=1000）相当の wire 段
/// レイテンシを手動計測するための `#[ignore]` テスト（Issue #481 受け入れ条件
/// (a)）。`cargo test --release -p fandhe-db-wire-server --test wire_bulk_response -- \
/// --ignored --nocapture wire_bulk_select_latency_measurement` で実行する。
/// CI では実行しない（本 doc の数値は共有開発環境の参考値であり、専有環境
/// 再実測はオーナー作業という本リポの既存方針 `docs/design/
/// benchmark-judgement-policy.md` に従う）。before/after の比較手順・実測値は
/// `docs/design/wire-response-buffering.md` に記録する。
#[test]
#[ignore]
fn wire_bulk_select_latency_measurement() {
    const ROWS: u64 = 1_000;
    const BODY_LEN: usize = 200; // crossdb-bench.md の `generate_body_text` 目安（約 200B）に合わせる
    const ROUNDS: usize = 20;

    let (core, _guard) = new_core_with_rows(ROWS, BODY_LEN);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql = format!("SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT {ROWS}");
    let mut micros = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = std::time::Instant::now();
        send_simple_query(&mut stream, &sql);
        let _columns = read_row_description(&mut stream);
        for _ in 0..ROWS {
            let _row = read_data_row(&mut stream);
        }
        let _tag = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
        micros.push(start.elapsed().as_micros());
    }
    micros.sort_unstable();
    let median = micros[micros.len() / 2];
    let min = micros[0];
    eprintln!(
        "wire_bulk_select_latency_measurement: rows={ROWS} body_len={BODY_LEN} rounds={ROUNDS} \
         min={min}us median={median}us raw={micros:?}"
    );
}
