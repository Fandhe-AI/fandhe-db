//! `COPY ... FROM STDIN WITH (FORMAT binary)` の結合テスト（WIRE-23・Issue #1769・親 #1766 の
//! ポインタ。仕様は `docs/spec/04-behavior/wire-protocol.md` WIRE-23、上限は `index.md` INDEX-4）。
//!
//! フレーム層の単体境界は `src/copy_binary.rs`、型ごとの変換は `src/binary_param.rs` が担う。
//! 本ファイルは wire 越しに「binary の CopyInResponse」「束縛経路の共有」「INDEX-4 上限」
//! 「全体ロールバック」「明示トランザクションの失敗遷移」「列型の事前拒否」「テナント境界」
//! 「拡張クエリ経路」を固定する。`BatchLimits` は環境変数由来の揺らぎを避けるため 4 項目を明示する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const COPY_BIN: &str =
    "COPY bin_docs (id, n, label, blob, price, uid, flag) FROM STDIN WITH (FORMAT binary)";

const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

fn limits(files: usize, body: usize, total: usize, chunks: usize) -> BatchLimits {
    BatchLimits {
        max_files_per_batch: files,
        max_file_body_bytes: body,
        max_batch_total_bytes: total,
        max_batch_chunks: chunks,
    }
}

fn wide_limits() -> BatchLimits {
    limits(1000, 1024 * 1024, 4 * 1024 * 1024, 1000)
}

fn new_core(batch: BatchLimits) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire23-copy-binary");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "bin_docs",
            vec![
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("label", ColumnType::Text, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("uid", ColumnType::Uuid, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create bin_docs");
    storage
        .create_table(&TableSchema::new(
            "vec_docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create vec_docs");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(batch),
    );
    (core, guard)
}

fn connect_as(core: Arc<EngineCore>, user: &str) -> TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let pw = if user == "alice" {
        "pw-alice"
    } else {
        "pw-bob"
    };
    authenticate_to_ready_for_query(addr, user, pw)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    connect_as(core, "alice")
}

// --- binary ストリームの組み立て ---

fn header() -> Vec<u8> {
    let mut v = SIGNATURE.to_vec();
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v
}

fn tuple(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut v = (fields.len() as i16).to_be_bytes().to_vec();
    for f in fields {
        match f {
            None => v.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(b) => {
                v.extend_from_slice(&(b.len() as i32).to_be_bytes());
                v.extend_from_slice(b);
            }
        }
    }
    v
}

fn trailer() -> Vec<u8> {
    (-1i16).to_be_bytes().to_vec()
}

/// 0..10000 の非負整数の `numeric_send`（1 桁群）。
fn numeric_int(v: u16) -> Vec<u8> {
    let mut b = Vec::new();
    for w in [1i16, 0, 0, 0, v as i16] {
        b.extend_from_slice(&w.to_be_bytes());
    }
    b
}

/// 12.50 の `numeric_send`（ndigits=2・weight=0・dscale=2・桁群 12 / 5000）。
fn numeric_12_50() -> Vec<u8> {
    let mut b = Vec::new();
    for w in [2i16, 0, 0, 2, 12, 5000] {
        b.extend_from_slice(&w.to_be_bytes());
    }
    b
}

const UID: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
];

/// `bin_docs` の 1 タプル（`COPY_BIN` の列順）。
fn good_tuple(id: u16, n: i32) -> Vec<u8> {
    tuple(&[
        Some(numeric_int(id)),
        Some(n.to_be_bytes().to_vec()),
        Some(b"hello".to_vec()),
        Some(vec![0xde, 0xad]),
        Some(numeric_12_50()),
        Some(UID.to_vec()),
        Some(vec![1]),
    ])
}

fn null_tuple(id: u16) -> Vec<u8> {
    tuple(&[Some(numeric_int(id)), None, None, None, None, None, None])
}

fn stream_of(tuples: &[Vec<u8>], with_trailer: bool) -> Vec<u8> {
    let mut v = header();
    for t in tuples {
        v.extend_from_slice(t);
    }
    if with_trailer {
        v.extend_from_slice(&trailer());
    }
    v
}

