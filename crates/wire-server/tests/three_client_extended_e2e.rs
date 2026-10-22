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
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
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

/// 型 OID 指定・NULL パラメータ・OID 0 の推論の検証専用 DB（Issue #2440。WIRE-24・SQL-45 の
/// ポインタ）。表 `param_items` は nullable の数値・文字列・真偽値・日付列と NOT NULL の
/// `req` を持つ。tenant-a は Public（id 1）と Private（id 2）、tenant-b は Private（id 3）の
/// 行を持ち、tenant-c は行を持たない（3 テナントで応答が揃うことの確認用）。
fn seed_param_types_db() -> (PathBuf, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("three-client-param-types");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "param_items",
            vec![
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, true),
                ColumnDef::new("d", ColumnType::Double, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("s", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("req", ColumnType::Text, false),
                ColumnDef::new("day", ColumnType::Date, true),
            ],
        ))
        .expect("create param_items");
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let row = |n: i32, s: &str| {
        vec![
            Value::Integer(n),
            Value::BigInt(i64::from(n) * 10),
            Value::Double(1.5),
            Value::Real(0.5),
            Value::Text(s.to_string()),
            Value::Bool(true),
            Value::Text("req".to_string()),
            Value::Null,
        ]
    };
    for (ctx, id, vis, n, label) in [
        (&ctx_a, 1u64, Visibility::Public, 10, "pub-a"),
        (&ctx_a, 2, Visibility::Private, 20, "priv-a"),
        (&ctx_b, 3, Visibility::Private, 30, "priv-b"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "param_items",
            ctx,
            id,
            vis,
            &row(n, label),
            &op(&format!("ext-e2e-param-op-{id}")),
        )
        .expect("insert param_items row");
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
    /// psycopg のみ（Issue #2440。WIRE-24 のポインタ）: 位置ごとの型付きパラメータ
    /// （`WIRE_PARAMS_TYPED` の JSON。本ファイルの定数のみ）。`params` とは排他。
    typed_json: Option<&'static str>,
    /// node pg のみ: Parse で宣言する型 OID（`query({ types })`）。値は `params` の text。
    param_types: Option<&'static str>,
    /// node pg のみ: NULL を含みうる値の JSON 配列（`WIRE_PARAMS_NULLABLE`）。`params` とは排他。
    nullable_json: Option<&'static str>,
    /// psycopg／node pg: 1 つ目の文が失敗しても同じ接続で実行する 2 つ目の文。
    sql_after: Option<&'static str>,
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
    assert!(
        opts.param_types.is_none() && opts.nullable_json.is_none(),
        "param_types/nullable_json are node pg only"
    );
    if let Some(typed) = opts.typed_json {
        assert!(
            params.is_empty() && !opts.decimal_params && !opts.int_params,
            "typed_json is exclusive with params"
        );
        cmd.env("WIRE_PARAMS_TYPED", typed);
    } else if opts.decimal_params {
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
    if let Some(after) = opts.sql_after {
        cmd.env("WIRE_SQL_AFTER", after);
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
        !opts.decimal_params && !opts.int_params && opts.typed_json.is_none(),
        "decimal_params/int_params/typed_json are psycopg only"
    );
    if let Some(types) = opts.param_types {
        cmd.env("WIRE_PARAM_TYPES", types);
    }
    if let Some(after) = opts.sql_after {
        cmd.env("WIRE_SQL_AFTER", after);
    }
    if let Some(nullable) = opts.nullable_json {
        assert!(params.is_empty(), "nullable_json is exclusive with params");
        cmd.env("WIRE_PARAMS_NULLABLE", nullable);
    } else if opts.binary_hex_params {
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
            assert!(
                opts.typed_json.is_none()
                    && opts.param_types.is_none()
                    && opts.nullable_json.is_none()
                    && opts.sql_after.is_none(),
                "psql sends OID 0 only and has no NULL/typed/after mode"
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
    let _ = rejected_stderr(client, port, user, password, sql, params, opts, sqlstate);
}

/// [`expect_sqlstate`] と同じ検査をしたうえで、拒否応答の stderr を返す（テナント間・
/// 宣言有無間で応答が一致することの比較に使う。Issue #2440）。
#[allow(clippy::too_many_arguments)] // 呼び出し側の可読性のため run_ext と同じ引数並びを保つ
fn rejected_stderr(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
    opts: Opts,
    sqlstate: &str,
) -> String {
    let output = if client == Client::Psql {
        spawn_psql_bind(port, user, password, sql, params, true)
    } else {
        spawn_client(client, port, user, password, sql, params, opts)
    };
    assert!(
        !output.status.success() || opts.sql_after.is_some(),
        "{client:?} must exit non-zero for a rejected statement: `{sql}`"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains(sqlstate),
        "{client:?}: expected SQLSTATE {sqlstate} in stderr, got: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|l| !l.starts_with("after:") && !l.trim().is_empty()),
        "{client:?}: rejected statement must not return rows"
    );
    stderr
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
    // Python int（int2 を宣言）を TEXT 列と比較する Parse は、宣言が文脈の型へ変換できない
    // ため Parse の段で 42883 になる（Issue #2437。WIRE-24 (1)・SQL-45 のポインタ）。#2437 より前は
    // バイナリ非対応スロットの 0A000 が先に返っていた。0A000 側の確認は
    // `psycopg_natural_binary_params_outside_the_compat_table_fail_closed` が担う。
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
        "42883",
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
// --- Issue #2440: 型 OID 指定・NULL パラメータ・OID 0 の推論（WIRE-24・SQL-45 のポインタ） ---
//
// 層 A（`wire24_declared_param_oids.rs`）が確定オラクルとして検証した規則が、無改造の実
// クライアントからも同じ SQLSTATE で観測できることの確認。クライアントごとの能力差:
// - psql: `\bind` の値は常に文字列で、宣言 OID は常に 0。NULL も送れない（値を省略すると
//   サーバーが「パラメータ数不一致」で拒否する）。OID 0 の推論だけを担当する。
// - psycopg: Python の型で宣言 OID が決まる。`None` は OID 0 の NULL。公開 API の
//   `register_dumper` で 25・705 などを text 形式で宣言する。
// - node pg: `query({ types })` が Parse の OID 列へそのまま書かれる。値は text 形式。

/// 拒否ケース（表示名・クライアント・SQL・text パラメータ・オプション・期待 SQLSTATE）。
type RejectionCase<'a> = (&'a str, Client, &'a str, Vec<String>, Opts, &'a str);

const PARAM_UPD_N: &str = "UPDATE param_items SET n = $1 WHERE id = 1 USING OPERATION_ID $2";
const PARAM_SEL_N: &str = "SELECT n FROM param_items WHERE id = 1 LIMIT 5";
const PARAM_UPD_REQ: &str = "UPDATE param_items SET req = $1 WHERE id = 1 USING OPERATION_ID $2";
/// 各テナントが自分の行として INSERT する NOT NULL 違反（id=1 は tenant-a の行で、他テナントの
/// UPDATE は 23502 に到達しないため、テナント越しの同一応答検証にはこちらを使う）。
const PARAM_INS_REQ_NULL: &str =
    "INSERT INTO param_items (id, n, s, req) VALUES (9001, 1, 'x', $1) USING OPERATION_ID $2";
const PARAM_WHERE_N: &str = "SELECT s FROM param_items WHERE n = $1 LIMIT 5";
const PARAM_WHERE_S: &str = "SELECT n FROM param_items WHERE s = $1 LIMIT 5";
/// 推論できる文脈を持たない 2 項演算（OID 0 は `42P18`）。
const PARAM_INDETERMINATE: &str =
    "UPDATE param_items SET n = $1 + $2 WHERE id = 1 USING OPERATION_ID 'p2440-indeterminate'";

/// `param_items` を載せたサーバー一式（子プロセス・DB・ユーザーファイルの寿命を束ねる）。
struct ParamEnv {
    server: ServerGuard,
    _db: temp_db::CleanupGuard,
    _users_dir: temp_db::TempDir,
}

fn start_param_env(tag: &str) -> ParamEnv {
    let (db_path, db) = seed_param_types_db();
    let users_dir = temp_db::TempDir::new(&format!("three-client-param-{tag}-users"));
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &db_path);
    ParamEnv {
        server,
        _db: db,
        _users_dir: users_dir,
    }
}

/// 同一サーバーに対する operation_id の重複（`23505`）を避ける連番付き ID。
fn next_op(tag: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    format!("p2440-{tag}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 動的に組んだ JSON を `Opts`（`'static`）へ渡すためのリーク。テスト専用で数が少ない。
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// JSON 断片（数値・`null`・引用済み文字列）に `\` と制御文字を許さない検査。
fn assert_plain_json_fragment(frag: &str) {
    assert!(
        !frag.contains('\\') && !frag.chars().any(|c| c.is_control()),
        "unsupported character in test parameter: {frag:?}"
    );
}

/// `WIRE_PARAMS_TYPED` の JSON を `[(語彙, JSON 値)]` から組む。語彙の検証は
/// スクリプト側（fail-closed）が担い、ここでは `\`・制御文字だけを拒否する。
fn typed_json(items: &[(&str, &str)]) -> &'static str {
    let body: Vec<String> = items
        .iter()
        .map(|(kind, value)| {
            assert_plain_json_fragment(kind);
            assert_plain_json_fragment(value);
            format!("{{\"type\":\"{kind}\",\"value\":{value}}}")
        })
        .collect();
    leak(format!("[{}]", body.join(",")))
}

/// `WIRE_PARAMS_NULLABLE` の JSON（`None` が NULL）。
fn nullable_json(items: &[Option<&str>]) -> &'static str {
    let body: Vec<String> = items
        .iter()
        .map(|item| match item {
            None => "null".to_string(),
            Some(s) => {
                assert_plain_json_fragment(s);
                format!("\"{}\"", s.replace('"', "\\\""))
            }
        })
        .collect();
    leak(format!("[{}]", body.join(",")))
}

/// `WIRE_PARAM_TYPES` の JSON。
fn oid_json(oids: &[i32]) -> &'static str {
    let body: Vec<String> = oids.iter().map(|o| o.to_string()).collect();
    leak(format!("[{}]", body.join(",")))
}

fn quoted(s: &str) -> String {
    assert_plain_json_fragment(s);
    format!("\"{}\"", s.replace('"', "\\\""))
}

/// 型付き psycopg 用: 末尾の operation_id を `unknown`（OID 0）の文字列として付ける。
fn typed_with_op(first: (&str, &str), tag: &str) -> Opts {
    Opts {
        typed_json: Some(typed_json(&[first, ("unknown", &quoted(&next_op(tag)))])),
        ..Opts::default()
    }
}

/// node pg 用: 宣言 OID 列 `[oid, 0]` と値 `[value, operation_id]`。
fn pg_declared(oid: i32) -> Opts {
    Opts {
        param_types: Some(oid_json(&[oid, 0])),
        ..Opts::default()
    }
}

/// alice が `param_items` の 1 セルを psycopg の型名付きで読み戻す（NULL は
/// `NoneType:None`）。クライアント非依存の読み戻しに使う。
fn read_param_cell(port: u16, col: &str, id: u64) -> String {
    let sql = format!("SELECT {col} FROM param_items WHERE id = {id} LIMIT 5");
    run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        &sql,
        &[],
        Opts::default(),
    )
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("no row for {col} id={id}"))
}

