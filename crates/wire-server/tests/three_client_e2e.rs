//! 無改造の実クライアント 3 種（`psql`／Python `psycopg`／Node.js `pg`）から
//! `wire-server` バイナリへ実接続し、C1〜C4（定義は TASK-73／ビヘイビア
//! WIRE-1、`crates/engine/src/sql/parser.rs` 参照）の実行・誤りパスワードの
//! 拒否を検証する層 B の統合テスト（codex-review P2 指摘・PR #210: 各ドライバ
//! での挙動差異を独立オラクルと照合し保証する）。
//!
//! 責務境界: 層 A（`tests/wire1_simple_query.rs`）が生バイトの wire クライアント
//! で常時（`make ci`）回帰保護する契約と同じバイト列を、実クライアント経由で
//! 追加検証する。ローカル・Docker 開発コンテナには `psql`／`psycopg`／`pg` が
//! 導入されていないため `#[ignore]` とし、`make e2e-three-client`
//! （`cargo test -p fandhe-vector-db-wire-server --test three_client_e2e -- --ignored`）から
//! 明示的に実行する（CI の必須チェックには含めない。psql・psycopg・pg の並
//! 導入をローカル環境へ強制すると `make ci` 自体が壊れるため。ADR:
//! `docs/design/three-client-e2e-harness.md`）。
//!
//! TASK-165（SQL-12／SEARCH-9）: `USING MODE`／`SET search_mode` の優先順位・
//! 確信度ゲートは層 A（`tests/wire_search_mode.rs`、常時 `make ci`）が主たる
//! 回帰保護を担う。本ファイルは `run_*_session` 系ヘルパー（`WIRE_SQL_PRELUDE`
//! で同一接続に複数文を送る）を使い、無改造クライアント経由でも同じ契約を
//! 最小限確認する（3 クライアントの子プロセス実行は本環境未導入のため
//! コンパイル通過とスクリプト構文確認のみで検証済み。詳細は PR 本文）。
//!
//! TASK-168（SQL-13／SQL-14）: 集計関数（`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`）・
//! `GROUP BY`/`HAVING` の拒否形状・NULL 契約の回帰保護は層 A
//! （`tests/wire_aggregate.rs`、常時 `make ci`）が主として担う。本ファイルは
//! `seed_aggregate_three_tenant_db` の同一コーパスに対する代表ケース（単一行
//! 集計・`GROUP BY`/`HAVING`・RLS 不変・拒否経路 2 種）のみを 3 クライアント
//! 経由で確認する（3 クライアントの子プロセス実行は本環境未導入のため層 A・
//! 層 B いずれもコンパイル通過とスクリプト構文確認のみで検証済み。詳細は
//! PR 本文）。
//!
//! ツール未検出・クライアントスクリプトの非 0 終了はいずれも `panic!` で
//! 失敗させ、silent skip はしない（`.claude/rules/coding-rust.md`・実行規約
//! 「テストの skip・ignore・アサーション弱体化で CI を通さない」の精神を、
//! 明示的に選択実行するこの導線でも維持する）。
//!
//! TASK-187（SQL-11）: `docs` 以外の任意テーブル（`kb_articles`）でも C1 相当の
//! SELECT・`INSERT ... USING OPERATION_ID` が `docs` と同じ契約（成否・
//! `wire_code`・RLS 暗黙適用）で通ることを 3 クライアント経由で確認する。
//! 評価順序・台帳スコープ・複数次元共存・`42P01` は engine 側
//! `crates/engine/tests/arbitrary_table.rs`（TASK-81・SQL-11 確定化の根拠）が
//! 既に機械検証済みのため、本ファイルでは重複網羅しない。
//!
//! TASK-97・TASK-153・ERR-5（Issue #706）: commit 成功境界を跨いだ panic 時の
//! 緊急応答（`S`=`ERROR`・`C`=`XX000`・`D`=`state=may_be_committed`）の
//! バイト列契約そのものは層 A（`tests/wire_emergency_response.rs`）が固定し、
//! テスト専用注入フラグ `--fault-inject post-commit-panic`（feature
//! `fault-injection`・Issue #705）の CLI 受理・発火・abort は層 A
//! （`tests/wire_fault_injection_cli.rs`）が検証済み。本ファイルはその先
//! ——無改造の実クライアント 3 種が、自身のドライバ API から実際に `detail`
//! 値へ到達できるか（psql の `DETAIL:` 行、psycopg の
//! `e.diag.message_detail`、node `pg` の `err.detail`）——を検証する
//! （`three_clients_receive_emergency_response_detail_after_post_commit_panic`）。
//! `--fault-inject` は 1 プロセスにつき 1 回しか発火しない take-once 契約
//! （Issue #705）のため、クライアントごとに独立したサーバー・DB を起動する。
//!
//! WIRE-19（Issue #943）: 明示トランザクションの `ReadyForQuery` 状態バイト
//! （`'I'`/`'T'`/`'E'`。production の中核は Issue #942・PR #1041 で実装済み）
//! が無改造クライアント自身の API から観測できることを
//! `three_clients_observe_transaction_status_transitions` で確認する。
//! 各ドライバの観測経路の選定理由・非 vacuous 性の検証は
//! `docs/design/three-client-e2e-harness.md`「トランザクション状態遷移
//! （Issue #943・WIRE-19）」節参照。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

