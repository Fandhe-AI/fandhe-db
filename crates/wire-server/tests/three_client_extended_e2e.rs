//! Issue #1176（対象ビヘイビア: WIRE-11・WIRE-12・WIRE-13・WIRE-14、SQL-10、RLS-11、
//! TASK-73・TASK-217・TASK-218）の層 B: 無改造の実クライアント 3 種（psql／Python
//! `psycopg`／Node.js `pg`）から `wire-server` バイナリへ実接続し、**拡張クエリ
//! プロトコル**（Parse／Describe／Bind／Execute）・**型復元**・**バイナリ結果受信**を
//! 検証する統合テスト。
//!
//! 責務境界: 既存の層 B（`tests/three_client_e2e.rs`・`tests/extended_syntax_e2e.rs`）は
//! 簡易クエリ経路のみを駆動する。`$n` 束縛（WIRE-12）とバイナリ結果の型拡張
//! （WIRE-14）が wire へ入った後の申し送りを本ファイルが回収する。各規則そのものは
//! 層 A（`tests/wire11_*.rs`・`wire12_param_binding.rs`・`wire13_type_oid.rs`・
//! `wire14_*.rs`、常時 `make ci`）が確定オラクルとして検証済みで、本ファイルは同じ
//! 挙動が無改造クライアント経由でも観測できることの代表ケース確認に徹する。
//!
//! クライアントごとの拡張プロトコル駆動方法:
//! - psql: stdin へ `<SQL> \bind '<p1>' ... \g` を流す（`-c` はメタコマンドと混在不可）。
//! - psycopg: `three_client/psycopg_extended.py`（`RawCursor`、パラメータ無しは
//!   `prepare=True`）。
//! - node pg: `three_client/pg_extended.js`（values 付き、パラメータ無しは
//!   `queryMode: 'extended'`）。
//!
//! 設計上の注意（詳細は ADR `docs/design/three-client-e2e-harness.md`）:
//! - `$n` の受理位置は限定されるため C4（`hybrid_rrf` 関数引数）は `$n` を持てず、
//!   パラメータ 0 個の拡張プロトコルで送る。関数引数への `$n` は `42601` で拒否される
//!   ことを負のケースとして固定する。
//! - `id` 列は numeric（OID 1700）でバイナリ非対応（`0A000`）。バイナリ受信の検証は
//!   `id`・VECTOR を投影しない型付きテーブル（`typed_items`）で行う。
//! - node pg のバイナリ受信は DataRow を UTF-8 文字列として読むため、0x80 以上の
//!   バイトを含む値は壊れる。3 クライアント共通行（行 A）は全送信バイトが `< 0x80` の
//!   値に限定し、その制約を非 ignore テスト（[`node_binary_fixture_bytes_are_utf8_safe`]）
//!   で機械検証する。負数・高位バイトの行 B は psycopg のみで検証する。
//! - psql は結果のバイナリ受信モードを持たず、`\gdesc` は `pg_catalog` に依存するため、
//!   型復元は PostgreSQL 正準テキスト表現の一致で確認する（OID の網羅は層 A）。
//!
//! ツール未検出・クライアントの想定外終了は `panic!` で失敗させ silent skip はしない。
//! ローカル・Docker 開発コンテナには実クライアントが無いため `#[ignore]` とし
//! `make e2e-three-client` から実行する（fixture ガードのみ常時実行）。

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

// --- fixture（seed とガードテストで共有する定数） -------------------------------

/// 行 A（3 クライアント共通。node pg のバイナリ制約を満たす全バイト `< 0x80`）。
const A_N: i32 = 7;
const A_B: i64 = 9_000_000_000;
const A_R: f32 = 0.5;
const A_D: f64 = 2.0;
const A_FLAG: bool = true;
const A_BLOB: &[u8] = b"ABC";
const A_UID: [u8; 16] = [0, 0, 0, 0, 0, 0, 0x40, 0, 0, 0, 0, 0, 0, 0, 0, 1];
const A_UID_TEXT: &str = "00000000-0000-4000-0000-000000000001";
const A_LABEL: &str = "shared-row";