/// 常時実行のガード: JSON 組み立て補助が `\`・制御文字を拒否し、期待形を作ること。
#[test]
fn param_json_builders_are_strict() {
    assert_eq!(
        typed_json(&[("int", "7"), ("unknown", "\"op\"")]),
        "[{\"type\":\"int\",\"value\":7},{\"type\":\"unknown\",\"value\":\"op\"}]"
    );
    assert_eq!(nullable_json(&[None, Some("a")]), "[null,\"a\"]");
    assert_eq!(oid_json(&[23, 0]), "[23,0]");
    for bad in ["a\\b", "a\nb"] {
        let r = std::panic::catch_unwind(|| quoted(bad));
        assert!(r.is_err(), "{bad:?} must be rejected");
    }
}

/// 3 クライアントすべてで OID 0（宣言なし）の `$n` が文脈から推論され、読み戻した値が
/// 一致すること。推論できない位置は 3 クライアントとも `42P18`（WIRE-24 (2)）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_infer_types_for_oid_zero_params() {
    let env = start_param_env("oid0");
    let port = env.server.port;
    let d = Opts::default();
    for (client, tag, insert_id) in [
        (Client::Psql, "psql", 100u64),
        (Client::Psycopg, "psycopg", 200),
        (Client::Pg, "pg", 300),
    ] {
        // 代入の位置（INTEGER 列へ文字列 `5` を推論して束縛）。
        let op1 = next_op(tag);
        let out = run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            PARAM_UPD_N,
            &["5", &op1],
            d,
        );
        assert!(
            out.is_empty(),
            "{client:?}: UPDATE returns no rows: {out:?}"
        );
        assert_eq!(
            read_param_cell(port, "n", 1),
            "int:5",
            "{client:?}: SET n = $1"
        );

        // 式の中（`$1 + 1` は INTEGER と推論）。
        let op2 = next_op(tag);
        run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            "UPDATE param_items SET n = $1 + 1 WHERE id = 1 USING OPERATION_ID $2",
            &["5", &op2],
            d,
        );
        assert_eq!(read_param_cell(port, "n", 1), "int:6", "{client:?}: $1 + 1");

        // INSERT の VALUES 位置。
        let op3 = next_op(tag);
        let sql = format!(
            "INSERT INTO param_items (id, n, s, req) VALUES ({insert_id}, $1, $2, 'r') \
             USING OPERATION_ID $3"
        );
        run_ext(
            client,
            port,
            "alice",
            "pw-alice",
            &sql,
            &["41", "ins", &op3],
            d,
        );
        assert_eq!(
            read_param_cell(port, "n", insert_id),
            "int:41",
            "{client:?}: INSERT"
        );
        assert_eq!(read_param_cell(port, "s", insert_id), "str:ins");

        // 比較の位置（bob の Private 行 n = 30）。他テナントの Private 行は alice 側に現れない。
        let rows = untag(&run_ext(
            client,
            port,
            "bob",
            "pw-bob",
            PARAM_WHERE_N,
            &["30"],
            d,
        ));
        assert_eq!(rows, s(&["priv-b"]), "{client:?}: WHERE n = $1");
        let rows = run_ext(client, port, "alice", "pw-alice", PARAM_WHERE_N, &["30"], d);
        assert!(
            rows.is_empty(),
            "{client:?}: alice must not see bob row: {rows:?}"
        );

        // 推論できない位置は 42P18。
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            PARAM_INDETERMINATE,
            &["1", "2"],
            d,
            "42P18",
        );
    }
    drop(env);
}

