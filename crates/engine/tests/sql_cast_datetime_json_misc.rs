//! 日付時刻・JSON・UUID・BYTEA・配列の `CAST` の値変換（対象ビヘイビア: SQL-47。Issue #1592。
//! 規則表は `docs/design/cast-conversion-rules.md`）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を
//! production 経路として、規則表の残り 300 セル（新しい 10 行 × 20 列 ＋ 既存 10 行 × 新しい
//! 10 列）を `<変換元>-><変換先>` のセルキーで検証する。`×` は `42846`、`=` は値を変えない変換、
//! それ以外は値と結果列の型。あわせて、セッション TimeZone への追従・日付時刻の
//! `22007`／`22008`／`22015`・jsonb → 数値の `22023`／`22003`・UUID／BYTEA／配列の入力不正・
//! 要素型の無い配列の `0A000`・消費位置の fail-closed・テナント境界を固定する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnType, MAX_ARRAY_ELEMENTS};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::ArrayValue;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::cast::{cast_rule, CastKind, CastRule};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const UUID_TEXT: &str = "123e4567-e89b-12d3-a456-426614174000";

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    tenant: &str,
    s: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    run(core, tenant, &mut s, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

fn select_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let sql = if sql.contains(" FROM ") && !sql.contains("LIMIT") {
        format!("{sql} LIMIT 10")
    } else {
        sql.to_string()
    };
    core.execute_sql(&ctx(tenant), &sql)
}

const CREATE: &str = "CREATE TABLE c (embedding VECTOR(2) NOT NULL, s2 SMALLINT, i4 INTEGER, \
    i8 BIGINT, r REAL, d DOUBLE PRECISION, n NUMERIC(10,3), b BOOLEAN, dt DATE, tm TIME, \
    ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, js JSON, jb JSONB, uu UUID, bt BYTEA, \
    ai INTEGER[], at TEXT[], tx TEXT, jn JSONB)";

/// alice: id=1 は全型に値を持つ行、id=3 は全列 NULL の行。bob: id=2 は不正な文字列を持つ行。
fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(&core, "alice", CREATE);
    ok(
        &core,
        "alice",
        &format!(
            "INSERT INTO c (id, embedding, s2, i4, i8, r, d, n, b, dt, tm, ts, tz, iv, js, jb, \
             uu, bt, ai, at, tx, jn) VALUES (1, '[0.1,0.2]', 7, 42, 1234, 1.5, 2.5, 3.145, \
             true, '2026-10-10', '10:20:30', '2026-10-10 10:20:30', '2026-10-10 10:20:30+00', \
             '1 day 02:03:04', '{{\"a\": 1}}', '{{\"b\": 2, \"a\": 1}}', '{UUID_TEXT}', \
             '\\x616263', '{{1,NULL,3}}', '{{\"a b\",NULL,\"NULL\"}}', 'abc', '2.5') \
             USING OPERATION_ID 'seed-alice-1'"
        ),
    );
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding) VALUES (3, '[0.3,0.4]') USING OPERATION_ID 'seed-alice-3'",
    );
    ok(
        &core,
        "bob",
        "INSERT INTO c (id, embedding, tx, uu) VALUES (2, '[0.5,0.6]', 'bob-secret-value', \
         '00000000-0000-0000-0000-00000000b0b0') USING OPERATION_ID 'seed-bob-2'",
    );
    (core, guard)
}

fn query(core: &EngineCore, expr: &str, id: u64) -> Result<(Cell, ColumnMeta), SqlSurfaceError> {
    let sql = format!("SELECT {expr} FROM c WHERE id = {id} LIMIT 1");
    let r = select_as(core, "alice", &sql)?;
    assert_eq!(r.rows.len(), 1, "{sql}");
    Ok((r.rows[0].cells[0].clone(), r.columns[0].clone()))
}

fn first(core: &EngineCore, expr: &str) -> (Cell, ColumnMeta) {
    query(core, expr, 1).unwrap_or_else(|e| panic!("{expr} failed: {e:?}"))
}