// --- wire 補助 ---

fn send_copy_data(stream: &mut TcpStream, chunk: &[u8]) {
    send_length_prefixed_message(stream, b'd', chunk);
}

fn send_copy_done(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'c', b"");
}

fn start_copy(stream: &mut TcpStream, sql: &str) -> Vec<u8> {
    send_simple_query(stream, sql);
    let (ty, body) = read_message(stream);
    assert_eq!(
        ty,
        b'G',
        "expected CopyInResponse, got {}: {:?}",
        ty as char,
        String::from_utf8_lossy(&body)
    );
    body
}

fn error_text(stream: &mut TcpStream) -> String {
    let (ty, body) = read_message(stream);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    String::from_utf8_lossy(&body).into_owned()
}

fn expect_error(stream: &mut TcpStream, sqlstate: &str) -> String {
    let text = error_text(stream);
    assert!(text.contains(sqlstate), "expected {sqlstate}, got {text:?}");
    assert!(!text.contains("tenant-a"), "tenant id leaked: {text:?}");
    text
}

fn read_until_ready(stream: &mut TcpStream) -> u8 {
    loop {
        let (kind, body) = read_message(stream);
        if kind == b'Z' {
            return body[0];
        }
    }
}

fn simple(stream: &mut TcpStream, sql: &str) -> u8 {
    send_simple_query(stream, sql);
    read_until_ready(stream)
}

fn count_rows(stream: &mut TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM bin_docs LIMIT 1000");
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => rows += 1,
            b'Z' => return rows,
            _ => {}
        }
    }
}

/// 正常な COPY を 1 回流して `COPY n` を確認する。
fn copy_ok(stream: &mut TcpStream, data: &[u8], tag: &str) {
    start_copy(stream, COPY_BIN);
    send_copy_data(stream, data);
    send_copy_done(stream);
    assert_eq!(read_command_complete(stream), tag);
    read_ready_for_query(stream);
}

/// 失敗する COPY を流し、SQLSTATE・ReadyForQuery `'I'`・行 0 件・接続維持を確認する。
fn copy_fails(batch: BatchLimits, chunks: &[&[u8]], sqlstate: &str) {
    let (core, _g) = new_core(batch);
    let mut s = connect(core);
    start_copy(&mut s, COPY_BIN);
    for c in chunks {
        send_copy_data(&mut s, c);
    }
    send_copy_done(&mut s);
    expect_error(&mut s, sqlstate);
    assert_eq!(read_until_ready(&mut s), b'I');
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

// --- 正常系 ---

#[test]
fn copy_in_response_declares_binary_for_overall_and_every_column() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    let body = start_copy(&mut s, COPY_BIN);
    assert_eq!(body[0], 1, "overall format");
    assert_eq!(&body[1..3], 7i16.to_be_bytes());
    for i in 0..7 {
        assert_eq!(&body[3 + i * 2..5 + i * 2], 1i16.to_be_bytes());
    }
    send_copy_data(&mut s, &stream_of(&[good_tuple(1, 5)], true));
    send_copy_done(&mut s);
    assert_eq!(read_command_complete(&mut s), "COPY 1");
    read_ready_for_query(&mut s);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn values_are_converted_and_visible_to_select() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    copy_ok(
        &mut s,
        &stream_of(&[good_tuple(1, -7), null_tuple(2)], true),
        "COPY 2",
    );
    send_simple_query(
        &mut s,
        "SELECT id, n, label, blob, price, uid, flag FROM bin_docs LIMIT 10",
    );
    let _ = read_row_description(&mut s);
    let mut rows = [read_data_row(&mut s), read_data_row(&mut s)];
    assert_eq!(read_command_complete(&mut s), "SELECT 2");
    read_ready_for_query(&mut s);
    rows.sort();
    let some = |v: &str| Some(v.to_string());
    assert_eq!(rows[0][0], some("1"));
    assert_eq!(rows[0][1], some("-7"));
    assert_eq!(rows[0][2], some("hello"));
    assert_eq!(rows[0][3], some("\\xdead"));
    assert_eq!(rows[0][4], some("12.50"));
    assert_eq!(rows[0][5], some("11223344-5566-7788-99aa-bbccddeeff00"));
    assert_eq!(rows[0][6], some("t"));
    assert_eq!(rows[1][0], some("2"));
    assert!(rows[1][1..].iter().all(Option::is_none), "NULL fields");
    s.shutdown(Shutdown::Write).ok();
}

