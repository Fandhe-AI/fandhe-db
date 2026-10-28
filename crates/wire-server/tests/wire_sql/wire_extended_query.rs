//! wire-server の結合テスト（TASK-71・WIRE-8。Bind/Execute/Sync/Close/Flush の
//! 一律拒否）と、Issue #933・TASK-71・WIRE-11（Parse・Describe の受理）の結合
//! テストをあわせて持つ。
//!
//! WIRE-8: 認証成功後のセッションが拡張クエリプロトコル系メッセージ
//! （Bind/Execute/Sync/Close/Flush）を受信した場合に、SQLSTATE `0A000` の
//! ErrorResponse を返したうえで接続を閉じること（黙って無視しない・
//! ReadyForQuery を返さない・エラー後に接続を維持しない）を確認する。
//! Parse（'P'）・Describe（'D' 種別 S）は WIRE-11 で本モジュールの対象から
//! 外れ、`crate::extended_query` が受理する経路へ切り替わった
//! （下部の `#933`／WIRE-11 セクション参照）。

use crate::common;

use std::net::Shutdown;
use std::time::Duration;

use common::{
    authenticate_to_ready_for_query, expect_connection_closed, expect_error_response_with_sqlstate,
    expect_error_response_with_sqlstate_and_message, send_length_prefixed_message,
    spawn_server_accepting_one, spawn_server_with_accept_loop, write_user_store_file,
};

/// `engine: None`（`handle_connection_bounded` 経由の後方互換パス）では
/// Parse・Describe も SQL 表層に到達しようがないため、従来どおり `0A000` +
/// 切断のまま（`crate::extended_query` へは到達しない）。engine 接続時の
/// Parse／Describe の受理は `tests/wire11_parse_describe.rs` が担う
/// （Issue #933・TASK-71・WIRE-11）。
#[test]
fn wire11_parse_and_describe_without_engine_are_still_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);

    for type_byte in *b"PD" {
        let addr = spawn_server_accepting_one(&users_path);
        let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

        send_length_prefixed_message(&mut stream, type_byte, b"");
        stream.shutdown(Shutdown::Write).ok();

        expect_error_response_with_sqlstate(&mut stream, "0A000");
        expect_connection_closed(&mut stream);
    }
}

/// ポインタ: TASK-71・WIRE-8。拡張クエリプロトコル系の型（Bind/Execute/Sync/
/// Close/Flush）それぞれを単独送信しても `0A000` + 切断となること（Parse・
/// Describe は WIRE-11 セクションの別テストへ切り出し済み）。
#[test]
fn wire8_each_extended_message_alone_is_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);

    for type_byte in *b"BESCH" {
        let addr = spawn_server_accepting_one(&users_path);
        let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

        send_length_prefixed_message(&mut stream, type_byte, b"");
        stream.shutdown(Shutdown::Write).ok();

        expect_error_response_with_sqlstate(&mut stream, "0A000");
        expect_connection_closed(&mut stream);
    }
}

/// 対照テスト（過剰クローズの回帰防止）: 簡易クエリ（'Q'）は従来どおり
/// ErrorResponse ('E') → ReadyForQuery ('Z') を返し、接続は維持される
/// （WIRE-8 の対象外であることの確認）。
#[test]
fn simple_query_still_keeps_connection_open() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 簡易クエリの構造検証（`handshake.rs`）を満たす最小 body（終端 NUL のみ）。
    send_length_prefixed_message(&mut stream, b'Q', b"\0");

    expect_error_response_with_sqlstate(&mut stream, "0A000");

    // ReadyForQuery('Z') を読めること（接続が閉じられていないこと）。
    use std::io::Read;
    let mut header = [0u8; 1];
    stream
        .read_exact(&mut header)
        .expect("read ReadyForQuery type");
    assert_eq!(
        header[0], b'Z',
        "expected ReadyForQuery after simple query error"
    );
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
}

/// ポインタ: TASK-71・WIRE-8。未知の型バイトも従来どおり拒否＋切断される
/// （`protocol_dispatch::classify` の `Unknown` 分類が回帰しないことの確認）。
/// メッセージ文言が「拡張クエリプロトコル」を名乗らないこと（レビュー指摘の
/// 回帰防止）も合わせて確認する。
#[test]
fn wire8_unknown_message_type_is_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'?', b"");
    stream.shutdown(Shutdown::Write).ok();

    expect_error_response_with_sqlstate_and_message(
        &mut stream,
        "0A000",
        "this frontend message type is not supported",
    );
    expect_connection_closed(&mut stream);
}

/// ポインタ: TASK-71・WIRE-8。COPY・関数呼び出し系（`UnsupportedFeature`）は
/// `ExtendedQuery` と同じ SQLSTATE `0A000` で拒否されるが、メッセージ文言は
/// 「拡張クエリプロトコル」を名乗らず COPY/FunctionCall であることを正しく
/// 述べること（レビュー指摘: 文言が分類ごとに事実へ即すことの確認）。
#[test]
fn wire8_unsupported_feature_message_type_gets_distinct_message_text() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);

    for type_byte in *b"Fdcf" {
        let addr = spawn_server_accepting_one(&users_path);
        let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

        send_length_prefixed_message(&mut stream, type_byte, b"");
        stream.shutdown(Shutdown::Write).ok();

        expect_error_response_with_sqlstate_and_message(
            &mut stream,
            "0A000",
            "COPY and function call protocol messages are not supported",
        );
        expect_connection_closed(&mut stream);
    }
}

/// ポインタ: TASK-71・WIRE-8。拒否後に接続スロットが解放され、次の接続が
/// 受理されること（有界 lingering close が枠を永続占有しないことの確認）。
#[test]
fn wire8_rejection_releases_connection_slot() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_accept_loop(&users_path, 1, Duration::from_secs(5));

    let mut rejected = common::authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_length_prefixed_message(&mut rejected, b'B', b"");
    rejected.shutdown(Shutdown::Write).ok();
    expect_error_response_with_sqlstate(&mut rejected, "0A000");
    expect_connection_closed(&mut rejected);

    // 枠(1)の解放は「拒否された 1 本目の書き込み方向 shutdown による EOF」では
    // 証明できない（drain 開始時の EOF は `ConnectionSlot` 解放より先に観測され
    // うるレース）。`connect_after_slot_available` が SSLRequest の 'N' 応答まで
    // 到達するのをポーリングで待つことで、over-capacity 即時クローズ経路を
    // 実際に外れた（＝スロットが解放された）ことを決定的に確認する。
    let mut second = common::connect_after_slot_available(addr, Duration::from_secs(5));
    common::send_startup_message(&mut second, "alice", "db");
    let auth_code = common::read_auth_request_type(&mut second);
    assert_eq!(
        auth_code, 3,
        "slot must be free for the next connection to reach AuthenticationCleartextPassword"
    );
}