fn value(core: &EngineCore, expr: &str) -> Cell {
    first(core, expr).0
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    match select_as(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn err_of(core: &EngineCore, expr: &str) -> SqlSurfaceError {
    let sql = format!("SELECT {expr} FROM c WHERE id = 1 LIMIT 1");
    match select_as(core, "alice", &sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

fn meta_type(meta: &ColumnMeta) -> Option<ColumnType> {
    match meta {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => None,
    }
}

fn int_array_ty() -> ColumnType {
    ColumnType::Array(ArrayType::new(ArrayElemType::Integer, MAX_ARRAY_ELEMENTS).expect("ty"))
}

// --- 20 型の見出し ---

/// 規則表の 20 型。`sql` は変換先としての型名、`ty` は変換結果の列型、`kind` は見出し。
struct Ty {
    name: &'static str,
    kind: CastKind,
    sql: &'static str,
    ty: ColumnType,
}

fn types() -> Vec<Ty> {
    let t = |name, kind, sql, ty| Ty {
        name,
        kind,
        sql,
        ty,
    };
    vec![
        t("int4", CastKind::Int4, "integer", ColumnType::Integer),
        t("int8", CastKind::Int8, "bigint", ColumnType::BigInt),
        t("int2", CastKind::Int2, "smallint", ColumnType::SmallInt),
        t("float4", CastKind::Float4, "real", ColumnType::Real),
        t(
            "float8",
            CastKind::Float8,
            "double precision",
            ColumnType::Double,
        ),
        t(
            "numeric",
            CastKind::Numeric,
            "numeric(12,3)",
            ColumnType::Numeric {
                precision: 12,
                scale: 3,
            },
        ),
        t("bool", CastKind::Bool, "boolean", ColumnType::Boolean),
        t("text", CastKind::Text, "text", ColumnType::Text),
        t(
            "varchar",
            CastKind::Varchar,
            "varchar(64)",
            ColumnType::Varchar(Some(64)),
        ),
        t(
            "bpchar",
            CastKind::Bpchar,
            "char(64)",
            ColumnType::Bpchar(64),
        ),
        t("date", CastKind::Date, "date", ColumnType::Date),
        t("time", CastKind::Time, "time", ColumnType::Time),
        t(
            "timestamp",
            CastKind::Timestamp,
            "timestamp",
            ColumnType::Timestamp,
        ),
        t(
            "timestamptz",
            CastKind::TimestampTz,
            "timestamptz",
            ColumnType::TimestampTz,
        ),
        t(
            "interval",
            CastKind::Interval,
            "interval",
            ColumnType::Interval,
        ),
        t("json", CastKind::Json, "json", ColumnType::Json),
        t("jsonb", CastKind::Jsonb, "jsonb", ColumnType::Jsonb),
        t("uuid", CastKind::Uuid, "uuid", ColumnType::Uuid),
        t("bytea", CastKind::Bytea, "bytea", ColumnType::Bytea),
        // 配列は要素型ごとに別の変換先になる。見出し `array` の変換先は integer[]。
        t("array", CastKind::Array, "integer[]", int_array_ty()),
    ]
}

fn pad(s: &str, n: usize) -> String {
    format!("{s:<n$}")
}

/// 変換元の 1 型の見え方。`expr` は id=1 の行で評価する式（NULL 行は id=3）。
struct Src {
    name: &'static str,
    kind: CastKind,
    /// 列の式。文字列型は「文字列型の式」を作る CAST。
    expr: String,
    /// 変換先の見出し → 値の文字列表現（文字列型への `io/a`）。
    text: &'static str,
}

fn new_sources() -> Vec<Src> {
    let s = |name, kind, expr: &str, text| Src {
        name,
        kind,
        expr: expr.to_string(),
        text,
    };
    vec![
        s("date", CastKind::Date, "dt", "2026-10-10"),
        s("time", CastKind::Time, "tm", "10:20:30"),
        s(
            "timestamp",
            CastKind::Timestamp,
            "ts",
            "2026-10-10 10:20:30",
        ),
        s(
            "timestamptz",
            CastKind::TimestampTz,
            "tz",
            "2026-10-10 10:20:30+00",
        ),
        s("interval", CastKind::Interval, "iv", "1 day 02:03:04"),
        s("json", CastKind::Json, "js", "{\"a\": 1}"),
        s("jsonb", CastKind::Jsonb, "jb", "{\"a\":1,\"b\":2}"),
        s("uuid", CastKind::Uuid, "uu", UUID_TEXT),
        s("bytea", CastKind::Bytea, "bt", "\\x616263"),
        s("array", CastKind::Array, "ai", "{1,NULL,3}"),
    ]
}

/// 文字列型の変換元（text／varchar／bpchar）。変換先ごとの有効な入力文字列を `lit` で埋める式。
fn text_source_expr(name: &str, lit: &str) -> String {
    match name {
        "text" => format!("CAST('{lit}' AS text)"),
        "varchar" => format!("CAST('{lit}' AS varchar(80))"),
        "bpchar" => format!("CAST('{lit}' AS char(80))"),
        other => panic!("unknown text source {other}"),
    }
}

/// 変換先（新しい 10 型）ごとの有効な入力文字列。
fn valid_input(dst: &str) -> &'static str {
    match dst {
        "date" => "2026-10-10",
        "time" => "10:20:30",
        "timestamp" => "2026-10-10 10:20:30",
        "timestamptz" => "2026-10-10 10:20:30+00",
        "interval" => "1 day 02:03:04",
        "json" => "{\"a\": 1}",
        "jsonb" => "{\"b\": 2, \"a\": 1}",
        "uuid" => "123E4567-E89B-12D3-A456-426614174000",
        "bytea" => "abc",
        "array" => "{1,NULL,3}",
        other => panic!("no valid input for {other}"),
    }
}

fn date_days() -> i32 {
    engine::datetime::parse_date("2026-10-10").expect("date")
}
fn time_micros() -> i64 {
    engine::datetime::parse_time("10:20:30").expect("time")
}
fn ts_micros() -> i64 {
    engine::datetime::parse_timestamp("2026-10-10 10:20:30").expect("timestamp")
}
fn interval_value() -> engine::datetime::Interval {
    engine::datetime::parse_interval("1 day 02:03:04").expect("interval")
}

/// 新しい 10 型のセルの値（列の見え方）。
fn cell_of(kind: CastKind) -> Cell {
    match kind {
        CastKind::Date => Cell::Date(date_days()),
        CastKind::Time => Cell::Time(time_micros()),
        CastKind::Timestamp => Cell::Timestamp(ts_micros()),
        CastKind::TimestampTz => Cell::TimestampTz(ts_micros()),
        CastKind::Interval => Cell::Interval(interval_value()),
        CastKind::Json => Cell::Json("{\"a\": 1}".to_string()),
        CastKind::Jsonb => Cell::Json("{\"a\":1,\"b\":2}".to_string()),
        CastKind::Uuid => Cell::Uuid(engine::uuid::parse_uuid_text(UUID_TEXT).expect("uuid")),
        CastKind::Bytea => Cell::Bytes(b"abc".to_vec()),
        other => panic!("not a new type: {other:?}"),
    }
}

/// 1 セルの期待。`None` は規則表で `×`（`42846`）。
enum Exp {
    Cannot,
    Value(Cell),
    Code(&'static str),
}

/// 新しい 10 行（変換元）× 20 列（変換先）の期待値。
fn expected_for_new_source(src: &Src, dst: &Ty) -> Exp {
    if cast_rule(src.kind, dst.kind) == CastRule::None {
        return Exp::Cannot;
    }
    match (src.kind, dst.kind) {
        // 文字列型への `io/a`
        (_, CastKind::Text | CastKind::Varchar) => Exp::Value(Cell::Text(src.text.to_string())),
        (_, CastKind::Bpchar) => Exp::Value(Cell::Text(pad(src.text, 64))),
        // 同一型は値を変えない（jsonb は正規化済み、配列は integer[] 同士）
        (CastKind::Date, CastKind::Date)
        | (CastKind::Time, CastKind::Time)
        | (CastKind::Timestamp, CastKind::Timestamp)
        | (CastKind::TimestampTz, CastKind::TimestampTz)
        | (CastKind::Interval, CastKind::Interval)
        | (CastKind::Json, CastKind::Json)
        | (CastKind::Uuid, CastKind::Uuid)
        | (CastKind::Bytea, CastKind::Bytea) => Exp::Value(cell_of(src.kind)),
        (CastKind::Jsonb, CastKind::Jsonb) => Exp::Value(cell_of(CastKind::Jsonb)),
        (CastKind::Array, CastKind::Array) => Exp::Value(Cell::Array(ArrayValue::Integer(vec![
            Some(1),
            None,
            Some(3),
        ]))),
        // 日付時刻間
        (CastKind::Date, CastKind::Timestamp | CastKind::TimestampTz) => {
            Exp::Value(match dst.kind {
                CastKind::Timestamp => Cell::Timestamp(i64::from(date_days()) * 86_400_000_000),
                _ => Cell::TimestampTz(i64::from(date_days()) * 86_400_000_000),
            })
        }
        (CastKind::Time, CastKind::Interval) => Exp::Value(Cell::Interval(
            engine::datetime::parse_interval("10:20:30").expect("interval"),
        )),
        (CastKind::Timestamp, CastKind::Date) | (CastKind::TimestampTz, CastKind::Date) => {
            Exp::Value(Cell::Date(date_days()))
        }
        (CastKind::Timestamp, CastKind::Time) | (CastKind::TimestampTz, CastKind::Time) => {
            Exp::Value(Cell::Time(time_micros()))
        }
        (CastKind::Timestamp, CastKind::TimestampTz) => Exp::Value(Cell::TimestampTz(ts_micros())),
        (CastKind::TimestampTz, CastKind::Timestamp) => Exp::Value(Cell::Timestamp(ts_micros())),
        (CastKind::Interval, CastKind::Time) => Exp::Value(Cell::Time(
            (2 * 3600 + 3 * 60 + 4) * 1_000_000, // 日の部分は捨てる
        )),
        // JSON
        (CastKind::Json, CastKind::Jsonb) => Exp::Value(Cell::Json("{\"a\":1}".to_string())),
        (CastKind::Jsonb, CastKind::Json) => {
            Exp::Value(Cell::Json("{\"a\":1,\"b\":2}".to_string()))
        }
        // 行 1 の jsonb はオブジェクトなので、スカラー型への `e` は 22023
        (
            CastKind::Jsonb,
            CastKind::Int4
            | CastKind::Int8
            | CastKind::Int2
            | CastKind::Float4
            | CastKind::Float8
            | CastKind::Numeric
            | CastKind::Bool,
        ) => Exp::Code("22023"),
        // 配列 → 配列（integer[] → integer[]）は上で処理済み
        other => panic!("unhandled cell {} -> {} ({other:?})", src.name, dst.name),
    }
}

/// 新しい 10 型へ変換する、既存の文字列型 3 行の期待値（`io/e`）。
fn expected_from_text(dst: &Ty) -> Exp {
    match dst.kind {
        CastKind::Date => Exp::Value(Cell::Date(date_days())),
        CastKind::Time => Exp::Value(Cell::Time(time_micros())),
        CastKind::Timestamp => Exp::Value(Cell::Timestamp(ts_micros())),
        CastKind::TimestampTz => Exp::Value(Cell::TimestampTz(ts_micros())),
        CastKind::Interval => Exp::Value(Cell::Interval(interval_value())),
        CastKind::Json => Exp::Value(Cell::Json("{\"a\": 1}".to_string())),
        CastKind::Jsonb => Exp::Value(Cell::Json("{\"a\":1,\"b\":2}".to_string())),
        CastKind::Uuid => Exp::Value(Cell::Uuid(
            engine::uuid::parse_uuid_text(UUID_TEXT).expect("uuid"),
        )),
        CastKind::Bytea => Exp::Value(Cell::Bytes(b"abc".to_vec())),
        CastKind::Array => Exp::Value(Cell::Array(ArrayValue::Integer(vec![
            Some(1),
            None,
            Some(3),
        ]))),
        other => panic!("not a new destination: {other:?}"),
    }
}

fn is_old(kind: CastKind) -> bool {
    matches!(
        kind,
        CastKind::Int4
            | CastKind::Int8
            | CastKind::Int2
            | CastKind::Float4
            | CastKind::Float8
            | CastKind::Numeric
            | CastKind::Bool
            | CastKind::Text
            | CastKind::Varchar
            | CastKind::Bpchar
    )
}

fn check(key: &str, core: &EngineCore, expr: &str, null_expr: &str, dst: &Ty, exp: Exp) {
    match exp {
        Exp::Cannot => {
            let e = query(core, expr, 1).expect_err(key);
            assert_eq!(e.wire_code(), "42846", "cell {key}");
            // `×` は値が NULL でも束縛時に確定する。
            let e = query(core, null_expr, 3).expect_err(key);
            assert_eq!(e.wire_code(), "42846", "cell {key} (NULL row)");
            assert!(
                e.to_string().starts_with("cannot cast type "),
                "cell {key}: {e}"
            );
        }
        Exp::Code(c) => {
            let e = query(core, expr, 1).expect_err(key);
            assert_eq!(e.wire_code(), c, "cell {key}: {e}");
            // NULL 行は NULL（実行時エラーは値に依存する）。
            let (cell, _) = query(core, null_expr, 3).unwrap_or_else(|e| panic!("{key}: {e:?}"));
            assert_eq!(cell, Cell::Null, "cell {key} NULL row");
        }
        Exp::Value(want) => {
            let (cell, meta) =
                query(core, expr, 1).unwrap_or_else(|e| panic!("cell {key}: {expr} failed: {e:?}"));
            assert_eq!(cell, want, "cell {key}");
            assert_eq!(
                meta_type(&meta),
                Some(dst.ty.clone()),
                "cell {key} column type"
            );
            let (null_cell, null_meta) = query(core, null_expr, 3)
                .unwrap_or_else(|e| panic!("cell {key} (NULL): {null_expr}: {e:?}"));
            assert_eq!(null_cell, Cell::Null, "cell {key} NULL propagation");
            assert_eq!(
                meta_type(&null_meta),
                Some(dst.ty.clone()),
                "cell {key} NULL type"
            );
        }
    }
}

#[test]
fn all_300_cells_of_the_datetime_json_uuid_bytea_array_block() {
    let (core, _g) = setup("cast-cells300");
    let all = types();
    let mut checked = 0;

    // (1) 新しい 10 行 × 20 列 = 200 セル
    for src in new_sources() {
        for dst in &all {
            let key = format!("{}->{}", src.name, dst.name);
            let expr = format!("CAST({} AS {})", src.expr, dst.sql);
            check(
                &key,
                &core,
                &expr,
                &expr,
                dst,
                expected_for_new_source(&src, dst),
            );
            checked += 1;
        }
    }

    // (2) 既存の数値 6 型・bool 7 行 × 新しい 10 列 = 70 セルは全て `×`
    for (name, expr) in [
        ("int4", "i4"),
        ("int8", "i8"),
        ("int2", "s2"),
        ("float4", "r"),
        ("float8", "d"),
        ("numeric", "n"),
        ("bool", "b"),
    ] {
        for dst in all.iter().filter(|t| !is_old(t.kind)) {
            let key = format!("{name}->{}", dst.name);
            let sql = format!("CAST({expr} AS {})", dst.sql);
            check(&key, &core, &sql, &sql, dst, Exp::Cannot);
            checked += 1;
        }
    }

    // (3) 文字列型 3 行 × 新しい 10 列 = 30 セルは `io/e`（文字列型の式からの入力関数）
    for name in ["text", "varchar", "bpchar"] {
        for dst in all.iter().filter(|t| !is_old(t.kind)) {
            let key = format!("{name}->{}", dst.name);
            let expr = format!(
                "CAST({} AS {})",
                text_source_expr(name, valid_input(dst.name)),
                dst.sql
            );
            let null_expr = format!(
                "CAST({} AS {})",
                match name {
                    "text" => "CAST(NULL AS text)",
                    "varchar" => "CAST(NULL AS varchar(80))",
                    _ => "CAST(NULL AS char(80))",
                },
                dst.sql
            );
            check(&key, &core, &expr, &null_expr, dst, expected_from_text(dst));
            checked += 1;
        }
    }

    assert_eq!(checked, 200 + 70 + 30);
}

/// 規則表の `×` が束縛時の判定で、実際の評価より前に出ることを、列の値が不正でも確認する。
#[test]
fn impossible_pairs_fail_before_any_value_is_read() {
    let (core, _g) = setup("cast-cannot");
    let e = err_of(&core, "CAST(CAST('abc' AS text) AS uuid)::integer");
    assert_eq!(e.wire_code(), "42846");
    assert_eq!(e.to_string(), "cannot cast type uuid to integer");
    let e = err_of(&core, "CAST(ai AS uuid[])");
    assert_eq!(e.wire_code(), "42846");
    assert_eq!(e.to_string(), "cannot cast type integer[] to uuid[]");
    let e = err_of(&core, "CAST(uu AS bytea)");
    assert_eq!(e.to_string(), "cannot cast type uuid to bytea");
}

// --- 日付時刻 ---

fn tz_session(core: &EngineCore, tz: &str) -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    run(core, "alice", &mut s, &format!("SET TIME ZONE '{tz}'")).expect("set time zone");
    s
}

fn tz_query(core: &EngineCore, s: &mut SessionState, expr: &str) -> Cell {
    let sql = format!("SELECT {expr} FROM c WHERE id = 1 LIMIT 1");
    match run(core, "alice", s, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
        SqlOutcome::Query(r) => r.rows[0].cells[0].clone(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn timestamptz_conversions_follow_the_session_time_zone() {
    let (core, _g) = setup("cast-tz");
    let mut s = tz_session(&core, "Asia/Tokyo");
    // timestamptz 2026-10-10 10:20:30 UTC は東京で 19:20:30
    assert_eq!(
        tz_query(&core, &mut s, "CAST(tz AS text)"),
        Cell::Text("2026-10-10 19:20:30+09".into())
    );
    assert_eq!(
        tz_query(&core, &mut s, "CAST(tz AS timestamp)"),
        Cell::Timestamp(ts_micros() + 9 * 3_600_000_000)
    );
    assert_eq!(
        tz_query(&core, &mut s, "CAST(tz AS time)"),
        Cell::Time(time_micros() + 9 * 3_600_000_000)
    );
    // 日付も局所時刻で決まる: 2026-10-10 15:00 UTC は東京で翌日 00:00
    ok(
        &core,
        "alice",
        "INSERT INTO c (id, embedding, tz, ts, dt) VALUES (7, '[0.1,0.2]', \
         '2026-10-10 15:00:00+00', '2026-10-10 00:00:00', '2026-10-10') \
         USING OPERATION_ID 'seed-alice-7'",
    );
    let q7 = |s: &mut SessionState, expr: &str| {
        let sql = format!("SELECT {expr} FROM c WHERE id = 7 LIMIT 1");
        match run(&core, "alice", s, &sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) {
            SqlOutcome::Query(r) => r.rows[0].cells[0].clone(),
            other => panic!("{sql}: {other:?}"),
        }
    };
    assert_eq!(q7(&mut s, "CAST(tz AS date)"), Cell::Date(date_days() + 1));
    // timestamp / date を timestamptz にするときは壁時計として解釈する（東京の 00:00 は前日 15:00 UTC）
    assert_eq!(
        q7(&mut s, "CAST(ts AS timestamptz)"),
        Cell::TimestampTz(i64::from(date_days()) * 86_400_000_000 - 9 * 3_600_000_000)
    );
    assert_eq!(
        q7(&mut s, "CAST(dt AS timestamptz)"),
        Cell::TimestampTz(i64::from(date_days()) * 86_400_000_000 - 9 * 3_600_000_000)
    );
    // 文字列 → timestamptz: ゾーンの無い入力は評価時のセッション TZ の壁時計
    assert_eq!(
        tz_query(
            &core,
            &mut s,
            "CAST(CAST('2026-10-10 10:20:30' AS text) AS timestamptz)"
        ),
        Cell::TimestampTz(ts_micros() - 9 * 3_600_000_000)
    );
    // UTC に戻すと結果が変わる（束縛時に固定されない）
    let mut utc = tz_session(&core, "UTC");
    assert_eq!(
        tz_query(
            &core,
            &mut utc,
            "CAST(CAST('2026-10-10 10:20:30' AS text) AS timestamptz)"
        ),
        Cell::TimestampTz(ts_micros())
    );
    assert_eq!(
        tz_query(&core, &mut utc, "CAST(tz AS text)"),
        Cell::Text("2026-10-10 10:20:30+00".into())
    );
}

#[test]
fn datetime_input_errors_use_the_documented_sqlstates() {
    let (core, _g) = setup("cast-dt-errors");
    for (lit, dst, want) in [
        ("abc", "date", "22007"),
        ("2026-13-45", "date", "22008"),
        ("25:00:00", "time", "22008"),
        ("xx", "time", "22007"),
        ("2026-02-30 10:00:00", "timestamp", "22008"),
        ("nonsense", "timestamptz", "22007"),
        ("2026-10-10 10:00:00 Nowhere/Land", "timestamptz", "22023"),
        ("99999999999999999999 years", "interval", "22015"),
        ("zzz", "interval", "22007"),
    ] {
        // 文字列型の式（評価時）と型なしリテラル（束縛時）で SQLSTATE が同じ。
        let by_expr = format!("CAST(CAST('{lit}' AS text) AS {dst})");
        let by_literal = format!("CAST('{lit}' AS {dst})");
        assert_eq!(err_of(&core, &by_expr).wire_code(), want, "{by_expr}");
        assert_eq!(err_of(&core, &by_literal).wire_code(), want, "{by_literal}");
    }
}

#[test]
fn time_and_interval_edge_conversions() {
    let (core, _g) = setup("cast-time-edge");
    // 負・24 時間超の時刻部分は 1 日の剰余。月・日は捨てる。
    for (lit, want) in [
        ("-1 hour", "23:00:00"),
        ("25 hours", "01:00:00"),
        ("2 days 03:04:05", "03:04:05"),
        ("1 month", "00:00:00"),
    ] {
        let sql = format!("CAST(CAST(CAST('{lit}' AS interval) AS time) AS text)");
        assert_eq!(value(&core, &sql), Cell::Text(want.into()), "{lit}");
    }
    // 24:00:00 は interval を経由しても 1 日の剰余で 00:00:00
    assert_eq!(
        value(
            &core,
            "CAST(CAST(CAST(CAST('24:00:00' AS time) AS interval) AS time) AS text)"
        ),
        Cell::Text("00:00:00".into())
    );
    // timestamp -> date は負の側へ丸める（エポック前）
    assert_eq!(
        value(
            &core,
            "CAST(CAST(CAST('1969-12-31 23:59:59' AS timestamp) AS date) AS text)"
        ),
        Cell::Text("1969-12-31".into())
    );
    assert_eq!(
        value(
            &core,
            "CAST(CAST(CAST('1969-12-31 23:59:59' AS timestamp) AS time) AS text)"
        ),
        Cell::Text("23:59:59".into())
    );
}

// --- JSON ---

#[test]
fn jsonb_to_scalar_follows_numeric_rounding_and_type_errors() {
    let (core, _g) = setup("cast-jsonb-scalar");
    // jn = 2.5（行 1）。numeric 経由で 0 から遠い側へ四捨五入する。
    assert_eq!(value(&core, "CAST(jn AS integer)"), Cell::SignedInteger(3));
    assert_eq!(value(&core, "CAST(jn AS bigint)"), Cell::SignedInteger(3));
    assert_eq!(value(&core, "CAST(jn AS smallint)"), Cell::SignedInteger(3));
    assert_eq!(
        value(&core, "CAST(jn AS double precision)"),
        Cell::Float(2.5)
    );
    assert_eq!(value(&core, "CAST(jn AS real)"), Cell::Float(2.5));
    assert!(
        matches!(value(&core, "CAST(jn AS numeric)"), Cell::Numeric(d) if d.to_string() == "2.5")
    );
    assert!(
        matches!(value(&core, "CAST(jn AS numeric(5,0))"), Cell::Numeric(d) if d.to_string() == "3")
    );
    assert_eq!(
        value(&core, "CAST(CAST('1.5' AS jsonb) AS integer)"),
        Cell::SignedInteger(2)
    );
    assert_eq!(
        value(&core, "CAST(CAST('-2.5' AS jsonb) AS integer)"),
        Cell::SignedInteger(-3)
    );
    assert_eq!(
        value(&core, "CAST(CAST('true' AS jsonb) AS boolean)"),
        Cell::Bool(true)
    );
    // 型が合わない JSON は 22023（値を含まない文言）
    for (lit, dst, msg) in [
        (
            "\"x\"",
            "integer",
            "cannot cast jsonb string to type integer",
        ),
        ("null", "integer", "cannot cast jsonb null to type integer"),
        ("[1]", "bigint", "cannot cast jsonb array to type bigint"),
        (
            "{\"a\":1}",
            "smallint",
            "cannot cast jsonb object to type smallint",
        ),
        (
            "true",
            "integer",
            "cannot cast jsonb boolean to type integer",
        ),
        ("1", "boolean", "cannot cast jsonb numeric to type boolean"),
        (
            "\"x\"",
            "numeric",
            "cannot cast jsonb string to type numeric",
        ),
        (
            "null",
            "double precision",
            "cannot cast jsonb null to type double precision",
        ),
        ("[]", "real", "cannot cast jsonb array to type real"),
    ] {
        let e = err_of(&core, &format!("CAST(CAST('{lit}' AS jsonb) AS {dst})"));
        assert_eq!(e.wire_code(), "22023", "{lit} -> {dst}");
        assert_eq!(e.to_string(), msg);
    }
    // 範囲外は 22003
    assert_eq!(
        err_of(&core, "CAST(CAST('99999999999' AS jsonb) AS integer)").wire_code(),
        "22003"
    );
    assert_eq!(
        err_of(&core, "CAST(CAST('40000' AS jsonb) AS smallint)").wire_code(),
        "22003"
    );
    // 浮動小数への変換は numeric を経由せず元テキストから直接解析する。
    assert_eq!(
        value(&core, "CAST(CAST('1e100' AS jsonb) AS double precision)"),
        Cell::Float(1e100)
    );
    assert_eq!(
        value(&core, "CAST(CAST('1e-100' AS jsonb) AS double precision)"),
        Cell::Float(1e-100)
    );
    // real は範囲外（オーバーフロー・アンダーフロー）を 22003 とする。
    for lit in ["1e100", "1e-100", "1e400"] {
        let dst = if lit == "1e400" {
            "double precision"
        } else {
            "real"
        };
        assert_eq!(
            err_of(&core, &format!("CAST(CAST('{lit}' AS jsonb) AS {dst})")).wire_code(),
            "22003",
            "{lit} -> {dst}"
        );
    }
    // 38 桁を超える数値は numeric に収まらない
    assert_eq!(
        err_of(&core, "CAST(CAST('1e400' AS jsonb) AS numeric)").wire_code(),
        "22003"
    );
}

#[test]
fn json_text_conversions() {
    let (core, _g) = setup("cast-json-text");
    // 文字列 -> json は原文を保持、-> jsonb は正規化する
    assert_eq!(
        value(&core, "CAST(CAST('{\"b\" : 2, \"a\":1}' AS text) AS json)"),
        Cell::Json("{\"b\" : 2, \"a\":1}".into())
    );
    assert_eq!(
        value(&core, "CAST(CAST('{\"b\" : 2, \"a\":1}' AS text) AS jsonb)"),
        Cell::Json("{\"a\":1,\"b\":2}".into())
    );
    // 型なしリテラルは束縛時に検証する
    assert_eq!(err_of(&core, "CAST('{' AS json)").wire_code(), "22P02");
    assert_eq!(
        err_of(&core, "CAST(CAST('{' AS text) AS jsonb)").wire_code(),
        "22P02"
    );
    // jsonb の null は文字列 null
    assert_eq!(
        value(&core, "CAST(CAST('null' AS jsonb) AS text)"),
        Cell::Text("null".into())
    );
    // json の重複キーは json -> jsonb で最後の値になる
    assert_eq!(
        value(
            &core,
            "CAST(CAST(CAST('{\"a\":1,\"a\":2}' AS json) AS jsonb) AS text)"
        ),
        Cell::Text("{\"a\":2}".into())
    );
}

// --- UUID・BYTEA ---

#[test]
fn uuid_conversions() {
    let (core, _g) = setup("cast-uuid");
    // 大文字入力は小文字の正規形で出る
    assert_eq!(
        value(
            &core,
            "CAST(CAST('123E4567-E89B-12D3-A456-426614174000' AS text) AS uuid)"
        ),
        Cell::Uuid(engine::uuid::parse_uuid_text(UUID_TEXT).unwrap())
    );
    assert_eq!(
        value(
            &core,
            "CAST(CAST(CAST('123E4567-E89B-12D3-A456-426614174000' AS text) AS uuid) AS text)"
        ),
        Cell::Text(UUID_TEXT.into())
    );
    // 厳密な 36 文字形以外は fail-closed で 22P02（PostgreSQL が受理する別表記も拒否）
    for bad in [
        "not-a-uuid",
        "{123e4567-e89b-12d3-a456-426614174000}",
        "123e4567e89b12d3a456426614174000",
        "123e4567-e89b-12d3-a456-42661417400",
        "",
    ] {
        let by_expr = format!("CAST(CAST('{bad}' AS text) AS uuid)");
        let e = err_of(&core, &by_expr);
        assert_eq!(e.wire_code(), "22P02", "{bad}");
        assert!(
            e.to_string().contains("invalid input syntax for type uuid"),
            "{e}"
        );
        assert_eq!(
            err_of(&core, &format!("CAST('{bad}' AS uuid)")).wire_code(),
            "22P02",
            "literal {bad}"
        );
    }
    // エラー文の値は先頭 64 文字まで
    let long = "x".repeat(200);
    let e = err_of(&core, &format!("CAST(CAST('{long}' AS text) AS uuid)"));
    assert!(e.to_string().len() < 150, "{e}");
}

#[test]
fn bytea_conversions() {
    let (core, _g) = setup("cast-bytea");
    // text -> bytea は PostgreSQL の入力関数（escape 形式）
    assert_eq!(
        value(&core, "CAST(CAST('abc' AS text) AS bytea)"),
        Cell::Bytes(b"abc".to_vec())
    );
    assert_eq!(
        value(&core, "CAST(CAST(CAST('abc' AS text) AS bytea) AS text)"),
        Cell::Text("\\x616263".into())
    );
    assert_eq!(
        value(&core, "CAST(CAST('\\x00ff' AS text) AS bytea)"),
        Cell::Bytes(vec![0x00, 0xff])
    );
    assert_eq!(
        value(&core, "CAST(CAST('\\101\\\\' AS text) AS bytea)"),
        Cell::Bytes(vec![0x41, 0x5c])
    );
    // 16 進の桁不正は 22023、escape の不正は 22P02
    let e = err_of(&core, "CAST(CAST('\\xabc' AS text) AS bytea)");
    assert_eq!(e.wire_code(), "22023");
    assert_eq!(
        e.to_string(),
        "invalid hexadecimal data: odd number of digits"
    );
    let e = err_of(&core, "CAST(CAST('\\xzz' AS text) AS bytea)");
    assert_eq!(e.wire_code(), "22023");
    assert_eq!(e.to_string(), "invalid hexadecimal digit");
    assert_eq!(
        err_of(&core, "CAST(CAST('a\\q' AS text) AS bytea)").wire_code(),
        "22P02"
    );
    // 型なしリテラルは束縛時に同じ入力関数
    assert_eq!(err_of(&core, "CAST('\\xzz' AS bytea)").wire_code(), "22023");
    // 列の bytea はそのまま文字列化できる
    assert_eq!(
        value(&core, "CAST(bt AS text)"),
        Cell::Text("\\x616263".into())
    );
}

// --- 配列 ---

#[test]
fn array_conversions() {
    let (core, _g) = setup("cast-array");
    // 配列 -> 文字列型（NULL 要素・引用）
    assert_eq!(
        value(&core, "CAST(ai AS text)"),
        Cell::Text("{1,NULL,3}".into())
    );
    assert_eq!(
        value(&core, "CAST(at AS text)"),
        Cell::Text("{\"a b\",NULL,\"NULL\"}".into())
    );
    // 文字列 -> 配列は往復できる
    assert_eq!(
        value(&core, "CAST(CAST(at AS text) AS text[])"),
        Cell::Array(ArrayValue::Text(vec![
            Some("a b".into()),
            None,
            Some("NULL".into())
        ]))
    );
    // 配列 -> 配列は要素ごとに変換し、NULL 要素は NULL のまま
    assert_eq!(
        value(&core, "CAST(ai AS bigint[])"),
        Cell::Array(ArrayValue::BigInt(vec![Some(1), None, Some(3)]))
    );
    assert_eq!(
        value(&core, "CAST(ai AS double precision[])"),
        Cell::Array(ArrayValue::Double(vec![Some(1.0), None, Some(3.0)]))
    );
    assert_eq!(
        value(&core, "CAST(CAST(ai AS bigint[]) AS integer[])"),
        Cell::Array(ArrayValue::Integer(vec![Some(1), None, Some(3)]))
    );
    assert_eq!(
        value(&core, "CAST(ai AS text[])"),
        Cell::Array(ArrayValue::Text(vec![
            Some("1".into()),
            None,
            Some("3".into())
        ]))
    );
    // 文字列配列 -> 整数配列。要素のエラーは要素型の SQLSTATE のまま
    assert_eq!(
        value(&core, "CAST(CAST('{\"7\",8}' AS text[]) AS integer[])"),
        Cell::Array(ArrayValue::Integer(vec![Some(7), Some(8)]))
    );
    assert_eq!(
        err_of(&core, "CAST(CAST('{1,x}' AS text[]) AS integer[])").wire_code(),
        "22P02"
    );
    assert_eq!(
        err_of(
            &core,
            "CAST(CAST('{99999999999}' AS bigint[]) AS integer[])"
        )
        .wire_code(),
        "22003"
    );
    // 配列リテラルの不正は 22P02、要素数の上限超過は 54000
    for bad in ["1,2", "{1,", "{1,{2}}", "[1:2]={1,2}"] {
        assert_eq!(
            err_of(&core, &format!("CAST(CAST('{bad}' AS text) AS integer[])")).wire_code(),
            "22P02",
            "{bad}"
        );
    }
    let many = format!(
        "{{{}}}",
        vec!["1"; MAX_ARRAY_ELEMENTS as usize + 1].join(",")
    );
    assert_eq!(
        err_of(&core, &format!("CAST(CAST('{many}' AS text) AS integer[])")).wire_code(),
        "54000"
    );
    // 型なしリテラルは束縛時に検証する
    assert_eq!(
        err_of(&core, "CAST('{1,x}' AS integer[])").wire_code(),
        "22P02"
    );
    assert_eq!(
        value(&core, "CAST('{1,2}' AS integer[])"),
        Cell::Array(ArrayValue::Integer(vec![Some(1), Some(2)]))
    );
    // 同じ要素型は値を変えない
    let (cell, meta) = first(&core, "CAST(ai AS integer[])");
    assert_eq!(
        cell,
        Cell::Array(ArrayValue::Integer(vec![Some(1), None, Some(3)]))
    );
    assert_eq!(meta_type(&meta), Some(int_array_ty()));
    // 型付き NULL と NULL 行
    let (cell, meta) = first(&core, "CAST(NULL AS integer[])");
    assert_eq!(cell, Cell::Null);
    assert_eq!(meta_type(&meta), Some(int_array_ty()));
    let (cell, _) = query(&core, "CAST(ai AS bigint[])", 3).unwrap();
    assert_eq!(cell, Cell::Null);
}

#[test]
fn arrays_without_a_representable_element_type_are_not_implemented() {
    let (core, _g) = setup("cast-array-unsupported");
    for target in [
        "smallint[]",
        "time[]",
        "timestamptz[]",
        "interval[]",
        "character varying(3)[]",
        "character(3)[]",
        "character varying[]",
    ] {
        // 文字列型・型付き NULL は常に 0A000。整数配列は要素の組（int4 -> 変換先の要素型）が
        // 規則表で `×` なら 42846 が先に出る（time／timestamptz／interval）。
        for operand in ["ai", "CAST('{1}' AS text)", "NULL"] {
            let e = err_of(&core, &format!("CAST({operand} AS {target})"));
            let elem_is_impossible =
                operand == "ai" && ["time[]", "timestamptz[]", "interval[]"].contains(&target);
            let want = if elem_is_impossible { "42846" } else { "0A000" };
            assert_eq!(e.wire_code(), want, "{operand} -> {target}");
        }
    }
    // 規則表の `×` は要素型が未実装でも 42846 が先
    assert_eq!(err_of(&core, "CAST(ai AS uuid[])").wire_code(), "42846");
    assert_eq!(err_of(&core, "CAST(i4 AS integer[])").wire_code(), "42846");
    assert_eq!(err_of(&core, "CAST(ai AS integer)").wire_code(), "42846");
}

// --- 消費位置・テナント境界 ---

#[test]
fn cast_results_are_only_projected_or_cast_again() {
    let (core, _g) = setup("cast-positions");
    for sql in [
        "SELECT CAST(uu AS uuid) = CAST(uu AS uuid) FROM c WHERE id = 1",
        "SELECT id FROM c WHERE CAST(uu AS uuid) = CAST(uu AS uuid)",
        "SELECT id FROM c ORDER BY CAST(uu AS uuid) LIMIT 10",
        "SELECT COALESCE(CAST(bt AS bytea), CAST(bt AS bytea)) FROM c WHERE id = 1",
        "SELECT CASE WHEN true THEN CAST(ai AS bigint[]) END FROM c WHERE id = 1",
        "SELECT lower(CAST(uu AS uuid)) FROM c WHERE id = 1",
        "SELECT CAST(ai AS bigint[]) || CAST(ai AS bigint[]) FROM c WHERE id = 1",
        "SELECT jsonb_build_array(CAST(uu AS uuid)) FROM c WHERE id = 1",
        "SELECT COUNT(CAST(uu AS uuid)) FROM c",
    ] {
        let c = code_of(&core, sql);
        assert!(
            ["0A000", "42883", "42804", "42601"].contains(&c),
            "{sql}: {c}"
        );
    }
    // 定数式の投影（FROM なし）では、比較・COALESCE・関数引数は 0A000 で拒否する
    let lit = "CAST('123e4567-e89b-12d3-a456-426614174000' AS uuid)";
    for sql in [
        format!("SELECT {lit} = {lit}"),
        format!("SELECT COALESCE({lit}, {lit})"),
        format!("SELECT lower({lit})"),
        format!("SELECT CASE WHEN true THEN {lit} END"),
        "SELECT CAST('{1}' AS integer[]) = CAST('{1}' AS integer[])".to_string(),
        "SELECT COALESCE(CAST('abc' AS bytea), CAST('abc' AS bytea))".to_string(),
    ] {
        let c = code_of(&core, &sql);
        assert!(
            ["0A000", "42883", "42804", "42601"].contains(&c),
            "{sql}: {c}"
        );
    }
    // 投影と再変換は通る
    let r = select_as(&core, "alice", &format!("SELECT {lit}")).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0].cells[0],
        Cell::Uuid(engine::uuid::parse_uuid_text(UUID_TEXT).unwrap())
    );
    // 列の裸の参照は従来どおり式に置けない
    for sql in [
        "SELECT id FROM c WHERE uu = uu LIMIT 10",
        "SELECT lower(uu) FROM c WHERE id = 1",
        "SELECT length(bt) FROM c WHERE id = 1",
    ] {
        let c = code_of(&core, sql);
        assert!(
            ["22000", "0A000", "42601", "42883"].contains(&c),
            "{sql}: {c}"
        );
    }
    // CHECK 本体・定数式での値の変換は 0A000
    let mut s = SessionState::default();
    s.allow_ddl();
    let e = run(
        &core,
        "alice",
        &mut s,
        "ALTER TABLE c ADD CONSTRAINT chk CHECK (CAST(CAST(uu AS text) AS uuid) IS NOT NULL)",
    );
    assert!(e.is_err());
    let e = run(
        &core,
        "alice",
        &mut s,
        "INSERT INTO c (id, embedding, uu) VALUES (9, '[0.1,0.2]', CAST('123e4567-e89b-12d3-a456-426614174000' AS uuid)) \
         USING OPERATION_ID 'x9'",
    )
    .expect_err("CAST in VALUES");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn cast_never_reads_or_reveals_other_tenants_values() {
    let (core, _g) = setup("cast-tenant");
    // bob の行（不正な uuid 文字列 `tx`）は alice の評価対象にならず、エラーにならない
    let r = select_as(&core, "alice", "SELECT CAST(uu AS text) FROM c LIMIT 10").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert!(r.rows.iter().all(|row| match &row.cells[0] {
        Cell::Text(t) => !t.contains("b0b0"),
        Cell::Null => true,
        _ => false,
    }));
    // alice 自身の可視行の入力不正だけがエラー文に現れ、bob の値は現れない
    let e = select_as(&core, "alice", "SELECT CAST(tx AS uuid) FROM c LIMIT 10").unwrap_err();
    assert_eq!(e.wire_code(), "22P02");
    let msg = e.to_string();
    assert!(msg.contains("abc"), "{msg}");
    assert!(!msg.contains("bob-secret"), "{msg}");
    // 他テナントから見える行が無ければ評価も起きない
    let r = select_as(&core, "carol", "SELECT CAST(tx AS uuid) FROM c LIMIT 10").unwrap();
    assert!(r.rows.is_empty());
    // `×` の文言は型名だけ
    let e = err_of(&core, "CAST(uu AS integer)");
    assert!(!e.to_string().contains("123e"), "{e}");
}

/// ENUM 要素の配列は要素型を CAST の型で表せないため、文字列型への変換だけが通り、配列への
/// 変換は未実装（`0A000`）、規則表で `×` の組は `42846`。
#[test]
fn enum_element_arrays_can_only_be_rendered_as_text() {
    let (core, _g) = setup("cast-enum-array");
    ok(&core, "alice", "CREATE TYPE mood AS ENUM ('happy', 'sad')");
    ok(
        &core,
        "alice",
        "CREATE TABLE e (embedding VECTOR(2) NOT NULL, m mood[])",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO e (id, embedding, m) VALUES (1, '[0.1,0.2]', '{happy,sad}') \
         USING OPERATION_ID 'seed-enum-1'",
    );
    let q = |expr: &str| {
        select_as(
            &core,
            "alice",
            &format!("SELECT {expr} FROM e WHERE id = 1 LIMIT 1"),
        )
    };
    assert_eq!(
        q("CAST(m AS text)").unwrap().rows[0].cells[0],
        Cell::Text("{happy,sad}".into())
    );
    assert_eq!(q("CAST(m AS text[])").unwrap_err().wire_code(), "0A000");
    assert_eq!(q("CAST(m AS integer)").unwrap_err().wire_code(), "42846");
}
