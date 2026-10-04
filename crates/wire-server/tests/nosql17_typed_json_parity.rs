//! NoSQL 表層（`POST /v1/query`）の型付き JSON 束縛（NUMERIC・BOOLEAN・DATE・
//! TIMESTAMP・UUID・ARRAY）について、(a) HTTP `insert` から HTTP `scan` までの
//! 往復と (b) `aggregate` の結果が、同一 `Arc<EngineCore>` 上の SQL テキスト
//! 実行と**バイト単位で一致**することを固定する層 A 結合テスト（Issue #1203。
//! ポインタ: `docs/spec/04-behavior/nosql-surface.md` NOSQL-17・NOSQL-3〜7・
//! `docs/spec/04-behavior/sql-surface.md` SQL-13〜15・
//! `docs/spec/04-behavior/rls.md` RLS-11・`docs/spec/05-tasks.md` TASK-195）。
//!
//! 役割分担: `typed_json.rs` の単体テストは JSON 種別と列型の対応判定を、
//! `wire_integer_bigint_column.rs`・`wire_float_columns.rs` は INTEGER・BIGINT・
//! REAL の往復を、`nosql4_5_aggregate_wire_parity.rs` は旧来型（TEXT・id）の
//! 集計パリティをそれぞれ固定する。本ファイルは Issue #896 で広がった新型
//! 側の往復と集計パリティだけを扱う。
//!
//! オラクル方針: 主たる保証は SQL 表層とのバイト一致（`response::encode` を
//! 通した JSON 本文）とし、固定値は表層に依らない正規化規則（UUID の小文字化・
//! DATE/TIMESTAMP の文字列形・明示 `null`）に限る。
//!
//! RLS 注意: NoSQL `insert`・SQL `INSERT` はどちらも `Visibility::Private` で
//! 書き込むため、wire ログインの `PolicyContext`（`Public` ＋ 自テナント
//! `Private`。RLS-11・TASK-195）と同じ可視性の ctx を SQL オラクルにも使う。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
// `temp_db` は `http_common` が再エクスポートするため独自に `mod temp_db;`
// を宣言しない（`clippy::duplicate_mod` 回避）。
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::query::response::encode as encode_query_result;
use wire_server::http::session::store::SessionStore;

const NOSQL_TABLE: &str = "docs_nosql";
const SQL_TABLE: &str = "docs_sql";

/// 新型を全て含むスキーマ（`docs_nosql`・`docs_sql` で同一定義）。
fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("day", ColumnType::Date, true),
            ColumnDef::new("at", ColumnType::Timestamp, true),
            ColumnDef::new("ext", ColumnType::Uuid, true),
            ColumnDef::new(
                "tags",
                ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array type")),
                true,
            ),
            ColumnDef::new(
                "bits",
                ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 4).expect("array type")),
                true,
            ),
        ],
    )
}

/// seed 1 行分の値。`json` は NoSQL `insert` の行オブジェクト断片、`sql` は
/// SQL `INSERT` の `VALUES` 断片で、同じ値を表す。
struct SeedRow {
    id: u64,
    json: &'static str,
    /// SQL `INSERT` の列リスト（`id`・`embedding` 以降）。SQL 表層は `NULL`
    /// リテラルを受け付けないため、`NULL` 列は列リストから省略して表す。
    sql_cols: &'static str,
    sql: &'static str,
}

/// tenant-a の seed（4 行）。行 4 は nullable 新型列を全て明示 `null` にする。
/// NUMERIC は JSON 数値・数値文字列の双方を含む。
const SEED: [SeedRow; 4] = [
    SeedRow {
        id: 1,
        sql_cols: "lang, amount, flag, day, at, ext, tags, bits",
        json: r#""lang":"ja","amount":10.5,"flag":true,"day":"2024-02-29","at":"2024-02-29 12:34:56.5","ext":"12345678-9ABC-DEF0-1234-56789ABCDEF0","tags":["a","b c",""],"bits":[true,false]"#,
        sql: "'ja', 10.5, TRUE, '2024-02-29', '2024-02-29 12:34:56.5', \
              '12345678-9ABC-DEF0-1234-56789ABCDEF0', '{a,\"b c\",\"\"}', '{true,false}'",
    },
    SeedRow {
        id: 2,
        sql_cols: "lang, amount, flag, day, at, ext, tags, bits",
        json: r#""lang":"ja","amount":"2.25","flag":false,"day":"2023-01-01","at":"2023-01-01 00:00:00","ext":"00000000-0000-0000-0000-000000000001","tags":["x"],"bits":[true]"#,
        sql: "'ja', 2.25, FALSE, '2023-01-01', '2023-01-01 00:00:00', \
              '00000000-0000-0000-0000-000000000001', '{x}', '{true}'",
    },
    SeedRow {
        id: 3,
        sql_cols: "lang, amount, flag, day, at, ext, tags, bits",
        json: r#""lang":"en","amount":7,"flag":true,"day":"2025-12-31","at":"2025-12-31 23:59:59.25","ext":"abcdefab-cdef-abcd-efab-cdefabcdefab","tags":[],"bits":[]"#,
        sql: "'en', 7, TRUE, '2025-12-31', '2025-12-31 23:59:59.25', \
              'abcdefab-cdef-abcd-efab-cdefabcdefab', '{}', '{}'",
    },
    SeedRow {
        id: 4,
        sql_cols: "lang",
        json: r#""lang":"en","amount":null,"flag":null,"day":null,"at":null,"ext":null,"tags":null,"bits":null"#,
        sql: "'en'",
    },
];

