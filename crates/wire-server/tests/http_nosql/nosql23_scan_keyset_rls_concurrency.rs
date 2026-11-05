//! `POST /v1/query`（`op: "scan"`）のキーセットページング（`after`／`next_cursor`）について、
//! RLS・ページ間の並行変更・SQL 表層とのパリティを production ルータ（HTTP 生バイト）と
//! pg wire を通した結合で固定する層 A テスト（Issue #2095。ポインタ: `docs/spec` NOSQL-23・
//! RLS-7。関連: RLS-11・SQL-24・SQL-25・NOSQL-15・TABLE-23）。
//!
//! 役割分担: カーソルの往復・エラー分類・束縛・不透明性の代表ケースは `nosql23_scan_keyset.rs`
//! （Issue #2094）、engine の束縛 API 越しの同種の性質は `engine/tests/sql_t01_39/sql25_keyset.rs`
//! が担う。本ファイルは「3 テナントが混在する表」「ページ間の書き込み〔同一・他テナント・SQL 表層〕」
//! 「SQL との等価性」を HTTP と pg wire の両入口で検証する。HTTP と pg wire は同一 `EngineCore`
//! 上に起動し、表層をまたいだ書き込みも観測する（`nosql12_partitioned_dml.rs` と同じ形）。
//!
//! 並行変更はスレッドを使わず「1 ページ取得 → 書き込み → 続きを取得」を段階実行して再現する
//! （タイミング依存の間欠失敗を避ける）。ページ取得ループは必ず上限（[`MAX_PAGES`]）を持つ。
//!
//! SQL へ埋め込む値はテスト自身が作った英数字だけの定数、または応答から取り出した値を
//! [`safe_literal`] で英数字だけと検査したものに限る（応答の文字列をそのまま連結しない）。

// テナント番号（0..3）で `TENANTS`・トークン・pg wire 接続を同じ添字で引くため、範囲ループを許す。
#![allow(clippy::needless_range_loop)]

use crate::common;
use crate::http_common;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::base64_std::{decode_base64_std, encode_base64_std};
use wire_server::http::query::response::encode as encode_query_result;
use wire_server::http::session::store::SessionStore;

/// ページ取得ループの上限（終わらないカーソルを無限ループにせず panic で検出する）。
const MAX_PAGES: usize = 100;

const TENANTS: [&str; 4] = ["tenant-a", "tenant-b", "tenant-c", "tenant-d"];
/// ユーザー名とパスワード（ダミー値）。tenant-d は表に 1 行も持たない閲覧者。
const USERS: [(&str, &str); 4] = [
    ("alice", "pw-alice"),
    ("bob", "pw-bob"),
    ("chris", "pw-chris"),
    ("dave", "pw-dave"),
];

const SORT_G_ASC: &str = r#"[{"column":"g","dir":"asc"}]"#;
const SORT_G_DESC: &str = r#"[{"column":"g","dir":"desc"}]"#;
const SORT_G_DESC_KEY_DESC: &str = r#"[{"column":"g","dir":"desc"},{"column":"k","dir":"desc"}]"#;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

// --- 応答の解析 ------------------------------------------------------------

/// 応答 1 行（キー列・並べ替え列 `g`・書き込んだテナント名 `owner`）。
#[derive(Debug, Clone, PartialEq)]
struct Row {
    key: String,
    g: Option<String>,
    owner: Option<String>,
}

struct Page {
    rows: Vec<Row>,
    next: Option<String>,
}

fn cell(v: &JsonValue) -> Option<String> {
    match v {
        JsonValue::Null => None,
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Number(n) => Some(format!("{}", n.as_f64())),
        other => panic!("unexpected cell: {other:?}"),
    }
}

/// `columns`／`rows`（と任意の `next_cursor`）を持つ本文を解析する。NoSQL 応答と
/// `response::encode` 済みの SQL 結果の両方に使う。
fn parse_body(body: &str) -> Page {
    let JsonValue::Object(mut top) = parse_json(body).expect("body json") else {
        panic!("object expected: {body}");
    };
    let names: Vec<String> = match top.remove("columns") {
        Some(JsonValue::Array(items)) => items
            .into_iter()
            .map(|c| match c {
                JsonValue::Object(mut o) => match o.remove("name") {
                    Some(JsonValue::String(s)) => s,
                    other => panic!("column name: {other:?}"),
                },
                other => panic!("column: {other:?}"),
            })
            .collect(),
        other => panic!("columns must be an array: {other:?}"),
    };
    let pos = |wanted: &[&str]| names.iter().position(|n| wanted.contains(&n.as_str()));
    let (ki, gi, oi) = (pos(&["k", "id", "n"]), pos(&["g"]), pos(&["owner"]));
    let rows = match top.remove("rows") {
        Some(JsonValue::Array(items)) => items
            .into_iter()
            .map(|r| {
                let JsonValue::Array(cells) = r else {
                    panic!("row must be an array")
                };
                Row {
                    key: ki
                        .and_then(|i| cells.get(i))
                        .and_then(cell)
                        .unwrap_or_default(),
                    g: gi.and_then(|i| cells.get(i)).and_then(cell),
                    owner: oi.and_then(|i| cells.get(i)).and_then(cell),
                }
            })
            .collect(),
        other => panic!("rows must be an array: {other:?}"),
    };
    let next = match top.remove("next_cursor") {
        Some(JsonValue::String(s)) => Some(s),
        Some(JsonValue::Null) | None => None,
        other => panic!("next_cursor: {other:?}"),
    };
    Page { rows, next }
}