/// 行 B（psycopg 専用。負数・高位バイトを含む）。
const B_N: i32 = -1;
const B_B: i64 = -9_000_000_000;
const B_R: f32 = -1.25;
const B_D: f64 = -0.5;
const B_FLAG: bool = false;
const B_BLOB: &[u8] = &[0x00, 0xFF, 0x80];
const B_UID: [u8; 16] = [
    0xff, 0xff, 0xff, 0xff, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 0xff,
];
const B_LABEL: &str = "psycopg-wide-row";

/// 行 C（RLS 用。tenant-a の Private 行）。
const SECRET_LABEL: &str = "secret-a";

/// C1〜C3（`three_client_e2e.rs` と同じ意味論の `$n` 版）。
const C1_SQL: &str = "SELECT id FROM docs ORDER BY embedding <=> $1 LIMIT 3";
const C2_SQL: &str = "SELECT id, lang FROM docs WHERE lang = $1 ORDER BY embedding <=> $2 LIMIT 3";
const C3_SQL: &str = "SELECT id FROM docs WHERE visible() ORDER BY embedding <=> $1 LIMIT 3";
/// C4（`$n` を持てないためリテラルのまま拡張プロトコルで送る）。
const C4_SQL: &str = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'zzz-term-absent-from-any-seed-body') LIMIT 3";
const INSERT_SQL_PREFIX: &str = "INSERT INTO docs (id, embedding, lang, body) VALUES (";
const READBACK_SQL: &str = "SELECT id, body FROM docs WHERE body = $1 LIMIT 10";
const TYPED_SQL: &str =
    "SELECT n, b, r, d, flag, blob, uid, label FROM typed_items WHERE label = $1 LIMIT 10";

// --- サーバー起動・ユーザーファイル（`three_client_e2e.rs` と同型） ---------------

fn resolve_tool(env_var: &str, default_name: &str) -> String {
    std::env::var(env_var).unwrap_or_else(|_| default_name.to_string())
}

/// `wire-server` 子プロセスのハンドル（`three_client_e2e.rs::ServerGuard` の簡略版）。
struct ServerGuard {
    child: Child,
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// listen 行からポートを得る。stderr は EOF まで読み続ける（読み手が閉じると
/// サーバー側の診断出力が失敗しうるため。Issue #943・#1081 の経緯）。
fn spawn_wire_server(users_path: &Path, db_path: &Path) -> ServerGuard {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            users_path.to_str().expect("utf-8 path"),
            "--db",
            db_path.to_str().expect("utf-8 path"),
            "--bind",
            "127.0.0.1:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server binary (built by `cargo test`)");

    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let _ = tx.send(std::mem::take(&mut line));
        }
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut port: Option<u16> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(addr) = line.trim().strip_prefix("wire-server: listening on ") {
                    if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
                        port = Some(addr.port());
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let Some(port) = port else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("wire-server did not report a listening port within the deadline");
    };
    ServerGuard { child, port }
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

const USERS: [(&str, &str); 3] = [
    ("alice", "pw-alice"),
    ("bob", "pw-bob"),
    ("carol", "pw-carol"),
];

// --- seed ---------------------------------------------------------------------

fn op(s: &str) -> OperationId {
    OperationId::parse(s).expect("valid operation_id")
}

/// `docs`（C1〜C4・INSERT 用。`three_client_e2e.rs::seed_three_tenant_db` と同一コーパス）
/// と `typed_items`（型復元・バイナリ用）を 1 つの DB に投入する。
fn seed_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-extended-e2e");
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
        .expect("create docs");
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
            &op("ext-e2e-docs-op"),
        )
        .expect("insert docs row");
    }

    storage
        .create_table(&TableSchema::new(
            "typed_items",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("label", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
                ColumnDef::new("uid", ColumnType::Uuid, true),
            ],
        ))
        .expect("create typed_items");
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let typed_row =
        |label: &str, n: i32, b: i64, r: f32, d: f64, flag: bool, blob: &[u8], uid: [u8; 16]| {
            vec![
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(label.to_string()),
                Value::Integer(n),
                Value::BigInt(b),
                Value::Real(r),
                Value::Double(d),
                Value::Bool(flag),
                Value::Bytes(blob.to_vec()),
                Value::Uuid(Uuid::from_bytes(uid)),
            ]
        };
    let rows = [
        (
            10u64,
            Visibility::Public,
            typed_row(A_LABEL, A_N, A_B, A_R, A_D, A_FLAG, A_BLOB, A_UID),
        ),
        (
            11,
            Visibility::Public,
            typed_row(B_LABEL, B_N, B_B, B_R, B_D, B_FLAG, B_BLOB, B_UID),
        ),
        (
            12,
            Visibility::Private,
            typed_row(SECRET_LABEL, 99, 99, 0.5, 0.5, true, b"S", A_UID),
        ),
    ];
    for (id, vis, row) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            "typed_items",
            &ctx_a,
            id,
            vis,
            &row,
            &op(&format!("ext-e2e-typed-op-{id}")),
        )
        .expect("insert typed row");
    }
    (path, guard)
}