const COLS: &str = "amount, flag, day, at, ext, tags, bits";

fn wire_scoped_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx (Public + own tenant Private, wire 既定)")
}

/// 2 テーブルを作成した空の engine を返す。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql17-typed-json-parity");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&schema(NOSQL_TABLE))
        .expect("create nosql table");
    storage
        .create_table(&schema(SQL_TABLE))
        .expect("create sql table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match engine::json::parse_json(&String::from_utf8_lossy(&resp.body))
        .expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post(addr: SocketAddr, token: &str, body: &str) -> HttpResponse {
    let auth_header = format!("Bearer {token}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// 1 要求ごとにログインし直して投げる（テナントはトークンからのみ決まる）。
fn query_as(addr: SocketAddr, user: &str, password: &str, body: &str) -> HttpResponse {
    let token = login(addr, user, password);
    post(addr, &token, body)
}

fn alice(addr: SocketAddr, body: &str) -> HttpResponse {
    query_as(addr, "alice", "pw-alice", body)
}

fn bob(addr: SocketAddr, body: &str) -> HttpResponse {
    query_as(addr, "bob", "pw-bob", body)
}

fn sql_oracle_body(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("oracle SQL should succeed: {sql:?}: {}", e.wire_code()));
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected SqlOutcome::Query for {sql:?}");
    };
    encode_query_result(&result).expect("oracle result should encode")
}

fn sql_exec(core: &EngineCore, tenant: &str, sql: &str) {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("SQL should succeed: {sql:?}: {}", e.wire_code()));
}

fn sql_oracle_err(core: &EngineCore, tenant: &str, sql: &str) -> String {
    let ctx = wire_scoped_ctx(tenant);
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .expect_err("oracle SQL should be rejected")
        .wire_code()
        .to_string()
}

fn body_utf8(resp: &HttpResponse) -> String {
    String::from_utf8(resp.body.clone()).expect("response body must be utf-8")
}

/// `docs_nosql` へ HTTP `insert`、`docs_sql` へ SQL `INSERT` で同じ seed を
/// alice（tenant-a）として投入する。
fn seed_both(core: &Arc<EngineCore>, addr: SocketAddr) {
    for row in &SEED {
        let body = format!(
            r#"{{"op":"insert","table":"{NOSQL_TABLE}","rows":[{{"id":{id},"embedding":[0.5,0.25],{json}}}],"operation_id":"seed-nosql-{id}"}}"#,
            id = row.id,
            json = row.json
        );
        let resp = alice(addr, &body);
        assert_eq!(resp.status, 200, "seed insert must succeed: {resp:?}");
        sql_exec(
            core,
            "tenant-a",
            &format!(
                "INSERT INTO {SQL_TABLE} (id, embedding, {cols}) VALUES \
                 ({id}, '[0.5,0.25]', {sql}) USING OPERATION_ID 'seed-sql-{id}'",
                id = row.id,
                cols = row.sql_cols,
                sql = row.sql
            ),
        );
    }
}

fn scan_body(table: &str) -> String {
    format!(
        r#"{{"op":"scan","table":"{table}","columns":["id","amount","flag","day","at","ext","tags","bits"],"sort":[{{"column":"id","dir":"asc"}}],"limit":100}}"#
    )
}

