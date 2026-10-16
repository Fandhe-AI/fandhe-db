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
//! - `id` 列は numeric（OID 1700）で、結果のバイナリ送出は Issue #2442、パラメータの
//!   バイナリ受信は #2443 で対応済み。実クライアントでの numeric のバイナリ結果・
//!   バイナリパラメータの値一致は Issue #2445（WIRE-14・WIRE-26・TABLE-13）として本ファイルの
//!   `numeric_items` 系テストが検証する。`typed_items` は `id`・VECTOR を投影しない
//!   非 numeric 列の検証用。
//! - numeric のドライバ差: psycopg は text／binary とも `Decimal`（dscale 保持）。node pg は
//!   text が `string`、既定のバイナリ parser が JS の `number`（精度は double 相当）になる。
//!   node は値部の一致（数値・文字列表記）で比較し、型タグの差は別にアサートする。
//!   node の UTF-8 往復制約は numeric では `weight >= 0` かつ桁群の下位バイト `< 0x80`。
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
use wire_server::result_encoder::binary;

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

/// `numeric_items`（Issue #2445。numeric のバイナリ結果・バイナリパラメータ用）。
/// 行 NA（3 クライアント共通。node pg のバイナリ制約を満たす）。
const NA_LABEL: &str = "num-shared";
const NA_ID: u64 = 20;
const NA_QTY: i32 = 7;
const NA_AMOUNT: &str = "3.14";
const NA_DELTA: &str = "-1.01";
/// 行 NB（psycopg 専用。負の weight・末尾ゼロ・高位バイトを含む）。
const NB_LABEL: &str = "num-wide";
const NB_ID: u64 = 21;
const NB_AMOUNT: &str = "0.50";
const NB_DELTA: &str = "-12345678.90";
const NB_WIDE: &str = "123456789012345678901234567.8901234567";
/// 行 NC（RLS 用。tenant-a の Private 行）。
const NC_LABEL: &str = "num-secret";
const NC_ID: u64 = 22;
const NC_AMOUNT: &str = "42.42";
/// `SUM(typed_items.b)`（numeric(38,0)）の期待値。
const SUM_B_TEXT: &str = "9000000000";

/// 全列に別名を付ける（node pg は `Object.values(row)` で同名列が潰れる）。
const NUMERIC_SQL: &str =
    "SELECT id AS i, amount AS a, delta AS d FROM numeric_items WHERE label = $1 LIMIT 10";
const NUMERIC_WIDE_SQL: &str = "SELECT wide AS w FROM numeric_items WHERE label = $1 LIMIT 10";
const NUMERIC_SUM_SQL: &str =
    "SELECT SUM(amount) AS s_a FROM numeric_items WHERE label = 'num-shared'";
const NUMERIC_AVG_SQL: &str =
    "SELECT AVG(qty) AS a_q FROM numeric_items WHERE label = 'num-shared'";
