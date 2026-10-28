//! wire-server の結合テスト（TASK-153、対象ビヘイビア: ERR-1。ポインタ:
//! `docs/spec/05-tasks.md` TASK-153・`docs/spec/04-behavior/error-format.md`）。
//!
//! `crate::error_response`（`crates/wire-server/src/error_response.rs`）の横断写像が
//! 実 TCP 経由で到達可能な各エラー分類について、`ErrorResponse`（'E'）の
//! フィールド構成（severity/SQLSTATE/message）どおりに送出されることを検証する。
//! 各分類の「どういう条件で発生するか」自体は既存の専用結合テスト
//! （`wire_auth.rs`・`wire_extended_query.rs`・`wire_framing.rs`・`wire_limits.rs`・
//! `wire1_simple_query.rs`）が個別に回帰保護済みであり、本ファイルはそれらと発生
//! 条件を重複させず「送出される ErrorResponse のフィールド構成」の横断検証に
//! 専念する。
//!
//! wire から到達不能な分類（`INSERT` 系の `23502`/`23505`/`22023`・集計関数の
//! `22003` 等。wire は `INSERT` を受理しない。`crate::simple_query` モジュール
//! コメント参照）は本ファイルの対象外とし、`crates/wire-server/src/
//! error_response.rs` の `ErrorClass::ALL` 全件ユニットテストが写像自体の網羅性を
//! 担保する。

use crate::common;

use crate::temp_db;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// `ErrorResponse`（'E'）を読み、`(severity, sqlstate, message)` フィールドを
/// 機械的に抽出する。`D`（detail）フィールドが含まれないことも合わせて確認する
/// （通常応答（`crate::error_response::encode`）は `D` を付けない契約。
/// ERR-5・TASK-153・`crate::error_response` モジュールドキュメント参照。
/// 本ファイルで検証する各エラー分類〔28P01・42601・0A000・08P01・53300・
/// 42P01〕はいずれも通常応答経路であり緊急応答（`RECOVER-6`。層 A 検証は
/// `wire_emergency_response.rs`）とは別経路である）。
fn read_error_response_fields(stream: &mut TcpStream) -> (String, String, String) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let declared_len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; declared_len - 4];
    stream.read_exact(&mut body).expect("read body");

    let mut severity = None;
    let mut sqlstate = None;
    let mut message = None;
    let mut has_detail = false;
    let mut idx = 0usize;
    while idx < body.len() {
        let tag = body[idx];
        if tag == 0 {
            break; // フィールド終端
        }
        let value_start = idx + 1;
        let nul_offset = body[value_start..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated field value");
        let value_end = value_start + nul_offset;
        let value = std::str::from_utf8(&body[value_start..value_end])
            .expect("utf8 field value")
            .to_string();
        match tag {
            b'S' => severity = Some(value),
            b'C' => sqlstate = Some(value),
            b'M' => message = Some(value),
            b'D' => has_detail = true,
            _ => {}
        }
        idx = value_end + 1;
    }

    assert!(
        !has_detail,
        "normal ErrorResponse must not carry a D (detail) field"
    );
    (
        severity.expect("S field present"),
        sqlstate.expect("C field present"),
        message.expect("M field present"),
    )
}

fn assert_error_response(stream: &mut TcpStream, expected_sqlstate: &str) {
    assert_error_response_with_severity(stream, expected_sqlstate, "ERROR");
}

/// `severity` を明示指定する版。`53300`（接続数上限超過）は接続そのものを閉じる
/// 拒否応答のため、既存実装（`crate::limits::reject_too_many_connections`）が
/// severity=`FATAL` を用いる（`ERROR` ではなく接続断を伴う致命度であることを示す
/// 既存の意図的な区別。本テストはこの既存挙動を変更しない）。
fn assert_error_response_with_severity(
    stream: &mut TcpStream,
    expected_sqlstate: &str,
    expected_severity: &str,
) {
    let (severity, sqlstate, message) = read_error_response_fields(stream);
    assert_eq!(
        severity, expected_severity,
        "severity mismatch (sqlstate={expected_sqlstate})"
    );
    assert_eq!(sqlstate, expected_sqlstate);
    assert!(
        !message.is_empty(),
        "message must be non-empty (sqlstate={expected_sqlstate})"
    );
}