fn agg_body(table: &str, aggregates: &str, extra: &str) -> String {
    format!(r#"{{"op":"aggregate","table":"{table}","aggregates":[{aggregates}]{extra}}}"#)
}

/// 集計を `docs_nosql`（HTTP）・`docs_sql`（HTTP）・SQL オラクルの 3 者で
/// 比較し、`docs_nosql` の本文を返す。
fn assert_aggregate_parity(
    core: &EngineCore,
    addr: SocketAddr,
    aggregates: &str,
    extra: &str,
    sql_tmpl: &str,
) -> String {
    let resp = alice(addr, &agg_body(NOSQL_TABLE, aggregates, extra));
    assert_eq!(resp.status, 200, "aggregate must succeed: {resp:?}");
    let oracle = sql_oracle_body(core, "tenant-a", &sql_tmpl.replace("{T}", NOSQL_TABLE));
    assert_eq!(body_utf8(&resp), oracle, "HTTP vs SQL oracle: {sql_tmpl}");
    let resp_sql_table = alice(addr, &agg_body(SQL_TABLE, aggregates, extra));
    assert_eq!(
        body_utf8(&resp_sql_table),
        body_utf8(&resp),
        "NoSQL-inserted vs SQL-inserted table: {sql_tmpl}"
    );
    body_utf8(&resp)
}

// ------------------------------------------------- 指数表記（Issue #1358）

/// NUMERIC(10,2) 列への指数表記の JSON 数値・数値文字列が HTTP `insert` で受理され、
/// SQL `INSERT`（数値トークン・文字列リテラル）と同じ値で `scan` に現れる。桁数超過は
/// `22003`（HTTP 400）で行が増えない。
#[test]
fn numeric_exponent_json_number_matches_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    // (id, JSON の amount 断片, SQL の amount リテラル)
    let cases: [(u64, &str, &str); 4] = [
        (1, "1.5e2", "1.5e2"),
        (2, "-5E-3", "-5E-3"),
        (3, "\"2.5e1\"", "'2.5e1'"),
        (4, "1e+3", "1e+3"),
    ];
    for (id, json, sql) in cases {
        let body = format!(
            r#"{{"op":"insert","table":"{NOSQL_TABLE}","rows":[{{"id":{id},"embedding":[0.5,0.25],"lang":"ja","amount":{json}}}],"operation_id":"exp-nosql-{id}"}}"#
        );
        let resp = alice(addr, &body);
        assert_eq!(resp.status, 200, "exponent insert {json}: {resp:?}");
        sql_exec(
            &core,
            "tenant-a",
            &format!(
                "INSERT INTO {SQL_TABLE} (id, embedding, lang, amount) VALUES \
                 ({id}, '[0.5,0.25]', 'ja', {sql}) USING OPERATION_ID 'exp-sql-{id}'"
            ),
        );
    }
    let scan = format!(
        r#"{{"op":"scan","table":"{NOSQL_TABLE}","columns":["id","amount"],"sort":[{{"column":"id","dir":"asc"}}],"limit":100}}"#
    );
    let resp = alice(addr, &scan);
    assert_eq!(resp.status, 200, "{resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        &format!("SELECT id, amount FROM {SQL_TABLE} ORDER BY id ASC LIMIT 100"),
    );
    let body = body_utf8(&resp);
    assert_eq!(body, oracle);
    for expected in ["150.00", "-0.01", "25.00", "1000.00"] {
        assert!(body.contains(expected), "{expected} missing: {body}");
    }

    // NUMERIC(10,2) の整数部は 8 桁まで。1e8 は桁あふれ（22003）で行は増えない。
    let resp = alice(
        addr,
        &format!(
            r#"{{"op":"insert","table":"{NOSQL_TABLE}","rows":[{{"id":9,"embedding":[0.5,0.25],"lang":"ja","amount":1e8}}],"operation_id":"exp-nosql-9"}}"#
        ),
    );
    assert_eq!(resp.status, 400, "{resp:?}");
    assert!(body_utf8(&resp).contains("22003"), "{}", body_utf8(&resp));
    let resp = alice(
        addr,
        &agg_body(NOSQL_TABLE, r#"{"fn":"count","column":"*"}"#, ""),
    );
    assert!(body_utf8(&resp).contains("[[4]]"), "{}", body_utf8(&resp));
}

// ---------------------------------------------------------------- 往復

/// seed の自己検査: 4 行が両テーブルに入っていることを固定する。
#[test]
fn self_check_seed_row_counts() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    for t in [NOSQL_TABLE, SQL_TABLE] {
        let resp = alice(addr, &agg_body(t, r#"{"fn":"count","column":"*"}"#, ""));
        assert_eq!(resp.status, 200, "{resp:?}");
        assert!(body_utf8(&resp).contains("[[4]]"), "{}", body_utf8(&resp));
    }
}

/// HTTP `insert` → HTTP `scan` の本文が SQL `SELECT` とバイト一致し、
/// 正規化規則（UUID 小文字化・DATE/TIMESTAMP 形・配列・明示 null）が出る。
#[test]
fn insert_then_scan_matches_sql_select_byte_exact() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);

    let resp = alice(addr, &scan_body(NOSQL_TABLE));
    assert_eq!(resp.status, 200, "{resp:?}");
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        &format!("SELECT id, {COLS} FROM {NOSQL_TABLE} ORDER BY id ASC LIMIT 100"),
    );
    let body = body_utf8(&resp);
    assert_eq!(body, oracle);

    for needle in [
        r#""type":"numeric""#,
        r#""type":"boolean""#,
        r#""type":"date""#,
        r#""type":"timestamp""#,
        r#""type":"uuid""#,
        r#""type":"text[]""#,
        r#""type":"boolean[]""#,
        // UUID は小文字へ正規化される
        r#""12345678-9abc-def0-1234-56789abcdef0""#,
        r#""2024-02-29""#,
        r#""2024-02-29 12:34:56.5""#,
        r#"["a","b c",""]"#,
        "[true,false]",
        // 行 4: 明示 null
        "null,null,null,null,null,null,null",
    ] {
        assert!(body.contains(needle), "missing {needle}: {body}");
    }
}

/// NoSQL 束縛で書いたテーブルと SQL 束縛で書いたテーブルが、同じ scan 要求
/// に対して同一本文を返す（束縛経路の同一性）。
#[test]
fn nosql_insert_and_sql_insert_yield_identical_scan() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let a = alice(addr, &scan_body(NOSQL_TABLE));
    let b = alice(addr, &scan_body(SQL_TABLE));
    assert_eq!(a.status, 200, "{a:?}");
    assert_eq!(body_utf8(&a), body_utf8(&b));
}