/// 環境変数（`PSQL_BIN`/`PYTHON_BIN`/`NODE_BIN`）で上書きできるツール解決。
/// 未指定時は `PATH` 上のデフォルト名を使う。ツール自体の存在確認はしない
/// （`Command::spawn` の失敗として顕在化させ、呼び出し元が案内メッセージ付きで
/// panic する）。
fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `wire-server` バイナリを子プロセスとして起動し、stderr の `listening on
/// 127.0.0.1:<port>` 行から実際に bind されたポートを取得する。
/// 呼び出し元が `Drop` 相当で必ず kill する（[`ServerGuard`]）。
///
/// `startup_lines` は listen 行到達までに観測した stderr の全行（トリム済み）
/// を保持する（Issue #706。`--fault-inject`〔Issue #705〕を渡して起動する
/// 場合の `fault injection armed` 行の観測に使う。`ServerGuard` の生存中に
/// 子プロセスが自発的に終了することがある（Issue #706 の commit 後 panic
/// 注入。`Drop` の `kill`／`wait` は既終了プロセスに対しても安全に no-op と
/// なる）ため、`wait_for_exit` で明示的に終了を待ち受けられるようにする。
///
/// stderr は listen 行の取得後も子プロセスの終了まで読み続ける（Issue #943）。
/// #1081 以前の挙動: 以前は listen 行の取得後に受信側チャネルが破棄されると
/// 読み取りスレッドが終了してパイプの読み口を閉じていたため、サーバーが
/// 接続エラー等を 2 行以上 stderr へ書くと `EPIPE` で `eprintln!` が panic し、
/// panic フック（TASK-97・RECOVER-6／TASK-99・RECOVER-8）経由で SIGABRT
/// 終了していた（高負荷下で後続クライアントが "server closed the connection
/// unexpectedly" となる偽陽性の原因）。Issue #1081 でサーバー側の診断ログを
/// `engine::log_stderr!`（書き込み失敗を無視する。RECOVER-8 の例外。ポインタ:
/// `docs/design/stderr-log-write-failure.md`）へ置き換えたため、読み手が閉じても
/// サーバー側では abort しなくなったが、本ハーネスは失敗時診断のため引き続き
/// listen 後の行を [`STDERR_TAIL_MAX_LINES`] 行まで保持し、テストが panic した
/// 場合に限り `Drop` で終了状態とあわせて出力する。
struct ServerGuard {
    child: Child,
    port: u16,
    startup_lines: Vec<String>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

/// 失敗時診断用に保持する listen 後の stderr 行数の上限（無制限に溜めない）。
const STDERR_TAIL_MAX_LINES: usize = 256;

impl Drop for ServerGuard {
    fn drop(&mut self) {
        // kill 前に終了状態を採取する（テスト中にサーバーが自発終了していたか
        // を失敗時診断で区別するため）。
        let exited_before_drop = self.child.try_wait().ok().flatten();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 子プロセス終了でパイプの書き口が閉じ、読み取りスレッドは EOF で終わる。
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        if std::thread::panicking() {
            let tail: Vec<String> = match self.stderr_tail.lock() {
                Ok(guard) => guard.iter().cloned().collect(),
                Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
            };
            eprintln!(
                "[e2e-diag] wire-server port={} exited_before_drop={exited_before_drop:?} \
                 stderr_after_listen={tail:#?}",
                self.port
            );
        }
    }
}

impl ServerGuard {
    /// 子プロセスの終了（緊急応答送出後の `fail_fast` による abort を含む。
    /// TASK-97・RECOVER-6・TASK-99・RECOVER-8）を `timeout` まで待ち受ける
    /// （`crates/wire-server/tests/wire_fault_injection_cli.rs::wait_for_exit`
    /// と同型）。超過した場合は kill してから panic する。
    fn wait_for_exit(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if start.elapsed() > timeout {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("subprocess did not terminate within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `extra_args`（`extended_syntax_e2e.rs::spawn_wire_server` と同型）で
/// `--fault-inject post-commit-panic`（Issue #705・#706）等の追加 CLI を
/// まとめて渡せるようにする。
fn spawn_wire_server(users_path: &Path, db_path: &Path, extra_args: &[String]) -> ServerGuard {
    let mut args: Vec<String> = vec![
        "--users".into(),
        users_path.to_str().expect("utf-8 path").into(),
        "--db".into(),
        db_path.to_str().expect("utf-8 path").into(),
        "--bind".into(),
        "127.0.0.1:0".into(),
    ];
    args.extend_from_slice(extra_args);

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server binary (built by `cargo test`)");

    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let tail_for_reader = Arc::clone(&stderr_tail);
    // EOF（子プロセス終了）まで読み続ける。受信側（listen 行待ち）が破棄された
    // 後も読み取りを止めない（止めるとパイプが閉じサーバーが EPIPE で abort
    // する。[`ServerGuard`] 参照）。
    let stderr_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        let mut listening = true;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 {
                break;
            }
            let taken = std::mem::take(&mut line);
            if listening {
                if let Err(mpsc::SendError(unsent)) = tx.send(taken) {
                    listening = false;
                    push_tail(&tail_for_reader, unsent);
                }
            } else {
                push_tail(&tail_for_reader, taken);
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut port: Option<u16> = None;
    let mut startup_lines: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if let Some(addr_str) = trimmed.strip_prefix("wire-server: listening on ") {
                    if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
                        port = Some(addr.port());
                        startup_lines.push(trimmed);
                        break;
                    }
                }
                startup_lines.push(trimmed);
            }
            Err(_) => break,
        }
    }

    // listen 行の待ち受けを終えたら受信側を破棄し、以降の行は `stderr_tail`
    // へ回す（読み取りスレッド側で送信失敗を検知して切り替える）。
    drop(rx);

    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_reader.join();
        panic!(
            "wire-server did not report a listening port within the deadline \
             (run via `make e2e-three-client` which builds with `--features \
             fault-injection` when `--fault-inject` is passed); lines so far: \
             {startup_lines:?}"
        );
    };

    ServerGuard {
        child,
        port,
        startup_lines,
        stderr_tail,
        stderr_reader: Some(stderr_reader),
    }
}

/// listen 後の stderr 行を上限付きで `tail` へ追加する（古い行から捨てる）。
fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut guard = match tail.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() >= STDERR_TAIL_MAX_LINES {
        guard.pop_front();
    }
    guard.push_back(line.trim_end().to_string());
}

/// 回帰テスト（Issue #943。#1081 以前は `EPIPE` 起因の panic → fail-fast
/// abort〔TASK-99・RECOVER-8〕がここで落ちる原因だった。Issue #1081 以降は
/// サーバー側の診断ログが `engine::log_stderr!` で書き込み失敗を無視するため
/// 読み手が閉じても abort しない）: [`spawn_wire_server`] が listen 行の
/// 取得後も子プロセスの stderr を読み続け、サーバーが接続エラーを複数行ログへ
/// 書いても落ちないことを固定する。未読データを残したまま接続を閉じて RST を送り、
/// サーバー側に `connection error: Connection reset by peer` を複数回ログ
/// させたうえで（`stderr_tail` に行が届いたことを確認し非 vacuous 化する）、
/// サーバーが生存し新規接続へ認証要求を返すことを確認する。外部クライアントを
/// 必要としないため `#[ignore]` を付けず常時実行する。
#[test]
fn server_guard_keeps_draining_stderr_so_logged_connection_errors_do_not_abort_server() {
    const RESET_CONNECTIONS: usize = 3;

    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-stderr-drain-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let mut server = spawn_wire_server(&users_path, &db_path, &[]);

    for _ in 0..RESET_CONNECTIONS {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
        common::send_startup_message(&mut stream, "alice", "docs");
        // サーバーの認証要求が受信バッファに届いたことを `peek`（消費しない）で
        // 確かめてから読まずに閉じる（未読データを残した close は RST になり、
        // サーバー側の読み取りが ECONNRESET となって stderr へ 1 行ログされる。
        // 固定 sleep だと高負荷下で到着前に閉じ FIN になりうるため待ち合わせる）。
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set_read_timeout");
        let mut probe = [0u8; 1];
        let peeked = stream
            .peek(&mut probe)
            .expect("peek authentication request");
        assert!(peeked >= 1, "expected pending authentication request bytes");
        drop(stream);
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let logged = server
            .stderr_tail
            .lock()
            .map(|t| t.iter().filter(|l| l.contains("connection error")).count())
            .unwrap_or(0);
        if logged >= 2 {
            break;
        }
        // 旧ハーネス（listen 後にパイプを閉じる）では、ここでサーバーが SIGABRT
        // 終了している。行数待ちのタイムアウトより先に終了を検出して報告する。
        if let Some(status) = server.child.try_wait().expect("try_wait") {
            panic!("wire-server exited while logging connection errors: {status:?}");
        }
        assert!(
            Instant::now() < deadline,
            "expected at least 2 'connection error' lines after listen; got {logged}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert!(
        server.child.try_wait().expect("try_wait").is_none(),
        "wire-server must stay alive after logging connection errors to stderr"
    );
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set_read_timeout");
    common::send_startup_message(&mut stream, "alice", "docs");
    let mut message_type = [0u8; 1];
    std::io::Read::read_exact(&mut stream, &mut message_type).expect("read message type");
    assert_eq!(message_type[0], b'R', "expected an Authentication* request");
}

/// 3 テナント（alice/bob/carol）に Public 行 1 件ずつを投入した `docs`
/// テーブルを持つ一時 DB を用意する（層 A の
/// `wire1_three_tenant_visibility_public_shared_own_private_visible` と同じ
/// seed 方針。本 seed は `Private` 行を持たないため RLS-11・TASK-195
/// （read-your-writes。ポインタ: `docs/spec/04-behavior/rls.md` RLS-11）の
/// 影響を受けない）。C1〜C4（TASK-73／WIRE-1）すべてを同じ 3 行のコーパスで
/// 検証できるよう列を構成する（codex-review P2 指摘・PR #210）。
fn seed_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let tenants: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in tenants {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (path, guard)
}

/// `docs` とは別名の任意テーブル（`kb_articles`）に `seed_three_tenant_db` と
/// 同じ Public 3 行を投入したうえで、tenant-a の Private 行（id=11,
/// lang="xx"）も追加した一時 DB を用意する（TASK-187・SQL-11）。`docs` と
/// 別名のテーブルでも wire 経由の C1 相当・RLS 暗黙適用が同一契約で成立する
/// ことを、Private 行の非漏洩という非自明な形で検証するための seed
/// （engine 側 `crates/engine/tests/arbitrary_table.rs` の対照検証と同じ
/// `kb_articles` という命名を踏襲する）。
fn seed_arbitrary_table_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-kb-articles");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "kb_articles",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let public_rows: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "kb_articles",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    // TASK-101（RECOVER-10）: 上の public_rows ループで tenant-a が既に
    // "test-op" を使用しているため、別内容の再利用は OperationIdContentMismatch
    // になる。Private 行専用の別 operation_id を使う。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "kb_articles",
        &ctx,
        11,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("private body".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op-private-kb")
            .expect("valid operation_id"),
    )
    .expect("insert private row");
    (path, guard)
}