/// psycopg と node pg が宣言した型 OID は Parse で検証され、受理・拒否が両クライアントで
/// 一致すること。705（unknown）は OID 0 と同じ推論になる（WIRE-24 (1)(2)(5)・SQL-45）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_declared_oids_are_used_and_checked() {
    let env = start_param_env("declared");
    let port = env.server.port;
    let d = Opts::default();

    // 受理: 変換できる宣言。psycopg は Python の型と整数ラッパー、node は types の OID。
    let accepted_psycopg: [(&str, &str, i32); 4] = [
        ("int", "7", 7),
        ("int2", "8", 8),
        ("int4", "9", 9),
        ("int8", "10", 10),
    ];
    for (kind, value, want) in accepted_psycopg {
        let opts = typed_with_op((kind, value), "decl-py");
        run_ext(
            Client::Psycopg,
            port,
            "alice",
            "pw-alice",
            PARAM_UPD_N,
            &[],
            opts,
        );
        assert_eq!(
            read_param_cell(port, "n", 1),
            format!("int:{want}"),
            "psycopg {kind}"
        );
    }
    for (oid, want) in [(23, 11), (20, 12), (21, 13), (701, 14)] {
        let op = next_op("decl-pg");
        run_ext(
            Client::Pg,
            port,
            "alice",
            "pw-alice",
            PARAM_UPD_N,
            &[&want.to_string(), &op],
            pg_declared(oid),
        );
        assert_eq!(
            read_param_cell(port, "n", 1),
            format!("int:{want}"),
            "node types=[{oid}]"
        );
    }
    // DOUBLE 列へ float（701）、BOOLEAN 列へ bool（16）、numeric（1700）で id を引く。
    let upd_d = "UPDATE param_items SET d = $1 WHERE id = 1 USING OPERATION_ID $2";
    run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        upd_d,
        &[],
        typed_with_op(("float", "2.5"), "decl-py"),
    );
    assert_eq!(read_param_cell(port, "d", 1), "float:2.5");
    let upd_flag = "UPDATE param_items SET flag = $1 WHERE id = 1 USING OPERATION_ID $2";
    run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        upd_flag,
        &[],
        typed_with_op(("bool", "false"), "decl-py"),
    );
    assert_eq!(read_param_cell(port, "flag", 1), "bool:False");
    let by_id = "SELECT s FROM param_items WHERE id = $1 LIMIT 5";
    let rows = run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        by_id,
        &[],
        Opts {
            typed_json: Some(typed_json(&[("decimal", "\"1\"")])),
            ..d
        },
    );
    assert_eq!(untag(&rows), s(&["pub-a"]), "psycopg Decimal vs id");

    // 拒否: 変換できない宣言（代入は 42804、比較は 42883）。両クライアントで同じ SQLSTATE。
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &[],
        typed_with_op(("text", "\"7\""), "decl-py"),
        "42804",
    );
    expect_sqlstate(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &["7", &next_op("decl-pg")],
        pg_declared(25),
        "42804",
    );
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_WHERE_S,
        &[],
        Opts {
            typed_json: Some(typed_json(&[("int", "7")])),
            ..d
        },
        "42883",
    );
    expect_sqlstate(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        PARAM_WHERE_S,
        &["t"],
        Opts {
            param_types: Some(oid_json(&[16])),
            ..d
        },
        "42883",
    );
    // 拒否後も接続は壊れていない（同一接続で続けて実行できる）。
    let after = Opts {
        sql_after: Some(PARAM_SEL_N),
        ..typed_with_op(("text", "\"7\""), "decl-py")
    };
    let out = spawn_client(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &[],
        after,
    );
    assert!(out.status.success(), "connection must survive 42804");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("after:"));

    // 705（unknown）は OID 0 と同じ推論: 列スロットでは受理、式の中では 42P18。
    run_ext(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &[],
        typed_with_op(("oid705", "\"15\""), "decl-py"),
    );
    assert_eq!(read_param_cell(port, "n", 1), "int:15", "psycopg 705");
    run_ext(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &["16", &next_op("decl-pg")],
        pg_declared(705),
    );
    assert_eq!(read_param_cell(port, "n", 1), "int:16", "node 705");
    expect_sqlstate(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_INDETERMINATE,
        &[],
        Opts {
            typed_json: Some(typed_json(&[("oid705", "\"1\""), ("oid705", "\"2\"")])),
            ..d
        },
        "42P18",
    );
    expect_sqlstate(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        PARAM_INDETERMINATE,
        &["1", "2"],
        Opts {
            param_types: Some(oid_json(&[705, 705])),
            ..d
        },
        "42P18",
    );
    drop(env);
}

/// 配列型 OID を宣言するドライバ（psycopg の `list[int]`＝1005、node の `types=[1007]`）は
/// `0A000` で拒否され、同じ接続で続けた文は成功すること。存在する表・存在しない表で
/// 応答が同一であること（WIRE-24 (1)・WIRE-11。#2438 の申し送り (b)）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_array_oid_is_rejected_and_connection_survives() {
    let env = start_param_env("array");
    let port = env.server.port;
    let d = Opts::default();
    let py = |sql_after: Option<&'static str>| Opts {
        sql_after,
        ..typed_with_op(("int_list", "[1]"), "arr-py")
    };
    let node = |sql_after: Option<&'static str>| Opts {
        sql_after,
        param_types: Some(oid_json(&[1007, 0])),
        ..d
    };
    let py_err = rejected_stderr(
        Client::Psycopg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &[],
        py(Some(PARAM_SEL_N)),
        "0A000",
    );
    let node_err = rejected_stderr(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        PARAM_UPD_N,
        &["7", &next_op("arr-pg")],
        node(Some(PARAM_SEL_N)),
        "0A000",
    );
    // 失敗の後に同じ接続で実行した文が成功する（exit 0 かつ `after:` の行がある）。
    for (client, opts, params) in [
        (Client::Psycopg, py(Some(PARAM_SEL_N)), vec![]),
        (
            Client::Pg,
            node(Some(PARAM_SEL_N)),
            vec!["7", "p2440-arr-after"],
        ),
    ] {
        let out = spawn_client(
            client,
            port,
            "alice",
            "pw-alice",
            PARAM_UPD_N,
            &params,
            opts,
        );
        assert!(
            out.status.success(),
            "{client:?}: connection must survive 0A000"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).starts_with("after:"),
            "{client:?}: follow-up statement must run"
        );
    }
    // 存在しない表でも応答は同じ（存在情報を漏らさない）。
    let missing = "UPDATE no_such_table SET n = $1 WHERE id = 1 USING OPERATION_ID $2";
    let node_missing = rejected_stderr(
        Client::Pg,
        port,
        "alice",
        "pw-alice",
        missing,
        &["7", &next_op("arr-pg")],
        node(None),
        "0A000",
    );
    assert_eq!(
        node_err, node_missing,
        "0A000 must not depend on the catalog"
    );
    assert!(!py_err.contains("param_items"), "{py_err}");
    drop(env);
}