/// 数値・日時・UUID 要素の配列列（`integer[]`・`bigint[]`・`double precision[]`・
/// `date[]`・`timestamp[]`・`uuid[]`）も HTTP `insert`→`scan` が SQL `INSERT`→
/// `SELECT` とバイト一致する。要素の NULL・空配列・列ごとの `null` を含む。
#[test]
fn numeric_temporal_uuid_element_arrays_round_trip_matches_sql() {
    const ARR_NOSQL: &str = "arr_nosql";
    const ARR_SQL: &str = "arr_sql";
    let arr = |elem| ColumnType::Array(ArrayType::new(elem, 4).expect("array type"));
    let arr_schema = |name: &str| {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("ints", arr(ArrayElemType::Integer), true),
                ColumnDef::new("bigs", arr(ArrayElemType::BigInt), true),
                ColumnDef::new("dbls", arr(ArrayElemType::Double), true),
                ColumnDef::new("days", arr(ArrayElemType::Date), true),
                ColumnDef::new("ats", arr(ArrayElemType::Timestamp), true),
                ColumnDef::new("uids", arr(ArrayElemType::Uuid), true),
            ],
        )
    };
    let path = temp_db::unique_db_path("nosql17-typed-json-arrays");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in [ARR_NOSQL, ARR_SQL] {
        storage.create_table(&arr_schema(t)).expect("create table");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addr = spawn(Arc::clone(&core));

    // (id, NoSQL 行断片, SQL 列リスト, SQL VALUES 断片)。行 3 は全配列列を `null`
    // にし、SQL 側は列リスト・VALUES を空にして省略で表す。
    let rows: [(u64, &str, &str, &str); 3] = [
        (
            1,
            r#""ints":[1,-2,null,2147483647],"bigs":[1234567890123],"dbls":[0.5,-1.25],"days":["2024-02-29",null],"ats":["2024-02-29 12:34:56.5"],"uids":["ABCDEFAB-CDEF-ABCD-EFAB-CDEFABCDEFAB"]"#,
            "ints, bigs, dbls, days, ats, uids",
            "'{1,-2,NULL,2147483647}', '{1234567890123}', '{0.5,-1.25}', \
             '{2024-02-29,NULL}', '{\"2024-02-29 12:34:56.5\"}', \
             '{ABCDEFAB-CDEF-ABCD-EFAB-CDEFABCDEFAB}'",
        ),
        (
            2,
            r#""ints":[],"bigs":[],"dbls":[],"days":[],"ats":[],"uids":[]"#,
            "ints, bigs, dbls, days, ats, uids",
            "'{}', '{}', '{}', '{}', '{}', '{}'",
        ),
        (
            3,
            r#""ints":null,"bigs":null,"dbls":null,"days":null,"ats":null,"uids":null"#,
            "",
            "",
        ),
    ];
    for (id, json, sql_cols, sql_vals) in rows {
        let body = format!(
            r#"{{"op":"insert","table":"{ARR_NOSQL}","rows":[{{"id":{id},"embedding":[0.5,0.25],{json}}}],"operation_id":"arr-nosql-{id}"}}"#
        );
        let resp = alice(addr, &body);
        assert_eq!(resp.status, 200, "array insert must succeed: {resp:?}");
        // SQL 表層は `NULL` リテラルを受け付けないため、行 3 は配列列を
        // 列リストから省略して NULL を表す（`SEED` 行 4 と同じ方針）。
        let sql = if sql_cols.is_empty() {
            format!(
                "INSERT INTO {ARR_SQL} (id, embedding) VALUES ({id}, '[0.5,0.25]') \
                 USING OPERATION_ID 'arr-sql-{id}'"
            )
        } else {
            format!(
                "INSERT INTO {ARR_SQL} (id, embedding, {sql_cols}) VALUES \
                 ({id}, '[0.5,0.25]', {sql_vals}) USING OPERATION_ID 'arr-sql-{id}'"
            )
        };
        sql_exec(&core, "tenant-a", &sql);
    }

    let scan = |t: &str| {
        format!(
            r#"{{"op":"scan","table":"{t}","columns":["id","ints","bigs","dbls","days","ats","uids"],"sort":[{{"column":"id","dir":"asc"}}],"limit":100}}"#
        )
    };
    let resp = alice(addr, &scan(ARR_NOSQL));
    assert_eq!(resp.status, 200, "{resp:?}");
    let body = body_utf8(&resp);
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        &format!(
            "SELECT id, ints, bigs, dbls, days, ats, uids FROM {ARR_NOSQL} \
             ORDER BY id ASC LIMIT 100"
        ),
    );
    assert_eq!(body, oracle, "HTTP scan vs SQL SELECT (NoSQL-inserted)");
    let resp_sql_table = alice(addr, &scan(ARR_SQL));
    assert_eq!(
        body_utf8(&resp_sql_table),
        body,
        "NoSQL-inserted vs SQL-inserted table"
    );
    for needle in [
        r#""type":"integer[]""#,
        r#""type":"bigint[]""#,
        r#""type":"double precision[]""#,
        r#""type":"date[]""#,
        r#""type":"timestamp[]""#,
        r#""type":"uuid[]""#,
        "[1,-2,null,2147483647]",
        "[1234567890123]",
        "[0.5,-1.25]",
        r#"["2024-02-29",null]"#,
        r#"["2024-02-29 12:34:56.5"]"#,
        // UUID 要素も小文字へ正規化される
        r#"["abcdefab-cdef-abcd-efab-cdefabcdefab"]"#,
        "[],[],[],[],[],[]",
        "null,null,null,null,null,null",
    ] {
        assert!(body.contains(needle), "missing {needle}: {body}");
    }
}

