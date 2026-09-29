//! `--fault-inject post-commit-panic`（Issue #705。テスト専用・feature
//! `fault-injection` 限定）を実バイナリ子プロセス（`CARGO_BIN_EXE_wire-server`）
//! として起動し、CLI 引数の受理・拒否（fail-closed）と実際の発火（commit 後
//! panic → 緊急応答の同期送出 → abort）を外形的に検証する結合テスト。
//! `tests/wire_search_engine_cli.rs`（Issue #656）・
//! `tests/wire_emergency_response.rs`（TASK-97・TASK-153・ERR-5）と同じ流儀
//! （`crate::fault_injection` の判定純関数そのものの単体テストは
//! `crates/wire-server/src/fault_injection.rs` 側が担う。本ファイルは wire
//! フレーミング・実プロセス越しの観測に専念する）。
//!
//! **カバレッジ経路**: 既定ビルド（feature 無効）で `--fault-inject`
//! が未知引数として拒否されることを固定する
//! [`default_build_rejects_fault_inject_flag_as_unknown_argument`] は
//! `cfg(not(feature = "fault-injection"))` の下でのみコンパイルされ、
//! `--all-features` で走る `make test`／`rust-ci` の対象には入らない。
//! この既定ビルド側の拒否契約は `make test-default-build`（`make ci` に
//! 含む）・`.github/workflows/ci.yml` の独立ジョブ `test-default-build`
//! が常時実行して検査する（Issue #705・#715・#716。
//! `docs/design/three-client-e2e-harness.md`「Issue #705」節参照）。

#[cfg(not(feature = "fault-injection"))]
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

// `tests/common/mod.rs` は feature `fault-injection` 有効時の `armed` モジュール
// のみが使う。`#[path]` はこのファイル自身のディレクトリ（`tests/`）基準で
// 解決されるため、`mod armed` の内側に再宣言すると `tests/armed/` という
// 実在しないディレクトリ基準で解決されてしまう（`armed` はファイルを持たない
// インライン module のため）。そのため top level で宣言し、`armed` からは
// `super::common` として参照する。
#[cfg(feature = "fault-injection")]
#[path = "common/mod.rs"]
mod common;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB ファイルの
/// 置き場）。`wire_search_engine_cli.rs::TempFixtureDir` と同型
/// （Issue #172 の一意性対策を踏襲）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-fault-injection-cli-{label}-{}-{}-{}",
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