/// 28P01（auth_invalid）: 誤ったパスワードでの認証失敗。
#[test]
fn err1_auth_invalid_password_returns_28p01_fields() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");
    send_password_message(&mut stream, "wrong-password");

    assert_error_response(&mut stream, "28P01");
}

/// 0A000（feature_not_supported）: 拡張クエリプロトコル（Parse）の受信。
#[test]
fn err1_extended_query_returns_0a000_fields() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'P', &[]);

    assert_error_response(&mut stream, "0A000");
}

/// 08P01（protocol_violation）: StartupMessage の length がフレーミング最小値未満。
#[test]
fn err1_malformed_startup_returns_08p01_fields() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut msg = Vec::new();
    msg.extend_from_slice(&4i32.to_be_bytes()); // MIN_STARTUP_LEN(8) 未満
    stream.write_all(&msg).expect("send undersized startup");

    assert_error_response(&mut stream, "08P01");
}

/// 53300（too_many_connections）: 接続数上限超過。
#[test]
fn err1_connection_limit_returns_53300_fields() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_accept_loop(&users_path, 1, Duration::from_secs(5));

    let _held = TcpStream::connect(addr).expect("connect first (holds the only slot)");
    std::thread::sleep(Duration::from_millis(50));

    let mut second = TcpStream::connect(addr).expect("connect second");
    assert_error_response_with_severity(&mut second, "53300", "FATAL");
}

/// `docs` テーブル（`embedding VECTOR(3)`）のみを持つ `EngineCore` を新設する
/// （42601/42P01 検証には検索対象データは不要）。
fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-err1-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 42601（unsupported_sql_syntax）: `UPDATE` は Issue #865（SQL-17・TASK-191。
/// 単一行・`id` 完全一致形）・Issue #871（SQL-19・TASK-192。述語形）で
/// 実行結線されたため、構造として正しい `UPDATE` 文（`USING OPERATION_ID` 付き）
/// は許可リスト外の statement 種別としては拒否されなくなった（この点は本ケースが
/// 元々検証していた「許可リストに存在しない statement 種別」の対象から外れた）。
/// `SET id = ...`（疑似列の書き換え）は `sql::parser::bind_update` が構造上受理
/// しない形として引き続き `42601` を返す契約（`crates/engine/tests/
/// sql_update_single_row.rs::set_id_is_rejected_as_unsupported_syntax` が確定
/// オラクル）のため、本ケースはそちらへ差し替えて `42601` 分類の wire 経由確認を
/// 維持する。
#[test]
fn err1_update_returns_42601_fields() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // `operation_id` 必須化ガード（TASK-92・RECOVER-1・`23502`）より先に
    // このケースが検証したい `42601`（`SET id = ...` の構造拒否）へ到達させる
    // ため `USING OPERATION_ID` を付与する。単一行・述語形いずれの
    // `ValidatedUpdateForm` 経由でも `sql::parser::bind_update` の疑似列拒否は
    // 変わらない（Issue #871 で `EngineCore::execute_predicate_update_form` の
    // `Single` 腕が実行結線に切り替わった後も本ケースの `42601` は
    // `bind_update` 由来のまま）。
    send_simple_query(
        &mut stream,
        "UPDATE docs SET id = 2 WHERE id = 1 USING OPERATION_ID 'err1-update-set-id'",
    );

    assert_error_response(&mut stream, "42601");
}

/// 42P01（table_not_found）: カタログ未存在テーブルへの `SELECT`。
#[test]
fn err1_undefined_table_returns_42p01_fields() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT id FROM missing_table ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
    );

    assert_error_response(&mut stream, "42P01");
}