/// NULL パラメータはリテラル NULL と同じ扱いになること（WIRE-24 (3)・SQL-45）。nullable 列
/// への INSERT・UPDATE は成功して NULL が読み戻せ、NOT NULL 列は `23502`、比較の位置は
/// 同じ文をリテラル `NULL` で書いたときと同じ SQLSTATE になる。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_null_params_match_literal_null() {
    let env = start_param_env("null");
    let port = env.server.port;
    let d = Opts::default();

    for (client, tag, insert_id) in [(Client::Psycopg, "py", 400u64), (Client::Pg, "pg", 500)] {
        let opts_for = |values: &[Option<&str>], extra: Opts| -> Opts {
            match client {
                Client::Psycopg => {
                    let items: Vec<(&str, String)> = values
                        .iter()
                        .map(|v| match v {
                            None => ("null", "null".to_string()),
                            Some(s) => ("unknown", quoted(s)),
                        })
                        .collect();
                    let refs: Vec<(&str, &str)> =
                        items.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    Opts {
                        typed_json: Some(typed_json(&refs)),
                        ..extra
                    }
                }
                _ => Opts {
                    nullable_json: Some(nullable_json(values)),
                    ..extra
                },
            }
        };
        // 値を入れてから NULL へ更新する（UPDATE）。
        let o = opts_for(&[Some("5"), Some(&next_op(tag))], d);
        run_ext(client, port, "alice", "pw-alice", PARAM_UPD_N, &[], o);
        assert_eq!(read_param_cell(port, "n", 1), "int:5");
        let o = opts_for(&[None, Some(&next_op(tag))], d);
        let out = run_ext(client, port, "alice", "pw-alice", PARAM_UPD_N, &[], o);
        assert!(out.is_empty(), "{client:?}: {out:?}");
        assert_eq!(
            read_param_cell(port, "n", 1),
            "NoneType:None",
            "{client:?}: UPDATE SET n = NULL"
        );
        // 型を宣言した NULL（text 形式）も同じ。
        if client == Client::Pg {
            let o = Opts {
                param_types: Some(oid_json(&[23, 0])),
                ..opts_for(&[None, Some(&next_op(tag))], d)
            };
            run_ext(client, port, "alice", "pw-alice", PARAM_UPD_N, &[], o);
            assert_eq!(read_param_cell(port, "n", 1), "NoneType:None");
        }
        // INSERT（nullable 列へ NULL）。
        let sql = format!(
            "INSERT INTO param_items (id, n, s, req) VALUES ({insert_id}, $1, $2, 'r') \
             USING OPERATION_ID $3"
        );
        let o = opts_for(&[None, Some("ins-null"), Some(&next_op(tag))], d);
        run_ext(client, port, "alice", "pw-alice", &sql, &[], o);
        assert_eq!(read_param_cell(port, "n", insert_id), "NoneType:None");
        assert_eq!(read_param_cell(port, "s", insert_id), "str:ins-null");
        // NOT NULL 列への NULL は 23502。
        let o = opts_for(&[None, Some(&next_op(tag))], d);
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            PARAM_UPD_REQ,
            &[],
            o,
            "23502",
        );
        // 比較の位置: リテラル NULL と同じ SQLSTATE（現状は SQL-41 系の `42703`。直書きせず
        // リテラル側の結果に追従する）。
        let literal = rejected_stderr(
            Client::Psycopg,
            port,
            "alice",
            "pw-alice",
            "SELECT s FROM param_items WHERE n = NULL LIMIT 5",
            &[],
            d,
            "SQLSTATE=",
        );
        let literal_state = literal
            .split("SQLSTATE=")
            .nth(1)
            .and_then(|r| r.get(..5))
            .unwrap_or_else(|| panic!("no SQLSTATE in {literal}"))
            .to_string();
        let o = opts_for(&[None], d);
        expect_sqlstate(
            client,
            port,
            "alice",
            "pw-alice",
            PARAM_WHERE_N,
            &[],
            o,
            &literal_state,
        );
        // インジェクション: NULL と隣り合う値は不透明なまま保存される。
        let sql = "UPDATE param_items SET n = $1, s = $2 WHERE id = 2 USING OPERATION_ID $3";
        let o = opts_for(&[None, Some("' OR '1'='1"), Some(&next_op(tag))], d);
        run_ext(client, port, "alice", "pw-alice", sql, &[], o);
        assert_eq!(read_param_cell(port, "s", 2), "str:' OR '1'='1");
        assert_eq!(
            read_param_cell(port, "s", 1),
            "str:pub-a",
            "other rows untouched"
        );
    }
    drop(env);
}