/// Issue #1357: `NUMERIC`・`BYTEA`・`JSON`・`JSONB`・`ENUM` 要素の配列列も HTTP
/// `insert`→`scan` が SQL `INSERT`→`SELECT` とバイト一致する（要素の NULL・空配列・
/// 列ごとの `null` を含む）。型不一致・base64 不正・語彙外・要素数超過の分類も固定する。
#[test]
fn numeric_bytea_json_enum_element_arrays_round_trip_matches_sql() {
    const X_NOSQL: &str = "xarr_nosql";
    const X_SQL: &str = "xarr_sql";
    let path = temp_db::unique_db_path("nosql17-typed-json-ext-arrays");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    let schema_of = |name: &str| {
        let arr = |elem| ColumnType::Array(ArrayType::new(elem, 4).expect("array type"));
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "nums",
                    arr(ArrayElemType::Numeric {
                        precision: 8,
                        scale: 2,
                    }),
                    true,
                ),
                ColumnDef::new("blobs", arr(ArrayElemType::Bytea), true),
                ColumnDef::new("js", arr(ArrayElemType::Json), true),
                ColumnDef::new("jb", arr(ArrayElemType::Jsonb), true),
                ColumnDef::new(
                    "moods",
                    ColumnType::Array(ArrayType::new_enum(mood.clone(), 4).expect("array type")),
                    true,
                ),
            ],
        )
    };
    for t in [X_NOSQL, X_SQL] {
        storage.create_table(&schema_of(t)).expect("create table");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let addr = spawn(Arc::clone(&core));

    let rows: [(u64, &str, &str, &str); 3] = [
        (
            1,
            r#""nums":[1.5,null,"2.25"],"blobs":["AQI=",null,""],"js":[{"b":1,"a":[1,2]},null,[3]],"jb":[{"b":1,"a":[1,2]},null],"moods":["happy",null,"sad"]"#,
            "nums, blobs, js, jb, moods",
            r#"'{1.5,NULL,2.25}', '{"\\x0102",NULL,"\\x"}', '{"{\"a\":[1,2],\"b\":1}",NULL,"[3]"}', '{"{\"a\":[1,2],\"b\":1}",NULL}', '{happy,NULL,sad}'"#,
        ),
        (
            2,
            r#""nums":[],"blobs":[],"js":[],"jb":[],"moods":[]"#,
            "nums, blobs, js, jb, moods",
            "'{}', '{}', '{}', '{}', '{}'",
        ),
        (
            3,
            r#""nums":null,"blobs":null,"js":null,"jb":null,"moods":null"#,
            "",
            "",
        ),
    ];
    for (id, json, sql_cols, sql_vals) in rows {
        let body = format!(
            r#"{{"op":"insert","table":"{X_NOSQL}","rows":[{{"id":{id},"embedding":[0.5,0.25],{json}}}],"operation_id":"xarr-nosql-{id}"}}"#
        );
        let resp = alice(addr, &body);
        assert_eq!(resp.status, 200, "array insert must succeed: {resp:?}");
        let sql = if sql_cols.is_empty() {
            format!(
                "INSERT INTO {X_SQL} (id, embedding) VALUES ({id}, '[0.5,0.25]') \
                 USING OPERATION_ID 'xarr-sql-{id}'"
            )
        } else {
            format!(
                "INSERT INTO {X_SQL} (id, embedding, {sql_cols}) VALUES \
                 ({id}, '[0.5,0.25]', {sql_vals}) USING OPERATION_ID 'xarr-sql-{id}'"
            )
        };
        sql_exec(&core, "tenant-a", &sql);
    }

    let scan = |t: &str| {
        format!(
            r#"{{"op":"scan","table":"{t}","columns":["id","nums","blobs","js","jb","moods"],"sort":[{{"column":"id","dir":"asc"}}],"limit":100}}"#
        )
    };
    let resp = alice(addr, &scan(X_NOSQL));
    assert_eq!(resp.status, 200, "{resp:?}");
    let body = body_utf8(&resp);
    let oracle = sql_oracle_body(
        &core,
        "tenant-a",
        &format!("SELECT id, nums, blobs, js, jb, moods FROM {X_NOSQL} ORDER BY id ASC LIMIT 100"),
    );
    assert_eq!(body, oracle, "HTTP scan vs SQL SELECT (NoSQL-inserted)");
    assert_eq!(
        body_utf8(&alice(addr, &scan(X_SQL))),
        body,
        "NoSQL-inserted vs SQL-inserted table"
    );
    for needle in [
        r#""type":"numeric[]""#,
        r#""type":"bytea[]""#,
        r#""type":"json[]""#,
        r#""type":"jsonb[]""#,
        r#""type":"enum[]""#,
        "[1.50,null,2.25]",
        r#"["AQI=",null,""]"#,
        // JSON／JSONB 要素は native JSON 値（キー昇順の正規形）で出力される。
        r#"[{"a":[1,2],"b":1},null,[3]]"#,
        r#"["happy",null,"sad"]"#,
        "[],[],[],[],[]",
        "null,null,null,null,null",
    ] {
        assert!(body.contains(needle), "missing {needle}: {body}");
    }

    // filter `eq` は `array_literal_text` 経由で配列列にも効く（行 1 だけに一致）。
    for filter in [
        r#"{"column":"moods","op":"eq","value":["happy",null,"sad"]}"#,
        r#"{"column":"nums","op":"eq","value":["1.50",null,2.25]}"#,
        r#"{"column":"blobs","op":"eq","value":["AQI=",null,""]}"#,
        r#"{"column":"js","op":"eq","value":[{"a":[1.0,2],"b":1},null,[3]]}"#,
    ] {
        let body = format!(
            r#"{{"op":"scan","table":"{X_NOSQL}","columns":["id"],"filter":[{filter}],"limit":10}}"#
        );
        let resp = alice(addr, &body);
        assert_eq!(resp.status, 200, "{filter}: {resp:?}");
        let text = body_utf8(&resp);
        assert!(text.contains("[[1]]"), "{filter}: {text}");
    }

    // 型不一致 42601・base64 不正 22P02・語彙外 22P02・要素数超過 54000。
    for (fragment, status_code) in [
        (r#""nums":[true]"#, "42601"),
        (r#""blobs":[1]"#, "42601"),
        (r#""blobs":["***"]"#, "22P02"),
        (r#""js":[1]"#, "42601"),
        (r#""moods":["ecstatic"]"#, "22P02"),
        (r#""moods":["happy","sad","happy","sad","happy"]"#, "54000"),
    ] {
        let body = format!(
            r#"{{"op":"insert","table":"{X_NOSQL}","rows":[{{"id":9,"embedding":[0.5,0.25],{fragment}}}],"operation_id":"xarr-bad"}}"#
        );
        let resp = alice(addr, &body);
        assert!(resp.status >= 400, "{fragment} must be rejected: {resp:?}");
        assert!(
            body_utf8(&resp).contains(status_code),
            "{fragment}: expected {status_code}, got {}",
            body_utf8(&resp)
        );
    }
}

// ------------------------------------------------------------ aggregate

#[test]
fn numeric_all_functions_match_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let body = assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"count","column":"amount"},{"fn":"sum","column":"amount"},
           {"fn":"avg","column":"amount"},{"fn":"min","column":"amount"},
           {"fn":"max","column":"amount"}"#,
        "",
        "SELECT COUNT(amount), SUM(amount), AVG(amount), MIN(amount), MAX(amount) FROM {T}",
    );
    // NULL 行は COUNT から除外される（4 行中 3 行）。SUM は 10.5 + 2.25 + 7。
    assert!(body.contains("[[3,"), "{body}");
    assert!(body.contains("19.75"), "{body}");
}

#[test]
fn date_timestamp_min_max_count_match_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let body = assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"count","column":"day"},{"fn":"min","column":"day"},{"fn":"max","column":"day"},
           {"fn":"count","column":"at"},{"fn":"min","column":"at"},{"fn":"max","column":"at"}"#,
        "",
        "SELECT COUNT(day), MIN(day), MAX(day), COUNT(at), MIN(at), MAX(at) FROM {T}",
    );
    assert!(body.contains(r#""2023-01-01""#), "{body}");
    assert!(body.contains(r#""2025-12-31 23:59:59.25""#), "{body}");
}

#[test]
fn boolean_uuid_array_count_match_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let body = assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"count","column":"*"},{"fn":"count","column":"flag"},
           {"fn":"count","column":"ext"},{"fn":"count","column":"tags"},
           {"fn":"count","column":"bits"}"#,
        "",
        "SELECT COUNT(*), COUNT(flag), COUNT(ext), COUNT(tags), COUNT(bits) FROM {T}",
    );
    // 明示 null 行の効果で COUNT(col) < COUNT(*)。
    assert!(body.contains("[[4,3,3,3,3]]"), "{body}");
}

#[test]
fn group_by_lang_with_new_type_aggregates_match_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"sum","column":"amount"},{"fn":"avg","column":"amount"},
           {"fn":"min","column":"day"},{"fn":"max","column":"at"},{"fn":"count","column":"ext"}"#,
        r#","group_by":["lang"]"#,
        "SELECT lang, SUM(amount), AVG(amount), MIN(day), MAX(at), COUNT(ext) \
         FROM {T} GROUP BY lang",
    );
}

