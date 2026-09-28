//! `--search-engine` opt-in（Issue #656）をバイナリ子プロセスとして起動し、
//! CLI 引数の受理・拒否（fail-closed）を外形的に検証する結合テスト。
//!
//! `tests/wire7_bind_guard.rs`（TASK-70・WIRE-7）・
//! `tests/three_client_e2e.rs::spawn_wire_server` と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行または非 0 終了・エラーメッセージを外形的に確認する）。
//!
//! `EXPLAIN`（Issue #411 の `engine:`／`hnsw_params:` 行）はクエリプランナー
//! 注入（`--planner-endpoint`／`--planner-model`。TASK-117）が必須で、実
//! Ollama 前提の CLI からは検証できないため、R4 の wire 経由観測は
//! `tests/wire_search_engine_opt.rs`（in-process。決定的スタブ `LlmClient`）が
//! 担う。本ファイルは:
//!
//! - R1: 4 トークン（`default`／`hnsw`／`hnsw_f16`／`hnsw_i8`）いずれも
//!   `listening on` に到達すること
//! - R2: 未指定と `--search-engine default` の 2 プロセスで、同一 C1 クエリの
//!   受信バイト列（`RowDescription`〜`ReadyForQuery`）が完全一致すること
//!   （既定経路がコード上も無変更であることの実測面の裏付け）
//! - R3: 不正値・値欠落・重複指定は非 0 終了・stderr に `--search-engine` を
//!   含む説明が出ること
//!
//! に加え、Issue #657（フィルタ付き ANN の探索パラメータ opt-in 露出）分の
//!
//! - R6: `--hnsw-full-scan-ratio`／`--hnsw-acorn-max-visible-ratio`／
//!   `--hnsw-sparse-visited-max` を正常値で指定した 3 エンジントークンいずれも
//!   `listening on` に到達すること
//! - R7: 値欠落・形状不正・意味不正・重複指定・D1（`--search-engine`
//!   未指定／`default` との組合せ）はいずれも非 0 終了・該当フラグ名を含む
//!   stderr が出ること
//! - R8: `--search-engine hnsw` 単独と、既定値を明示した
//!   `--hnsw-full-scan-ratio 1/10 --hnsw-sparse-visited-max 0` を加えた
//!   プロセスとで、同一 C1 クエリの受信バイト列が完全一致すること
//!   （未指定＝既定値の裏付け）
//!
//! を固定する。

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
/// 置き場）を確保し、`Drop` で確実に削除するガード（`wire7_bind_guard.rs::
/// TempUserStore` と同型。Issue #172 の一意性対策を踏襲）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-search-engine-cli-{label}-{}-{}-{}",
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

/// 空のユーザーストア（R1・R3 は認証まで到達する必要がないため）。
fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// `alice:tenant-a:<phc>` の 1 行を持つユーザーストア（R2 の認証・簡易クエリ
/// 発行に使う）。パスワードハッシュは `wire-server hash-password` サブ
/// コマンドを子プロセスとして呼び、平文をテストコード内に決め打ちしない
/// （`crates/wire-server/src/main.rs::run_hash_password` と同じ経路を通す）。
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
/// 用意する（R2 の C1 クエリが決定的な 1 行を返すようにするため）。呼び出し元
/// が子プロセスを起動する **前に** `Storage` を drop すること（redb は
/// ファイルロックを持つため、開いたまま子プロセスの `EngineCore::open` を
/// 呼ぶと衝突する）。
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

/// R1: 4 トークンすべてで起動が拒否されず `listening on` に到達すること。
#[test]
fn all_four_tokens_start_listening() {
    for token in ["default", "hnsw", "hnsw_f16", "hnsw_i8"] {
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
                "--search-engine",
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

/// R3: 不正値・値欠落・重複指定はいずれも fail-closed（非 0 終了・
/// `--search-engine` を含む stderr）。
#[test]
fn invalid_or_missing_or_duplicate_search_engine_arg_is_rejected() {
    let fixture = TempFixtureDir::new("r3");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    let cases: [&[&str]; 5] = [
        &["--search-engine", "bogus"],
        &["--search-engine", "HNSW"],
        // ベンチ env のトークン（`RecallEngine`）は CLI では受理しない
        // （`search_engine_opt.rs` モジュールドキュメント参照）。
        &["--search-engine", "brute_force"],
        // 直後の既知フラグ名をそのまま値として食い、閉じた語彙のいずれとも
        // 一致しないため拒否されるケース（`--search-engine` が次トークンを
        // 無条件で値とみなす仕様上、これも fail-closed で弾かれることの確認）。
        &["--search-engine", "--bind"],
        // 重複指定（D6）。
        &["--search-engine", "hnsw", "--search-engine", "hnsw_f16"],
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
            stderr.contains("--search-engine"),
            "args={extra_args:?}: expected stderr to mention --search-engine, got: {stderr}"
        );
    }

    // 値欠落単体（末尾に `--search-engine` だけを置くケース）。
    let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &users_path,
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--search-engine",
        ])
        .output()
        .expect("spawn wire-server");
    assert!(!output.status.success(), "missing value must be rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--search-engine"),
        "expected stderr to mention --search-engine, got: {stderr}"
    );
}

