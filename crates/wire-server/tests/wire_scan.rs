//! 広域取得（ソートなしのフィルタ取得。`SELECT ... [WHERE ...] LIMIT n`。
//! Issue #454）が PostgreSQL wire プロトコル v3 の簡易クエリ経路（生バイト
//! クライアント）で契約どおりの応答（`RowDescription`／`DataRow`／
//! `CommandComplete`／`ErrorResponse` の SQLSTATE）として観測できることを検証する
//! 結合テスト（層 A。`docs/design/three-client-e2e-harness.md` 参照）。
//!
//! 実行契約そのもの（早期終了・投影種別・`LIMIT` 範囲・取得モード非依存）は
//! `crates/engine/tests/sql_scan.rs`（in-process）が既に確定オラクルとして検証
//! 済みのため、本ファイルは同じ規則を **wire フレーミング** 越しに再確認する
//! ことに徹する（`wire_aggregate.rs`・`wire_search_mode.rs` と同方針）。
//! 無改造の実クライアント（psql／psycopg／pg）を使う 3 クライアント統合検証は
//! `tests/extended_syntax_e2e.rs`（`#[ignore]`）が層 B として担う。
//!
//! 本ファイルの seed に対する pg wire ↔ NoSQL 2 表層の行集合パリティ
//! （順序保証なし・早期終了・RLS 暗黙適用）は `nosql3_scan_wire_parity.rs`
//! （Issue #767）が別途固定する。

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

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設する。
/// 常に可視な Public 行: id=1 (tenant-a, "ja") / id=2 (tenant-b, "en") /
/// id=3 (tenant-c, "ja")。`lang="xx"` の Private 行（id=11, tenant-a）は
/// wire 認証したテナント自身（alice＝tenant-a）には可視（RLS-11・TASK-195。
/// read-your-writes）で、他テナントには不可視のまま。
fn new_core_scan_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-scan-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let public_rows: [(&str, u64, [f32; 2], &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja"),
        ("tenant-b", 2, [0.0, 1.0], "en"),
        ("tenant-c", 3, [-1.0, 0.0], "ja"),
    ];
    for (tenant, id, emb, lang) in public_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        11,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("xx".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("test-op-11")
            .expect("valid operation_id"),
    )
    .expect("insert private row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn connect_alice(addr: std::net::SocketAddr) -> std::net::TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, std::path::PathBuf) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (connect_alice(addr), users_path)
}

/// Issue #454: `ORDER BY`／`USING PLAN` を伴わない `SELECT ... LIMIT n` が
/// `RowDescription`／`DataRow`（複数行）／`CommandComplete` として返る。alice
/// （tenant-a）は自テナントの `Private` 行（id=11, lang="xx"）も可視になる
/// （RLS-11・TASK-195）。他テナントの `Private` 行が存在しないことは
/// `wire1_three_tenant_visibility_public_shared_own_private_visible` が別途
/// 固定する。
#[test]
fn bare_limit_scan_returns_rows_over_wire_including_own_tenant_private_row() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id, lang FROM docs LIMIT 10");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "lang"]);

    let mut seen: Vec<(String, String)> = Vec::new();
    for _ in 0..4 {
        let row = read_data_row(&mut stream);
        let id = row[0].clone().expect("id must not be NULL");
        let lang = row[1].clone().expect("lang must not be NULL");
        seen.push((id, lang));
    }
    let mut sorted_ids: Vec<String> = seen.iter().map(|(id, _)| id.clone()).collect();
    sorted_ids.sort();
    assert_eq!(sorted_ids, vec!["1", "11", "2", "3"]);
    assert!(
        seen.iter().any(|(id, lang)| id == "11" && lang == "xx"),
        "alice must observe her own tenant's Private row (read-your-writes), got {seen:?}"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 4");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `WHERE` による絞り込みが early-termination の対象行にも適用される。
#[test]
fn scan_with_where_filters_rows_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE lang = 'ja' LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id must not be NULL"));
    }
    ids.sort();
    assert_eq!(ids, vec!["1", "3"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `LIMIT` 未満の行数しか可視集合に無い場合、その件数だけを返す
/// （早期終了は行わない=全件読み切って `LIMIT` に満たない、という契約側の確認）。
#[test]
fn scan_limit_larger_than_visible_set_returns_all_visible_rows() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10000");
    let _columns = read_row_description(&mut stream);
    // alice の可視行は Public 3 件 + 自テナント Private 1 件の計 4 件
    // （RLS-11・TASK-195）。
    for _ in 0..4 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 4");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `LIMIT` 直後に `USING MODE` を付けた形は許可リスト外
/// （`42601`）。取得モードは広域取得に適用対象を持たない。
#[test]
fn scan_rejects_using_mode_suffix_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 10 USING MODE 'precision'",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `ORDER BY`／`USING PLAN`／`LIMIT` のいずれも無い形は許可リスト外
/// （`42601`）のまま（広域取得の追加が既存の必須句判定を緩めていないことの
/// 回帰確認）。
#[test]
fn plain_select_without_order_by_or_limit_still_rejected_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #922（SQL-27）: `EXPLAIN` の対象を広域取得へ拡大したため、bare LIMIT
/// scan の前置はもはや拒否されず受理される（受理テストへ反転。`sql::scan` は
/// ランキング段・索引を持たないため `scalar_plan: plain_scan`／
/// `access_path: full_scan` に固定）。
#[test]
fn explain_accepts_bare_limit_scan_with_query_plan() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "EXPLAIN SELECT id FROM docs LIMIT 10");

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["QUERY PLAN".to_string()]);

    for expected in ["scalar_plan: plain_scan", "access_path: full_scan"] {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].as_deref(), Some(expected));
    }

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

/// Issue #454: `SET search_mode` の直後でも広域取得の結果は変わらない
/// （取得モードからの独立性の wire 越し確認）。
#[test]
fn scan_result_unaffected_by_search_mode_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SET search_mode = 'precision'");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10000");
    let _columns = read_row_description(&mut stream);
    // alice の可視行は Public 3 件 + 自テナント Private 1 件の計 4 件
    // （RLS-11・TASK-195）。
    for _ in 0..4 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 4");
    read_ready_for_query(&mut stream);
}