// --- クライアント実行ヘルパー -----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Client {
    Psql,
    Psycopg,
    Pg,
}

impl Client {
    const ALL: [Client; 3] = [Client::Psql, Client::Psycopg, Client::Pg];
}

#[derive(Clone, Copy, Default)]
struct Opts {
    /// 結果をバイナリ形式で要求する（psql は非対応）。
    binary: bool,
    /// psycopg のみ: `params` を Python `int`（バイナリパラメータ）として束縛する。
    int_params: bool,
}

/// `["a","b"]` 形式の最小 JSON エンコーダ。`\`・制御文字を含む文字列は拒否する
/// （本ファイルの定数リテラルのみを渡す前提。fail-closed）。
fn json_string_array(items: &[&str]) -> String {
    let mut out = String::from("[");
    for (i, s) in items.iter().enumerate() {
        assert!(
            !s.contains('\\') && !s.chars().any(|c| c.is_control()),
            "unsupported character in test parameter: {s:?}"
        );
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&s.replace('"', "\\\""));
        out.push('"');
    }
    out.push(']');
    out
}

/// psql（無改造）へ stdin 経由で `\bind` 付きの 1 文を流す。パラメータが 0 個でも
/// `\bind \g` で拡張プロトコルを強制する。パラメータは本ファイルの定数のみを渡す前提
/// （`'` は `''` へエスケープ、制御文字・`\` は拒否）。
fn spawn_psql_bind(
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    verbose: bool,
) -> std::process::Output {
    let psql = resolve_tool("PSQL_BIN", "psql");
    let mut args: Vec<String> = vec![
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
        "-F",
        "|",
        "-v",
        "ON_ERROR_STOP=1",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if verbose {
        args.push("-v".into());
        args.push("VERBOSITY=verbose".into());
    }
    let mut script = String::from(sql);
    script.push_str(" \\bind");
    for p in params {
        assert!(
            !p.contains('\\') && !p.chars().any(|c| c.is_control()),
            "unsupported character in test parameter: {p:?}"
        );
        script.push_str(&format!(" '{}'", p.replace('\'', "''")));
    }
    script.push_str(" \\g\n");

    let mut child = Command::new(&psql)
        .env("PGPASSWORD", password)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            panic!("failed to spawn {psql} (install libpq-client tools or set PSQL_BIN): {e}")
        });
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(script.as_bytes())
        .expect("write psql script");
    child.wait_with_output().expect("wait psql")
}