fn keys(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|r| r.key.clone()).collect()
}

fn flat(pages: &[Page]) -> Vec<Row> {
    pages.iter().flat_map(|p| p.rows.clone()).collect()
}

/// 英数字だけの値を SQL リテラルへ埋め込む（応答由来の値の連結を安全側に限定する）。
fn safe_literal(s: &str) -> &str {
    assert!(
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()),
        "unsafe literal: {s:?}"
    );
    s
}

// --- テスト環境 -------------------------------------------------------------

/// HTTP ルータ（4 ユーザー）と pg wire 接続（tenant-a〜c）を同一 `EngineCore` 上に持つ環境。
struct Env {
    core: Arc<EngineCore>,
    http: SocketAddr,
    tokens: Vec<String>,
    sql: RefCell<Vec<TcpStream>>,
    ops: Cell<u64>,
    _guard: temp_db::CleanupGuard,
}

fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let req = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &req,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

fn text(resp: &HttpResponse) -> String {
    String::from_utf8_lossy(&resp.body).into_owned()
}

fn ok(resp: &HttpResponse) -> String {
    assert_eq!(resp.status, 200, "{resp:?} {}", text(resp));
    text(resp)
}

fn assert_22000(resp: &HttpResponse) {
    assert_eq!(resp.status, 400, "{resp:?} {}", text(resp));
    assert_eq!(http_common::wire_code_of(resp), "22000", "{}", text(resp));
    assert_eq!(
        http_common::error_message_of(resp),
        "invalid keyset position"
    );
}

/// scan 要求（`filter`・`columns` は生の JSON 断片。空文字なら付けない）。
#[derive(Clone)]
struct Req<'a> {
    table: &'a str,
    limit: usize,
    sort: &'a str,
    filter: &'a str,
    columns: &'a str,
}

impl<'a> Req<'a> {
    fn new(table: &'a str, limit: usize, sort: &'a str) -> Self {
        let columns = match table {
            "nopk" => r#"["n","g","owner"]"#,
            "ltab" => r#"["id","g","owner"]"#,
            _ => r#"["k","g","owner"]"#,
        };
        Req {
            table,
            limit,
            sort,
            filter: "",
            columns,
        }
    }

