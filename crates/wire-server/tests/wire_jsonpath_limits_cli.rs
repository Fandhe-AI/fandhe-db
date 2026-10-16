//! `--max-jsonpath-steps`／`--max-jsonpath-depth` opt-in
//! （Issue #2184。ポインタ: TABLE-25）をバイナリ子プロセスとして起動し、CLI 引数の
//! 受理・拒否（fail-closed）を外形的に検証する結合テスト。`wire_schema_limits_cli.rs`
//! と同じ流儀（実バイナリを `CARGO_BIN_EXE_wire-server` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを確認する）。
//!
//! - R1: 範囲内（下限 `1`・各ハード上限ちょうど）の値は `listening on` に到達する
//! - R2: 値の欠落は非 0 終了・stderr にフラグ名を含む
//! - R3: 重複指定は非 0 終了・stderr に "specified more than once" を含む
//! - R4: 範囲外（`0`・上限+1）・非数値（先頭 `+`・空白・英字混じり・空文字・
//!   全角数字）は非 0 終了・stderr にフラグ名を含む
//!
//! 設定値が評価の判定（`54000`）へ届くことは
//! `crates/engine/tests/jsonpath_eval.rs` の担当（本ファイルは CLI 解析の外形確認）。
//! ただし Issue #2185 以降は、`EngineCore::with_jsonpath_limits` で下げた上限が pg wire 越しの
//! `jsonb_path_*` の評価（`54000`）に効くことを R5 で確認する。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリを確保し `Drop` で削除するガード。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-jsonpath-limits-cli-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    fn users_path_str(&self) -> String {
        self.dir
            .join("users.txt")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.dir.display()
                );
            }
        }
    }
}

fn wait_for_listening(child: &mut Child) -> bool {
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if line.contains("listening on") => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// R1: 範囲内の値はいずれのフラグも起動を妨げない。
#[test]
fn in_range_values_start_listening() {
    let cases: [&[&str]; 5] = [
        &["--max-jsonpath-steps", "1"],
        &["--max-jsonpath-steps", "4000000"],
        &["--max-jsonpath-depth", "1"],
        &["--max-jsonpath-depth", "1024"],
        &["--max-jsonpath-steps", "1000", "--max-jsonpath-depth", "8"],
    ];
    for (idx, extra_args) in cases.iter().enumerate() {
        let fixture = TempFixtureDir::new(&format!("r1-{idx}"));
        let users_path = fixture.users_path_str();
        std::fs::write(&users_path, "").expect("write empty user store");
        let db_path = fixture.db_path_str();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(*extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");
        let listening = wait_for_listening(&mut child);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            listening,
            "args={extra_args:?}: expected to reach listening state"
        );
    }
}

/// R2〜R4: 値欠落・重複指定・範囲外・非数値はいずれも非 0 終了・stderr にフラグ名を含む。
#[test]
fn invalid_missing_duplicate_or_out_of_range_values_are_rejected() {
    let fixture = TempFixtureDir::new("r2-r4");
    let users_path = fixture.users_path_str();
    std::fs::write(&users_path, "").expect("write empty user store");
    let db_path = fixture.db_path_str();

    // (フラグ名, 上限+1)
    let flags = [
        ("--max-jsonpath-steps", "4000001"),
        ("--max-jsonpath-depth", "1025"),
    ];
    let mut cases: Vec<(&str, Vec<&str>, Option<&str>)> = Vec::new();
    for (flag, over) in flags {
        // R2: 値欠落。
        cases.push((flag, vec![flag], None));
        // R3: 重複指定。
        cases.push((
            flag,
            vec![flag, "5", flag, "6"],
            Some("specified more than once"),
        ));
        // R4: 範囲外・非数値。
        for bad in ["0", over, "+5", " 5", "5a", "", "１２", "-1"] {
            cases.push((flag, vec![flag, bad], None));
        }
    }

    for (expected_flag, extra_args, expected_msg) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(&extra_args)
            .output()
            .expect("spawn wire-server");
        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected_flag),
            "args={extra_args:?}: expected stderr to mention {expected_flag}, got: {stderr}"
        );
        if let Some(msg) = expected_msg {
            assert!(
                stderr.contains(msg),
                "args={extra_args:?}: expected stderr to contain {msg:?}, got: {stderr}"
            );
        }
    }
}

/// R5（Issue #2185）: 上限を下げて組み立てた engine では病的な path が `54000` になり、既定の
/// 上限では同じ文が成功する。`silent` を付けても `54000` は抑止されない。
#[test]
fn lowered_limits_reach_jsonb_path_evaluation_over_pg_wire() {
    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    use engine::core::EngineCore;
    use engine::jsonpath_limits::JsonPathLimits;
    use engine::kernel::CpuScalarProvider;
    use engine::storage::Storage;
    use std::sync::Arc;

    let run = |limits: Option<JsonPathLimits>, tag: &str| {
        let path = http_common::temp_db::unique_db_path(tag);
        let guard = http_common::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("docb", ColumnType::Jsonb, true),
                ],
            ))
            .expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let core = match limits {
            Some(l) => core.with_jsonpath_limits(l),
            None => core,
        };
        let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
        let addr = common::spawn_server_with_engine(&users_path, Arc::new(core));
        let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        common::send_simple_query(
            &mut alice,
            r#"INSERT INTO docs (id, embedding, docb) VALUES (1, '[0.1,0.2,0.3]', '{"a":[1,2,3]}') USING OPERATION_ID 'op-lim-1'"#,
        );
        assert_eq!(common::read_command_complete(&mut alice), "INSERT 0 1");
        common::read_ready_for_query(&mut alice);
        (alice, guard)
    };

    let sql = "SELECT jsonb_path_query_array(docb, '$.**.**.**') FROM docs WHERE id = 1 LIMIT 1";
    let (mut default_conn, _g1) = run(None, "wire-jsonpath-limits-default");
    common::send_simple_query(&mut default_conn, sql);
    let _ = common::read_row_description_with_oids(&mut default_conn);
    let _ = common::read_data_row(&mut default_conn);
    common::read_command_complete(&mut default_conn);
    common::read_ready_for_query(&mut default_conn);

    let (mut small_conn, _g2) = run(
        Some(JsonPathLimits::new(5, 4)),
        "wire-jsonpath-limits-small",
    );
    for stmt in [
        sql,
        "SELECT jsonb_path_query_array(docb, '$.**.**.**', '{}', true) FROM docs WHERE id = 1 LIMIT 1",
    ] {
        common::send_simple_query(&mut small_conn, stmt);
        common::expect_error_response_with_sqlstate(&mut small_conn, "54000");
        common::read_ready_for_query(&mut small_conn);
    }
}