fn spawn_psycopg_ext(
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
) -> std::process::Output {
    let python = resolve_tool("PYTHON_BIN", "python3");
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_extended.py");
    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if opts.int_params {
        cmd.env("WIRE_PARAMS_INT", format!("[{}]", params.join(",")));
    } else if !params.is_empty() {
        cmd.env("WIRE_PARAMS", json_string_array(params));
    } else {
        cmd.env("WIRE_EXTENDED_NOPARAM", "1");
    }
    if opts.binary {
        cmd.env("WIRE_BINARY", "1");
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"))
}

fn spawn_pg_ext(
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
) -> std::process::Output {
    let node = resolve_tool("NODE_BIN", "node");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_extended.js");
    let mut cmd = Command::new(&node);
    cmd.arg(&script)
        .env("WIRE_HOST", "127.0.0.1")
        .env("WIRE_PORT", port.to_string())
        .env("WIRE_USER", user)
        .env("WIRE_PASSWORD", password)
        .env("WIRE_SQL", sql);
    if !params.is_empty() {
        cmd.env("WIRE_PARAMS", json_string_array(params));
    } else {
        cmd.env("WIRE_EXTENDED_NOPARAM", "1");
    }
    if opts.binary {
        cmd.env("WIRE_BINARY", "1");
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"))
}

fn spawn_client(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
) -> std::process::Output {
    match client {
        Client::Psql => {
            assert!(!opts.binary, "psql has no binary-result mode");
            spawn_psql_bind(port, user, password, sql, params, false)
        }
        Client::Psycopg => spawn_psycopg_ext(port, user, password, sql, params, opts),
        Client::Pg => spawn_pg_ext(port, user, password, sql, params, opts),
    }
}

/// 拡張プロトコルで `sql` を実行し、結果行（`|` 区切り。psycopg／pg は
/// `<型名>:<値>` 形式）を返す。非 0 終了は stderr 付きで panic する。
fn run_ext(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
) -> Vec<String> {
    let output = spawn_client(client, port, user, password, sql, params, opts);
    assert!(
        output.status.success(),
        "{client:?} failed for user {user} on `{sql}`: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// 拒否経路: 非 0 終了かつ stderr に期待 SQLSTATE を含むこと。標準出力へ結果行を
/// 出していないこと（他テナント行の漏えいが無いこと）も確認する。
#[allow(clippy::too_many_arguments)] // 呼び出し側の可読性のため run_ext と同じ引数並びを保つ
fn expect_sqlstate(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
    sqlstate: &str,
) {
    let output = if client == Client::Psql {
        spawn_psql_bind(port, user, password, sql, params, true)
    } else {
        spawn_client(client, port, user, password, sql, params, opts)
    };
    assert!(
        !output.status.success(),
        "{client:?} must exit non-zero for a rejected statement: `{sql}`"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(sqlstate),
        "{client:?}: expected SQLSTATE {sqlstate} in stderr, got: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "{client:?}: rejected statement must not return rows"
    );
}

/// `<型名>:` プレフィックスを各セルから取り除く（psql の出力には無いため no-op）。
fn untag(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|r| {
            r.split('|')
                .map(|c| c.split_once(':').map_or(c, |(_, v)| v))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

// --- テスト -----------------------------------------------------------------------

/// 3 クライアントの拡張プロトコルで、3 テナントいずれのユーザーでも C1〜C4
/// （TASK-73／WIRE-1 と同じ独立オラクル）が簡易クエリ時と同じ結果になること
/// （WIRE-11・WIRE-12）。id の型復元（psycopg `Decimal`／node pg `string`）も確認する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_run_c1_through_c4_via_extended_query_protocol() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;

    let cases: [(&str, &str, Vec<&str>, Vec<String>); 4] = [
        ("C1", C1_SQL, vec!["[1.0,0.0]"], s(&["1", "2", "3"])),
        ("C2", C2_SQL, vec!["ja", "[1.0,0.0]"], s(&["1|ja", "3|ja"])),
        ("C3", C3_SQL, vec!["[1.0,0.0]"], s(&["1", "2", "3"])),
        ("C4", C4_SQL, vec![], s(&["1", "2", "3"])),
    ];
    for (user, pw) in USERS {
        for (label, sql, params, expected) in &cases {
            for client in Client::ALL {
                let rows = run_ext(client, port, user, pw, sql, params, Opts::default());
                assert_eq!(
                    &untag(&rows),
                    expected,
                    "{client:?}: unexpected {label} result for user {user}"
                );
                if *label == "C2" {
                    // id は numeric(1700)。ドライバのネイティブ型は psycopg=Decimal、
                    // node pg=string（WIRE-13 の記録済み判断）。psql は素の値。
                    let want = match client {
                        Client::Psql => s(&["1|ja", "3|ja"]),
                        Client::Psycopg => s(&["Decimal:1|str:ja", "Decimal:3|str:ja"]),
                        Client::Pg => s(&["string:1|string:ja", "string:3|string:ja"]),
                    };
                    assert_eq!(rows, want, "{client:?}: C2 native types for {user}");
                }
            }
        }
    }
    drop(server);
}

/// `$n` 束縛付き INSERT（SQL-10 相当）が 3 クライアントで成功し、挿入者の新規接続の
/// `$n` SELECT で読み戻せ（RLS-11）、他テナントの同じ SELECT には現れないこと
/// （非 vacuous な RLS 検証）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_insert_with_bound_values_and_respect_rls() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-insert-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;

    for (client, id, tag) in [
        (Client::Psql, 101, "psql"),
        (Client::Psycopg, 201, "psycopg"),
        (Client::Pg, 301, "pg"),
    ] {
        let sql = format!("{INSERT_SQL_PREFIX}{id}, $1, $2, $3) USING OPERATION_ID $4");
        let body = format!("ext-insert-{tag}");
        let op_id = format!("ext-e2e-insert-{tag}");
        let out = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            &sql,
            &["[0.5,0.5]", "ja", &body, &op_id],
            Opts::default(),
        );
        assert!(
            out.is_empty(),
            "{client:?}: INSERT must return no rows: {out:?}"
        );

        let want = match client {
            Client::Psql => vec![format!("{id}|{body}")],
            Client::Psycopg => vec![format!("Decimal:{id}|str:{body}")],
            Client::Pg => vec![format!("string:{id}|string:{body}")],
        };
        let alice = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            READBACK_SQL,
            &[&body],
            Opts::default(),
        );
        assert_eq!(alice, want, "{client:?}: alice must read back her row");
        for (other, pw) in [("bob", "pw-bob"), ("carol", "pw-carol")] {
            let rows = run_ext(
                client,
                port,
                other,
                pw,
                READBACK_SQL,
                &[&body],
                Opts::default(),
            );
            assert!(
                rows.is_empty(),
                "{client:?}: {other} must not see alice's row: {rows:?}"
            );
        }
    }
    drop(server);
}

fn typed_expected_text(client: Client) -> Vec<String> {
    match client {
        Client::Psql => vec![format!("7|9000000000|0.5|2|t|\\x414243|{A_UID_TEXT}|{A_LABEL}")],
        Client::Psycopg => vec![format!(
            "int:7|int:9000000000|float:0.5|float:2.0|bool:True|bytes:414243|UUID:{A_UID_TEXT}|str:{A_LABEL}"
        )],
        // int8 が string なのは node pg／pg-types の既定仕様（精度保持）。
        Client::Pg => vec![format!(
            "number:7|string:9000000000|number:0.5|number:2|boolean:true|Buffer:414243|string:{A_UID_TEXT}|string:{A_LABEL}"
        )],
    }
}

/// テキスト結果で 3 クライアントが数値・真偽値・bytea・uuid を各ドライバの
/// ネイティブ型で受け取ること（WIRE-13）。psql は PostgreSQL 正準テキスト表現で確認する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_decode_native_types_in_text_format() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-typed-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;

    for client in Client::ALL {
        let rows = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            TYPED_SQL,
            &[A_LABEL],
            Opts::default(),
        );
        assert_eq!(
            rows,
            typed_expected_text(client),
            "{client:?}: native type decode"
        );
    }
    drop(server);
}