    fn body(&self, after: Option<&str>) -> String {
        let mut s = format!(
            r#"{{"op":"scan","table":"{}","limit":{},"columns":{}"#,
            self.table, self.limit, self.columns
        );
        if !self.sort.is_empty() {
            s.push_str(&format!(r#","sort":{}"#, self.sort));
        }
        if !self.filter.is_empty() {
            s.push_str(&format!(r#","filter":{}"#, self.filter));
        }
        match after {
            Some(c) => s.push_str(&format!(r#","after":"{c}"}}"#)),
            None => s.push_str(r#","after":null}"#),
        }
        s
    }
}

impl Env {
    fn start(core: Arc<EngineCore>, guard: temp_db::CleanupGuard) -> Env {
        let records: Vec<(&str, &str, &str)> = USERS
            .iter()
            .zip(TENANTS)
            .map(|((u, p), t)| (*u, t, *p))
            .collect();
        let users = common::write_user_store_file(&records);
        let http = http_common::spawn_router_listener_with_engine(
            &users,
            SessionStore::new(),
            core.clone(),
        );
        let tokens = USERS.iter().map(|(u, p)| login(http, u, p)).collect();
        let sql_addr = common::spawn_server_with_engine(&users, core.clone());
        let sql = USERS
            .iter()
            .take(3)
            .map(|(u, p)| common::authenticate_to_ready_for_query(sql_addr, u, p))
            .collect();
        Env {
            core,
            http,
            tokens,
            sql: RefCell::new(sql),
            ops: Cell::new(0),
            _guard: guard,
        }
    }

    /// 採番方式の表（`pkt`・`nopk`・`pkn`・`scn`）を作り、`tenants` の行を交互に投入する。
    /// 同じ `k`・`g` を各テナントが書く（rowid の払い出しがテナント間で混ざる）。
    fn build(tenants: &[usize]) -> Env {
        let path = temp_db::unique_db_path("nosql23-keyset-rls-concurrency");
        let guard = temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let core = Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        ));
        for ddl in [
            "CREATE TABLE pkt (k INTEGER PRIMARY KEY, g TEXT, owner TEXT)",
            "CREATE TABLE pkn (k INTEGER PRIMARY KEY, g TEXT, owner TEXT)",
            "CREATE TABLE scn (k INTEGER PRIMARY KEY, g TEXT, owner TEXT)",
            "CREATE TABLE nopk (n INTEGER, g TEXT, owner TEXT)",
        ] {
            let mut session = SessionState::default();
            session.allow_ddl();
            core.execute_sql_in_session(&ctx("tenant-a"), &mut session, ddl)
                .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
        }
        let env = Env::start(core, guard);
        // k は昇順でない順に投入する（PK 昇順と rowid 昇順を区別できるようにする）。
        let ks = [5, 2, 8, 1, 9, 3, 7, 4, 6];
        let g_null = [
            Some("b"),
            Some("a"),
            None,
            Some("a"),
            Some("c"),
            Some("b"),
            Some("a"),
            None,
            Some("c"),
        ];
        let g_full = ["b", "a", "d", "a", "c", "b", "a", "d", "c"];
        for i in 0..ks.len() {
            for &t in tenants {
                env.nosql_insert(t, "pkt", ks[i], g_null[i]);
                env.nosql_insert(t, "pkn", ks[i], Some(g_full[i]));
                env.nosql_insert(t, "nopk", i as i64 + 1, g_null[i]);
            }
        }
        env
    }

    /// 従来方式の表 `ltab`（`id` 疑似列。Public・Private 混在）を作る。各テナントは
    /// `base = 100 * (t + 1)` に対し Public 2 行（`base+1`・`base+2`）と Private 2 行
    /// （`base+3`・`base+4`）を持ち、`g` の値は全テナントで共通にする。
    fn build_legacy() -> Env {
        let path = temp_db::unique_db_path("nosql23-keyset-rls-concurrency-legacy");
        let guard = temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "ltab",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("g", ColumnType::Text, true),
                    ColumnDef::new("owner", ColumnType::Text, true),
                ],
            ))
            .expect("create legacy table");
        for (t, tenant) in TENANTS.iter().take(3).enumerate() {
            let base = 100 * (t as u64 + 1);
            let rows: [(u64, Visibility, Option<&str>); 4] = [
                (base + 1, Visibility::Public, Some("a")),
                (base + 2, Visibility::Public, None),
                (base + 3, Visibility::Private, Some("b")),
                (base + 4, Visibility::Private, Some("a")),
            ];
            for (id, vis, g) in rows {
                engine::tenant::insert_typed_row(
                    &storage,
                    "ltab",
                    &ctx(tenant),
                    id,
                    vis,
                    &[
                        Value::Vector(vec![id as f32, 0.0]),
                        g.map_or(Value::Null, |s| Value::Text(s.to_string())),
                        Value::Text((*tenant).to_string()),
                    ],
                    &engine::recovery::required_op_id::OperationId::parse(&format!("leg-{id}"))
                        .expect("valid operation_id"),
                )
                .expect("insert legacy row");
            }
        }
        let core = Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        ));
        Env::start(core, guard)
    }

    fn op_id(&self) -> String {
        self.ops.set(self.ops.get() + 1);
        format!("op-{}", self.ops.get())
    }

    fn post(&self, who: usize, body: &str) -> HttpResponse {
        let req = http_common::build_request(
            "/v1/query",
            &[
                ("Authorization", &format!("Bearer {}", self.tokens[who])),
                ("Content-Type", "application/json"),
                ("Content-Length", &body.len().to_string()),
            ],
            body.as_bytes(),
        );
        http_common::parse_single_response(&http_common::send_raw(
            self.http,
            &req,
            AfterWrite::HalfClose,
        ))
    }

    fn scan(&self, who: usize, req: &Req, after: Option<&str>) -> HttpResponse {
        self.post(who, &req.body(after))
    }

    fn page(&self, who: usize, req: &Req, after: Option<&str>) -> Page {
        parse_body(&ok(&self.scan(who, req, after)))
    }