/// CopyData の分割位置（2 分割）に依存せず同じ結果になる。
#[test]
fn every_two_way_split_of_the_stream_is_accepted() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    let data = stream_of(&[good_tuple(1, 1), null_tuple(2)], true);
    for k in 1..data.len() {
        start_copy(&mut s, COPY_BIN);
        // id が重複しないよう、各反復で別の表を使う代わりに TRUNCATE で空に戻す。
        send_copy_data(&mut s, &data[..k]);
        send_copy_data(&mut s, &data[k..]);
        send_copy_done(&mut s);
        assert_eq!(read_command_complete(&mut s), "COPY 2", "split at {k}");
        read_ready_for_query(&mut s);
        assert_eq!(simple(&mut s, "TRUNCATE bin_docs"), b'I');
    }
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn byte_by_byte_delivery_is_accepted() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    start_copy(&mut s, COPY_BIN);
    for b in stream_of(&[good_tuple(1, 1), good_tuple(2, 2)], true) {
        send_copy_data(&mut s, &[b]);
    }
    send_copy_done(&mut s);
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    read_ready_for_query(&mut s);
    s.shutdown(Shutdown::Write).ok();
}

/// トレーラが無くてもタプル境界で CopyDone を受ければ成功（PostgreSQL と同じ）。
#[test]
fn missing_trailer_at_tuple_boundary_is_accepted() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    copy_ok(&mut s, &stream_of(&[good_tuple(1, 1)], false), "COPY 1");
    assert_eq!(count_rows(&mut s), 1);
    s.shutdown(Shutdown::Write).ok();
}

/// ヘッダとトレーラだけ（0 タプル）は text/csv の空入力と同じ契約（`22000`）。
#[test]
fn zero_tuples_is_rejected_like_empty_text_input() {
    copy_fails(wide_limits(), &[&stream_of(&[], true)], "22000");
}

// --- フレーム不正（22P04） ---

#[test]
fn bad_signature_is_22p04_with_zero_rows() {
    let mut data = stream_of(&[good_tuple(1, 1)], true);
    data[0] = b'X';
    copy_fails(wide_limits(), &[&data], "22P04");
}

#[test]
fn field_count_mismatch_is_22p04_with_zero_rows() {
    let mut data = header();
    data.extend_from_slice(&tuple(&[Some(numeric_int(1)), None]));
    copy_fails(wide_limits(), &[&data], "22P04");
}

#[test]
fn copy_done_in_the_middle_of_a_tuple_is_22p04() {
    let full = stream_of(&[good_tuple(1, 1)], false);
    copy_fails(wide_limits(), &[&full[..full.len() - 3]], "22P04");
}

#[test]
fn empty_stream_without_header_is_22p04() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    start_copy(&mut s, COPY_BIN);
    send_copy_done(&mut s);
    expect_error(&mut s, "22P04");
    assert_eq!(read_until_ready(&mut s), b'I');
    s.shutdown(Shutdown::Write).ok();
}

// --- 値の不正 ---

#[test]
fn int4_with_wrong_length_is_22p03_with_zero_rows() {
    let bad = tuple(&[
        Some(numeric_int(1)),
        Some(vec![0, 0, 1]),
        None,
        None,
        None,
        None,
        None,
    ]);
    copy_fails(wide_limits(), &[&stream_of(&[bad], true)], "22P03");
}