/// 認証後、簡易クエリ 1 文を送って `RowDescription`〜`ReadyForQuery` までの
/// 生バイト列をそのまま返す（バイト列比較のため wire フレームの構造には
/// 立ち入らず、受信済みバイト列を単純連結する）。
fn run_c1_query_and_collect_bytes(addr: std::net::SocketAddr) -> Vec<u8> {
    // 認証フローそのもの（`StartupMessage`〜認証後 `ReadyForQuery` までの
    // `BackendKeyData`／`ParameterStatus*` を含む可変長メッセージ列）は
    // `tests/common/mod.rs::authenticate_to_ready_for_query`（`wire1_simple_query.rs`
    // 等が使う既存ヘルパー）にそのまま委譲する。本テストの比較対象は
    // 「簡易クエリ 1 文への応答バイト列」に絞り、認証フローの実装詳細
    // （ParameterStatus の内容等）には立ち入らない。
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    // 簡易クエリ（'Q'）を送る。
    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1";
    let mut q_body = Vec::new();
    q_body.extend_from_slice(sql.as_bytes());
    q_body.push(0);
    let mut q_msg = Vec::new();
    q_msg.push(b'Q');
    q_msg.extend_from_slice(&((q_body.len() + 4) as i32).to_be_bytes());
    q_msg.extend_from_slice(&q_body);
    stream.write_all(&q_msg).expect("send simple query");

    // RowDescription('T') → DataRow('D') → CommandComplete('C') →
    // ReadyForQuery('Z') の 4 フレームを、各フレームの長さプレフィックスに
    // 従ってそのまま読み切る（内容の意味解釈はせず、受信バイト列比較用に
    // 蓄積するだけ）。
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

/// R2: 未指定と `--search-engine default` は同一クエリに対し完全に同一の
/// 受信バイト列を返す（既定経路がコード上もビット同一であることの実測面の
/// 裏付け。`main.rs::run_server` が `search_engine_kind == None` の場合のみ
/// `EngineCore::open` をそのまま呼ぶ設計の外形的な確認）。
#[test]
fn unset_and_default_token_produce_identical_wire_bytes() {
    let mut outputs: Vec<Vec<u8>> = Vec::new();

    for extra_args in [Vec::<&str>::new(), vec!["--search-engine", "default"]] {
        let fixture = TempFixtureDir::new("r2");
        let users_path = fixture.users_path_str();
        write_user_store_with_alice(&users_path);
        let db_path = fixture.db_path_str();
        // 子プロセス起動前に `Storage` を必ず drop する（redb のファイル
        // ロック回避。`seed_single_row_db` 内で drop 済み）。
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

        // `listening on 127.0.0.1:<port>` から実際の bind アドレスを取得する
        // （`--bind 127.0.0.1:0` の ephemeral port 割り当て結果。
        // `docs/design/three-client-e2e-harness.md` と同じ取得手順）。
        // `common::drain_stderr` は listen 行到達後も stderr を EOF まで
        // 読み続ける（Issue #1082）。診断情報（`args`・観測済み `lines`）は
        // 呼び出し側でメッセージへ含める。
        let mut drain = common::drain_stderr(&mut child);
        let (addr, lines) = drain.wait_for_listening(Duration::from_secs(10));
        let addr = addr.unwrap_or_else(|| {
            panic!("did not observe listening address, args={args:?} lines={lines:?}")
        });

        let bytes = run_c1_query_and_collect_bytes(addr);

        let _ = child.kill();
        let _ = child.wait();

        outputs.push(bytes);
    }

    assert_eq!(
        outputs[0], outputs[1],
        "unset --search-engine and --search-engine default must produce identical wire bytes"
    );
}

/// R6: `--hnsw-*` 3 フラグを正常値で指定した各エンジントークンで起動が拒否
/// されず `listening on` に到達すること。
#[test]
fn hnsw_tuning_flags_with_valid_values_start_listening() {
    for token in ["hnsw", "hnsw_f16", "hnsw_i8"] {
        let fixture = TempFixtureDir::new(&format!("r6-{token}"));
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
                "--search-engine",
                token,
                "--hnsw-full-scan-ratio",
                "1/4",
                "--hnsw-acorn-max-visible-ratio",
                "1/2",
                "--hnsw-sparse-visited-max",
                "8",
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
            "token={token}: expected to reach listening state with hnsw tuning flags"
        );
    }
}