/// BOOLEAN・DATE・UUID 列の `group_by`（NULL グループを含む）も SQL の
/// `GROUP BY` とバイト一致する。
#[test]
fn group_by_boolean_date_uuid_columns_match_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    for col in ["flag", "day", "ext"] {
        assert_aggregate_parity(
            &core,
            addr,
            r#"{"fn":"count","column":"*"}"#,
            &format!(r#","group_by":["{col}"]"#),
            &format!("SELECT {col}, COUNT(*) FROM {{T}} GROUP BY {col}"),
        );
    }
}

#[test]
fn filter_eq_on_new_type_lanes_matches_sql_where() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let aggs = r#"{"fn":"count","column":"*"},{"fn":"sum","column":"amount"}"#;
    // 3 要素目は期待一致件数。両表層がともに 0 件へ落ちる「空の一致」で
    // パリティが自明に成立するのを防ぐ（各レーンが実際に行を選ぶことを固定）。
    let cases: [(&str, &str, u32); 6] = [
        (
            r#"{"column":"flag","op":"eq","value":true}"#,
            "flag = TRUE",
            2,
        ),
        (
            r#"{"column":"day","op":"eq","value":"2024-02-29"}"#,
            "day = '2024-02-29'",
            1,
        ),
        (
            r#"{"column":"ext","op":"eq","value":"12345678-9ABC-DEF0-1234-56789ABCDEF0"}"#,
            "ext = '12345678-9abc-def0-1234-56789abcdef0'",
            1,
        ),
        (
            r#"{"column":"amount","op":"eq","value":7}"#,
            "amount = '7.00'",
            1,
        ),
        (
            r#"{"column":"amount","op":"eq","value":"2.25"}"#,
            "amount = '2.25'",
            1,
        ),
        (
            r#"{"column":"at","op":"eq","value":"2023-01-01 00:00:00"}"#,
            "at = '2023-01-01 00:00:00'",
            1,
        ),
    ];
    for (filter, where_sql, expected) in cases {
        let body = assert_aggregate_parity(
            &core,
            addr,
            aggs,
            &format!(r#","filter":[{filter}]"#),
            &format!("SELECT COUNT(*), SUM(amount) FROM {{T}} WHERE {where_sql}"),
        );
        assert!(
            body.contains(&format!("[[{expected},")),
            "{filter}: expected {expected} matching rows: {body}"
        );
    }
}