    /// `after: null` から `next_cursor` を辿って全ページを取る。
    fn pages(&self, who: usize, req: &Req) -> Vec<Page> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = self.page(who, req, cursor.as_deref());
            assert!(page.rows.len() <= req.limit);
            let next = page.next.clone();
            out.push(page);
            match next {
                Some(c) => cursor = Some(c),
                None => return out,
            }
        }
        panic!("cursor did not terminate");
    }

    /// 与えた 1 ページ目のカーソルから続きの全ページを取る。
    fn rest(&self, who: usize, req: &Req, first_next: Option<String>) -> Vec<Row> {
        let mut out = Vec::new();
        let mut cursor = first_next;
        for _ in 0..MAX_PAGES {
            let Some(c) = cursor else { return out };
            let page = self.page(who, req, Some(&c));
            out.extend(page.rows);
            cursor = page.next;
        }
        panic!("cursor did not terminate");
    }

    // --- NoSQL 書き込み ---

    fn nosql_insert(&self, who: usize, table: &str, key: i64, g: Option<&str>) {
        let kname = if table == "nopk" { "n" } else { "k" };
        let g_json = g.map_or("null".to_string(), |s| format!("\"{s}\""));
        let body = format!(
            r#"{{"op":"insert","table":"{table}","rows":[{{"{kname}":{key},"g":{g_json},"owner":"{}"}}],"operation_id":"{}"}}"#,
            TENANTS[who],
            self.op_id()
        );
        ok(&self.post(who, &body));
    }

    fn nosql_delete(&self, who: usize, table: &str, key: i64) {
        let body = format!(
            r#"{{"op":"delete","table":"{table}","where":{{"k":{key}}},"operation_id":"{}"}}"#,
            self.op_id()
        );
        ok(&self.post(who, &body));
    }

    fn nosql_set_g(&self, who: usize, table: &str, key: i64, g: &str) {
        let body = format!(
            r#"{{"op":"update","table":"{table}","set":{{"g":"{g}"}},"where":{{"k":{key}}},"operation_id":"{}"}}"#,
            self.op_id()
        );
        ok(&self.post(who, &body));
    }

    // --- pg wire ---

    /// simple query を送り、DataRow をすべて返す（エラー応答は panic）。
    fn sql(&self, who: usize, stmt: &str) -> Vec<Vec<Option<String>>> {
        let mut conns = self.sql.borrow_mut();
        let stream = &mut conns[who];
        common::send_simple_query(stream, stmt);
        stream.flush().expect("flush");
        let mut rows = Vec::new();
        for _ in 0..10_000 {
            let (ty, body) = common::read_message(stream);
            match ty {
                b'D' => rows.push(parse_data_row(&body)),
                b'Z' => return rows,
                b'E' => panic!("{stmt}: error {}", String::from_utf8_lossy(&body)),
                _ => {}
            }
        }
        panic!("too many messages for {stmt}");
    }

    fn sql_rows(&self, who: usize, stmt: &str) -> Vec<Row> {
        self.sql(who, stmt)
            .into_iter()
            .map(|c| Row {
                key: c.first().cloned().flatten().unwrap_or_default(),
                g: c.get(1).cloned().flatten(),
                owner: c.get(2).cloned().flatten(),
            })
            .collect()
    }

    fn sql_insert(&self, who: usize, table: &str, key: i64, g: &str) {
        self.sql(
            who,
            &format!(
                "INSERT INTO {table} (k, g, owner) VALUES ({key}, '{g}', '{}') USING OPERATION_ID '{}'",
                TENANTS[who],
                self.op_id()
            ),
        );
    }

    fn sql_delete(&self, who: usize, table: &str, key: i64) {
        self.sql(
            who,
            &format!(
                "DELETE FROM {table} WHERE k = {key} USING OPERATION_ID '{}'",
                self.op_id()
            ),
        );
    }

    fn sql_set_g(&self, who: usize, table: &str, key: i64, g: &str) {
        self.sql(
            who,
            &format!(
                "UPDATE {table} SET g = '{g}' WHERE k = {key} USING OPERATION_ID '{}'",
                self.op_id()
            ),
        );
    }

    /// `PolicyContext`（Public ＋ 自テナント Private。wire ログインと同じ可視性）で SQL を実行し、
    /// `response::encode` 済みの JSON 本文を解析して返す（インプロセスのオラクル）。
    fn oracle(&self, who: usize, stmt: &str) -> Vec<Row> {
        let mut session = SessionState::default();
        let outcome = self
            .core
            .execute_sql_in_session(&ctx(TENANTS[who]), &mut session, stmt)
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
        let SqlOutcome::Query(result) = outcome else {
            panic!("expected a query outcome: {stmt}");
        };
        parse_body(&encode_query_result(&result).expect("encode")).rows
    }
}

