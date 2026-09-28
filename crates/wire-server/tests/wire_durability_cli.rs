//! `--durability` opt-in（Issue #850）をバイナリ子プロセスとして起動し、
//! CLI 引数の受理・拒否（fail-closed）・既定不変・起動ログ警告を外形的に
//! 検証する結合テスト。
//!
//! `tests/wire_search_engine_cli.rs`（Issue #656）・
//! `tests/wire_fault_injection_cli.rs`（Issue #705）と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! - R1: `immediate`／`none` のいずれも `listening on` に到達すること
//! - R2: 未指定と `--durability immediate` の 2 プロセスで、同一 C1 クエリの
//!   受信バイト列（`RowDescription`〜`ReadyForQuery`）が完全一致すること
//!   （既定経路がコード上も無変更であることの実測面の裏付け）
//! - R3: 不正値・値欠落・重複指定は非 0 終了・stderr に `--durability` を
//!   含む説明が出ること
//! - R4: `--durability none` のみ起動ログに `WARNING` を含む行が出現し、
//!   未指定・`--durability immediate` では出現しないこと
//! - R5: `--durability none --search-engine hnsw` の組合せでも
//!   `listening on` に到達すること（非既定 durability × ANN opt-in の
//!   組合せセルが動作することの外形確認）
//!
//! `Storage::write_txn_creations`（engine クレート `#[cfg(test)]` 限定）が
//! 実際に fsync 相当を省略したかどうかの非 vacuous な実測は本ファイルの対象
//! 外（wire-server 側からは観測できない。durability 別の性能実測は別 Issue の
//! 担当）。

#[path = "common/mod.rs"]
mod common;

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::authenticate_to_ready_for_query;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）を確保し、`Drop` で確実に削除するガード
/// （`wire_search_engine_cli.rs::TempFixtureDir` と同型）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-durability-cli-{label}-{}-{}-{}",
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
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 空のユーザーストア（R1・R3・R5 は認証まで到達する必要がないため）。
fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// `alice:tenant-a:<phc>` の 1 行を持つユーザーストア（R2・R4 の認証・簡易
/// クエリ発行に使う）。パスワードハッシュは `wire-server hash-password`
/// サブコマンドを子プロセスとして呼び、平文をテストコード内に決め打ちしない
/// （`wire_search_engine_cli.rs::write_user_store_with_alice` と同型）。
fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw-alice\n")
        .expect("write password to stdin");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout)
        .expect("utf-8 phc")
        .trim()
        .to_string();
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// `docs(embedding VECTOR(2))` テーブルへ 1 行だけ投入した DB ファイルを
/// 用意する（R2・R4 の C1 クエリが決定的な 1 行を返すようにするため）。
/// 呼び出し元が子プロセスを起動する **前に** `Storage` を drop すること
/// （redb はファイルロックを持つため、開いたまま子プロセスの
/// `EngineCore::open` を呼ぶと衝突する）。
fn seed_single_row_db(db_path: &str) {
    let storage = Storage::open(db_path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0])],
        &OperationId::parse("seed-op").expect("valid operation_id"),
    )
    .expect("insert row");
    drop(storage);
}

/// 認証後、簡易クエリ 1 文を送って `RowDescription`〜`ReadyForQuery` までの
/// 生バイト列をそのまま返す（`wire_search_engine_cli.rs::
/// run_c1_query_and_collect_bytes` と同型）。
fn run_c1_query_and_collect_bytes(addr: std::net::SocketAddr) -> Vec<u8> {
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1";
    let mut q_body = Vec::new();
    q_body.extend_from_slice(sql.as_bytes());
    q_body.push(0);
    let mut q_msg = Vec::new();
    q_msg.push(b'Q');
    q_msg.extend_from_slice(&((q_body.len() + 4) as i32).to_be_bytes());
    q_msg.extend_from_slice(&q_body);
    stream.write_all(&q_msg).expect("send simple query");

    let mut collected = Vec::new();
    for _ in 0..4 {
        let mut frame_header = [0u8; 5];
        stream
            .read_exact(&mut frame_header)
            .expect("read frame header");
        let len = i32::from_be_bytes([
            frame_header[1],
            frame_header[2],
            frame_header[3],
            frame_header[4],
        ]);
        let body_len = (len as usize).saturating_sub(4);
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).expect("read frame body");
        collected.extend_from_slice(&frame_header);
        collected.extend_from_slice(&body);
    }
    collected
}

/// R1: `immediate`／`none` いずれも起動が拒否されず `listening on` に到達
/// すること。
#[test]
fn both_tokens_start_listening() {
    for token in ["immediate", "none"] {
        let fixture = TempFixtureDir::new(&format!("r1-{token}"));
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
                "--durability",
                token,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            listening,
            "token={token}: expected to reach listening state"
        );
    }
}