#[test]
fn having_count_on_group_matches_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    // `ja` は amount 非 NULL が 2 行、`en` は行 4 が NULL のため 1 行。
    // `COUNT(amount) >= 2` は `ja` だけを残し、HAVING が実際に効くことを固定する
    // （`COUNT(*)` だと両グループ 2 行で HAVING 無しと区別できない）。
    let body = assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"count","column":"amount"}"#,
        r#","group_by":["lang"],"having":[{"fn":"count","column":"amount","op":">=","value":2}]"#,
        "SELECT lang, COUNT(amount) FROM {T} GROUP BY lang HAVING count >= 2",
    );
    assert!(body.contains(r#"["ja",2]"#), "{body}");
    assert!(!body.contains(r#""en""#), "{body}");
}

#[test]
fn empty_set_contract_matches_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let body = assert_aggregate_parity(
        &core,
        addr,
        r#"{"fn":"count","column":"*"},{"fn":"sum","column":"amount"},
           {"fn":"min","column":"day"},{"fn":"max","column":"at"}"#,
        r#","filter":[{"column":"lang","op":"eq","value":"zz"}]"#,
        "SELECT COUNT(*), SUM(amount), MIN(day), MAX(at) FROM {T} WHERE lang = 'zz'",
    );
    assert!(body.contains("[0,null,null,null]"), "{body}");
}

// -------------------------------------------------------- エラー契約

/// 集計の拒否は SQL と同じ `wire_code` になる（非数値型への集計・DATE/TIMESTAMP への SUM/AVG は
/// `42883`。Issue #1186・#1349）。
#[test]
fn unsupported_aggregate_shapes_are_rejected_like_sql() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);

    let cases: [(&str, &str, &str); 7] = [
        (
            r#"{"fn":"sum","column":"day"}"#,
            "",
            "SELECT SUM(day) FROM {T}",
        ),
        (
            r#"{"fn":"avg","column":"at"}"#,
            "",
            "SELECT AVG(at) FROM {T}",
        ),
        (
            r#"{"fn":"min","column":"flag"}"#,
            "",
            "SELECT MIN(flag) FROM {T}",
        ),
        (
            r#"{"fn":"max","column":"ext"}"#,
            "",
            "SELECT MAX(ext) FROM {T}",
        ),
        (
            r#"{"fn":"sum","column":"tags"}"#,
            "",
            "SELECT SUM(tags) FROM {T}",
        ),
        (
            r#"{"fn":"min","column":"bits"}"#,
            "",
            "SELECT MIN(bits) FROM {T}",
        ),
        (
            r#"{"fn":"sum","column":"amount"}"#,
            r#","group_by":["lang"],"having":[{"fn":"sum","column":"amount","op":">","value":1}]"#,
            "SELECT lang, SUM(amount) FROM {T} GROUP BY lang HAVING sum > 1",
        ),
    ];
    for (agg, extra, sql) in cases {
        let resp = alice(addr, &agg_body(NOSQL_TABLE, agg, extra));
        let sql_code = sql_oracle_err(&core, "tenant-a", &sql.replace("{T}", NOSQL_TABLE));
        assert_eq!(
            http_common::wire_code_of(&resp),
            sql_code,
            "{agg} {extra}: {resp:?}"
        );
        // HAVING 付きの最終ケースは集計型ではなく HAVING 比較の拒否（従来どおり 22000）。
        let want = if extra.is_empty() { "42883" } else { "22000" };
        assert_eq!(sql_code, want, "{agg} {extra}: {sql_code}");
    }
}

