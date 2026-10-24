//! 結果行数上限（`--max-result-rows` 相当。SQL-34・WIRE-15 のポインタ。Issue #2211）の
//! wire 層での境界確認。
//!
//! 既定構成（`with_result_limits` を呼ばない＝フラグ未指定と同じ 10,000）で、`LIMIT` 省略の
//! SELECT が 10,000 行ちょうどなら成功し、10,001 行なら `54000` で拒否されて DataRow が
//! 1 件も送られず、接続が維持されることを simple query と extended（`max_rows = 0`）で固定する。
//! 意味論の確定オラクルは `crates/engine/tests/sql34_*.rs`。Execute の `max_rows` 分割送出・
//! キャンセルは #1798 の担当でここでは扱わない。CLI 解析は `wire_result_limits_cli.rs`。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

/// 認証ユーザー alice のテナント（`connect` のユーザーストアと一致させる）。
const TENANT: &str = "tenant-a";

type Reply = (usize, Option<String>, Option<String>);

/// `t` に `rows` 行を持つ engine を作る。`cap` が `Some` のときだけ上限を注入する。
fn new_core(rows: i64, cap: Option<usize>) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-result-rows-cap");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(n) = cap {
        core = core.with_result_limits(ResultLimits::new(n));
    }
    let ctx = PolicyContext::with_visibilities(TENANT, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER)",
    )
    .expect("ddl");
    let ids: Vec<i64> = (1..=rows).collect();
    // 1 文あたりのバッチ上限（64 件）を超えない単位で投入する。
    for chunk in ids.chunks(60) {
        let values: Vec<String> = chunk.iter().map(|i| format!("({i}, {i})")).collect();
        core.execute_insert_sql(
            &ctx,
            &format!("INSERT INTO t (id, k) VALUES {}", values.join(", ")),
        )
        .expect("seed");
    }
    (Arc::new(core), guard)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", TENANT, "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// 応答を ReadyForQuery まで読み、(DataRow 件数, ErrorResponse の本文, CommandComplete タグ) を返す。
fn drain(stream: &mut TcpStream) -> Reply {
    let (mut data, mut err, mut tag) = (0, None, None);
    loop {
        let (ty, body) = read_message(stream);
        match ty {
            b'D' => data += 1,
            b'E' => err = Some(String::from_utf8_lossy(&body).into_owned()),
            b'C' => {
                tag = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                )
            }
            b'Z' => return (data, err, tag),
            _ => {}
        }
    }
}

fn simple(stream: &mut TcpStream, sql: &str) -> Reply {
    send_simple_query(stream, sql);
    drain(stream)
}

fn extended(stream: &mut TcpStream, sql: &str) -> Reply {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(stream, b'B', &bind_body("", ""));
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(stream, b'S', &[]);
    drain(stream)
}

fn assert_over_cap(r: Reply, what: &str) {
    assert_eq!(r.0, 0, "{what}: DataRow must not be sent");
    let text =
        r.1.unwrap_or_else(|| panic!("{what}: expected ErrorResponse"));
    assert!(text.contains("54000"), "{what}: {text:?}");
    assert!(!text.contains(TENANT), "{what}: tenant leaked: {text:?}");
}

/// 既定 10,000: ちょうどは成功し、+1 は 54000 で DataRow 0 件、接続は維持される。
#[test]
fn default_cap_boundary_over_simple_and_extended() {
    let (core, _g) = new_core(10_001, None);
    let mut s = connect(core);

    let ok = simple(&mut s, "SELECT id FROM t WHERE id <= 10000");
    assert_eq!(ok.0, 10_000);
    assert_eq!(ok.2.as_deref(), Some("SELECT 10000"));
    assert!(ok.1.is_none());

    for sql in ["SELECT id FROM t", "SELECT id FROM t LIMIT ALL"] {
        assert_over_cap(simple(&mut s, sql), sql);
    }
    assert_over_cap(extended(&mut s, "SELECT id FROM t"), "extended");

    let after = simple(&mut s, "SELECT 1");
    assert_eq!((after.0, after.1.is_none()), (1, true));
}

/// 下げた設定値（3）でも simple の境界が同じ形で成り立つ。
#[test]
fn lowered_cap_boundary() {
    let (core, _g) = new_core(4, Some(3));
    let mut s = connect(core);
    let ok = simple(&mut s, "SELECT id FROM t WHERE id <= 3");
    assert_eq!((ok.0, ok.2.as_deref()), (3, Some("SELECT 3")));
    assert_over_cap(simple(&mut s, "SELECT id FROM t"), "lowered");
}