/// `documents`（`VECTOR` 列を持つ書き込み対象）と `notes`（未書き込みの別
/// テーブル）の 2 テーブルを持つ一時 DB を用意する（Issue #943・WIRE-19。
/// 明示トランザクション内で「直前に書き込んだテーブル自身は読めない」
/// 制約（`docs/design/explicit-transaction.md` 参照）を避けつつ、同一
/// トランザクション内の `SELECT` を検証するために `notes` を用意する）。
/// `write_users_file` と異なり alice 1 テナントのみで十分（状態遷移の
/// 検証にテナント分離は関与しない）。
fn seed_txn_status_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-txn-status");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    storage
        .create_table(&TableSchema::new(
            "notes",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        ))
        .expect("create table");
    (path, guard)
}

/// `seed_txn_status_db` 用の単一ユーザー（alice）だけの認証ファイル。
fn write_alice_only_users_file(path: &Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let phc = argon2id::encode_phc(b"correct-horse", salt, &argon2id::RECOMMENDED_PARAMS)
        .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write users file");
}

/// `psycopg_txn_status.py` を子プロセスとして起動し、stdout の状態名の列
/// （`IDLE`/`INTRANS`/`INERROR`）を返す。非 0 終了は `panic!`（silent skip
/// しない）。
fn run_psycopg_txn_status(port: u16, insert_id: u64, op: &str) -> Vec<String> {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_txn_status.py");
    let output = Command::new(&python)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", "alice")
        .env("WIRE_PASSWORD", "correct-horse")
        .env("WIRE_TXN_INSERT_SQL", insert_sql(insert_id, op))
        .env("WIRE_TXN_SELECT_SQL", "SELECT id FROM notes LIMIT 1")
        .env(
            "WIRE_TXN_VERIFY_SQL",
            format!("SELECT id FROM documents WHERE id = {insert_id} LIMIT 1"),
        )
        .env("WIRE_TXN_BAD_SQL", "SELEC id FROM notes")
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {python} (install psycopg via `pip install psycopg` or set PYTHON_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psycopg_txn_status.py failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `pg_txn_status.js` を子プロセスとして起動し、stdout の `ReadyForQuery`
/// 状態バイトの列（`'I'`/`'T'`/`'E'`）を返す。非 0 終了は `panic!`。
fn run_pg_txn_status(port: u16, insert_id: u64, op: &str) -> Vec<String> {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_txn_status.js");
    let output = Command::new(&node)
        .arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", "alice")
        .env("WIRE_PASSWORD", "correct-horse")
        .env("WIRE_TXN_INSERT_SQL", insert_sql(insert_id, op))
        .env("WIRE_TXN_SELECT_SQL", "SELECT id FROM notes LIMIT 1")
        .env(
            "WIRE_TXN_VERIFY_SQL",
            format!("SELECT id FROM documents WHERE id = {insert_id} LIMIT 1"),
        )
        .env("WIRE_TXN_BAD_SQL", "SELEC id FROM notes")
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {node} (install pg via `npm install pg` or set NODE_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "pg_txn_status.js failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}'"
    )
}

/// psql（無改造）でトランザクション状態の反映を**間接的に**確認する。psql
/// は `\set AUTOCOMMIT off` の下では、libpq が `ReadyForQuery` の状態バイト
/// から導出する `PQtransactionStatus()` を見て、`IDLE` のときに限り
/// 次の文の前に暗黙の `BEGIN` を送る
/// （`docs/design/three-client-e2e-harness.md`「トランザクション状態遷移
/// （Issue #943・WIRE-19）」節参照。プロンプト文字列 `%x` は対話端末専用の
/// ため非対話実行では観測できず採らない判断の記録も同節にある）。
/// もし `wire-server` が `ReadyForQuery` の状態バイトを常に `'I'` のまま
/// 返す不具合があれば、`INSERT` の後の 2 文目の前にも `BEGIN` が再送され
/// 「入れ子の BEGIN」（`25001`）で失敗し、続く `COMMIT` も `25P02` で
/// 拒否されて非 0 終了する。正しく `'T'` を反映していれば `BEGIN` は
/// 1 回しか送られず、全体が正常終了して `COMMIT` タグが確認できる。
fn assert_psql_autocommit_off_reflects_transaction_status(
    port: u16,
    user: &str,
    password: &str,
    insert_id: u64,
    op: &str,
) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let insert = insert_sql(insert_id, op);
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            user,
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-At",
            "-c",
            "\\set AUTOCOMMIT off",
            "-c",
            &insert,
            "-c",
            "SELECT id FROM notes LIMIT 1",
            "-c",
            "COMMIT",
        ])
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psql -c sequence under AUTOCOMMIT off must succeed exactly once per BEGIN \
         (a bug that always reports 'I' would cause a spurious nested BEGIN → 25001 → \
         25P02 on COMMIT): stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // commit 済みの行が読み戻せる（read-your-writes）ことも独立に確認する。
    let rows = run_psql(
        port,
        user,
        password,
        &format!("SELECT id FROM documents WHERE id = {insert_id} LIMIT 1"),
    );
    assert_eq!(
        rows,
        vec![insert_id.to_string()],
        "committed row must be visible after AUTOCOMMIT off session"
    );
}