/// 3 テナントのいずれでも、型宣言・NULL による拒否の応答（SQLSTATE とメッセージ）が同一で、
/// 他テナントの表・行の存在を観測できないこと（RLS-11・WIRE-24 (5)）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_param_type_rejections_do_not_reveal_other_tenants() {
    let env = start_param_env("tenants");
    let port = env.server.port;
    let d = Opts::default();

    // クライアントごとの拒否ケース: (表示名, client, sql, params, opts, SQLSTATE)。
    let upd_param = |tag: &str| -> String { next_op(tag) };
    let cases: Vec<RejectionCase> = vec![
        (
            "psql 42P18",
            Client::Psql,
            PARAM_INDETERMINATE,
            vec!["1".into(), "2".into()],
            d,
            "42P18",
        ),
        (
            "psycopg 42804",
            Client::Psycopg,
            PARAM_UPD_N,
            vec![],
            Opts {
                typed_json: Some(typed_json(&[
                    ("text", "\"7\""),
                    ("unknown", &quoted("p2440-tenants-fixed-1")),
                ])),
                ..d
            },
            "42804",
        ),
        (
            "psycopg 42883",
            Client::Psycopg,
            PARAM_WHERE_S,
            vec![],
            Opts {
                typed_json: Some(typed_json(&[("int", "7")])),
                ..d
            },
            "42883",
        ),
        (
            "psycopg 0A000",
            Client::Psycopg,
            PARAM_UPD_N,
            vec![],
            Opts {
                typed_json: Some(typed_json(&[
                    ("int_list", "[1]"),
                    ("unknown", &quoted("p2440-tenants-fixed-2")),
                ])),
                ..d
            },
            "0A000",
        ),
        (
            "psycopg 23502",
            Client::Psycopg,
            PARAM_INS_REQ_NULL,
            vec![],
            Opts {
                typed_json: Some(typed_json(&[
                    ("null", "null"),
                    ("unknown", &quoted("p2440-tenants-fixed-3")),
                ])),
                ..d
            },
            "23502",
        ),
        (
            "pg 42804",
            Client::Pg,
            PARAM_UPD_N,
            vec!["7".into(), "p2440-tenants-fixed-4".into()],
            pg_declared(25),
            "42804",
        ),
        (
            "pg 0A000",
            Client::Pg,
            PARAM_UPD_N,
            vec!["7".into(), "p2440-tenants-fixed-5".into()],
            pg_declared(1007),
            "0A000",
        ),
        (
            "pg 42883",
            Client::Pg,
            PARAM_WHERE_S,
            vec!["t".into()],
            Opts {
                param_types: Some(oid_json(&[16])),
                ..d
            },
            "42883",
        ),
    ];
    for (name, client, sql, params, opts, state) in &cases {
        let refs: Vec<&str> = params.iter().map(String::as_str).collect();
        let mut seen: Vec<String> = Vec::new();
        for (user, pw) in USERS {
            let err = rejected_stderr(*client, port, user, pw, sql, &refs, *opts, state);
            for needle in [
                "param_items",
                "tenant-",
                "alice",
                "bob",
                "carol",
                "pub-",
                "priv-",
            ] {
                assert!(
                    !err.contains(needle),
                    "{name}: leaks `{needle}` to {user}: {err}"
                );
            }
            seen.push(err);
        }
        assert!(
            seen.windows(2).all(|w| w[0] == w[1]),
            "{name}: responses differ across tenants: {seen:?}"
        );
    }

    // 宣言 OID 付きの UPDATE: 他テナントの Private 行（id=2）と存在しない行（id=999）は同じ結果
    // （成功・行なし）で、alice の行は変わらない。
    let upd_by_id = "UPDATE param_items SET n = $1 WHERE id = $2 USING OPERATION_ID $3";
    let before = read_param_cell(port, "n", 2);
    for (client, opts_for) in [
        (
            Client::Psycopg,
            (|id: &str, op: &str| Opts {
                typed_json: Some(typed_json(&[
                    ("int4", "99"),
                    ("unknown", &quoted(id)),
                    ("unknown", &quoted(op)),
                ])),
                ..Opts::default()
            }) as fn(&str, &str) -> Opts,
        ),
        (
            Client::Pg,
            (|_id: &str, _op: &str| Opts {
                param_types: Some(oid_json(&[23, 0, 0])),
                ..Opts::default()
            }) as fn(&str, &str) -> Opts,
        ),
    ] {
        let mut results = Vec::new();
        for id in ["2", "999"] {
            let op = upd_param("tenants-upd");
            let params: Vec<&str> = if client == Client::Pg {
                vec!["99", id, &op]
            } else {
                vec![]
            };
            let out = spawn_client(
                client,
                port,
                "bob",
                "pw-bob",
                upd_by_id,
                &params,
                opts_for(id, &op),
            );
            assert!(
                out.status.success(),
                "{client:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            results.push((
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        assert_eq!(
            results[0], results[1],
            "{client:?}: foreign id must look absent"
        );
    }
    assert_eq!(
        read_param_cell(port, "n", 2),
        before,
        "alice's row is unchanged"
    );

    // 宣言の有無が 42P01（存在しない表）の応答を変えない。
    let missing = "SELECT n FROM no_such_table WHERE n = $1 LIMIT 5";
    let plain = rejected_stderr(
        Client::Pg,
        port,
        "carol",
        "pw-carol",
        missing,
        &["1"],
        d,
        "42P01",
    );
    let declared = rejected_stderr(
        Client::Pg,
        port,
        "carol",
        "pw-carol",
        missing,
        &["1"],
        Opts {
            param_types: Some(oid_json(&[23])),
            ..d
        },
        "42P01",
    );
    assert_eq!(
        plain, declared,
        "declaration must not become an existence oracle"
    );
    drop(env);
}

/// psycopg の自然な binary 送信のうち互換表に載らない組み合わせは `0A000`（fail-closed）で
/// 拒否され、接続は保たれること。これは「既知の差分」の固定であり、WIRE-24 (1) を
/// 満たしたことにはしない（`binary_param_kind` の互換表の見直しは別 Issue。#2496 で据え置き）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_natural_binary_params_outside_the_compat_table_fail_closed() {
    let env = start_param_env("binary");
    let port = env.server.port;
    let cases = [
        (
            "float (float8) into REAL",
            "UPDATE param_items SET r = $1 WHERE id = 1 USING OPERATION_ID $2",
            ("float", "1.5"),
        ),
        (
            "date into DATE",
            "UPDATE param_items SET day = $1 WHERE id = 1 USING OPERATION_ID $2",
            ("date", "\"2024-01-02\""),
        ),
    ];
    for (name, sql, first) in cases {
        let opts = Opts {
            sql_after: Some(PARAM_SEL_N),
            ..typed_with_op(first, "binary")
        };
        let err = rejected_stderr(
            Client::Psycopg,
            port,
            "alice",
            "pw-alice",
            sql,
            &[],
            opts,
            "0A000",
        );
        assert!(!err.contains("param_items"), "{name}: {err}");
        let out = spawn_client(Client::Psycopg, port, "alice", "pw-alice", sql, &[], opts);
        assert!(out.status.success(), "{name}: connection must survive");
        assert!(
            String::from_utf8_lossy(&out.stdout).starts_with("after:"),
            "{name}"
        );
    }
    drop(env);
}
// --- Issue #2433: 非等価・BETWEEN の `$n` のリテラル形一致と RLS（SQL-78・SQL-24・SQL-19・WIRE-12・TASK-336 のポインタ） ---
//
// 層 A（`wire_sql78_noneq_between_rls.rs`）が確定オラクルとして固定した「非 NULL の Bind は同じ値の
// リテラル形（簡易クエリ）と同じ結果になる」「他テナントの行・存在情報が現れない」を、無改造の実
// クライアントからも確認する。比較の原則:
// - 比較は**同一クライアント内**で行う（拡張 `$n` 形 vs 同じクライアントの簡易クエリ）。ドライバごとの
//   値の表記差（真偽・日付など）を比較に持ち込まないため、結果は `label`（TEXT）列だけを読む。
// - 受理か拒否かはリテラル形に追従させ、SQLSTATE は値を直書きせず両形の一致だけを固定する。
// - 行集合はソートして比べる（順序は本題ではない）。
// - 状態を変える DML は、クライアント・形ごとに新しい環境（DB・サーバー）を起動して分離する。
// - 表は SQL の `CREATE TABLE`（暗黙の `id` を持たない採番方式）で作る。BOOLEAN の比較と TEXT の
//   BETWEEN が `TableSchema::new` の表では `22000` になるため、リテラル形と比べられる範囲が広い。

