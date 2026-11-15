//! 集合演算の括弧なし末尾 `ORDER BY`（集合全体のソートキー。Issue #3266。ポインタ: SQL-29 (c)・
//! SQL-56・WIRE-12）が PostgreSQL wire v3 の簡易・拡張クエリ経路で契約どおりの `ErrorResponse`
//! （SQLSTATE）になることを固定する結合テスト。
//!
//! 構文と束縛を通ったキーは、ソート実行（Issue #3267）までは `0A000`（`42601` ではない）。
//! 解決できないキーは `42703`／`42P10`。拒否の後は `ReadyForQuery` が返り接続は継続する。
//! 分類の確定オラクルは `crates/engine/tests/sql_t01_39/sql29_set_operations.rs`（in-process）で、
//! 本ファイルは同じ規則を wire フレーミング越しに再確認する。

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

type Stream = std::net::TcpStream;

const BASE: &str = "SELECT lang FROM docs UNION SELECT lang FROM other_docs";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql29-set-order-by");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for table in ["docs", "other_docs"] {
        storage
            .create_table(&TableSchema::new(
                table,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                ],
            ))
            .expect("create table");
    }
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (table, id, lang) in [("docs", 1u64, "ja"), ("other_docs", 2u64, "en")] {
        engine::tenant::insert_typed_row(
            &storage,
            table,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (Stream, common::UserStoreFile) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        users_path,
    )
}

fn describe_statement_body(name: &str) -> Vec<u8> {
    let mut body = vec![b'S'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

/// `Z`（ReadyForQuery）までのメッセージを読み、最初の `ErrorResponse` の SQLSTATE を返す。
fn first_error_code_until_ready(s: &mut Stream) -> Option<String> {
    let mut code = None;
    loop {
        let (kind, body) = read_message(s);
        if kind == b'E' && code.is_none() {
            let text = String::from_utf8_lossy(&body).to_string();
            code = text
                .split('\0')
                .find_map(|f| f.strip_prefix('C').map(str::to_string));
        }
        if kind == b'Z' {
            return code;
        }
    }
}

/// 簡易クエリ: 関門の形は `0A000`、解決できないキーは `42703`／`42P10`。各エラーの後も接続は継続する。
// tracked: #3267
#[test]
fn set_operation_order_by_simple_query_classification_and_recovery() {
    let (core, _guard) = new_core();
    let (mut stream, _users_path) = spawn_with_alice(core);
    for (sql, expected) in [
        (format!("{BASE} ORDER BY 1;"), "0A000"),
        (format!("{BASE} ORDER BY lang DESC LIMIT 1"), "0A000"),
        (format!("{BASE} ORDER BY nope;"), "42703"),
        (format!("{BASE} ORDER BY 3"), "42P10"),
        (format!("{BASE} ORDER BY lower(lang)"), "0A000"),
        // 構文の誤りは `42601` のまま。
        (format!("{BASE} LIMIT 1 ORDER BY lang"), "42601"),
        (
            "SELECT lang FROM docs ORDER BY lang UNION SELECT lang FROM other_docs".to_string(),
            "42601",
        ),
    ] {
        send_simple_query(&mut stream, &sql);
        expect_error_response_with_sqlstate(&mut stream, expected);
        read_ready_for_query(&mut stream);
    }
    send_simple_query(&mut stream, BASE);
    let _columns = read_row_description(&mut stream);
    let _row1 = read_data_row(&mut stream);
    let _row2 = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Describe と Execute が同じ分類を返す（Describe と Execute の受理・拒否の一致）。
// tracked: #3267
#[test]
fn set_operation_order_by_extended_query_describe_matches_execute() {
    let (core, _guard) = new_core();
    let (mut stream, _users_path) = spawn_with_alice(core);
    for (i, (sql, expected)) in [
        (format!("{BASE} ORDER BY 1"), "0A000"),
        (format!("{BASE} ORDER BY nope"), "42703"),
    ]
    .into_iter()
    .enumerate()
    {
        // 名前付き文は Parse 成功後も残るため、反復ごとに別名にする。
        let name = format!("s{i}");
        // Parse → Describe(S) → Sync。
        send_length_prefixed_message(&mut stream, b'P', &parse_body(&name, &sql, 0));
        send_length_prefixed_message(&mut stream, b'D', &describe_statement_body(&name));
        send_length_prefixed_message(&mut stream, b'S', b"");
        assert_eq!(
            first_error_code_until_ready(&mut stream).as_deref(),
            Some(expected),
            "describe: {sql}"
        );
        // Parse → Bind → Execute → Sync。
        send_length_prefixed_message(&mut stream, b'P', &parse_body("", &sql, 0));
        send_length_prefixed_message(&mut stream, b'B', &bind_body("", ""));
        send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
        send_length_prefixed_message(&mut stream, b'S', b"");
        assert_eq!(
            first_error_code_until_ready(&mut stream).as_deref(),
            Some(expected),
            "execute: {sql}"
        );
    }
}