/// WIRE-19（Issue #943）: `ReadyForQuery` の状態バイトが明示トランザクション
/// 状態（`Idle`/`InTransaction`/`Failed`）を反映することを、無改造の実
/// クライアント 3 種（psql／psycopg／pg）から検証する。層 A
/// （`wire942_extended_transaction.rs`・`wire19_ready_for_query_status.rs`）が
/// 生バイトの wire クライアントで固定する契約と同じものを、各ドライバ自身の
/// トランザクション状態 API・暗黙 `BEGIN` 挙動を通じて追加確認する
/// （responsibility boundary は本ファイル冒頭のドキュメンテーションコメント
/// と同じ方針）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_observe_transaction_status_transitions() {
    // psql・psycopg・pg で 1 つずつ独立したサーバー・DB を使う（各クライアント
    // が別々の `operation_id`／`id` で書き込むため、テナント境界の検証は
    // 不要。同一 DB を使い回しても害はないが、状態遷移の検証観点を独立に
    // 保つため分ける）。
    for (label, run) in [
        (
            "psql",
            (|port: u16| {
                assert_psql_autocommit_off_reflects_transaction_status(
                    port,
                    "alice",
                    "correct-horse",
                    50,
                    "op-943-psql",
                );
            }) as fn(u16),
        ),
        (
            "psycopg",
            (|port: u16| {
                let statuses = run_psycopg_txn_status(port, 51, "op-943-psycopg");
                assert_eq!(
                    statuses,
                    vec!["IDLE", "INTRANS", "INTRANS", "IDLE", "INERROR", "IDLE"],
                    "psycopg: unexpected transaction_status sequence"
                );
            }) as fn(u16),
        ),
        (
            "pg",
            (|port: u16| {
                let statuses = run_pg_txn_status(port, 52, "op-943-pg");
                assert_eq!(
                    statuses,
                    vec!["I", "T", "T", "T", "I", "I", "T", "E", "I"],
                    "pg: unexpected ReadyForQuery status sequence"
                );
            }) as fn(u16),
        ),
    ] {
        let (db_path, _db_guard) = seed_txn_status_db();
        let users_dir = temp_db::TempDir::new("three-client-e2e-txn-status-users");
        let users_path = users_dir.path().join("users.txt");
        write_alice_only_users_file(&users_path);
        let server = spawn_wire_server(&users_path, &db_path, &[]);
        run(server.port);
        drop(server);
        let _ = std::io::stdout().flush();
        eprintln!("[e2e-record] three_clients_observe_transaction_status_transitions: {label} ok");
    }
}

/// `seed_three_tenant_db` と同じ Public 3 行（`embedding`/`lang`/`body`）に加え、
/// tenant-a の Private 行（id=11, lang="xx"）・tenant-b の Private 行
/// （id=12, lang="ja"）を投入した一時 DB を用意する（TASK-168・SQL-13/14）。
/// wire 認証経路の `PolicyContext` は `Public` ＋ 自テナントの `Private` を
/// 許可可視性とする（RLS-11・TASK-195。read-your-writes）ため、tenant-a の
/// 接続では id=11 が、tenant-b の接続では id=12 が集計・GROUP BY の対象に
/// 含まれる。他テナントの `Private` 行は引き続きどの接続からも不可視
/// （carol は Private 行を持たないため元の集計結果のまま不変）。既存
/// C1〜C4 テストの seed（`seed_three_tenant_db`）はこの関数の追加では
/// 変更しない。
fn seed_aggregate_three_tenant_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-e2e-aggregate-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let public_rows: [(&str, u64, [f32; 2], &str, &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja", "vector database intro"),
        ("tenant-b", 2, [0.0, 1.0], "en", "query planning notes"),
        ("tenant-c", 3, [-1.0, 0.0], "ja", "unrelated topic"),
    ];
    for (tenant, id, dir, lang, body) in public_rows {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text(body.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op")
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    let private_rows: [(&str, u64, [f32; 2], &str); 2] = [
        ("tenant-a", 11, [1.0, 0.0], "xx"),
        ("tenant-b", 12, [0.0, 1.0], "ja"),
    ];
    for (tenant, id, dir, lang) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        // TASK-101（RECOVER-10）: 上の public_rows ループで同一テナントが既に
        // "test-op" を使用しているため、別内容の再利用は OperationIdContentMismatch
        // になる。別の operation_id を使う。
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(dir.to_vec()),
                Value::Text(lang.to_string()),
                Value::Text("private body".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse("test-op-private")
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }
    (path, guard)
}

fn write_users_file(path: &Path) {
    use wire_server::auth::argon2id;
    let salt = b"0123456789abcdef";
    let mut content = String::new();
    for (user, tenant, pw) in [
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
        ("carol", "tenant-c", "pw-carol"),
    ] {
        let phc = argon2id::encode_phc(pw.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{user}:{tenant}:{phc}\n"));
    }
    std::fs::write(path, content).expect("write users file");
}

/// C1（TASK-73／WIRE-1）。
const C1_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C2（TASK-73／WIRE-1）。各ドライバでの型変換も合わせて検証する。
const C2_SQL: &str =
    "SELECT id, lang FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C3（TASK-73／WIRE-1。`crates/engine/tests/sql_surface.rs`
/// `sql3_rls_is_enforced_regardless_of_visible_predicate_presence` と同じ契約）。
const C3_SQL: &str =
    "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
/// C4（TASK-73／WIRE-1。`crates/engine/tests/sql_surface.rs`
/// `sql4_hybrid_degrades_to_dense_only_when_no_visible_body_text` と同じ契約）。
const C4_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3";

/// TASK-187（SQL-11）: `docs` 以外の任意テーブル（`kb_articles`）での C1 相当。
/// `LIMIT` は本ファイルの `INSERT` 検証（`three_clients_run_c1_and_insert_on_
/// arbitrary_table`）で 3 テナント分の追加行（最大 9 行）を積んでも総行数
/// （最大 3 Public + 1 Private + 9 = 13）を上回る 20 に設定し、RLS-11・
/// TASK-195（read-your-writes）の許可可視性で行が増減しても `LIMIT` に隠れず
/// 必ず観測できる形にする（非 vacuous な RLS チェック）。
const C1_SQL_ARBITRARY_TABLE: &str =
    "SELECT id FROM kb_articles ORDER BY embedding <=> '[1.0,0.0]' LIMIT 20";

/// psql（無改造）で任意の SQL を実行し、返却された各行を `|` 区切りで結合した
/// 文字列の集合として返す（単一列なら値そのもの）。`-F '|'` で区切り文字を
/// 明示指定し（`-X` で `~/.psqlrc` 経由の `\pset fieldsep` 上書きも遮断する
/// ため、環境差異で暗黙に変わらない）、`run_psycopg`／`run_pg` 側も同じ区切りで
/// 出力を揃える。
fn run_psql(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_psql_session(port, user, password, &[], sql)
}

/// psql（無改造）で `prelude` の各文を先に実行してから `sql` を実行し、`sql` の
/// 結果行を `run_psql` と同じ `|` 区切りの集合として返す（TASK-165・SQL-12。
/// 同一接続で `SET search_mode = ...` を先行実行してから `SELECT` を送る、
/// セッション複数文の検証に使う）。`-q`（quiet）で prelude の `SET` タグが
/// stdout の結果集合へ混入するのを防ぎ、複数の `-c` は psql が同一セッションで
/// 順次送信する（`run_psql` は本関数の prelude 無しの薄いラッパー）。
fn run_psql_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let mut args: Vec<String> = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        "irrelevant-db-name".into(),
        "-X".into(),
        "-w".into(),
        "-q".into(),
        "-At".into(),
        "-F".into(),
        "|".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
    ];
    for stmt in prelude {
        args.push("-c".into());
        args.push((*stmt).into());
    }
    args.push("-c".into());
    args.push(sql.into());

    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    assert!(
        output.status.success(),
        "psql exited non-zero for user {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// psql で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE を含めて拒否されることを確認する（TASK-165 の拒否経路検証。
/// `-v VERBOSITY=verbose` で SQLSTATE を stderr へ出させる）。
fn run_psql_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let mut args: Vec<String> = vec![
        "-h".into(),
        "127.0.0.1".into(),
        "-p".into(),
        port.to_string(),
        "-U".into(),
        user.into(),
        "-d".into(),
        "irrelevant-db-name".into(),
        "-X".into(),
        "-w".into(),
        "-q".into(),
        "-At".into(),
        "-v".into(),
        "ON_ERROR_STOP=1".into(),
        "-v".into(),
        "VERBOSITY=verbose".into(),
    ];
    for stmt in prelude {
        args.push("-c".into());
        args.push((*stmt).into());
    }
    args.push("-c".into());
    args.push(sql.into());

    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in psql stderr, got: {stderr}"
    );
}

/// psql で誤りパスワードを送り、非 0 終了・`28P01`／認証失敗の文言が出ることを
/// 確認する。
fn run_psql_wrong_password(port: u16, user: &str) {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", "definitely-not-the-password")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            user,
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-At",
            "-c",
            "SELECT 1",
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero on wrong password"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("password") || stderr.contains("28P01"),
        "expected password-authentication failure text, got: {stderr}"
    );
}