/// `DataRow` 本文（フィールド数 + 長さ付きセル）を解析する。
fn parse_data_row(body: &[u8]) -> Vec<Option<String>> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut cells = Vec::new();
    for _ in 0..count {
        let len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if len < 0 {
            cells.push(None);
        } else {
            let len = len as usize;
            cells.push(Some(
                String::from_utf8_lossy(&body[pos..pos + len]).into_owned(),
            ));
            pos += len;
        }
    }
    cells
}

fn sorts_for(table: &str) -> Vec<&'static str> {
    match table {
        "pkt" => vec!["", SORT_G_ASC, SORT_G_DESC, SORT_G_DESC_KEY_DESC],
        _ => vec!["", SORT_G_ASC, SORT_G_DESC],
    }
}

// --- RLS（3 テナント混在） --------------------------------------------------

/// 閲覧者 `who` のページ列（ページごとの行）。
fn page_rows(pages: &[Page]) -> Vec<Vec<Row>> {
    pages.iter().map(|p| p.rows.clone()).collect()
}

#[test]
fn three_tenants_with_identical_keys_page_only_their_own_rows() {
    let all = Env::build(&[0, 1, 2]);
    for table in ["pkt", "nopk"] {
        for who in 0..3 {
            // 非 vacuous な対照: 自テナントだけが書いた環境でのページ列と完全に一致する。
            let solo = Env::build(&[who]);
            for sort in sorts_for(table) {
                let whole = flat(&all.pages(who, &Req::new(table, 50, sort)));
                assert_eq!(whole.len(), 9, "{table} {sort} who={who}");
                for limit in [1, 2, 3, 9] {
                    let req = Req::new(table, limit, sort);
                    let pages = all.pages(who, &req);
                    let rows = flat(&pages);
                    assert_eq!(rows, whole, "{table} sort={sort} limit={limit} who={who}");
                    assert!(
                        rows.iter()
                            .all(|r| r.owner.as_deref() == Some(TENANTS[who])),
                        "foreign row leaked: {rows:?}"
                    );
                    let uniq: BTreeSet<_> = keys(&rows).into_iter().collect();
                    assert_eq!(uniq.len(), 9, "duplicate keys: {rows:?}");
                    let baseline = page_rows(&solo.pages(who, &req));
                    assert_eq!(
                        page_rows(&pages),
                        baseline,
                        "other tenants changed page boundaries: {table} sort={sort} limit={limit}"
                    );
                }
            }
        }
    }
}

#[test]
fn legacy_table_cross_tenant_public_rows_page_without_gaps_or_private_leak() {
    // 現行挙動の固定。従来方式の表では他テナントの Public 行も可視集合に入り、順序と位置に
    // 含まれる。他テナントの Public 行を位置に含める点はオーナー判断待ち
    // （`docs/design/scalar-order-by-scan.md` キーセットモード節の判断事項）で、判断が変われば
    // このテストを改める。
    let env = Env::build_legacy();
    for who in 0..3 {
        let expected: BTreeSet<String> = (0..3u64)
            .flat_map(|t| [100 * (t + 1) + 1, 100 * (t + 1) + 2])
            .chain([100 * (who as u64 + 1) + 3, 100 * (who as u64 + 1) + 4])
            .map(|id| id.to_string())
            .collect();
        for sort in ["", SORT_G_ASC, SORT_G_DESC] {
            for limit in 1..=5 {
                let pages = env.pages(who, &Req::new("ltab", limit, sort));
                let rows = flat(&pages);
                let got: BTreeSet<String> = keys(&rows).into_iter().collect();
                assert_eq!(
                    rows.len(),
                    got.len(),
                    "duplicates: who={who} {sort} {limit}"
                );
                assert_eq!(got, expected, "who={who} sort={sort} limit={limit}");
                for r in &rows {
                    let id: u64 = r.key.parse().expect("numeric id");
                    if id % 100 >= 3 {
                        // Private 行は自テナントのものだけ。
                        assert_eq!(r.owner.as_deref(), Some(TENANTS[who]), "{r:?}");
                    }
                }
                // カーソルにテナント名・値の平文は出ない（他テナントの Public 行が最終行の
                // ページから作られるものを含む）。
                for p in &pages {
                    if let Some(c) = &p.next {
                        let raw = decode_base64_std(c, 1 << 20).expect("base64");
                        let hay = String::from_utf8_lossy(&raw);
                        assert!(!hay.contains("tenant-") && !c.contains("tenant-"), "leak");
                    }
                }
            }
        }
    }
}

fn flip_one_byte(cursor: &str) -> String {
    let mut raw = decode_base64_std(cursor, 1 << 20).expect("valid base64");
    let mid = raw.len() / 2;
    raw[mid] ^= 0x01;
    encode_base64_std(&raw)
}