/// `r` の各列（`n INTEGER, b BIGINT, d DOUBLE PRECISION, s TEXT, day DATE, u UUID, flag BOOLEAN`）。
const RANGE_COLS: [&str; 7] = ["n", "b", "d", "s", "day", "u", "flag"];
/// tenant-a と tenant-b は `n` の値域を交互に重ねて持つ（`n > 0` が漏れれば両テナントにまたがる）。
const RANGE_A_VALUES: [u32; 3] = [1, 5, 9];
const RANGE_B_VALUES: [u32; 3] = [2, 6, 10];
const RANGE_SEL_TAIL: &str = "ORDER BY label LIMIT 100";

fn range_raw(col: &str, v: u32) -> String {
    match col {
        "n" => v.to_string(),
        "b" => (v * 100).to_string(),
        "d" => format!("{v}.5"),
        "s" => format!("s{v:02}"),
        "day" => format!("2024-01-{v:02}"),
        "u" => format!("00000000-0000-0000-0000-{v:012}"),
        "flag" => (v % 2 == 1).to_string(),
        other => panic!("unknown column {other}"),
    }
}

/// リテラル形での値の書き方。数値は素のまま、それ以外は `'..'`（`'` は二重化）。BOOLEAN 列の
/// `BETWEEN` 境界は文字列リテラルが受理形のため、`between` のときは `flag` も引用する。
fn range_literal(col: &str, raw: &str, between: bool) -> String {
    match col {
        "n" | "b" | "d" => raw.to_string(),
        "flag" if !between => raw.to_string(),
        _ => format!("'{}'", raw.replace('\'', "''")),
    }
}

/// `r` を載せたサーバー一式（子プロセス・DB・ユーザーファイルの寿命を束ねる）。
struct RangeEnv {
    server: ServerGuard,
    _db: temp_db::CleanupGuard,
    _users_dir: temp_db::TempDir,
}

/// SQL の `CREATE TABLE` と SQL の `INSERT`（所有テナントの Private 行になる）で seed した DB を作り、
/// サーバーを起動する。seed 用の engine はサーバー起動前に drop する（DB のロックを解放する）。
fn start_range_env(tag: &str) -> RangeEnv {
    let path = temp_db::unique_db_path(&format!("three-client-range-{tag}"));
    let db = temp_db::CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut ddl = SessionState::default();
        ddl.allow_ddl();
        core.execute_sql_in_session(
            &PolicyContext::new("tenant-a").expect("valid tenant"),
            &mut ddl,
            "CREATE TABLE r (n INTEGER, b BIGINT, d DOUBLE PRECISION, s TEXT, day DATE, u UUID, \
             flag BOOLEAN, label TEXT)",
        )
        .expect("create table r");
        for (tenant, prefix, values) in [
            ("tenant-a", "a", RANGE_A_VALUES),
            ("tenant-b", "b", RANGE_B_VALUES),
        ] {
            let mut session = SessionState::default();
            for v in values {
                core.execute_sql_in_session(
                    &PolicyContext::new(tenant).expect("valid tenant"),
                    &mut session,
                    &format!(
                        "INSERT INTO r (n, b, d, s, day, u, flag, label) VALUES ({n}, {b}, {d}, \
                         '{s}', '{day}', '{u}', {flag}, '{prefix}-{v}') \
                         USING OPERATION_ID 'range-seed-{prefix}-{v}'",
                        n = range_raw("n", v),
                        b = range_raw("b", v),
                        d = range_raw("d", v),
                        s = range_raw("s", v),
                        day = range_raw("day", v),
                        u = range_raw("u", v),
                        flag = v % 2 == 1,
                    ),
                )
                .expect("seed insert");
            }
        }
    }
    let users_dir = temp_db::TempDir::new(&format!("three-client-range-{tag}-users"));
    let users_path = users_dir.path().join("users.txt");
    write_users_file(&users_path);
    let server = spawn_wire_server(&users_path, &path);
    RangeEnv {
        server,
        _db: db,
        _users_dir: users_dir,
    }
}

/// 簡易クエリ（'Q'）で `sql` を 1 文送る。psql は `-c`、psycopg は `ClientCursor`、node pg は
/// values なしの `query(text)`（いずれも既存の簡易クエリ用ハーネスと同じ駆動方法）。
/// 拒否の SQLSTATE を読めるよう psql は `VERBOSITY=verbose` で起動する。
fn spawn_simple(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
) -> std::process::Output {
    match client {
        Client::Psql => {
            let psql = resolve_tool("PSQL_BIN", "psql");
            Command::new(&psql)
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
                    "-F",
                    "|",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-v",
                    "VERBOSITY=verbose",
                    "-c",
                    sql,
                ])
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn {psql}: {e}"))
        }
        Client::Psycopg => {
            let python = resolve_tool("PYTHON_BIN", "python3");
            let script =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/psycopg_client.py");
            Command::new(&python)
                .arg(&script)
                .env("WIRE_HOST", "127.0.0.1")
                .env("WIRE_PORT", port.to_string())
                .env("WIRE_USER", user)
                .env("WIRE_PASSWORD", password)
                .env("WIRE_SQL", sql)
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn {python}: {e}"))
        }
        Client::Pg => {
            let node = resolve_tool("NODE_BIN", "node");
            let script =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/three_client/pg_client.js");
            Command::new(&node)
                .arg(&script)
                .env("WIRE_HOST", "127.0.0.1")
                .env("WIRE_PORT", port.to_string())
                .env("WIRE_USER", user)
                .env("WIRE_PASSWORD", password)
                .env("WIRE_SQL", sql)
                .output()
                .unwrap_or_else(|e| panic!("failed to spawn {node}: {e}"))
        }
    }
}

/// stderr から SQLSTATE を取り出す。psycopg／pg は `[SQLSTATE=XXXXX]`、psql（verbose）は
/// `ERROR:  XXXXX:`。取れなければ panic する（fail-closed: 想定外の失敗を「一致」と誤認しない）。
fn sqlstate_of(client: Client, stderr: &str) -> String {
    let code = match client {
        Client::Psql => stderr
            .split("ERROR:  ")
            .nth(1)
            .and_then(|rest| rest.get(..5)),
        _ => stderr
            .split("[SQLSTATE=")
            .nth(1)
            .and_then(|rest| rest.get(..5)),
    };
    let code = code.unwrap_or_else(|| panic!("{client:?}: no SQLSTATE in stderr: {stderr}"));
    assert!(
        code.chars().all(|c| c.is_ascii_alphanumeric()),
        "{client:?}: malformed SQLSTATE {code:?} in {stderr}"
    );
    code.to_string()
}

/// 実行結果を「ソート済みの行集合（`<型名>:` を除去）」または「SQLSTATE」へ畳む。
fn fold_output(client: Client, output: &std::process::Output) -> Result<Vec<String>, String> {
    if output.status.success() {
        let mut rows = untag(
            &String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>(),
        );
        rows.sort();
        Ok(rows)
    } else {
        Err(sqlstate_of(
            client,
            &String::from_utf8_lossy(&output.stderr),
        ))
    }
}