#[test]
fn invalid_utf8_and_nul_in_text_are_22p03() {
    for payload in [vec![0xff, 0xfe], b"a\0b".to_vec()] {
        let bad = tuple(&[
            Some(numeric_int(1)),
            None,
            Some(payload),
            None,
            None,
            None,
            None,
        ]);
        copy_fails(wide_limits(), &[&stream_of(&[bad], true)], "22P03");
    }
}

// --- 上限（INDEX-4）と全体ロールバック ---

#[test]
fn row_count_over_limit_is_54000_with_zero_rows() {
    let data = stream_of(
        &[good_tuple(1, 1), good_tuple(2, 2), good_tuple(3, 3)],
        true,
    );
    copy_fails(limits(2, 1024 * 1024, 1024 * 1024, 1000), &[&data], "54000");
}

#[test]
fn raw_byte_total_over_limit_is_54000_with_zero_rows() {
    let data = stream_of(&[good_tuple(1, 1)], true);
    copy_fails(limits(1000, 1024 * 1024, 40, 1000), &[&data], "54000");
}

#[test]
fn row_body_over_limit_is_54000_with_zero_rows() {
    let data = stream_of(&[good_tuple(1, 1)], true);
    copy_fails(limits(1000, 4, 1024 * 1024, 1000), &[&data], "54000");
}

/// 上限超過が CopyDone より前（CopyData 受信中）に検出され、以降の CopyData は読み捨てられる。
/// 検出が CopyDone 時点まで遅れる退行があれば、後続 CopyData が取り込み経路へ流れ discard 予算を
/// 消費しないため、`08P01` の予算超過に到達せず失敗する（`wire17_copy_limits.rs` と同じ観測方法）。
#[test]
fn limit_error_is_detected_before_copy_done_and_later_data_is_discarded() {
    let (core, _g) = new_core(limits(2, 1024 * 1024, 1024 * 1024, 1000));
    let mut s = connect(core);
    start_copy(&mut s, COPY_BIN);
    let data = stream_of(
        &[good_tuple(1, 1), good_tuple(2, 2), good_tuple(3, 3)],
        true,
    );
    send_copy_data(&mut s, &data);
    for _ in 0..=wire_server::limits::COPY_DISCARD_MAX_MESSAGES {
        send_copy_data(&mut s, b"x");
    }
    let text = error_text(&mut s);
    assert!(
        text.contains("08P01") && text.contains("COPY discard budget exceeded"),
        "expected discard budget error before CopyDone, got: {text:?}"
    );
    assert!(
        !text.contains("54000"),
        "limit error must wait for CopyDone: {text:?}"
    );
}

#[test]
fn bad_tuple_after_good_ones_rolls_back_everything() {
    let bad = tuple(&[
        Some(numeric_int(9)),
        Some(vec![1]),
        None,
        None,
        None,
        None,
        None,
    ]);
    let data = stream_of(&[good_tuple(1, 1), good_tuple(2, 2), bad], true);
    copy_fails(wide_limits(), &[&data], "22P03");
}

#[test]
fn copy_fail_after_good_tuples_leaves_no_rows() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    start_copy(&mut s, COPY_BIN);
    send_copy_data(
        &mut s,
        &stream_of(&[good_tuple(1, 1), good_tuple(2, 2)], false),
    );
    send_length_prefixed_message(&mut s, b'f', b"abort\0");
    expect_error(&mut s, "22000");
    assert_eq!(read_until_ready(&mut s), b'I');
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

// --- 明示トランザクション ---

#[test]
fn failure_inside_explicit_transaction_fails_it_and_rollback_removes_prior_insert() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    assert_eq!(
        simple(&mut s, "INSERT INTO bin_docs (id, n) VALUES (100, 1)"),
        b'T'
    );
    start_copy(&mut s, COPY_BIN);
    send_copy_data(&mut s, &stream_of(&[good_tuple(1, 1)], false)[..30]);
    send_copy_done(&mut s);
    expect_error(&mut s, "22P04");
    assert_eq!(read_until_ready(&mut s), b'E');
    assert_eq!(simple(&mut s, "ROLLBACK"), b'I');
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn success_inside_explicit_transaction_is_visible_after_commit() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    start_copy(&mut s, COPY_BIN);
    send_copy_data(&mut s, &stream_of(&[good_tuple(1, 1)], true));
    send_copy_done(&mut s);
    assert_eq!(read_command_complete(&mut s), "COPY 1");
    assert_eq!(read_until_ready(&mut s), b'T');
    assert_eq!(simple(&mut s, "COMMIT"), b'I');
    assert_eq!(count_rows(&mut s), 1);
    s.shutdown(Shutdown::Write).ok();
}