#[test]
fn cursor_of_one_tenant_is_rejected_identically_by_the_others() {
    let env = Env::build(&[0, 1, 2]);
    let req = Req::new("pkt", 2, SORT_G_ASC);
    let cursors: Vec<String> = (0..3)
        .map(|t| env.page(t, &req, None).next.expect("cursor"))
        .collect();
    // 対照: 発行者自身の提示は通る。
    for (t, c) in cursors.iter().enumerate() {
        ok(&env.scan(t, &req, Some(c)));
    }
    let mut bodies = Vec::new();
    // 改ざんカーソルの応答（基準）。
    let tampered = env.scan(0, &req, Some(&flip_one_byte(&cursors[0])));
    assert_22000(&tampered);
    bodies.push(tampered.body.clone());
    for issuer in 0..3 {
        // 他の 2 テナントに加え、その表に 1 行も持たない tenant-d も同じ応答になる。
        for presenter in (0..4).filter(|p| *p != issuer) {
            let resp = env.scan(presenter, &req, Some(&cursors[issuer]));
            assert_22000(&resp);
            bodies.push(resp.body.clone());
        }
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "responses must be byte-identical"
    );
    // tenant-d は行を持たないため、自分の 1 ページ目は空の 200 で next_cursor は null（対照）。
    let empty = env.page(3, &req, None);
    assert!(empty.rows.is_empty() && empty.next.is_none());
}

// --- 並行変更（ページ間の注入） ---------------------------------------------