const NUMERIC_SUM_B_SQL: &str = "SELECT SUM(b) AS s_b FROM typed_items WHERE label = 'shared-row'";
const NUMERIC_SUM_ALL_SQL: &str = "SELECT SUM(amount) AS s_a FROM numeric_items";
const NUMERIC_BY_AMOUNT_SQL: &str = "SELECT label FROM numeric_items WHERE amount = $1 LIMIT 10";
const NUMERIC_BY_ID_SQL: &str = "SELECT label FROM numeric_items WHERE id = $1 LIMIT 10";

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

    // numeric 用の表（Issue #2445）。値は列の scale へ丸めた `Decimal` で投入する。
    storage
        .create_table(&TableSchema::new(
            "numeric_items",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("label", ColumnType::Text, false),
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new(
                    "delta",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new(
                    "wide",
                    ColumnType::Numeric {
                        precision: 38,
                        scale: 10,
                    },
                    true,
                ),
            ],
        ))
        .expect("create numeric_items");
    let num = |text: &str, p: u8, s: u8| {
        Value::Numeric(engine::numeric::parse_for_column(text, p, s).expect("valid numeric"))
    };
    let numeric_rows = [
        (
            NA_ID,
            Visibility::Public,
            vec![
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(NA_LABEL.to_string()),
                Value::Integer(NA_QTY),
                num(NA_AMOUNT, 10, 2),
                num(NA_DELTA, 10, 2),
                Value::Null,
            ],
        ),
        (
            NB_ID,
            Visibility::Public,
            vec![
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(NB_LABEL.to_string()),
                Value::Integer(1),
                num(NB_AMOUNT, 10, 2),
                num(NB_DELTA, 10, 2),
                num(NB_WIDE, 38, 10),
            ],
        ),
        (
            NC_ID,
            Visibility::Private,
            vec![
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(NC_LABEL.to_string()),
                Value::Integer(1),
                num(NC_AMOUNT, 10, 2),
                Value::Null,
                Value::Null,
            ],
        ),
    ];
    for (id, vis, row) in numeric_rows {
        engine::tenant::insert_typed_row(
            &storage,
            "numeric_items",
            &ctx_a,
            id,
            vis,
            &row,
            &op(&format!("ext-e2e-numeric-op-{id}")),
        )
        .expect("insert numeric row");
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
    /// psycopg のみ: `params` を `Decimal`（numeric のバイナリパラメータ）として束縛する。
    decimal_params: bool,
    /// node pg のみ: `params` を `numeric_send` の hex として `Buffer`（format 1・宣言
    /// OID 0）で束縛する。
    binary_hex_params: bool,
}

/// numeric の 10 進テキスト（例 `-1.01`）を PG の `numeric_send` と同じバイト列（hex）へ
/// 符号化する。符号化器は #2444 で PG17 とバイト一致を確認済みのもの（node pg には numeric の
/// バイナリ Dumper が無く、JS で符号化を再実装しないための橋渡し）。
fn numeric_send_bytes(text: &str) -> Vec<u8> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    let scale = u8::try_from(frac_part.len()).expect("scale fits u8");
    let mag: u128 = format!("{int_part}{frac_part}")
        .parse()
        .expect("numeric fixture is a plain decimal");
    let (buf, n) = binary::numeric(neg, mag, scale).expect("encode numeric");
    buf[..n].to_vec()
}