// --- 列型・オプション ---

#[test]
fn unsupported_column_type_is_0a000_before_copy_in_response() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    send_simple_query(
        &mut s,
        "COPY vec_docs (id, embedding) FROM STDIN WITH (FORMAT binary)",
    );
    // 'G' ではなく ErrorResponse が最初に来る（copy mode へ入らない）。
    expect_error(&mut s, "0A000");
    assert_eq!(read_until_ready(&mut s), b'I');
    assert_eq!(simple(&mut s, "SELECT id FROM vec_docs LIMIT 1"), b'I');
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn binary_with_row_format_options_is_rejected() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);
    for (sql, state) in [
        (
            "COPY bin_docs (id, n) FROM STDIN WITH (FORMAT binary, HEADER)",
            "0A000",
        ),
        (
            "COPY bin_docs (id, n) FROM STDIN WITH (FORMAT binary, DELIMITER ',')",
            "0A000",
        ),
    ] {
        send_simple_query(&mut s, sql);
        expect_error(&mut s, state);
        assert_eq!(read_until_ready(&mut s), b'I');
    }
    s.shutdown(Shutdown::Write).ok();
}

// --- テナント境界 ---

#[test]
fn rows_loaded_by_one_tenant_are_invisible_to_another() {
    let (core, _g) = new_core(wide_limits());
    let mut alice = connect_as(core.clone(), "alice");
    copy_ok(&mut alice, &stream_of(&[good_tuple(1, 1)], true), "COPY 1");
    let mut bob = connect_as(core, "bob");
    assert_eq!(count_rows(&mut bob), 0);
    assert_eq!(count_rows(&mut alice), 1);
    alice.shutdown(Shutdown::Write).ok();
    bob.shutdown(Shutdown::Write).ok();
}

// --- 拡張クエリ ---

fn parse_bind_execute(stream: &mut TcpStream, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(stream, b'B', &bind_body("p", ""));
    let (k, _) = read_message(stream);
    assert_eq!(k, b'1');
    let (k, _) = read_message(stream);
    assert_eq!(k, b'2');
    send_length_prefixed_message(stream, b'E', &execute_body("p", 0));
    send_length_prefixed_message(stream, b'H', b"");
}

#[test]
fn extended_query_copy_binary_succeeds_and_recovers_after_failure() {
    let (core, _g) = new_core(wide_limits());
    let mut s = connect(core);

    parse_bind_execute(&mut s, COPY_BIN);
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'G');
    assert_eq!(body[0], 1);
    send_copy_data(
        &mut s,
        &stream_of(&[good_tuple(1, 1), good_tuple(2, 2)], true),
    );
    send_copy_done(&mut s);
    assert_eq!(read_command_complete(&mut s), "COPY 2");
    send_length_prefixed_message(&mut s, b'S', b"");
    assert_eq!(read_until_ready(&mut s), b'I');
    assert_eq!(count_rows(&mut s), 2);

    // 失敗（フレーム不正）→ Sync で回復する。
    parse_bind_execute(&mut s, COPY_BIN);
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'G');
    send_copy_data(&mut s, b"not a binary copy stream");
    send_copy_done(&mut s);
    expect_error(&mut s, "22P04");
    send_length_prefixed_message(&mut s, b'S', b"");
    assert_eq!(read_until_ready(&mut s), b'I');
    assert_eq!(count_rows(&mut s), 2);
    s.shutdown(Shutdown::Write).ok();
}