/// R2: 未指定と `--durability immediate` は同一クエリに対し完全に同一の
/// 受信バイト列を返す（既定経路がコード上もビット同一であることの実測面の
/// 裏付け）。
#[test]
fn unset_and_immediate_token_produce_identical_wire_bytes() {
    let mut outputs: Vec<Vec<u8>> = Vec::new();

    for extra_args in [Vec::<&str>::new(), vec!["--durability", "immediate"]] {
        let fixture = TempFixtureDir::new("r2");
        let users_path = fixture.users_path_str();
        write_user_store_with_alice(&users_path);
        let db_path = fixture.db_path_str();
        seed_single_row_db(&db_path);

        let mut args = vec![
            "--users".to_string(),
            users_path,
            "--db".to_string(),
            db_path,
            "--bind".to_string(),
            "127.0.0.1:0".to_string(),
        ];
        args.extend(extra_args.iter().map(|s| s.to_string()));

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let (addr, _lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
        let bytes = run_c1_query_and_collect_bytes(addr);

        let _ = child.kill();
        let _ = child.wait();

        outputs.push(bytes);
    }

    assert_eq!(
        outputs[0], outputs[1],
        "unset --durability and --durability immediate must produce identical wire bytes"
    );
}

/// R3: 不正値・値欠落・重複指定はいずれも fail-closed（非 0 終了・
/// `--durability` を含む stderr）。
#[test]
fn invalid_or_missing_or_duplicate_durability_arg_is_rejected() {
    let fixture = TempFixtureDir::new("r3");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: [&[&str]; 6] = [
        &["--durability", "sync"],
        &["--durability", "Immediate"],
        &["--durability", "IMMEDIATE"],
        &["--durability", ""],
        // 直後の既知フラグ名をそのまま値として食い、閉じた語彙のいずれとも
        // 一致しないため拒否されるケース（`--search-engine` と同じ
        // 「次トークンを無条件で値とみなす」仕様の確認）。
        &["--durability", "--bind"],
        // 重複指定（D2）。
        &["--durability", "immediate", "--durability", "none"],
    ];

    for extra_args in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
            ])
            .args(extra_args)
            .output()
            .expect("spawn wire-server");

        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--durability"),
            "args={extra_args:?}: expected stderr to mention --durability, got: {stderr}"
        );
    }

    // 値欠落単体（末尾に `--durability` だけを置くケース）。
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--durability",
        ])
        .output()
        .expect("spawn wire-server");
    assert!(!output.status.success(), "missing value must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--durability"),
        "expected stderr to mention --durability, got: {stderr}"
    );
}

/// R4: `--durability none` のみ起動ログに `WARNING` を含む行が出現し、
/// 未指定・`--durability immediate` では出現しないこと。
#[test]
fn only_none_durability_emits_warning_line() {
    let cases: [(&str, Option<&str>); 3] = [
        ("r4-unset", None),
        ("r4-immediate", Some("immediate")),
        ("r4-none", Some("none")),
    ];

    for (label, token) in cases {
        let fixture = TempFixtureDir::new(label);
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let mut args = vec![
            "--users".to_string(),
            users_path,
            "--db".to_string(),
            db_path,
            "--bind".to_string(),
            "127.0.0.1:0".to_string(),
        ];
        if let Some(tok) = token {
            args.push("--durability".to_string());
            args.push(tok.to_string());
        }

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let (_addr, lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();

        let has_warning = lines.iter().any(|l| l.contains("WARNING"));
        match token {
            Some("none") => assert!(
                has_warning,
                "label={label}: expected a WARNING line, got lines={lines:?}"
            ),
            _ => assert!(
                !has_warning,
                "label={label}: expected no WARNING line, got lines={lines:?}"
            ),
        }
    }
}

/// R5: `--durability none --search-engine hnsw` の組合せでも `listening on`
/// に到達すること（非既定 durability × ANN opt-in セルの外形確認。
/// `EXPLAIN` は `--planner-endpoint` が必須で本ファイルでは検証しない
/// —— `main.rs` 内 `#[cfg(test)] mod tests` の
/// `open_engine_core_non_default_durability_with_hnsw_engine_sets_hnsw_kind`
/// が担う）。
#[test]
fn none_durability_with_hnsw_engine_starts_listening() {
    let fixture = TempFixtureDir::new("r5");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--durability",
            "none",
            "--search-engine",
            "hnsw",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        listening,
        "expected to reach listening state with --durability none --search-engine hnsw"
    );
}