/// 拡張 `$n` 形の実行（psql は verbose、psycopg／pg は text の OID 0 パラメータ）。
fn ext_folded(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
    params: &[&str],
) -> Result<Vec<String>, String> {
    let output = if client == Client::Psql {
        spawn_psql_bind(port, user, password, sql, params, true)
    } else {
        spawn_client(client, port, user, password, sql, params, Opts::default())
    };
    fold_output(client, &output)
}

fn simple_folded(
    client: Client,
    port: u16,
    user: &str,
    password: &str,
    sql: &str,
) -> Result<Vec<String>, String> {
    fold_output(client, &spawn_simple(client, port, user, password, sql))
}

/// 拡張形（`$n`）とリテラル形の組。
struct RangeCase {
    ext: String,
    literal: String,
    values: Vec<String>,
}

fn range_cases() -> Vec<RangeCase> {
    let head = "SELECT label FROM r WHERE";
    let tail = RANGE_SEL_TAIL;
    let mut cases = Vec::new();
    for col in RANGE_COLS {
        let mid = range_raw(col, 5);
        for op_sym in [">", "<>"] {
            cases.push(RangeCase {
                ext: format!("{head} {col} {op_sym} $1 {tail}"),
                literal: format!(
                    "{head} {col} {op_sym} {} {tail}",
                    range_literal(col, &mid, false)
                ),
                values: vec![mid.clone()],
            });
        }
        let (lo, hi) = (range_raw(col, 2), range_raw(col, 6));
        for neg in ["", "NOT "] {
            cases.push(RangeCase {
                ext: format!("{head} {col} {neg}BETWEEN $1 AND $2 {tail}"),
                literal: format!(
                    "{head} {col} {neg}BETWEEN {} AND {} {tail}",
                    range_literal(col, &lo, true),
                    range_literal(col, &hi, true)
                ),
                values: vec![lo.clone(), hi.clone()],
            });
        }
    }
    cases.push(RangeCase {
        ext: format!("{head} n > $1 AND s <= $2 {tail}"),
        literal: format!("{head} n > 1 AND s <= 's06' {tail}"),
        values: vec!["1".into(), "s06".into()],
    });
    cases.push(RangeCase {
        ext: format!("{head} n < $1 OR day >= $2 {tail}"),
        literal: format!("{head} n < 3 OR day >= '2024-01-09' {tail}"),
        values: vec!["3".into(), "2024-01-09".into()],
    });
    // 不正値・インジェクション風の値は不透明なリテラルのまま扱われる（結果はリテラル形に追従）。
    for (col, bad) in [("day", "not-a-date"), ("n", "abc"), ("s", "x' OR '1'='1")] {
        cases.push(RangeCase {
            ext: format!("{head} {col} <> $1 {tail}"),
            literal: format!("{head} {col} <> '{}' {tail}", bad.replace('\'', "''")),
            values: vec![bad.to_string()],
        });
    }
    cases
}

/// 常時実行のガード: seed の値域が両テナントにまたがる（`n > 0` が漏れれば他テナントの行が
/// 見える）形であり、ケース表が全列・両形を網羅していること。
#[test]
fn range_fixture_overlaps_tenants_and_cases_cover_all_columns() {
    let overlap = RANGE_A_VALUES.iter().min() < RANGE_B_VALUES.iter().max()
        && RANGE_B_VALUES.iter().min() < RANGE_A_VALUES.iter().max();
    assert!(overlap, "tenant value ranges must interleave");
    let cases = range_cases();
    for col in RANGE_COLS {
        assert!(cases
            .iter()
            .any(|c| c.ext.contains(&format!("WHERE {col} >"))));
        assert!(cases
            .iter()
            .any(|c| c.ext.contains(&format!("{col} NOT BETWEEN"))));
    }
    for c in &cases {
        let placeholders = (1..=c.values.len())
            .filter(|i| c.ext.contains(&format!("${i}")))
            .count();
        assert_eq!(placeholders, c.values.len(), "{}", c.ext);
        assert!(!c.literal.contains('$'), "{}", c.literal);
    }
}

/// 3 クライアント × 3 テナントで、非 NULL の `$n`（非等価・`[NOT] BETWEEN`・複合・不正値）の結果が
/// 同じクライアントの簡易クエリのリテラル形と一致すること（受理時は行集合、拒否時は SQLSTATE）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_noneq_between_params_match_simple_literal_form() {
    let env = start_range_env("literal");
    let port = env.server.port;
    let cases = range_cases();
    let mut accepted_nonempty = 0usize;
    for client in Client::ALL {
        for (idx, (user, pw)) in USERS.iter().enumerate() {
            // alice は全ケース、bob・carol は RLS の確認に足りる間引き（4 件に 1 件）。
            let step = if idx == 0 { 1 } else { 4 };
            for case in cases.iter().step_by(step) {
                let vals: Vec<&str> = case.values.iter().map(String::as_str).collect();
                let ext = ext_folded(client, port, user, pw, &case.ext, &vals);
                let lit = simple_folded(client, port, user, pw, &case.literal);
                assert_eq!(
                    ext, lit,
                    "{client:?}/{user}: {} vs {}",
                    case.ext, case.literal
                );
                if idx == 0 && matches!(&ext, Ok(rows) if !rows.is_empty()) {
                    accepted_nonempty += 1;
                }
            }
        }
    }
    // 非 vacuous: 受理されて行を返したケースが十分にある。
    assert!(
        accepted_nonempty >= 3 * 20,
        "too few accepted non-empty cases: {accepted_nonempty}"
    );
    drop(env);
}

/// 3 クライアントで、非等価・BETWEEN の `$n` の結果が自テナントの行だけであり（carol は空）、拒否応答が
/// 3 テナントで同一でテナント id・他テナントの値を含まないこと。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_noneq_between_params_respect_rls() {
    let env = start_range_env("rls");
    let port = env.server.port;
    let probes: [(&str, Vec<&str>); 2] = [
        (
            "SELECT label FROM r WHERE n > $1 ORDER BY label LIMIT 100",
            vec!["0"],
        ),
        (
            "SELECT label FROM r WHERE n BETWEEN $1 AND $2 ORDER BY label LIMIT 100",
            vec!["0", "100"],
        ),
    ];
    // (自テナントの label 接頭辞, 件数)
    let expect = [(Some("a-"), 3usize), (Some("b-"), 3), (None, 0)];
    let reject_sqls: [(&str, Vec<&str>, &str); 2] = [
        (
            "SELECT label FROM r WHERE nosuchcol > $1 LIMIT 5",
            vec!["1"],
            "42703",
        ),
        (
            "SELECT label FROM r WHERE $1 < n LIMIT 5",
            vec!["1"],
            "42601",
        ),
    ];
    for client in Client::ALL {
        for (sql, params) in &probes {
            for (i, (user, pw)) in USERS.iter().enumerate() {
                let rows = ext_folded(client, port, user, pw, sql, params)
                    .unwrap_or_else(|e| panic!("{client:?}/{user}: rejected {e}: {sql}"));
                assert_eq!(rows.len(), expect[i].1, "{client:?}/{user}: {rows:?}");
                if let Some(prefix) = expect[i].0 {
                    assert!(rows.iter().all(|r| r.starts_with(prefix)), "{rows:?}");
                }
            }
        }
        for (sql, params, sqlstate) in &reject_sqls {
            let mut bodies = Vec::new();
            for (user, pw) in USERS {
                bodies.push(rejected_stderr(
                    client,
                    port,
                    user,
                    pw,
                    sql,
                    params,
                    Opts::default(),
                    sqlstate,
                ));
            }
            assert_eq!(bodies[0], bodies[1], "{client:?}: {sql}");
            assert_eq!(bodies[0], bodies[2], "{client:?}: {sql}");
            for leak in ["tenant", "a-1", "a-5", "a-9", "b-2", "b-6", "b-10"] {
                assert!(!bodies[0].contains(leak), "{client:?}: leak {leak:?}");
            }
        }
    }
    drop(env);
}