/// R7: 値欠落・形状不正・意味不正・重複指定・D1（`--search-engine` 未指定／
/// `default` との組合せ）はいずれも非 0 終了・該当フラグ名を含む stderr。
#[test]
fn hnsw_tuning_flag_rejection_matrix() {
    let fixture = TempFixtureDir::new("r7");
    let users_path = fixture.users_path_str();
    write_empty_user_store(&users_path);
    let db_path = fixture.db_path_str();

    // (追加引数, stderr に含まれるべきフラグ名)
    let cases: Vec<(Vec<&str>, &str)> = vec![
        // 値欠落（末尾単体）。
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-acorn-max-visible-ratio"],
            "--hnsw-acorn-max-visible-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-sparse-visited-max"],
            "--hnsw-sparse-visited-max",
        ),
        // 次トークンが別フラグ名の場合、既存 `--search-engine` と同じ
        // 「次トークンを無条件で値とみなす」仕様により、そのフラグ名文字列が
        // そのまま値として飲み込まれる（ここでは末尾に他の引数を残さない
        // ことで、消費後に余る `8` が「unknown argument」として先に検出
        // されてしまう混同を避ける）。結果として `parse_ratio` が
        // `--hnsw-sparse-visited-max` という文字列を `<num>/<den>` として
        // 拒否し、`--hnsw-full-scan-ratio` のフラグ名を含むエラーになる。
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-full-scan-ratio",
                "--hnsw-sparse-visited-max",
            ],
            "--hnsw-full-scan-ratio",
        ),
        // 形状不正。
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", "1"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", "1/"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", "0.5"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", " 1/2"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-sparse-visited-max",
                "abc",
            ],
            "--hnsw-sparse-visited-max",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-sparse-visited-max", "-1"],
            "--hnsw-sparse-visited-max",
        ),
        // 意味不正。
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", "1/0"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec!["--search-engine", "hnsw", "--hnsw-full-scan-ratio", "3/2"],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-acorn-max-visible-ratio",
                "1/20",
            ],
            "--hnsw-acorn-max-visible-ratio",
        ),
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-full-scan-ratio",
                "1/2",
                "--hnsw-acorn-max-visible-ratio",
                "1/4",
            ],
            "--hnsw-acorn-max-visible-ratio",
        ),
        // 重複指定。
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-full-scan-ratio",
                "1/4",
                "--hnsw-full-scan-ratio",
                "1/2",
            ],
            "--hnsw-full-scan-ratio",
        ),
        (
            vec![
                "--search-engine",
                "hnsw",
                "--hnsw-sparse-visited-max",
                "1",
                "--hnsw-sparse-visited-max",
                "2",
            ],
            "--hnsw-sparse-visited-max",
        ),
        // D1: `--search-engine default` との組合せ。
        (
            vec![
                "--search-engine",
                "default",
                "--hnsw-sparse-visited-max",
                "8",
            ],
            "--search-engine",
        ),
        // D1: `--search-engine` 未指定との組合せ。
        (vec!["--hnsw-full-scan-ratio", "1/4"], "--search-engine"),
    ];

    for (extra_args, expected_flag) in cases {
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
    }
}

/// R8: `--search-engine hnsw` 単独と、既定値を明示した
/// `--hnsw-full-scan-ratio 1/10 --hnsw-sparse-visited-max 0` を加えたプロセス
/// とで、同一 C1 クエリの受信バイト列が完全一致すること（未指定＝既定値の
/// 裏付け。`acorn_max_visible_ratio` は「既定＝none」を表す明示値が無いため
/// 対象外とし、単体テストの `PartialEq`（`search_engine_opt::tests::
/// empty_tuning_matches_to_engine_kind_for_all_hnsw_tokens`）で担保する）。
#[test]
fn hnsw_token_alone_and_with_explicit_default_tuning_produce_identical_wire_bytes() {
    let mut outputs: Vec<Vec<u8>> = Vec::new();

    for extra_args in [
        vec!["--search-engine", "hnsw"],
        vec![
            "--search-engine",
            "hnsw",
            "--hnsw-full-scan-ratio",
            "1/10",
            "--hnsw-sparse-visited-max",
            "0",
        ],
    ] {
        let fixture = TempFixtureDir::new("r8");
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

        // `common::drain_stderr` は listen 行到達後も stderr を EOF まで
        // 読み続ける（Issue #1082）。診断情報（`args`・観測済み `lines`）は
        // 呼び出し側でメッセージへ含める。
        let mut drain = common::drain_stderr(&mut child);
        let (addr, lines) = drain.wait_for_listening(Duration::from_secs(10));
        let addr = addr.unwrap_or_else(|| {
            panic!("did not observe listening address, args={args:?} lines={lines:?}")
        });

        let bytes = run_c1_query_and_collect_bytes(addr);

        let _ = child.kill();
        let _ = child.wait();

        outputs.push(bytes);
    }

    assert_eq!(
        outputs[0], outputs[1],
        "hnsw alone and hnsw with explicit default tuning must produce identical wire bytes"
    );
}
