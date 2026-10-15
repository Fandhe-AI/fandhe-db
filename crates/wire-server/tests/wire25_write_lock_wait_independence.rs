//! 接続資源上限 3 種と書き込みロック待機（`--write-lock-wait-ms`）が互いに連動しないことの
//! 結合テスト（Issue #2455。ポインタ: WIRE-25・SQL-46。親 TASK-338）。
//!
//! 3 つの面で固定する。
//! - I1: 解決用の純関数が互いの値に影響されない（`connection_limits_opt::resolve` と
//!   `write_lock_wait_opt::resolve`）
//! - I2: wire 越しの挙動。書き込みロック待機（0.5 秒）の超過は読み取りタイムアウト（3 秒）を
//!   待たずに `55P03` で返り、アイドル接続は書き込みロック待機では切られず読み取りタイムアウトで
//!   切られる。向きを「読み取りタイムアウト > 書き込みロック待機」にしているのは、逆向きだと
//!   holder がアイドルで切断され自動 ROLLBACK で writer が解放されて待機側が成功し、別の事柄を
//!   検証してしまうため（設計メモ `docs/design/connection-limits-cli.md` の連動の節）
//! - I3: 起動時に両者を比較する警告を出さない（読み取りタイムアウトは
//!   `transaction_limits_opt::duration_over_write_lock_wait_warning` の比較に関与しない）

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::{Storage, DEFAULT_WRITE_LOCK_WAIT};
use wire_server::connection_limits_opt::{self, ConnectionLimits, ConnectionLimitsRawFlags};
use wire_server::limits::SessionLimits;
use wire_server::write_lock_wait_opt;

use common::*;

/// I1: 3 値を既定値以外にしても書き込みロック待機は既定のまま。逆に待機を変えても 3 値は既定のまま。
#[test]
fn resolvers_do_not_influence_each_other() {
    let read_timeouts = [None, Some("1"), Some("600")];
    let max_conns = [None, Some("1"), Some("256")];
    let max_stmts = [None, Some("1"), Some("1024")];
    for rt in read_timeouts {
        for mc in max_conns {
            for ms in max_stmts {
                let limits = connection_limits_opt::resolve(ConnectionLimitsRawFlags {
                    read_timeout_secs: rt,
                    max_connections: mc,
                    max_prepared_statements: ms,
                })
                .expect("in range");
                // 接続資源上限をどう解決しても、書き込みロック待機の未指定は既定のまま。
                assert_eq!(
                    write_lock_wait_opt::resolve(None).expect("default"),
                    DEFAULT_WRITE_LOCK_WAIT
                );
                // 指定した項目だけが既定から変わり、他は既定のまま。
                let d = ConnectionLimits::default();
                assert_eq!(limits.read_timeout == d.read_timeout, rt.is_none());
                assert_eq!(
                    limits.max_connections == d.max_connections,
                    mc.is_none() || mc == Some("64")
                );
                assert_eq!(
                    limits.session.max_prepared_statements == d.session.max_prepared_statements,
                    ms.is_none()
                );
            }
        }
    }
    for wait in ["100", "500", "60000"] {
        let resolved = write_lock_wait_opt::resolve(Some(wait)).expect("in range");
        assert_eq!(
            resolved,
            Duration::from_millis(wait.parse().expect("number"))
        );
        // 書き込みロック待機を変えても接続資源上限は既定（30 秒・64・64）のまま。
        let limits =
            connection_limits_opt::resolve(ConnectionLimitsRawFlags::default()).expect("default");
        assert_eq!(limits, ConnectionLimits::default());
        assert_eq!(limits.session, SessionLimits::default());
    }
}

/// I2: 書き込みロック待機 0.5 秒・読み取りタイムアウト 3 秒。待機側は 0.5 秒前後で `55P03`
/// になり ReadyForQuery に戻る（3 秒まで待たされない）。アイドル接続は 0.5 秒では切られず、
/// 読み取りタイムアウトで切られる。
#[test]
fn write_lock_wait_and_read_timeout_act_independently() {
    let path = temp_db::unique_db_path("wire25-independence");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let wait = write_lock_wait_opt::resolve(Some("500")).expect("in range");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait),
    );
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
        ("carol", "tenant-c", "staple-battery"),
    ]);
    let (addr, _limiter) = spawn_server_with_engine_and_limits(
        &users_path,
        core,
        8,
        Duration::from_secs(3),
        SessionLimits::default(),
    );
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    let mut idle = authenticate_to_ready_for_query(addr, "carol", "staple-battery");
    let idle_started = Instant::now();

    let insert = |id: u32| {
        format!(
            "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-2455-{id}'"
        )
    };

    send_simple_query(&mut a, "BEGIN");
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, &insert(1));
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    read_ready_for_query(&mut a);

    let started = Instant::now();
    send_simple_query(&mut b, &insert(2));
    expect_error_response_with_sqlstate(&mut b, "55P03");
    read_ready_for_query(&mut b);
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(2),
        "55P03 must follow the write lock wait (0.5s), not the read timeout (3s): {waited:?}"
    );

    // アイドル接続はこの時点（書き込みロック待機の経過後）ではまだ開いている。
    idle.set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set probe timeout");
    let mut buf = [0u8; 8];
    match idle.read(&mut buf) {
        Err(ref e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        other => panic!("idle connection must survive the write lock wait, got {other:?}"),
    }
    assert!(
        idle_started.elapsed() < Duration::from_secs(3),
        "probe must run inside the read timeout window"
    );

    // 読み取りタイムアウトを過ぎると 0 バイトのまま EOF になる。
    idle.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    match idle.read(&mut buf) {
        Ok(0) => {}
        other => panic!("idle connection must be closed by the read timeout, got {other:?}"),
    }
    assert!(
        idle_started.elapsed() >= Duration::from_millis(2500),
        "idle connection must not be closed before the read timeout: {:?}",
        idle_started.elapsed()
    );
}

/// I3: 起動時に読み取りタイムアウトと書き込みロック待機を比較する警告を出さず、
/// どちらの大小関係でも `listening on` に到達する。
#[test]
fn startup_emits_no_comparison_warning_between_read_timeout_and_write_lock_wait() {
    for (read_timeout, wait_ms) in [("1", "60000"), ("600", "100")] {
        let fixture = TempFixtureDir::new("wire25-i3");
        write_empty_user_store(&fixture.users_path_str());
        let users = fixture.users_path_str();
        let db = fixture.db_path_str();
        let mut server = SpawnedServer::spawn(&[
            "--users",
            &users,
            "--db",
            &db,
            "--bind",
            "127.0.0.1:0",
            "--read-timeout-secs",
            read_timeout,
            "--write-lock-wait-ms",
            wait_ms,
        ]);
        let addr = server.wait_for_listening(Instant::now() + Duration::from_secs(10));
        assert!(
            addr.is_some(),
            "read-timeout={read_timeout} write-lock-wait-ms={wait_ms}: must reach listening"
        );
        let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(2));
        for line in &lines {
            assert!(
                !line.contains("--read-timeout-secs"),
                "no startup warning may compare the read timeout with the write lock wait: {line}"
            );
            assert!(
                !line.to_lowercase().contains("warning"),
                "unexpected startup warning: {line}"
            );
        }
    }
}