/// 述語形 UPDATE／DELETE の非等価・BETWEEN の `$n` を bob が実行した後の読み戻し（3 テナント分）が、
/// 同じクライアントのリテラル形で実行した場合と一致し、alice の行は変わらず、carol の DML は 0 件のまま
/// であること。状態を分離するため、クライアント・形ごとに新しい環境を起動する。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn three_clients_predicate_dml_noneq_between_match_literal_form() {
    let snapshots = |port: u16| -> Vec<Vec<String>> {
        USERS
            .iter()
            .map(|(user, pw)| run_psycopg_snapshot(port, user, pw))
            .collect()
    };
    for (client, tag) in [
        (Client::Psql, "psql"),
        (Client::Psycopg, "psycopg"),
        (Client::Pg, "pg"),
    ] {
        let mut results: Vec<Vec<Vec<String>>> = Vec::new();
        for form in ["ext", "lit"] {
            let env = start_range_env(&format!("dml-{tag}-{form}"));
            let port = env.server.port;
            // (ユーザー, 拡張形, パラメータ, リテラル形)。operation_id は連番で一意にする。
            let dmls: Vec<(usize, &str, Vec<String>, String)> = {
                let (o1, o2, o3, o4, o5) = (
                    next_op(tag),
                    next_op(tag),
                    next_op(tag),
                    next_op(tag),
                    next_op(tag),
                );
                vec![
                    (
                        1,
                        "UPDATE r SET label = 'upd' WHERE n > $1 USING OPERATION_ID $2",
                        vec!["4".into(), o1.clone()],
                        format!("UPDATE r SET label = 'upd' WHERE n > 4 USING OPERATION_ID '{o1}'"),
                    ),
                    (
                        1,
                        "DELETE FROM r WHERE day BETWEEN $1 AND $2 USING OPERATION_ID $3",
                        vec!["2024-01-01".into(), "2024-01-07".into(), o2.clone()],
                        format!(
                            "DELETE FROM r WHERE day BETWEEN '2024-01-01' AND '2024-01-07' \
                             USING OPERATION_ID '{o2}'"
                        ),
                    ),
                    (
                        2,
                        "UPDATE r SET label = 'hijack' WHERE n > $1 USING OPERATION_ID $2",
                        vec!["0".into(), o3.clone()],
                        format!(
                            "UPDATE r SET label = 'hijack' WHERE n > 0 USING OPERATION_ID '{o3}'"
                        ),
                    ),
                    (
                        2,
                        "DELETE FROM r WHERE n <> $1 USING OPERATION_ID $2",
                        vec!["-1".into(), o4.clone()],
                        format!("DELETE FROM r WHERE n <> -1 USING OPERATION_ID '{o4}'"),
                    ),
                    (
                        0,
                        "UPDATE r SET label = 'a-upd' WHERE n <> $1 USING OPERATION_ID $2",
                        vec!["5".into(), o5.clone()],
                        format!(
                            "UPDATE r SET label = 'a-upd' WHERE n <> 5 USING OPERATION_ID '{o5}'"
                        ),
                    ),
                ]
            };
            for (user_idx, ext_sql, params, lit_sql) in &dmls {
                let (user, pw) = USERS[*user_idx];
                let output = if form == "ext" {
                    let p: Vec<&str> = params.iter().map(String::as_str).collect();
                    if client == Client::Psql {
                        spawn_psql_bind(port, user, pw, ext_sql, &p, true)
                    } else {
                        spawn_client(client, port, user, pw, ext_sql, &p, Opts::default())
                    }
                } else {
                    spawn_simple(client, port, user, pw, lit_sql)
                };
                assert!(
                    output.status.success(),
                    "{client:?}/{form}/{user}: {ext_sql}: stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            results.push(snapshots(port));
            drop(env);
        }
        assert_eq!(
            results[0], results[1],
            "{client:?}: ext vs literal snapshots"
        );
        let snap = &results[0];
        // 非 vacuous: bob は n=10 の 1 行だけ残り、alice は自分の 2 行だけが更新され、carol は空のまま。
        assert_eq!(snap[1], vec!["upd|10".to_string()], "{client:?}: bob");
        assert_eq!(
            snap[0],
            s(&["a-upd|1", "a-5|5", "a-upd|9"]),
            "{client:?}: alice"
        );
        assert!(snap[2].is_empty(), "{client:?}: carol");
    }
}

/// 読み戻し用: 指定ユーザーが見える `r` の `label|n`（`n` 昇順）。クライアント非依存にするため
/// psycopg の簡易クエリで固定して読む。
fn run_psycopg_snapshot(port: u16, user: &str, password: &str) -> Vec<String> {
    let output = spawn_simple(
        Client::Psycopg,
        port,
        user,
        password,
        "SELECT label, n FROM r ORDER BY n LIMIT 100",
    );
    assert!(
        output.status.success(),
        "snapshot failed for {user}: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// psycopg（`None`）と node pg（`null`）の NULL パラメータを非等価の位置へ渡した結果が、簡易クエリの
/// リテラル `NULL` と一致すること（受理・拒否とも一致。psql は NULL を送れないため対象外）。
#[test]
#[ignore = "requires psql, python3+psycopg, node+pg; run via `make e2e-three-client`"]
fn psycopg_and_pg_null_param_in_noneq_matches_literal() {
    let env = start_range_env("nullparam");
    let port = env.server.port;
    let ext_sql = "SELECT label FROM r WHERE n > $1 ORDER BY label LIMIT 100";
    let lit_sql = "SELECT label FROM r WHERE n > NULL ORDER BY label LIMIT 100";
    for client in [Client::Psycopg, Client::Pg] {
        let opts = match client {
            Client::Psycopg => Opts {
                typed_json: Some(typed_json(&[("null", "null")])),
                ..Opts::default()
            },
            _ => Opts {
                nullable_json: Some(nullable_json(&[None])),
                ..Opts::default()
            },
        };
        let output = spawn_client(client, port, "alice", "pw-alice", ext_sql, &[], opts);
        let ext = fold_output(client, &output);
        let lit = simple_folded(client, port, "alice", "pw-alice", lit_sql);
        assert_eq!(ext, lit, "{client:?}: NULL param vs literal NULL");
    }
    drop(env);
}