#[derive(Clone, Copy)]
enum Op {
    Insert(i64, &'static str),
    Delete(i64),
    SetG(i64, &'static str),
}

#[derive(Clone, Copy)]
enum Surface {
    NoSql,
    Sql,
}

fn apply(env: &Env, who: usize, surface: Surface, ops: &[Op]) {
    for op in ops {
        match (surface, *op) {
            (Surface::NoSql, Op::Insert(k, g)) => env.nosql_insert(who, "scn", k, Some(g)),
            (Surface::NoSql, Op::Delete(k)) => env.nosql_delete(who, "scn", k),
            (Surface::NoSql, Op::SetG(k, g)) => env.nosql_set_g(who, "scn", k, g),
            (Surface::Sql, Op::Insert(k, g)) => env.sql_insert(who, "scn", k, g),
            (Surface::Sql, Op::Delete(k)) => env.sql_delete(who, "scn", k),
            (Surface::Sql, Op::SetG(k, g)) => env.sql_set_g(who, "scn", k, g),
        }
    }
}

const BASE: [(i64, &str); 6] = [
    (10, "b"),
    (20, "b"),
    (30, "d"),
    (40, "d"),
    (50, "f"),
    (60, "f"),
];

/// 1 ページ目（limit 2 → (b,10)・(b,20)、位置は (b,20)）を取り、`ops` を `writer` が `surface`
/// で実行してから続きを取る。続きは `expected`（キーと `g`）に一致し、全行が tenant-a の行。
fn run_scenario(
    tenants: &[usize],
    writer: usize,
    surface: Surface,
    ops: &[Op],
    expected: &[(i64, &str)],
) {
    let env = Env::build(tenants);
    for &t in tenants {
        for (k, g) in BASE {
            env.nosql_insert(t, "scn", k, Some(g));
        }
    }
    let req = Req::new("scn", 2, SORT_G_ASC);
    let p1 = env.page(0, &req, None);
    assert_eq!(keys(&p1.rows), ["10", "20"]);
    apply(&env, writer, surface, ops);
    let rest = env.rest(0, &req, p1.next);
    let got: Vec<(String, Option<String>)> =
        rest.iter().map(|r| (r.key.clone(), r.g.clone())).collect();
    let want: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|(k, g)| (k.to_string(), Some(g.to_string())))
        .collect();
    assert_eq!(got, want);
    assert!(rest.iter().all(|r| r.owner.as_deref() == Some("tenant-a")));
}

const UNTOUCHED: [(i64, &str); 4] = [(30, "d"), (40, "d"), (50, "f"), (60, "f")];

const INSERTS: [Op; 5] = [
    Op::Insert(5, "a"),
    Op::Insert(15, "b"),
    Op::Insert(25, "b"),
    Op::Insert(35, "c"),
    Op::Insert(70, "z"),
];
const INSERTS_EXPECTED: [(i64, &str); 7] = [
    (25, "b"),
    (35, "c"),
    (30, "d"),
    (40, "d"),
    (50, "f"),
    (60, "f"),
    (70, "z"),
];

const DELETES: [Op; 4] = [
    Op::Delete(20),
    Op::Delete(30),
    Op::Delete(10),
    Op::Delete(60),
];
const DELETES_EXPECTED: [(i64, &str); 2] = [(40, "d"), (50, "f")];

// 位置の行 (b,20) を消して同じ PK で再挿入しても、位置は値で決まるので再挿入行は返らない。
const REINSERT: [Op; 3] = [Op::Delete(20), Op::Insert(20, "b"), Op::Delete(30)];
const REINSERT_EXPECTED: [(i64, &str); 3] = [(40, "d"), (50, "f"), (60, "f")];

// 未取得行を位置より前へ（消える）、取得済み行を後ろへ（再び現れる）、後ろ同士の並べ替え。
const UPDATES: [Op; 3] = [Op::SetG(40, "a"), Op::SetG(10, "e"), Op::SetG(50, "d")];
const UPDATES_EXPECTED: [(i64, &str); 4] = [(30, "d"), (50, "d"), (10, "e"), (60, "f")];

#[test]
fn inserts_between_pages_follow_the_position() {
    for surface in [Surface::NoSql, Surface::Sql] {
        run_scenario(&[0], 0, surface, &INSERTS, &INSERTS_EXPECTED);
    }
}

#[test]
fn deletes_between_pages_including_the_position_row() {
    for surface in [Surface::NoSql, Surface::Sql] {
        run_scenario(&[0], 0, surface, &DELETES, &DELETES_EXPECTED);
        run_scenario(&[0], 0, surface, &REINSERT, &REINSERT_EXPECTED);
    }
}

#[test]
fn sort_key_updates_move_rows_across_the_position() {
    for surface in [Surface::NoSql, Surface::Sql] {
        run_scenario(&[0], 0, surface, &UPDATES, &UPDATES_EXPECTED);
    }
}

#[test]
fn other_tenant_writes_between_pages_never_appear_or_shift_boundaries() {
    // tenant-b（NoSQL）と tenant-c（pg wire）が、tenant-a と同じ k・g の行に対して位置の前後へ
    // 挿入・削除・更新する。tenant-a の続きは注入なしのときと一致する。
    for ops in [&INSERTS[..], &DELETES[..], &REINSERT[..], &UPDATES[..]] {
        for (writer, surface) in [(1, Surface::NoSql), (2, Surface::Sql)] {
            run_scenario(&[0, 1, 2], writer, surface, ops, &UNTOUCHED);
        }
    }
}

#[test]
fn filter_columns_and_limit_may_change_between_pages() {
    let env = Env::build(&[0, 1, 2]);
    for t in [0, 1, 2] {
        for (k, g) in BASE {
            env.nosql_insert(t, "scn", k, Some(g));
        }
    }
    let first = Req::new("scn", 2, SORT_G_ASC);
    let p1 = env.page(0, &first, None);
    assert_eq!(keys(&p1.rows), ["10", "20"]);
    // 2 ページ目: filter・columns・limit をすべて変える。
    let mut second = Req::new("scn", 3, SORT_G_ASC);
    second.filter = r#"[{"column":"g","op":"ne","value":"d"}]"#;
    second.columns = r#"["k","g"]"#;
    let p2 = env.page(0, &second, p1.next.as_deref());
    // オラクル（pg wire）: 位置 (b,20) より後ろ ∧ 新しい filter。
    let expected = env.sql_rows(
        0,
        "SELECT k, g FROM scn WHERE g <> 'd' AND (g > 'b' OR (g = 'b' AND k > 20)) \
         ORDER BY g, k LIMIT 3",
    );
    let got: Vec<(String, Option<String>)> = p2
        .rows
        .iter()
        .map(|r| (r.key.clone(), r.g.clone()))
        .collect();
    let want: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|r| (r.key.clone(), r.g.clone()))
        .collect();
    assert_eq!(got, want);
    assert_eq!(
        got.len(),
        2,
        "only (f,50) and (f,60) remain after the filter"
    );
    assert!(p2.next.is_none());
    // 3 ページ目に当たる続きでも、さらに条件を変えて続けられる（カーソルは有効のまま）。
    let p2b = env.page(0, &Req::new("scn", 1, SORT_G_ASC), p1.next.as_deref());
    assert_eq!(keys(&p2b.rows), ["30"]);
    assert!(p2b.next.is_some());
}

// --- SQL パリティ ------------------------------------------------------------