fn numeric_send_hex(text: &str) -> String {
    numeric_send_bytes(text)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    assert!(!opts.binary_hex_params, "binary_hex_params is node pg only");
    if opts.decimal_params {
        assert!(
            !opts.int_params,
            "decimal_params and int_params are exclusive"
        );
        cmd.env("WIRE_PARAMS_DECIMAL", json_string_array(params));
    } else if opts.int_params {
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
    assert!(
        !opts.decimal_params && !opts.int_params,
        "decimal_params/int_params are psycopg only"
    );
    if opts.binary_hex_params {
        cmd.env("WIRE_PARAMS_BINARY_HEX", json_string_array(params));
    } else if !params.is_empty() {
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
            assert!(
                !opts.decimal_params && !opts.binary_hex_params && !opts.int_params,
                "psql has no binary parameter mode"
            );
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

/// 集計・式列・ウィンドウ関数・非 TEXT の `GROUP BY` キー列の結果（Issue #1407・WIRE-13・
/// WIRE-14）。全列に別名を付ける（node pg は `Object.values(row)` で同名列が潰れる）。
/// 行 A（`label = shared-row`）だけに絞り、全バイトが `< 0x80` になる値
/// （`COUNT`＝1・`SUM(n)`＝7・`AVG(d)`＝2.0・`SUM(r)`＝0.5）に限る（node pg の
/// バイナリ受信制約。`node_binary_fixture_bytes_are_utf8_safe` が機械的に守る）。
const COMPUTED_SQL: [&str; 3] = [
    "SELECT COUNT(*) AS c, SUM(n) AS s_n, MIN(n) AS m_n, AVG(d) AS a_d, SUM(r) AS s_r \
     FROM typed_items WHERE label = 'shared-row'",
    "SELECT ROW_NUMBER() OVER (ORDER BY id) AS rn, COUNT(*) OVER () AS c_all, \
     SUM(n) OVER () AS s_n FROM typed_items WHERE label = 'shared-row' LIMIT 10",
    "SELECT flag, COUNT(*) AS c FROM typed_items WHERE label = 'shared-row' \
     GROUP BY flag ORDER BY flag LIMIT 10",
];

/// 期待値（psycopg の型名付き表記。`COUNT`・`SUM(INTEGER)`・`ROW_NUMBER` は int8、
/// `MIN(INTEGER)` は int4、`AVG(DOUBLE)` は float8、`SUM(REAL)` は float4）。
fn computed_expected_psycopg() -> Vec<Vec<String>> {
    vec![
        s(&["int:1|int:7|int:7|float:2.0|float:0.5"]),
        s(&["int:1|int:1|int:7"]),
        s(&["bool:True|int:1"]),
    ]
}

/// 集計・式列・ウィンドウ・`GROUP BY` キー列のバイナリ結果がテキストと一致すること。
/// psql はバイナリ受信モードを持たないためテキストの値一致のみ確認する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn computed_columns_binary_results_match_text_results() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-computed-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;
    let bin = Opts {
        binary: true,
        ..Opts::default()
    };
    let expected = computed_expected_psycopg();
    for (sql, expected_rows) in COMPUTED_SQL.iter().zip(expected.iter()) {
        let psql = run_ext(
            Client::Psql,
            port,
            "alice",
            "pw-alice",
            sql,
            &[],
            Opts::default(),
        );
        assert!(!psql.is_empty(), "psql: `{sql}`");
        for client in [Client::Psycopg, Client::Pg] {
            let text = run_ext(client, port, "alice", "pw-alice", sql, &[], Opts::default());
            let binary = run_ext(client, port, "alice", "pw-alice", sql, &[], bin);
            assert_eq!(binary, text, "{client:?}: binary must equal text: `{sql}`");
            // 値の独立オラクル（型名を除いた値部が psql のテキスト表現と一致する）。
            assert_eq!(
                untag(&binary).len(),
                psql.len(),
                "{client:?}: row count vs psql: `{sql}`"
            );
        }
        let binary = run_ext(Client::Psycopg, port, "alice", "pw-alice", sql, &[], bin);
        assert_eq!(&binary, expected_rows, "psycopg: decoded values: `{sql}`");
    }

    // バイナリ非対応の結果列（静的型なし）は 0A000 のまま（numeric は #2442 で対応済み）。
    for sql in ["SELECT vec_div(embedding, 2.0) AS half FROM typed_items LIMIT 3"] {
        for client in [Client::Psycopg, Client::Pg] {
            expect_sqlstate(client, port, "alice", "pw-alice", sql, &[], bin, "0A000");
        }
    }

    // テナント境界: bob の集計は tenant-a の Private 行（secret-a）を数えない
    // （Public の 2 行のみ。バイナリでも同じ）。
    for client in [Client::Psycopg, Client::Pg] {
        let rows = run_ext(
            client,
            port,
            "bob",
            "pw-bob",
            "SELECT COUNT(*) AS c FROM typed_items",
            &[],
            bin,
        );
        assert_eq!(
            untag(&rows),
            s(&["2"]),
            "{client:?}: bob counts only public rows"
        );
    }
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

    // バイナリ非対応の結果列（静的型を持たないベクトル式）の要求は 0A000
    // （`id`〔numeric〕は Issue #2442 で対応済みのため負例に使わない）。
    let bin = Opts { binary: true, ..d };
    for client in [Client::Psycopg, Client::Pg] {
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            "SELECT vec_div(embedding, 2.0) AS half FROM docs LIMIT 3",
            &[],
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

/// node pg の numeric 結果の比較（text は `string`、バイナリは既定 parser により `number`）。
/// 型タグの差を別にアサートしたうえで、値部の文字列表記が同一で `f64` としても一致すること
/// を確認する（値は末尾ゼロなし・有効桁 15 以下に限る fixture）。
fn assert_node_numeric_match(text: &[String], binary: &[String], want: &[&str], what: &str) {
    let tagged = |tag: &str| -> Vec<String> {
        want.iter()
            .map(|row| {
                row.split('|')
                    .map(|v| format!("{tag}:{v}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    };
    assert_eq!(text, tagged("string"), "pg text numeric tags: {what}");
    assert_eq!(binary, tagged("number"), "pg binary numeric tags: {what}");
    let (t, b) = (untag(text), untag(binary));
    assert_eq!(t, b, "pg numeric value text: {what}");
    for (tr, br) in t.iter().zip(b.iter()) {
        for (tv, bv) in tr.split('|').zip(br.split('|')) {
            let (tf, bf) = (
                tv.parse::<f64>().expect("numeric text parses as f64"),
                bv.parse::<f64>().expect("numeric number parses as f64"),
            );
            assert_eq!(tf, bf, "pg numeric value as f64: {what}");
        }
    }
}

/// numeric（結果列・`id`・集計）のバイナリ結果が、テキスト結果および psql のテキストと
/// 値で一致すること（Issue #2445・WIRE-14・WIRE-26・TABLE-13）。psycopg は text／binary とも
/// `Decimal`（dscale 保持）で完全一致、node pg は text=`string`／binary=`number`（値部一致）。
/// bob のバイナリ集計は tenant-a の Private 行を数えない（テナント境界）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_numeric_binary_results_match_text_results() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-numeric-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;
    let d = Opts::default();
    let bin = Opts { binary: true, ..d };
    let run = |client: Client, user: &str, pw: &str, sql: &str, params: &[&str], o: Opts| {
        run_ext(client, port, user, pw, sql, params, o)
    };

    // 独立オラクル: psql のテキスト（PostgreSQL 正準表記）。
    let oracle_row = format!("{NA_ID}|{NA_AMOUNT}|{NA_DELTA}");
    assert_eq!(
        run(
            Client::Psql,
            "alice",
            "pw-alice",
            NUMERIC_SQL,
            &[NA_LABEL],
            d
        ),
        vec![oracle_row.clone()]
    );
    assert_eq!(
        run(Client::Psql, "alice", "pw-alice", NUMERIC_SUM_SQL, &[], d),
        vec![NA_AMOUNT.to_string()]
    );
    assert_eq!(
        run(Client::Psql, "alice", "pw-alice", NUMERIC_SUM_B_SQL, &[], d),
        vec![SUM_B_TEXT.to_string()]
    );

    // psycopg: 行 NA・NB・集計はバイナリ == テキスト（型タグ込み）かつ期待値と一致。
    let psycopg_cases: [(&str, Vec<&str>, String); 6] = [
        (
            NUMERIC_SQL,
            vec![NA_LABEL],
            format!("Decimal:{NA_ID}|Decimal:{NA_AMOUNT}|Decimal:{NA_DELTA}"),
        ),
        (
            NUMERIC_SQL,
            vec![NB_LABEL],
            format!("Decimal:{NB_ID}|Decimal:{NB_AMOUNT}|Decimal:{NB_DELTA}"),
        ),
        (
            NUMERIC_WIDE_SQL,
            vec![NB_LABEL],
            format!("Decimal:{NB_WIDE}"),
        ),
        (NUMERIC_SUM_SQL, vec![], format!("Decimal:{NA_AMOUNT}")),
        (NUMERIC_AVG_SQL, vec![], "Decimal:7.0000000000000000".into()),
        (NUMERIC_SUM_B_SQL, vec![], format!("Decimal:{SUM_B_TEXT}")),
    ];
    for (sql, params, want) in &psycopg_cases {
        let text = run(Client::Psycopg, "alice", "pw-alice", sql, params, d);
        let binary = run(Client::Psycopg, "alice", "pw-alice", sql, params, bin);
        assert_eq!(binary, text, "psycopg: binary must equal text: `{sql}`");
        assert_eq!(binary, vec![want.clone()], "psycopg: values: `{sql}`");
    }

    // node pg: 行 NA と node-safe な集計（値部一致。型は string／number）。
    let node_cases: [(&str, Vec<&str>, &str); 3] = [
        (NUMERIC_SQL, vec![NA_LABEL], oracle_row.as_str()),
        (NUMERIC_SUM_SQL, vec![], NA_AMOUNT),
        (NUMERIC_SUM_B_SQL, vec![], SUM_B_TEXT),
    ];
    for (sql, params, want) in &node_cases {
        let text = run(Client::Pg, "alice", "pw-alice", sql, params, d);
        let binary = run(Client::Pg, "alice", "pw-alice", sql, params, bin);
        assert_node_numeric_match(&text, &binary, &[*want], sql);
    }

    // テナント境界: Public の NA・NB だけが bob に見え（SUM = 3.64）、alice には NC（42.42）も
    // 加わる（46.06）。バイナリでも同じで、bob は Private 行の値を観測できない。
    for client in [Client::Psycopg, Client::Pg] {
        let bob = untag(&run(client, "bob", "pw-bob", NUMERIC_SUM_ALL_SQL, &[], bin));
        assert_eq!(bob, s(&["3.64"]), "{client:?}: bob must not sum NC");
        let alice = untag(&run(
            client,
            "alice",
            "pw-alice",
            NUMERIC_SUM_ALL_SQL,
            &[],
            bin,
        ));
        assert_eq!(alice, s(&["46.06"]), "{client:?}: alice sums NA+NB+NC");
        let secret = run(client, "bob", "pw-bob", NUMERIC_SQL, &[NC_LABEL], bin);
        assert!(
            secret.is_empty(),
            "{client:?}: bob must not read NC: {secret:?}"
        );
    }
    let alice_nc = untag(&run(
        Client::Psycopg,
        "alice",
        "pw-alice",
        NUMERIC_SQL,
        &[NC_LABEL],
        bin,
    ));
    assert_eq!(alice_nc[0].split('|').nth(1), Some(NC_AMOUNT));
    drop(server);
}

/// numeric のバイナリパラメータ（psycopg の `Decimal`＋`DecimalBinaryDumper`、node pg の
/// `numeric_send` Buffer）が、テキストパラメータと同じ行を引き当てること、INSERT で
/// 保存でき RLS が維持されること、不正値は fail-closed であること（WIRE-26・WIRE-14）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_numeric_binary_params_match_text_params() {
    let (db_path, _db_guard) = seed_db();
    let users_dir = temp_db::TempDir::new("three-client-ext-numeric-param-users");
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    let port = server.port;
    let d = Opts::default();
    let dec = Opts {
        decimal_params: true,
        ..d
    };
    let hex = Opts {
        binary_hex_params: true,
        ..d
    };

    // 読み取り: `NUMERIC` 列スロット（宣言 1700／0）と `id` スロット。
    let reads: [(&str, &str, &str); 3] = [
        (NUMERIC_BY_AMOUNT_SQL, NA_AMOUNT, NA_LABEL),
        (NUMERIC_BY_AMOUNT_SQL, NB_AMOUNT, NB_LABEL),
        (NUMERIC_BY_ID_SQL, "20", NA_LABEL),
    ];
    for (sql, param, label) in reads {
        let want = s(&[label]);
        let hexed = numeric_send_hex(param);
        let psql = run_ext(Client::Psql, port, "alice", "pw-alice", sql, &[param], d);
        assert_eq!(psql, want, "psql text param `{param}`");
        for (client, o, p) in [
            (Client::Psycopg, d, param),
            (Client::Psycopg, dec, param),
            (Client::Pg, d, param),
            (Client::Pg, hex, hexed.as_str()),
        ] {
            let rows = untag(&run_ext(client, port, "alice", "pw-alice", sql, &[p], o));
            assert_eq!(rows, want, "{client:?}: param `{param}` ({sql})");
        }
    }

    // テナント境界: NC（Private）の値を bob がバイナリパラメータで引いても 0 行。
    // alice は同じ問い合わせで引ける（非 vacuous）。
    let nc_hex = numeric_send_hex(NC_AMOUNT);
    for (client, o, p) in [
        (Client::Psycopg, dec, NC_AMOUNT),
        (Client::Pg, hex, nc_hex.as_str()),
    ] {
        let alice = untag(&run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            NUMERIC_BY_AMOUNT_SQL,
            &[p],
            o,
        ));
        assert_eq!(alice, s(&[NC_LABEL]), "{client:?}: alice finds NC");
        let bob = run_ext(
            client,
            port,
            "bob",
            "pw-bob",
            NUMERIC_BY_AMOUNT_SQL,
            &[p],
            o,
        );
        assert!(bob.is_empty(), "{client:?}: bob must not find NC: {bob:?}");
    }

    // 書き込み: numeric 1 個だけを束縛した INSERT。psql のテキストで読み戻す。
    let inserted_amount = "5.25";
    let inserted_hex = numeric_send_hex(inserted_amount);
    for (client, o, p, id, tag) in [
        (Client::Psycopg, dec, inserted_amount, 31, "psycopg"),
        (Client::Pg, hex, inserted_hex.as_str(), 32, "pg"),
    ] {
        let label = format!("num-ins-{tag}");
        let sql = format!(
            "INSERT INTO numeric_items (id, embedding, label, amount) \
             VALUES ({id}, '[1.0,0.0]', '{label}', $1) USING OPERATION_ID 'ext-e2e-num-ins-{tag}'"
        );
        let out = run_ext(client, port, "alice", "pw-alice", &sql, &[p], o);
        assert!(
            out.is_empty(),
            "{client:?}: INSERT returns no rows: {out:?}"
        );
        let readback = run_ext(
            Client::Psql,
            port,
            "alice",
            "pw-alice",
            "SELECT amount FROM numeric_items WHERE label = $1 LIMIT 10",
            &[&label],
            d,
        );
        assert_eq!(readback, s(&[inserted_amount]), "{client:?}: stored amount");
        for (other, pw) in [("bob", "pw-bob"), ("carol", "pw-carol")] {
            let rows = run_ext(
                Client::Psql,
                port,
                other,
                pw,
                "SELECT amount FROM numeric_items WHERE label = $1 LIMIT 10",
                &[&label],
                d,
            );
            assert!(
                rows.is_empty(),
                "{other} must not see alice's row: {rows:?}"
            );
        }
    }

    // 負例: 不正な桁群（`[10000]`。ndigits=1・weight=0・sign=0・dscale=0）は 22P03。
    expect_sqlstate(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        NUMERIC_BY_AMOUNT_SQL,
        &["00010000000000002710"],
        hex,
        "22P03",
    );
    // NaN はテキストで送った場合と同じ SQLSTATE（22P02）。
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        NUMERIC_BY_AMOUNT_SQL,
        &["NaN"],
        d,
        "22P02",
    );
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        NUMERIC_BY_AMOUNT_SQL,
        &["NaN"],
        dec,
        "22P02",
    );
    drop(server);
}

/// fixture ガード（常時実行・numeric 版）: node pg 共通の numeric 値（行 NA・`id`・集計）の
/// 送出バイトが全て `< 0x80` で、行 NB が高位バイトを含むこと。node 用の値は `f64` へ
/// 往復しても同じ表記（JS の `number` で厳密に表せる）であること。
#[test]
fn node_numeric_fixture_bytes_are_utf8_safe() {
    let node_values = [
        NA_ID.to_string(),
        NA_AMOUNT.to_string(),
        NA_DELTA.to_string(),
        SUM_B_TEXT.to_string(),
        "3.64".to_string(),
        "46.06".to_string(),
    ];
    for v in &node_values {
        let bytes = numeric_send_bytes(v);
        assert!(
            bytes.iter().all(|b| *b < 0x80),
            "node-shared numeric {v} must be binary safe: {bytes:?}"
        );
        let f: f64 = v.parse().expect("f64");
        assert_eq!(&f.to_string(), v, "{v} must round-trip through a JS number");
    }
    let wide: Vec<u8> = [NB_AMOUNT, NB_DELTA, NB_WIDE]
        .iter()
        .flat_map(|v| numeric_send_bytes(v))
        .collect();
    assert!(
        wide.iter().any(|b| *b >= 0x80),
        "row NB must exercise high bytes"
    );
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