/// バイナリ結果形式（WIRE-14）で受け取った値がテキスト形式の値と完全一致すること。
/// テキスト結果は上のテストで独立オラクルと照合済みのため推移的に正しい。psql は
/// バイナリ受信モードを持たず対象外。node pg は行 B（高位バイト）を扱わない
/// （ドライバの UTF-8 デコード制約。psycopg のみで検証）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_binary_results_match_text_results() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-binary-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;

    let bin = Opts {
        binary: true,
        ..Opts::default()
    };
    for client in [Client::Psycopg, Client::Pg] {
        let text = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            TYPED_SQL,
            &[A_LABEL],
            Opts::default(),
        );
        let binary = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            TYPED_SQL,
            &[A_LABEL],
            bin,
        );
        assert_eq!(binary, text, "{client:?}: binary must equal text (row A)");
        assert_eq!(binary, typed_expected_text(client));
    }
    let text = run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        TYPED_SQL,
        &[B_LABEL],
        Opts::default(),
    );
    let binary = run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        TYPED_SQL,
        &[B_LABEL],
        bin,
    );
    assert_eq!(binary, text, "psycopg: binary must equal text (row B)");
    assert_eq!(
        binary,
        vec![format!(
            "int:-1|int:-9000000000|float:-1.25|float:-0.5|bool:False|bytes:00ff80|UUID:ffffffff-0000-4000-8000-0000000000ff|str:{B_LABEL}"
        )],
        "psycopg: row B decoded values"
    );
    drop(server);
}