#[test]
fn concatenated_pages_equal_the_sql_order_by_with_primary_key_tiebreak() {
    let env = Env::build(&[0, 1, 2]);
    // 自己検査: `ORDER BY g` だけでは同点順が rowid のままで、キーセットの連結（末尾キー = PK）と
    // 異なる。つまり PK を明示した比較が実際に末尾キーの違いを観測できる。
    let keyset = flat(&env.pages(0, &Req::new("pkt", 3, SORT_G_ASC)));
    let g_only = env.oracle(0, "SELECT k, g, owner FROM pkt ORDER BY g ASC");
    assert_ne!(
        keyset, g_only,
        "fixture must make the PK tiebreak observable"
    );
    assert_eq!(keys(&keyset).len(), 9);

    let cases = [
        ("", "ORDER BY k ASC"),
        (SORT_G_ASC, "ORDER BY g ASC, k ASC"),
        (SORT_G_DESC, "ORDER BY g DESC, k ASC"),
        (SORT_G_DESC_KEY_DESC, "ORDER BY g DESC, k DESC"),
    ];
    for who in 0..3 {
        for (sort, order) in cases {
            let want = env.oracle(who, &format!("SELECT k, g, owner FROM pkt {order}"));
            assert_eq!(want.len(), 9);
            for limit in [1, 2, 3] {
                let got = flat(&env.pages(who, &Req::new("pkt", limit, sort)));
                assert_eq!(got, want, "who={who} sort={sort} limit={limit}");
            }
        }
        // PK なしの表: SQL では rowid を参照できないので、同点順（rowid 昇順）を `ORDER BY g` に任せる。
        for (sort, order) in [(SORT_G_ASC, "g ASC"), (SORT_G_DESC, "g DESC")] {
            let want = env.oracle(
                who,
                &format!("SELECT n, g, owner FROM nopk ORDER BY {order}"),
            );
            for limit in [1, 2, 3] {
                let got = flat(&env.pages(who, &Req::new("nopk", limit, sort)));
                assert_eq!(got, want, "nopk who={who} sort={sort} limit={limit}");
            }
        }
    }
}

#[test]
fn legacy_table_pages_equal_the_sql_order_by_id_tiebreak() {
    let env = Env::build_legacy();
    for who in 0..3 {
        for (sort, order) in [
            ("", "ORDER BY id ASC"),
            (SORT_G_ASC, "ORDER BY g ASC, id ASC"),
            (SORT_G_DESC, "ORDER BY g DESC, id ASC"),
        ] {
            let want = env.oracle(who, &format!("SELECT id, g, owner FROM ltab {order}"));
            assert_eq!(want.len(), 8);
            for limit in [1, 3, 5] {
                let got = flat(&env.pages(who, &Req::new("ltab", limit, sort)));
                assert_eq!(got, want, "who={who} sort={sort} limit={limit}");
            }
        }
    }
}

#[test]
fn each_page_equals_the_sql_where_comparison_with_limit_over_pg_wire() {
    let env = Env::build(&[0, 1, 2]);
    // `pkn` は g に NULL を含まない（比較式が NULL を落とさない形で書ける）。
    for (sort, order) in [("", "k"), (SORT_G_ASC, "g, k")] {
        for limit in [1usize, 2, 3] {
            let req = Req::new("pkn", limit, sort);
            let mut cursor: Option<String> = None;
            let mut last: Option<Row> = None;
            for _ in 0..MAX_PAGES {
                let page = env.page(0, &req, cursor.as_deref());
                let cond = match (&last, sort.is_empty()) {
                    (None, _) => String::new(),
                    (Some(r), true) => format!("WHERE k > {}", safe_literal(&r.key)),
                    (Some(r), false) => {
                        let g = safe_literal(r.g.as_deref().expect("non-null g"));
                        format!(
                            "WHERE g > '{g}' OR (g = '{g}' AND k > {})",
                            safe_literal(&r.key)
                        )
                    }
                };
                let sql =
                    format!("SELECT k, g, owner FROM pkn {cond} ORDER BY {order} LIMIT {limit}");
                assert_eq!(page.rows, env.sql_rows(0, &sql), "{sql}");
                last = page.rows.last().cloned();
                match page.next {
                    Some(c) => cursor = Some(c),
                    None => {
                        // 最後のページの後ろには何も残っていない。
                        let r = last.as_ref().expect("last page has rows");
                        let tail = if sort.is_empty() {
                            format!(
                                "SELECT k, g, owner FROM pkn WHERE k > {}",
                                safe_literal(&r.key)
                            )
                        } else {
                            let g = safe_literal(r.g.as_deref().expect("non-null g"));
                            format!(
                                "SELECT k, g, owner FROM pkn WHERE g > '{g}' OR (g = '{g}' AND k > {})",
                                safe_literal(&r.key)
                            )
                        };
                        assert!(env.sql_rows(0, &tail).is_empty(), "{tail}");
                        break;
                    }
                }
            }
        }
    }
}