/// 空のユーザーストア（CLI 引数の受理・拒否だけを見るテストは認証まで
/// 到達する必要がないため）。
fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// 既定ビルド（feature `fault-injection` 無効）では `--fault-inject` 自体が
/// 存在せず、`main.rs` の `other =>` 分岐で未知引数として拒否される
/// （fail-closed。受け入れ条件 1）。
#[cfg(not(feature = "fault-injection"))]
#[test]
fn default_build_rejects_fault_inject_flag_as_unknown_argument() {
    let fixture = TempFixtureDir::new("default-reject");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--fault-inject",
            "post-commit-panic",
        ])
        .output()
        .expect("spawn wire-server");

    assert!(
        !output.status.success(),
        "expected non-zero exit in default (non-fault-injection) build"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown argument: --fault-inject"),
        "expected stderr to reject --fault-inject as an unknown argument, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// feature `fault-injection` 有効ビルド限定のテスト群（受け入れ条件 2・3）。
// ---------------------------------------------------------------------------

#[cfg(feature = "fault-injection")]
mod armed {
    use super::TempFixtureDir;
    use std::io::{Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use engine::catalog::{ColumnDef, ColumnType, TableSchema};
    use engine::core::EngineCore;
    use engine::kernel::CpuScalarProvider;
    use engine::policy::PolicyContext;
    use engine::storage::{Storage, Visibility};

    use super::common::{
        self, authenticate_to_ready_for_query, expect_error_response_with_sqlstate,
        read_command_complete, read_ready_for_query, send_simple_query,
    };
    use super::write_empty_user_store;

    /// `wire-server hash-password` サブコマンドを子プロセスとして呼び、
    /// `alice:tenant-a:<phc>` の 1 行を持つユーザーストアを用意する
    /// （`wire_search_engine_cli.rs::write_user_store_with_alice` と同型。
    /// 平文パスワードをテストコード内に決め打ちしないため実経路を通す）。
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

    /// `docs(embedding VECTOR(3))` の空テーブルを用意する（子プロセス起動前に
    /// `Storage` を drop すること。redb のファイルロック回避）。
    fn create_empty_docs_table(db_path: &str) {
        let storage = Storage::open(db_path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");
        drop(storage);
    }

    fn insert_sql(id: u64, op_id: &str) -> String {
        format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID '{op_id}'"
        )
    }

    fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return status;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn assert_aborted(status: std::process::ExitStatus) {
        use std::os::unix::process::ExitStatusExt as _;
        assert!(
            !status.success(),
            "child must not exit successfully; status={status:?}"
        );
        assert_eq!(
            status.signal(),
            Some(6),
            "child must be terminated by SIGABRT (std::process::abort); status={status:?}"
        );
    }

    /// body 中の 1 フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出
    /// する（`wire_emergency_response.rs::find_field` と同型）。
    fn find_field(body: &[u8], tag: u8) -> Option<String> {
        let mut idx = 0;
        while idx < body.len() {
            let this_tag = *body.get(idx)?;
            if this_tag == 0 {
                return None;
            }
            let value_start = idx + 1;
            let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
            let value_end = value_start + nul_offset;
            if this_tag == tag {
                let bytes = body.get(value_start..value_end)?;
                return std::str::from_utf8(bytes).ok().map(str::to_string);
            }
            idx = value_end + 1;
        }
        None
    }

    /// `E`（ErrorResponse）フレームを宣言長どおり読み切り、緊急応答の
    /// `S`/`C`/`M`/`D` フィールドが期待どおりであることを確認する。
    fn assert_emergency_response(stream: &mut std::net::TcpStream) {
        let mut header = [0u8; 1];
        stream.read_exact(&mut header).expect("read type byte");
        assert_eq!(header[0], b'E', "expected ErrorResponse type byte");

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read length field");
        let declared_len = i32::from_be_bytes(len_buf);
        assert!(
            declared_len >= 4,
            "declared length must cover at least the length field itself; got {declared_len}"
        );
        let body_len = declared_len as usize - 4;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).expect("read body");

        assert_eq!(
            find_field(&body, b'S').as_deref(),
            Some("ERROR"),
            "severity"
        );
        assert_eq!(
            find_field(&body, b'C').as_deref(),
            Some("XX000"),
            "sqlstate"
        );
        assert_eq!(
            find_field(&body, b'M').as_deref(),
            Some("internal error"),
            "message"
        );
        assert_eq!(
            find_field(&body, b'D').as_deref(),
            Some("state=may_be_committed"),
            "detail (ERR-5)"
        );
        assert_eq!(
            body.iter().filter(|&&b| b == b'D').count(),
            1,
            "D field must appear exactly once"
        );

        // 唯一の応答であること（追加バイトが来ない）を確認する。接続断
        // （`Err`）も「追加バイトなし」として扱う
        // （`wire_emergency_response.rs` と同じ判定方針）。
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .expect("set short read timeout");
        let mut extra = [0u8; 16];
        let n = stream.read(&mut extra).unwrap_or(0);
        assert_eq!(
            n, 0,
            "no additional bytes must follow the emergency response (sole-response contract)"
        );
    }

    fn spawn_armed(fixture: &TempFixtureDir) -> Child {
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &fixture.users_path_str(),
                "--db",
                &fixture.db_path_str(),
                "--bind",
                "127.0.0.1:0",
                "--fault-inject",
                "post-commit-panic",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server")
    }

    /// 値欠落・不正値・重複指定はいずれも fail-closed（非 0 終了・
    /// `--fault-inject` を含む stderr）。
    #[test]
    fn flag_rejects_missing_unknown_and_duplicate_values() {
        let fixture = TempFixtureDir::new("r-reject");
        let users_path = fixture.users_path_str();
        write_empty_user_store(&users_path);
        let db_path = fixture.db_path_str();

        let cases: [&[&str]; 3] = [
            &["--fault-inject", "bogus"],
            &["--fault-inject", "POST-COMMIT-PANIC"],
            &[
                "--fault-inject",
                "post-commit-panic",
                "--fault-inject",
                "post-commit-panic",
            ],
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
                stderr.contains("--fault-inject"),
                "args={extra_args:?}: expected stderr to mention --fault-inject, got: {stderr}"
            );
        }

        // 値欠落単体（末尾に `--fault-inject` だけを置くケース）。
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &users_path,
                "--db",
                &db_path,
                "--bind",
                "127.0.0.1:0",
                "--fault-inject",
            ])
            .output()
            .expect("spawn wire-server");
        assert!(!output.status.success(), "missing value must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--fault-inject"),
            "expected stderr to mention --fault-inject, got: {stderr}"
        );
    }

    /// feature ビルドでもフラグ無しなら不活性（既定ビルドと同じ通常応答）。
    #[test]
    fn feature_build_without_flag_inserts_normally() {
        let fixture = TempFixtureDir::new("unarmed");
        write_user_store_with_alice(&fixture.users_path_str());
        create_empty_docs_table(&fixture.db_path_str());

        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                &fixture.users_path_str(),
                "--db",
                &fixture.db_path_str(),
                "--bind",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let (addr, lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
        assert!(
            !lines.iter().any(|l| l.contains("fault injection armed")),
            "unarmed process must not announce fault injection; lines={lines:?}"
        );

        let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        send_simple_query(&mut stream, &insert_sql(1, "fi-unarmed-op-1"));
        let tag = read_command_complete(&mut stream);
        assert_eq!(tag, "INSERT 0 1");
        read_ready_for_query(&mut stream);

        let _ = child.kill();
        let _ = child.wait();
    }

    /// 受け入れ条件 2: arm 済み・commit 成功 `INSERT` の直後に緊急応答
    /// （`C`=`XX000`・`D`=`state=may_be_committed`）を送出し、プロセスが
    /// abort すること。commit 自体は成功しているため再オープン後も行は
    /// 可視のまま。
    #[test]
    fn armed_post_commit_panic_sends_emergency_response_then_aborts() {
        let fixture = TempFixtureDir::new("armed-fire");
        write_user_store_with_alice(&fixture.users_path_str());
        create_empty_docs_table(&fixture.db_path_str());
        let db_path = fixture.db_path_str();

        let mut child = spawn_armed(&fixture);
        let (addr, lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
        let armed_idx = lines
            .iter()
            .position(|l| l.contains("fault injection armed"))
            .expect("expected 'fault injection armed' line before 'listening on'");
        let listening_idx = lines
            .iter()
            .position(|l| l.contains("listening on"))
            .expect("expected 'listening on' line");
        assert!(
            armed_idx < listening_idx,
            "fault injection must be armed before listen is announced; lines={lines:?}"
        );

        let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        send_simple_query(&mut stream, &insert_sql(1, "fi-armed-op-1"));
        assert_emergency_response(&mut stream);

        let status = wait_for_exit(&mut child, Duration::from_secs(30));
        assert_aborted(status);

        // commit 自体は成功しているため、再オープン後も行が可視であること
        // （`wire_emergency_response.rs` と同じ確認方法）。
        let storage = Storage::open(&db_path).expect("reopen storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let read_ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let result = core
            .execute_sql(
                &read_ctx,
                "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
            )
            .expect("select should succeed");
        assert_eq!(
            result.rows.len(),
            1,
            "the committed row must remain visible after the emergency-abort path"
        );
    }

    /// arm は「commit 成功 INSERT」以外（SELECT・operation_id 省略で拒否された
    /// INSERT）では消費されず、同一接続で後続の成功 INSERT が来て初めて発火
    /// する（arm-once の非 vacuous 性）。
    #[test]
    fn armed_flag_ignores_select_and_rejected_insert_then_fires_on_committed_insert() {
        let fixture = TempFixtureDir::new("armed-hold");
        write_user_store_with_alice(&fixture.users_path_str());
        create_empty_docs_table(&fixture.db_path_str());

        let mut child = spawn_armed(&fixture);
        let (addr, _lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));

        let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

        // (1) 読み取り専用の広域取得（Issue #454）は通常応答で、接続は維持
        // される（Insert ではないため arm を消費しない）。
        send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 5");
        let _ = common::read_row_description(&mut stream);
        let tag = read_command_complete(&mut stream);
        assert_eq!(tag, "SELECT 0");
        read_ready_for_query(&mut stream);

        // (2) `USING OPERATION_ID` 省略の INSERT は commit 前に拒否される
        // （`23502`。`wire_insert_operation_id.rs` と同じ契約）。
        send_simple_query(
            &mut stream,
            "INSERT INTO docs (id, embedding) VALUES (1, '[0.1,0.2,0.3]')",
        );
        expect_error_response_with_sqlstate(&mut stream, "23502");
        read_ready_for_query(&mut stream);

        // (3) 正しい INSERT は commit 成功 → 緊急応答 → abort。
        send_simple_query(&mut stream, &insert_sql(1, "fi-armed-hold-op-1"));
        assert_emergency_response(&mut stream);

        let status = wait_for_exit(&mut child, Duration::from_secs(30));
        assert_aborted(status);
    }

    /// Issue #1175: 暗黙トランザクション（`INSERT; INSERT`）では、途中の `INSERT` は
    /// commit 前なので障害注入の対象にならず、最後の文の commit 成功の直後にだけ緊急応答
    /// （`D`=`state=may_be_committed`）が 1 回送出されてプロセスが abort する。commit は
    /// 成功しているため、再オープン後は両方の行が可視のまま。
    #[test]
    fn armed_post_commit_panic_fires_once_after_the_implicit_transaction_commit() {
        let fixture = TempFixtureDir::new("armed-implicit");
        write_user_store_with_alice(&fixture.users_path_str());
        create_empty_docs_table(&fixture.db_path_str());
        let db_path = fixture.db_path_str();

        let mut child = spawn_armed(&fixture);
        let (addr, _lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));

        let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        send_simple_query(
            &mut stream,
            &format!(
                "{}; {}",
                insert_sql(1, "fi-implicit-op-1"),
                insert_sql(2, "fi-implicit-op-2")
            ),
        );
        assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
        assert_emergency_response(&mut stream);

        let status = wait_for_exit(&mut child, Duration::from_secs(30));
        assert_aborted(status);

        let storage = Storage::open(&db_path).expect("reopen storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let read_ctx =
            PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        let result = core
            .execute_sql(
                &read_ctx,
                "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
            )
            .expect("select should succeed");
        assert_eq!(
            result.rows.len(),
            2,
            "both rows of the committed implicit transaction must be visible"
        );
    }
}