/// Python `psycopg`（無改造）で任意の SQL を実行し、各行を `|` 区切りで
/// 結合した文字列の集合を返す（`run_psql` と同じ区切り規約。複数列を返す
/// C2 の型変換検証に対応する）。
fn run_psycopg(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_psycopg_session(port, user, password, &[], sql)
}

/// `psycopg_client.py` に `WIRE_SQL_PRELUDE`（JSON 配列）を渡し、`prelude` の
/// 各文を同一接続で先行実行してから `sql` を実行する（TASK-165・SQL-12。
/// `run_psycopg` は本関数の prelude 無しの薄いラッパー）。
fn run_psycopg_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let output = spawn_psycopg_client(port, user, password, prelude, sql);
    assert!(
        output.status.success(),
        "psycopg_client.py failed for user {user} (install psycopg via \
         `pip install psycopg[binary]` or set PYTHON_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// psycopg で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE（`psycopg_client.py` の `[SQLSTATE=<code>]` 表記）を含めて拒否
/// されることを確認する（TASK-165 の拒否経路検証）。
fn run_psycopg_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let output = spawn_psycopg_client(port, user, password, prelude, sql);
    assert!(
        !output.status.success(),
        "psycopg_client.py must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in psycopg_client.py stderr, got: {stderr}"
    );
}