/// fail-closed 契約（`42601`／`0A000`）・RLS・インジェクション耐性が拡張プロトコル経路
/// でも維持されること。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_keep_fail_closed_contracts_on_extended_path() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-negative-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;
    let d = Opts::default();

    // 受理位置外の `$n`（hybrid 関数引数）は 42601。
    let hybrid = "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, $1, body, 'x') LIMIT 3";
    for client in Client::ALL {
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            hybrid,
            &["[1.0,0.0]"],
            d,
            "42601",
        );
    }

    // id（numeric）を含む結果のバイナリ要求は 0A000。
    let bin = Opts { binary: true, ..d };
    for client in [Client::Psycopg, Client::Pg] {
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            C1_SQL,
            &["[1.0,0.0]"],
            bin,
            "0A000",
        );
    }
    // 非 text スロットのバイナリパラメータ（Python int）は 0A000。
    let int_params = Opts {
        int_params: true,
        ..d
    };
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        "SELECT label FROM typed_items WHERE label = $1 LIMIT 10",
        &["1"],
        int_params,
        "0A000",
    );

    // RLS: tenant-a の Private 行は alice のみに見える。
    let secret_sql = "SELECT label FROM typed_items WHERE label = $1 LIMIT 10";
    for client in Client::ALL {
        let alice = untag(&run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            secret_sql,
            &[SECRET_LABEL],
            d,
        ));
        assert_eq!(
            alice,
            s(&[SECRET_LABEL]),
            "{client:?}: alice sees her private row"
        );
        let bob = run_ext(
            client,
            port,
            "bob",
            "pw-bob",
            secret_sql,
            &[SECRET_LABEL],
            d,
        );
        assert!(
            bob.is_empty(),
            "{client:?}: bob must not see tenant-a private row: {bob:?}"
        );
    }

    // インジェクション: 束縛値は不透明リテラルのまま（全行にマッチしない）。
    for client in Client::ALL {
        let rows = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            secret_sql,
            &["' OR '1'='1"],
            d,
        );
        assert!(
            rows.is_empty(),
            "{client:?}: bound value must stay opaque: {rows:?}"
        );
    }
    drop(server);
}

/// fixture ガード（常時実行）: 行 A の送信形式（バイナリ）が全バイト `< 0x80` であること
/// （node pg のバイナリ受信は DataRow を UTF-8 文字列として読むため、0x80 以上を含むと
/// 壊れて偽の失敗になる）と、行 B が意図どおり高位バイトを含むこと、REAL／DOUBLE 値が
/// f32 で厳密表現できること（テキストとバイナリの ドライバ側変換差による偽の不一致を
/// 避ける）を機械的に守る。
#[test]
fn node_binary_fixture_bytes_are_utf8_safe() {
    let mut a: Vec<u8> = Vec::new();
    a.extend_from_slice(&A_N.to_be_bytes());
    a.extend_from_slice(&A_B.to_be_bytes());
    a.extend_from_slice(&A_R.to_bits().to_be_bytes());
    a.extend_from_slice(&A_D.to_bits().to_be_bytes());
    a.push(u8::from(A_FLAG));
    a.extend_from_slice(A_BLOB);
    a.extend_from_slice(&A_UID);
    a.extend_from_slice(A_LABEL.as_bytes());
    assert!(
        a.iter().all(|b| *b < 0x80),
        "row A must be node-pg-binary safe: {a:?}"
    );

    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(&B_N.to_be_bytes());
    b.extend_from_slice(&B_B.to_be_bytes());
    b.extend_from_slice(B_BLOB);
    b.extend_from_slice(&B_UID);
    assert!(
        b.iter().any(|x| *x >= 0x80),
        "row B must exercise high bytes"
    );

    for v in [A_D, B_D] {
        assert_eq!(f64::from(v as f32), v, "DOUBLE fixture must be f32-exact");
    }
    for v in [A_R, B_R] {
        assert_eq!(f32::from_bits(v.to_bits()), v);
    }
}