fn count_rows(addr: SocketAddr, table: &str) -> String {
    body_utf8(&alice(
        addr,
        &agg_body(table, r#"{"fn":"count","column":"*"}"#, ""),
    ))
}

fn insert_one(addr: SocketAddr, fields: &str, op: &str) -> HttpResponse {
    alice(
        addr,
        &format!(
            r#"{{"op":"insert","table":"{NOSQL_TABLE}","rows":[{{"id":100,"embedding":[0.5,0.25],"lang":"ja",{fields}}}],"operation_id":"{op}"}}"#
        ),
    )
}

/// engine の束縛で拒否される値は SQL `INSERT` と同じ `wire_code` になり、
/// 拒否後も件数が変わらない（fail-closed・副作用ゼロ）。
#[test]
fn engine_rejected_values_match_sql_and_leave_no_rows() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let before = count_rows(addr, NOSQL_TABLE);

    let cases: [(&str, &str, &str); 3] = [
        (r#""amount":123456789012.5"#, "123456789012.5", "amount"),
        (r#""day":"2024-02-30""#, "'2024-02-30'", "day"),
        (r#""ext":"not-a-uuid""#, "'not-a-uuid'", "ext"),
    ];
    for (i, (json, sql_val, col)) in cases.into_iter().enumerate() {
        let resp = insert_one(addr, json, &format!("rej-{i}"));
        let sql_code = sql_oracle_err(
            &core,
            "tenant-a",
            &format!(
                "INSERT INTO {SQL_TABLE} (id, embedding, lang, {col}) VALUES \
                 (100, '[0.5,0.25]', 'ja', {sql_val}) USING OPERATION_ID 'rej-sql-{i}'"
            ),
        );
        assert_eq!(
            http_common::wire_code_of(&resp),
            sql_code,
            "{json}: {resp:?}"
        );
        assert!(resp.status >= 400, "{json}: {resp:?}");
        assert_eq!(count_rows(addr, NOSQL_TABLE), before, "{json}");
    }
}

/// NoSQL 固有の JSON 形状判定（`typed_json.rs`）の拒否は固定コードで、
/// 不正値をエラー本文へ echo せず、副作用も無い。
#[test]
fn json_shape_rejections_have_fixed_codes_and_no_side_effects() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);
    let before = count_rows(addr, NOSQL_TABLE);

    let cases: [(&str, &str, &str); 5] = [
        (r#""flag":"true-marker""#, "42601", "true-marker"),
        (r#""day":20240229"#, "42601", "20240229"),
        (r#""tags":"not-array-marker""#, "42601", "not-array-marker"),
        (r#""bits":["str-elem-marker"]"#, "42601", "str-elem-marker"),
        (
            r#""tags":["a","b","c","d","overflow-marker"]"#,
            "54000",
            "overflow-marker",
        ),
    ];
    for (i, (json, code, marker)) in cases.into_iter().enumerate() {
        let resp = insert_one(addr, json, &format!("shape-{i}"));
        assert_eq!(http_common::wire_code_of(&resp), code, "{json}: {resp:?}");
        http_common::assert_message_does_not_echo(&resp, marker);
        assert_eq!(count_rows(addr, NOSQL_TABLE), before, "{json}");
    }
}

// ------------------------------------------------------------- RLS 境界

/// bob（tenant-b）が極端値の行を書いても、alice の scan／集計は本文が
/// バイト単位で変わらず、bob の scan には alice の行が現れない。
#[test]
fn other_tenant_rows_never_leak_into_scan_or_aggregate() {
    let (core, _g) = new_core();
    let addr = spawn(Arc::clone(&core));
    seed_both(&core, addr);

    let agg = agg_body(
        NOSQL_TABLE,
        r#"{"fn":"count","column":"*"},{"fn":"sum","column":"amount"},
           {"fn":"min","column":"amount"},{"fn":"max","column":"amount"},
           {"fn":"min","column":"day"},{"fn":"max","column":"at"}"#,
        "",
    );
    let scan = scan_body(NOSQL_TABLE);
    let agg_before = body_utf8(&alice(addr, &agg));
    let scan_before = body_utf8(&alice(addr, &scan));

    let resp = bob(
        addr,
        &format!(
            r#"{{"op":"insert","table":"{NOSQL_TABLE}","rows":[{{"id":900,"embedding":[1.0,1.0],"lang":"zz","amount":99999999.99,"flag":true,"day":"1970-01-01","at":"2999-12-31 23:59:59","ext":"ffffffff-ffff-ffff-ffff-ffffffffffff","tags":["bob-only"],"bits":[true]}}],"operation_id":"bob-1"}}"#
        ),
    );
    assert_eq!(resp.status, 200, "{resp:?}");

    let agg_after = body_utf8(&alice(addr, &agg));
    let scan_after = body_utf8(&alice(addr, &scan));
    assert_eq!(agg_before, agg_after);
    assert_eq!(scan_before, scan_after);
    assert_eq!(
        agg_after,
        sql_oracle_body(
            &core,
            "tenant-a",
            &format!(
                "SELECT COUNT(*), SUM(amount), MIN(amount), MAX(amount), MIN(day), MAX(at) \
                 FROM {NOSQL_TABLE}"
            )
        )
    );

    let bob_scan = bob(addr, &scan);
    assert_eq!(bob_scan.status, 200, "{bob_scan:?}");
    let bob_body = body_utf8(&bob_scan);
    assert_eq!(
        bob_body,
        sql_oracle_body(
            &core,
            "tenant-b",
            &format!("SELECT id, {COLS} FROM {NOSQL_TABLE} ORDER BY id ASC LIMIT 100")
        )
    );
    assert!(bob_body.contains("bob-only"), "{bob_body}");
    assert!(!bob_body.contains("2024-02-29"), "{bob_body}");
    assert!(!bob_body.contains("12345678-9abc"), "{bob_body}");
}