fn spawn_psycopg_client(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> std::process::Output {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_client.py");
    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if !prelude.is_empty() {
        let prelude_json =
            serde_json_prelude(prelude).expect("prelude statements must encode as a JSON array");
        cmd.env("WIRE_SQL_PRELUDE", prelude_json);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"))
}

/// `["a","b"]` 形式の最小 JSON エンコーダ（依存追加なしで `WIRE_SQL_PRELUDE` を
/// 組み立てる。`prelude` は本ファイル内の定数リテラルのみを渡す前提で、
/// 制御文字・バックスラッシュを含まない SQL 文だけを扱う。`\`・制御文字を
/// 含む文字列を渡した場合は panic して不正なエンコードを未然に防ぐ）。
fn serde_json_prelude(statements: &[&str]) -> Option<String> {
    let mut out = String::from("[");
    for (i, stmt) in statements.iter().enumerate() {
        if stmt.contains('\\') || stmt.chars().any(|c| c.is_control()) {
            return None;
        }
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&stmt.replace('"', "\\\""));
        out.push('"');
    }
    out.push(']');
    Some(out)
}

/// Node.js `pg`（無改造）で任意の SQL を実行し、各行を `|` 区切りで結合
/// した文字列の集合を返す（`run_psql` と同じ区切り規約）。
fn run_pg(port: u16, user: &str, password: &str, sql: &str) -> Vec<String> {
    run_pg_session(port, user, password, &[], sql)
}

/// `pg_client.js` に `WIRE_SQL_PRELUDE`（JSON 配列）を渡し、`prelude` の各文を
/// 同一接続で先行実行してから `sql` を実行する（TASK-165・SQL-12。`run_pg` は
/// 本関数の prelude 無しの薄いラッパー）。
fn run_pg_session(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> Vec<String> {
    let output = spawn_pg_client(port, user, password, prelude, sql);
    assert!(
        output.status.success(),
        "pg_client.js failed for user {user} (install pg via \
         `npm install pg` or set NODE_BIN): stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// pg で `prelude` を先行実行後、最終文が非 0 終了かつ stderr に期待
/// SQLSTATE（`pg_client.js` の `[SQLSTATE=<code>]` 表記）を含めて拒否される
/// ことを確認する（TASK-165 の拒否経路検証）。
fn run_pg_session_expect_sqlstate(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
    expected_sqlstate: &str,
) {
    let output = spawn_pg_client(port, user, password, prelude, sql);
    assert!(
        !output.status.success(),
        "pg_client.js must exit non-zero for a rejected statement (user {user})"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate} in pg_client.js stderr, got: {stderr}"
    );
}

fn spawn_pg_client(
    port: u16,
    user: &str,
    password: &str,
    prelude: &[&str],
    sql: &str,
) -> std::process::Output {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
    let mut cmd = Command::new(&node);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if !prelude.is_empty() {
        let prelude_json =
            serde_json_prelude(prelude).expect("prelude statements must encode as a JSON array");
        cmd.env("WIRE_SQL_PRELUDE", prelude_json);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"))
}

/// 3 クライアント（psql / psycopg / pg）それぞれで、3 テナントいずれの
/// ユーザーで接続しても C1〜C4（TASK-73／WIRE-1）の結果が独立オラクルと一致
/// すること・誤りパスワードが拒否されることを検証する（可視性契約は層 A の
/// `wire1_three_tenant_visibility_public_shared_own_private_visible` と同じ。
/// codex-review P2 指摘・PR #210）。ツール未導入・スクリプト失敗は silent
/// skip せず `panic!` で失敗させる（本ファイル先頭のドキュメンテーション
/// コメント参照）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_through_c4_and_reject_wrong_password() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    // 独立オラクル（TASK-73／WIRE-1。各定数のドキュメンテーションコメント
    // 参照）。
    let expected_c1 = vec!["1".to_string(), "2".to_string(), "3".to_string()];
    let expected_c2 = vec!["1|ja".to_string(), "3|ja".to_string()];
    let expected_c3 = expected_c1.clone();
    let expected_c4 = expected_c1.clone();

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        for (label, sql, expected) in [
            ("C1", C1_SQL, &expected_c1),
            ("C2", C2_SQL, &expected_c2),
            ("C3", C3_SQL, &expected_c3),
            ("C4", C4_SQL, &expected_c4),
        ] {
            let psql_rows = run_psql(port, user, pw, sql);
            assert_eq!(
                &psql_rows, expected,
                "psql: unexpected {label} result for user {user}"
            );

            let psycopg_rows = run_psycopg(port, user, pw, sql);
            assert_eq!(
                &psycopg_rows, expected,
                "psycopg: unexpected {label} result for user {user}"
            );

            let pg_rows = run_pg(port, user, pw, sql);
            assert_eq!(
                &pg_rows, expected,
                "pg: unexpected {label} result for user {user}"
            );
        }
    }

    run_psql_wrong_password(port, "alice");

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-165（SQL-12／SEARCH-9）: `USING MODE` 句・`SET search_mode` セッション
/// 変数・未知モード値の拒否を無改造クライアント経由で最小限確認する。閾値
/// そのものの回帰保護は層 A（`tests/wire_search_mode.rs`、常時 `make ci`）が
/// 担うため、ここでは同じオラクル（`seed_three_tenant_db` の
/// `[1,0]`／`[0,1]`／`[-1,0]` コーパス）に対する代表ケースのみを 3 クライアントで
/// 確認する（M2: クエリ句 precision が Top-1 のみを返す／M5: `SET
/// search_mode='precision'` が後続の句なし SELECT に適用される／R1: クエリ句の
/// 未知モード値が拒否される）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_verify_search_mode_switch_and_precision_contract() {
    let (db_path, _db_guard) = seed_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-search-mode-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    const PRECISION_CLAUSE_SQL: &str =
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'precision'";
    const UNKNOWN_MODE_SQL: &str =
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3 USING MODE 'fuzzy'";
    let expected_top1_only = vec!["1".to_string()];

    for (user, pw) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ] {
        // M2: クエリ句 precision（明確な勝者 → Top-1 のみ）。
        assert_eq!(
            run_psql_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "psql: USING MODE 'precision' must return only id=1 for user {user}"
        );
        assert_eq!(
            run_psycopg_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "psycopg: USING MODE 'precision' must return only id=1 for user {user}"
        );
        assert_eq!(
            run_pg_session(port, user, pw, &[], PRECISION_CLAUSE_SQL),
            expected_top1_only,
            "pg: USING MODE 'precision' must return only id=1 for user {user}"
        );

        // M5: SET search_mode='precision' → 句なし SELECT が Top-1 のみ返る
        // （セッション複数文の同一接続内適用。`WIRE_SQL_PRELUDE`／複数 `-c` 経由）。
        let prelude = ["SET search_mode = 'precision'"];
        assert_eq!(
            run_psql_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "psql: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );
        assert_eq!(
            run_psycopg_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "psycopg: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );
        assert_eq!(
            run_pg_session(port, user, pw, &prelude, C1_SQL),
            expected_top1_only,
            "pg: SET search_mode='precision' must apply to the subsequent SELECT for user {user}"
        );

        // R1: クエリ句の未知モード値は 22000 で拒否される。
        run_psql_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
        run_pg_session_expect_sqlstate(port, user, pw, &[], UNKNOWN_MODE_SQL, "22000");
    }

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-168（SQL-13／SQL-14）: 集計クエリ（単一行の `COUNT`/`SUM`/`AVG`/`MIN`/
/// `MAX`・`GROUP BY`/`HAVING`）と RLS 不変性の代表ケースを無改造クライアント
/// 経由で確認する。閾値・拒否形状そのものの回帰保護は層 A
/// （`tests/wire_aggregate.rs`、常時 `make ci`）が担う。
///
/// wire 認証経路の `PolicyContext` は `Public` ＋ 自テナントの `Private` を
/// 許可可視性とする（RLS-11・TASK-195。read-your-writes）ため、tenant-a
/// （alice）は自身の Private 行（id=11, lang="xx"）を、tenant-b（bob）は
/// 自身の Private 行（id=12, lang="ja"）を集計対象に含む。期待値は
/// `EngineCore::execute_sql_in_session` を `seed_aggregate_three_tenant_db`
/// と同じ seed・各テナントの `PolicyContext::with_visibilities(tenant,
/// [Public, Private])` で直接呼ぶ独立オラクルにより導出した固定リテラルで、
/// production の判定関数を経由しない（carol は Private 行を持たないため
/// 元の集計結果のまま不変であることも非漏えいの証跡になる）。
///
/// すべての SELECT に一意の `AS` 別名を付け、NULL を返す SQL は使わない
/// （Node `pg` は `Object.values(row)` で行を出力するため同名列が潰れ、NULL の
/// 描画も psql（空文字）／pg（`Array.join` で空文字）／psycopg（`str(None)`=
/// "None"）で異なる。NULL 契約の検証は層 A に閉じる）。集計列は
/// `result_encoder.rs` の `ColumnMeta::Computed` 契約により実行時型に関わらず
/// 常に wire 型 `text`（OID 25）で送出されるため、`AVG` の非整数値（"4.25"
/// 等）も 3 クライアントで文字列として同一に描画される。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_verify_aggregate_queries_and_rls_invariance() {
    let (db_path, _db_guard) = seed_aggregate_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-aggregate-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    const AGG1_SQL: &str = "SELECT COUNT(*) AS n, SUM(id) AS s, AVG(id) AS a, MIN(lang) AS l_min, MAX(lang) AS l_max FROM docs";
    const AGG2_SQL: &str = "SELECT COUNT(*) AS n FROM docs WHERE lang = 'ja'";
    const AGG3_SQL: &str = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY n DESC";
    const AGG4_SQL: &str = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang HAVING n >= 2";

    // テナント別の独立オラクル値（RLS-11 導入後。alice=own Private id=11
    // lang="xx"・bob=own Private id=12 lang="ja"・carol=Private 行なし）。
    let alice_agg1 = vec!["4|17|4.25|en|xx".to_string()];
    let alice_agg2 = vec!["2".to_string()];
    let alice_agg3 = vec!["ja|2".to_string(), "en|1".to_string(), "xx|1".to_string()];
    let alice_agg4 = vec!["ja|2".to_string()];

    let bob_agg1 = vec!["4|18|4.5|en|ja".to_string()];
    let bob_agg2 = vec!["3".to_string()];
    let bob_agg3 = vec!["ja|3".to_string(), "en|1".to_string()];
    let bob_agg4 = vec!["ja|3".to_string()];

    let carol_agg1 = vec!["3|6|2|en|ja".to_string()];
    let carol_agg2 = vec!["2".to_string()];
    let carol_agg3 = vec!["ja|2".to_string(), "en|1".to_string()];
    let carol_agg4 = vec!["ja|2".to_string()];

    for (user, pw, expected_agg1, expected_agg2, expected_agg3, expected_agg4) in [
        (
            "alice",
            "pw-alice",
            &alice_agg1,
            &alice_agg2,
            &alice_agg3,
            &alice_agg4,
        ),
        ("bob", "pw-bob", &bob_agg1, &bob_agg2, &bob_agg3, &bob_agg4),
        (
            "carol",
            "pw-carol",
            &carol_agg1,
            &carol_agg2,
            &carol_agg3,
            &carol_agg4,
        ),
    ] {
        for (label, sql, expected) in [
            ("AGG1", AGG1_SQL, expected_agg1),
            ("AGG2", AGG2_SQL, expected_agg2),
            ("AGG3", AGG3_SQL, expected_agg3),
            ("AGG4", AGG4_SQL, expected_agg4),
        ] {
            let psql_rows = run_psql(port, user, pw, sql);
            assert_eq!(
                &psql_rows, expected,
                "psql: unexpected {label} result for user {user} (must include only this \
                 tenant's own Private group, never another tenant's)"
            );

            let psycopg_rows = run_psycopg(port, user, pw, sql);
            assert_eq!(
                &psycopg_rows, expected,
                "psycopg: unexpected {label} result for user {user}"
            );

            let pg_rows = run_pg(port, user, pw, sql);
            assert_eq!(
                &pg_rows, expected,
                "pg: unexpected {label} result for user {user}"
            );
        }

        // 拒否経路: 型不整合（VECTOR 列への SUM）・許可形状外
        // （集計と裸の列の混在）はいずれも接続を破棄せず拒否される。
        const REJECT_TYPE_MISMATCH_SQL: &str = "SELECT SUM(embedding) FROM docs";
        run_psql_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");
        run_pg_session_expect_sqlstate(port, user, pw, &[], REJECT_TYPE_MISMATCH_SQL, "22000");

        const REJECT_MIXED_SHAPE_SQL: &str = "SELECT COUNT(*), lang FROM docs";
        run_psql_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
        run_psycopg_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
        run_pg_session_expect_sqlstate(port, user, pw, &[], REJECT_MIXED_SHAPE_SQL, "42601");
    }

    drop(server);
    let _ = std::io::stdout().flush();
}

/// TASK-187（SQL-11）: `docs` とは別名の任意テーブル（`kb_articles`）でも、
/// wire 経由・3 クライアントで C1 相当 SELECT と `INSERT ... USING
/// OPERATION_ID` が `docs` と同じ契約（成否・RLS 暗黙適用）で通ることを
/// 確認する。評価順序・台帳スコープ・複数次元共存・`42P01` は engine 側
/// `crates/engine/tests/arbitrary_table.rs`（TASK-81）が既に機械検証済みの
/// ため、本テストは wire 経由の C1 相当 SELECT・`INSERT` の成否契約確認に
/// 限定する。
///
/// wire 認証経路の `PolicyContext` は `Public` ＋ 自テナントの `Private` を
/// 許可可視性とする（RLS-11・TASK-195。read-your-writes）ため、
/// `seed_arbitrary_table_three_tenant_db` の tenant-a 自身の Private 行
/// （id=11）は alice の C1 に含まれる（bob・carol は Private 行を持たない
/// ため元の Public 3 行のまま）。さらに本テストは `INSERT` 後の可視性を
/// (1) 同一接続（`run_*_session` の prelude に `INSERT` を積み、続く C1 を
/// 同じ接続で実行）、(2) 同一テナントの新規接続（`run_*` で改めて接続）の
/// 双方で確認し、(3) 後続テナントの反復で先行テナントの挿入 id が現れない
/// こと（他テナントへ越境しないこと）を、反復ごとに再構築する期待集合との
/// 完全一致（`assert_eq!` の集合比較）で固定する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_and_insert_on_arbitrary_table() {
    let (db_path, _db_guard) = seed_arbitrary_table_three_tenant_db();
    let users_dir = temp_db::TempDir::new("three-client-e2e-arbitrary-table-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);

    let server = spawn_wire_server(&users_path, &db_path, &[]);
    let port = server.port;

    let insert_sql = |id: u64, op: &str| -> String {
        format!(
            "INSERT INTO kb_articles (id, embedding, lang, body) VALUES \
             ({id}, '[0.5,0.5]', 'en', 'inserted via three-client e2e') \
             USING OPERATION_ID '{op}'"
        )
    };

    // `assert_set_eq` は順序を問わない集合一致（distance の同点タイブレーク
    // 順まで固定オラクルへ持ち込まないための比較。id の重複が無いことは
    // production の `id` 主キー一意性契約に委ねる）。
    let assert_set_eq =
        |actual: Vec<String>, expected: &std::collections::BTreeSet<String>, ctx: &str| {
            let actual_set: std::collections::BTreeSet<String> = actual.into_iter().collect();
            assert_eq!(&actual_set, expected, "{ctx}");
        };

    // tenant ごとに id・operation_id のブロックを分け、台帳（TASK-93・
    // RECOVER-2）の内容照合ハッシュ（TASK-101・RECOVER-10）が誤って
    // 別テナント・別クライアントの再送と衝突判定しないようにする。
    for (i, (user, pw)) in [
        ("alice", "pw-alice"),
        ("bob", "pw-bob"),
        ("carol", "pw-carol"),
    ]
    .into_iter()
    .enumerate()
    {
        // このテナントの可視集合は seed 由来の Public 3 行 + （alice のみ）
        // 自身の Private 行 id=11 から始まる。他テナントが直前の反復で
        // kb_articles へ挿入した id はここには含まれない（越境しないことの
        // 証跡そのもの）。
        let mut expected: std::collections::BTreeSet<String> =
            ["1", "2", "3"].into_iter().map(str::to_string).collect();
        if user == "alice" {
            expected.insert("11".to_string());
        }

        // 挿入前: 新規接続の C1 は seed 由来の可視集合とちょうど一致する。
        assert_set_eq(
            run_psql(port, user, pw, C1_SQL_ARBITRARY_TABLE),
            &expected,
            &format!("psql: unexpected pre-insert C1 result on kb_articles for user {user}"),
        );

        let base_id: u64 = 200 + (i as u64) * 10;

        // psql: 同一接続（prelude の INSERT → 同じ接続で C1）で自分が
        // 書いた行を直ちに読み戻せる（RLS-11・read-your-writes）。
        let insert1 = insert_sql(base_id, &format!("arbitrary-table-e2e-insert-psql-{user}"));
        let rows = run_psql_session(port, user, pw, &[&insert1], C1_SQL_ARBITRARY_TABLE);
        expected.insert(base_id.to_string());
        assert_set_eq(
            rows,
            &expected,
            &format!("psql: same-connection C1 must observe the row just inserted by {user}"),
        );

        // psycopg: 同様に同一接続での read-your-writes を確認する。
        let insert2 = insert_sql(
            base_id + 1,
            &format!("arbitrary-table-e2e-insert-psycopg-{user}"),
        );
        let rows = run_psycopg_session(port, user, pw, &[&insert2], C1_SQL_ARBITRARY_TABLE);
        expected.insert((base_id + 1).to_string());
        assert_set_eq(
            rows,
            &expected,
            &format!("psycopg: same-connection C1 must observe the row just inserted by {user}"),
        );

        // pg: 同様に同一接続での read-your-writes を確認する。
        let insert3 = insert_sql(
            base_id + 2,
            &format!("arbitrary-table-e2e-insert-pg-{user}"),
        );
        let rows = run_pg_session(port, user, pw, &[&insert3], C1_SQL_ARBITRARY_TABLE);
        expected.insert((base_id + 2).to_string());
        assert_set_eq(
            rows,
            &expected,
            &format!("pg: same-connection C1 must observe the row just inserted by {user}"),
        );

        // 挿入後: 同一テナントの**新規接続**（`run_psql` は毎回新しい接続を
        // 張る）でも 3 件すべてが可視のまま（同一テナント別セッションの
        // read-your-writes）。
        assert_set_eq(
            run_psql(port, user, pw, C1_SQL_ARBITRARY_TABLE),
            &expected,
            &format!("psql: fresh same-tenant connection must observe all rows {user} inserted"),
        );
    }

    drop(server);
    let _ = std::io::stdout().flush();
}
// -----------------------------------------------------------------------
// Issue #706: 緊急応答（TASK-97・TASK-153・ERR-5）の 3 クライアント detail
// 到達検証。
// -----------------------------------------------------------------------

/// psql（無改造）で `sql` を送り、commit 後 panic の緊急応答（TASK-97・
/// TASK-153・ERR-5）を検証する。psql は `ErrorResponse` 受信直後の接続断を
/// 「connection to server was lost」として終了コード 2 で報告するため
/// （通常の拒否経路の終了コード 1 とは異なる）、`run_psql_session_expect_sqlstate`
/// と同じ `!success()` のみで判定する。`-v VERBOSITY=verbose` で `DETAIL:`
/// 行を出力させ、`LC_ALL=C` で libpq の gettext 翻訳によるラベル文言差を
/// 避ける（`DETAIL:` 自体は翻訳されうるが、`state=may_be_committed` の
/// 生値は翻訳対象外）。観測した stderr 全文を返す（実行記録用）。
fn run_psql_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let output = Command::new(&psql)
        .env("PGPASSWORD", password)
        .env("LC_ALL", "C")
        .args([
            "-h",
            "127.0.0.1",
            "-p",
            &port.to_string(),
            "-U",
            user,
            "-d",
            "irrelevant-db-name",
            "-X",
            "-w",
            "-q",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            "VERBOSITY=verbose",
            "-c",
            sql,
        ])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"));
    assert!(
        !output.status.success(),
        "psql must exit non-zero after the emergency response / connection loss"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("XX000"),
        "expected SQLSTATE XX000 in psql stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("state=may_be_committed"),
        "expected ERR-5 detail 'state=may_be_committed' in psql stderr, got: {stderr}"
    );
    stderr
}

/// psycopg（無改造）で `sql` を送り、commit 後 panic の緊急応答を検証する
/// （`psycopg_client.py` が `e.diag.message_detail` を `[DETAIL=...]` として
/// stderr へ出力する。Issue #706 で追加）。
fn run_psycopg_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let output = spawn_psycopg_client(port, user, password, &[], sql);
    assert!(
        !output.status.success(),
        "psycopg_client.py must exit non-zero for the emergency response"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("[SQLSTATE=XX000]"),
        "expected [SQLSTATE=XX000] in psycopg_client.py stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("[DETAIL=state=may_be_committed]"),
        "expected ERR-5 [DETAIL=state=may_be_committed] in psycopg_client.py stderr, got: {stderr}"
    );
    stderr
}

/// node `pg`（無改造）で `sql` を送り、commit 後 panic の緊急応答を検証する
/// （`pg_client.js` が `err.detail` を `[DETAIL=...]` として stderr へ出力する。
/// Issue #706 で追加）。
fn run_pg_expect_emergency(port: u16, user: &str, password: &str, sql: &str) -> String {
    let output = spawn_pg_client(port, user, password, &[], sql);
    assert!(
        !output.status.success(),
        "pg_client.js must exit non-zero for the emergency response"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.contains("[SQLSTATE=XX000]"),
        "expected [SQLSTATE=XX000] in pg_client.js stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("[DETAIL=state=may_be_committed]"),
        "expected ERR-5 [DETAIL=state=may_be_committed] in pg_client.js stderr, got: {stderr}"
    );
    stderr
}

/// commit 成功境界を跨いだ panic（TASK-97・RECOVER-6）の緊急応答が、無改造の
/// psql／psycopg／node `pg` それぞれのドライバ API から観測できる `detail`
/// フィールド（ERR-5・`state=may_be_committed`）として実際に到達することを
/// 検証する（Issue #706）。`--fault-inject post-commit-panic`（Issue #705）は
/// 1 プロセスにつき 1 回のみ発火する take-once 契約のため、クライアントごとに
/// 独立したサーバー・DB・テナントを用意する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg, and a `--features fault-injection` \
            build; run via `make e2e-three-client`"]
fn three_clients_receive_emergency_response_detail_after_post_commit_panic() {
    struct Case {
        client: &'static str,
        user: &'static str,
        password: &'static str,
        tenant: &'static str,
        id: u64,
    }

    let cases = [
        Case {
            client: "psql",
            user: "alice",
            password: "pw-alice",
            tenant: "tenant-a",
            id: 901,
        },
        Case {
            client: "psycopg",
            user: "bob",
            password: "pw-bob",
            tenant: "tenant-b",
            id: 902,
        },
        Case {
            client: "pg",
            user: "carol",
            password: "pw-carol",
            tenant: "tenant-c",
            id: 903,
        },
    ];

    for case in cases {
        let (db_path, _db_guard) = seed_three_tenant_db();
        let users_dir = temp_db::TempDir::new("three-client-e2e-emergency-users");
        let users_path = users_dir.path().join("users.txt");
        write_users_file(&users_path);

        let mut server = spawn_wire_server(
            &users_path,
            &db_path,
            &["--fault-inject".into(), "post-commit-panic".into()],
        );
        // 「fault injection armed」が listen 到達前に観測されること（非
        // vacuous な arm 確認。`wire_fault_injection_cli.rs::
        // armed_post_commit_panic_sends_emergency_response_then_aborts` と
        // 同じ検査方針）。feature 無効ビルドで実行された場合は `--fault-inject`
        // が未知引数として拒否され listen 行に到達できないため、この時点で
        // `spawn_wire_server` 側の panic として明示的に失敗する。
        assert!(
            server
                .startup_lines
                .iter()
                .any(|l| l.contains("fault injection armed")),
            "client={}: expected 'fault injection armed' before listen; lines={:?}",
            case.client,
            server.startup_lines,
        );

        let insert_sql = format!(
            "INSERT INTO docs (id, embedding, lang, body) VALUES \
             ({}, '[0.5,0.5]', 'en', 'emergency e2e') \
             USING OPERATION_ID 'emergency-e2e-{}'",
            case.id, case.client
        );

        let stderr = match case.client {
            "psql" => run_psql_expect_emergency(server.port, case.user, case.password, &insert_sql),
            "psycopg" => {
                run_psycopg_expect_emergency(server.port, case.user, case.password, &insert_sql)
            }
            "pg" => run_pg_expect_emergency(server.port, case.user, case.password, &insert_sql),
            other => panic!("unknown client label: {other}"),
        };
        eprintln!("[e2e-record] {}: {stderr}", case.client);

        let status = server.wait_for_exit(Duration::from_secs(30));
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert!(
                !status.success(),
                "client={}: server must not exit successfully; status={status:?}",
                case.client
            );
            assert_eq!(
                status.signal(),
                Some(6),
                "client={}: server must be terminated by SIGABRT \
                 (std::process::abort); status={status:?}",
                case.client
            );
        }

        // commit 自体は成功しているため、再オープン後も投入行が可視のまま
        // であること（`state=may_be_committed` が実際に committed だった
        // ことの確認。他テナントの Public 行 3 件と合わせて 4 件になる）。
        drop(server);
        let storage = Storage::open(&db_path).expect("reopen storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let read_ctx = PolicyContext::with_visibilities(
            case.tenant,
            [Visibility::Public, Visibility::Private],
        )
        .expect("valid tenant");
        let result = core
            .execute_sql(
                &read_ctx,
                "SELECT id FROM docs ORDER BY embedding <=> '[0.5,0.5]' LIMIT 10",
            )
            .expect("select should succeed after the emergency-abort path");
        assert_eq!(
            result.rows.len(),
            4,
            "client={}: expected 3 seeded Public rows + 1 committed Private row",
            case.client
        );
        assert!(
            result.rows.iter().any(|row| row.id == case.id),
            "client={}: expected the committed row (id={}) to remain visible; rows={:?}",
            case.client,
            case.id,
            result.rows,
        );
    }
}
