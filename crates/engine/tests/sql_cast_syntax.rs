//! `CAST(<expr> AS <type>)` と `<type> '<literal>'` の構文解析・束縛（対象ビヘイビア: SQL-47。
//! Issue #1590。規則表は `docs/design/cast-conversion-rules.md`）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → 束縛 → 評価）を
//! production 経路として検証する。固定する契約: 型の組が `×` のときの `42846`（値が NULL でも同じ）、
//! 恒等・型なしリテラル・型付き NULL の束縛と結果列の型、未知の型名の `42704`、値の変換が未実装の組の
//! `0A000`、定数式・CHECK 本体・UDF 名の従来の拒否の維持、テナント境界、エラー文に値が出ないこと。
//! 値の変換そのもの（`i`・`a`・`e`・`io/*`）は #1591／#1592 の担当でここでは固定しない。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::cast::{cast_rule, CastKind, CastRule};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
}

/// `MAX_EXPR_DEPTH`（32）のもとで受理される CAST の入れ子の最大段数（1 段が値式・加減算・乗除算の約 3 の深さを消費する）。
const MAX_CAST_NESTING: usize = 10;

const CREATE: &str = "CREATE TABLE c (embedding VECTOR(2) NOT NULL, s2 SMALLINT, i4 INTEGER, \
    i8 BIGINT, r REAL, d DOUBLE PRECISION, t TEXT, v VARCHAR(10), ch CHAR(3), dt DATE, \
    tm TIME, ts TIMESTAMP, tz TIMESTAMPTZ, iv INTERVAL, j JSON, jb JSONB)";

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    ok(&core, "alice", CREATE);
    for (id, tenant) in [(1, "alice"), (2, "bob")] {
        let sql = format!(
            "INSERT INTO c (id, embedding, s2, i4, i8, r, d, t, v, ch, dt, tm, ts, tz, iv, j, jb) \
             VALUES ({id}, '[0.1,0.2]', 1, 2, 3, 1.5, 2.5, 'abc', 'vv', 'ab', '2026-10-10', \
             '10:20:30', '2026-10-10 10:20:30', '2026-10-10 10:20:30+00', '1 day', \
             '{{\"a\":1}}', '{{\"a\":1}}') USING OPERATION_ID 'seed-{tenant}-{id}'"
        );
        ok(&core, tenant, &sql);
    }
    (core, guard)
}

fn select(core: &EngineCore, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    core.execute_sql(&ctx("alice"), sql)
}

/// 失敗を前提にした SQL の `wire_code`。成功してしまった場合は SQL を添えて落とす。
fn code(core: &EngineCore, sql: &str) -> &'static str {
    match run(core, "alice", sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn first(core: &EngineCore, expr: &str) -> (Cell, ColumnMeta) {
    let sql = format!("SELECT {expr} FROM c WHERE id = 1 LIMIT 1");
    let r = select(core, &sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
    assert_eq!(r.rows.len(), 1, "{sql}");
    (r.rows[0].cells[0].clone(), r.columns[0].clone())
}

fn meta_type(meta: &ColumnMeta) -> Option<ColumnType> {
    match meta {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => None,
    }
}

// --- 構文・恒等・結果列の型 ---

#[test]
fn identity_casts_keep_value_and_report_target_type() {
    let (core, _g) = setup("cast-identity");
    for (expr, ty) in [
        ("CAST(s2 AS smallint)", ColumnType::SmallInt),
        ("CAST(i4 AS integer)", ColumnType::Integer),
        ("CAST(i8 AS bigint)", ColumnType::BigInt),
        ("CAST(r AS real)", ColumnType::Real),
        ("CAST(d AS double precision)", ColumnType::Double),
        ("CAST(t AS text)", ColumnType::Text),
        ("CAST(v AS varchar)", ColumnType::Varchar(None)),
        ("CAST(dt AS date)", ColumnType::Date),
        ("CAST(tm AS time)", ColumnType::Time),
        ("CAST(ts AS timestamp)", ColumnType::Timestamp),
        ("CAST(tz AS timestamptz)", ColumnType::TimestampTz),
        ("CAST(iv AS interval)", ColumnType::Interval),
        ("CAST(jb AS jsonb)", ColumnType::Jsonb),
        ("CAST(j AS json)", ColumnType::Json),
        // 大小文字・別名・複数語の型名
        ("cast(i4 as INT4)", ColumnType::Integer),
        ("CAST(i8 AS int8)", ColumnType::BigInt),
        ("CAST(d AS float8)", ColumnType::Double),
        (
            "CAST(ts AS timestamp without time zone)",
            ColumnType::Timestamp,
        ),
        (
            "CAST(tz AS timestamp with time zone)",
            ColumnType::TimestampTz,
        ),
        ("CAST(v AS character varying)", ColumnType::Varchar(None)),
    ] {
        let (_, meta) = first(&core, expr);
        assert_eq!(meta_type(&meta), Some(ty), "{expr}");
    }
    // 値が変わらないこと
    assert_eq!(first(&core, "CAST(t AS text)").0, Cell::Text("abc".into()));
    assert_eq!(first(&core, "CAST(dt AS date)").0, first(&core, "dt").0);
    assert_eq!(first(&core, "CAST(i4 AS integer)").0, first(&core, "i4").0);
}

#[test]
fn integer_cast_keeps_integer_arithmetic_and_typed_null() {
    let (core, _g) = setup("cast-arith");
    let (cell, meta) = first(&core, "CAST(i4 AS integer) + 1");
    assert_eq!(meta_type(&meta), Some(ColumnType::Integer));
    assert!(matches!(cell, Cell::SignedInteger(3)), "{cell:?}");
    // 別名
    let sql = "SELECT CAST(i4 AS integer) AS m FROM c WHERE id = 1 LIMIT 1";
    let r = select(&core, sql).expect("alias");
    assert_eq!(meta_type(&r.columns[0]), Some(ColumnType::Integer));
    // 型付き NULL との算術は NULL（整数のまま）
    let (cell, meta) = first(&core, "CAST(NULL AS integer) + 1");
    assert_eq!(cell, Cell::Null);
    assert_eq!(meta_type(&meta), Some(ColumnType::Integer));
    // 既定の列名は PostgreSQL と同じ（被演算子が名前を持てばその名前、無ければ型のカタログ名）
    for (expr, name) in [
        ("CAST(i4 AS integer)", "i4"),
        ("CAST(NULL AS integer)", "int4"),
        ("CAST(NULL AS timestamptz)", "timestamptz"),
    ] {
        let sql = format!("SELECT {expr} FROM c WHERE id = 1 LIMIT 1");
        {
            let r = select(&core, &sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
            let got = match &r.columns[0] {
                ColumnMeta::Computed { name, .. } | ColumnMeta::Scalar { name, .. } => name.clone(),
                other => panic!("unexpected meta {other:?}"),
            };
            assert_eq!(got, name, "{expr}");
        }
    }
    // 型付き NULL の結果列の型は変換先
    for (expr, ty) in [
        ("CAST(NULL AS integer)", ColumnType::Integer),
        ("CAST(NULL AS bigint)", ColumnType::BigInt),
        ("CAST(NULL AS text)", ColumnType::Text),
        ("CAST(NULL AS date)", ColumnType::Date),
        ("CAST(NULL AS jsonb)", ColumnType::Jsonb),
    ] {
        let (cell, meta) = first(&core, expr);
        assert_eq!(cell, Cell::Null, "{expr}");
        assert_eq!(meta_type(&meta), Some(ty), "{expr}");
    }
}

#[test]
fn untyped_string_literals_are_read_as_the_target_type() {
    let (core, _g) = setup("cast-literal");
    let (cell, meta) = first(&core, "CAST('2026-10-10' AS date)");
    assert_eq!(cell, first(&core, "dt").0);
    assert_eq!(meta_type(&meta), Some(ColumnType::Date));
    let r = select(&core, "SELECT DATE '2026-10-10'").expect("date literal");
    assert_eq!(r.rows[0].cells[0], first(&core, "dt").0);
    assert_eq!(
        first(&core, "CAST('1 day' AS interval)").0,
        first(&core, "iv").0
    );
    assert_eq!(
        first(&core, "CAST('{\"a\": 1}' AS jsonb)").0,
        first(&core, "jb").0
    );
    let r = select(&core, "SELECT jsonb '{\"a\": 1}'").expect("jsonb literal");
    assert_eq!(r.rows[0].cells[0], first(&core, "jb").0);
    assert_eq!(
        first(&core, "CAST('abc' AS text)").0,
        Cell::Text("abc".into())
    );
    let r = select(&core, "SELECT text 'abc'").expect("text literal");
    assert_eq!(r.rows[0].cells[0], Cell::Text("abc".into()));
    // 不正な入力は入力関数のエラー（値はエラー文に出ない）
    assert_eq!(code(&core, "SELECT CAST('x' AS date)"), "22007");
    assert_eq!(code(&core, "SELECT CAST('2026-13-45' AS date)"), "22008");
    assert_eq!(code(&core, "SELECT CAST('{' AS jsonb)"), "22P02");
}

#[test]
fn where_clause_accepts_cast_operands() {
    let (core, _g) = setup("cast-where");
    for pred in [
        "CAST(dt AS date) = DATE '2026-10-10'",
        "CAST('2026-10-10' AS date) = dt",
        "CAST(i4 AS integer) = 2",
        "CAST(t AS text) = 'abc'",
    ] {
        let sql = format!("SELECT id FROM c WHERE {pred} LIMIT 10");
        let r = select(&core, &sql).unwrap_or_else(|e| panic!("{sql} failed: {e:?}"));
        assert_eq!(r.rows.len(), 1, "{sql}");
    }
}

#[test]
fn from_less_select_accepts_cast_and_typed_literals() {
    let (core, _g) = setup("cast-fromless");
    let r = select(&core, "SELECT CAST('2026-10-10' AS date)").expect("cast");
    assert_eq!(r.rows[0].cells[0], Cell::Date(20_736));
    let r = select(&core, "SELECT jsonb '{\"a\":1}'").expect("typed literal");
    assert_eq!(r.rows[0].cells[0], first(&core, "jb").0);
    assert_eq!(code(&core, "SELECT CAST(1 AS uuid)"), "42846");
    assert_eq!(code(&core, "SELECT CAST('x' AS date)"), "22007");
}

// --- 42846（型の組だけで決まる） ---

#[test]
fn incompatible_pairs_are_cannot_coerce_for_every_reachable_cell() {
    let (core, _g) = setup("cast-cells");
    let sources: [(&str, CastKind); 17] = [
        ("i4", CastKind::Int4),
        ("i8", CastKind::Int8),
        ("s2", CastKind::Int2),
        ("r", CastKind::Float4),
        ("d", CastKind::Float8),
        ("1.5", CastKind::Numeric),
        ("jb @> '{\"a\":1}'", CastKind::Bool),
        ("t", CastKind::Text),
        ("v", CastKind::Varchar),
        ("ch", CastKind::Bpchar),
        ("dt", CastKind::Date),
        ("tm", CastKind::Time),
        ("ts", CastKind::Timestamp),
        ("tz", CastKind::TimestampTz),
        ("iv", CastKind::Interval),
        ("j", CastKind::Json),
        ("jb", CastKind::Jsonb),
    ];
    let targets: [(&str, CastKind); 20] = [
        ("integer", CastKind::Int4),
        ("bigint", CastKind::Int8),
        ("smallint", CastKind::Int2),
        ("real", CastKind::Float4),
        ("double precision", CastKind::Float8),
        ("numeric(5,2)", CastKind::Numeric),
        ("boolean", CastKind::Bool),
        ("text", CastKind::Text),
        ("varchar", CastKind::Varchar),
        ("char(3)", CastKind::Bpchar),
        ("date", CastKind::Date),
        ("time", CastKind::Time),
        ("timestamp", CastKind::Timestamp),
        ("timestamptz", CastKind::TimestampTz),
        ("interval", CastKind::Interval),
        ("json", CastKind::Json),
        ("jsonb", CastKind::Jsonb),
        ("uuid", CastKind::Uuid),
        ("bytea", CastKind::Bytea),
        ("integer[]", CastKind::Array),
    ];
    for (src, from) in sources {
        for (ty, to) in targets {
            let sql = format!("SELECT CAST({src} AS {ty}) FROM c WHERE id = 1 LIMIT 1");
            let got = match select(&core, &sql) {
                Ok(_) => "ok",
                Err(e) => e.wire_code(),
            };
            let cell = format!(
                "{}->{} ({sql})",
                format!("{from:?}").to_ascii_lowercase(),
                format!("{to:?}").to_ascii_lowercase()
            );
            if cast_rule(from, to) == CastRule::None {
                assert_eq!(got, "42846", "{cell}");
            } else {
                assert_ne!(got, "42846", "{cell}");
            }
        }
    }
}

#[test]
fn cannot_coerce_message_names_only_types() {
    let (core, _g) = setup("cast-message");
    let e = select(&core, "SELECT CAST(i4 AS uuid) FROM c WHERE id = 1 LIMIT 1").unwrap_err();
    assert_eq!(e.wire_code(), "42846");
    assert_eq!(e.to_string(), "cannot cast type integer to uuid");
    // 値が NULL でも型の組だけで決まる
    assert_eq!(
        code(&core, "SELECT CAST(CAST(NULL AS date) AS integer)"),
        "42846"
    );
    let e = select(&core, "SELECT CAST(d AS boolean) FROM c LIMIT 1").unwrap_err();
    assert_eq!(
        e.to_string(),
        "cannot cast type double precision to boolean"
    );
}

// --- 未実装の組・未知の型 ---

#[test]
fn unimplemented_conversions_are_rejected_not_evaluated() {
    let (core, _g) = setup("cast-unimpl");
    for expr in [
        "CAST(i4 AS bigint)",
        "CAST(t AS integer)",
        "CAST('1' AS integer)",
        "CAST(i4 AS boolean)",
        "CAST(t AS varchar(3))",
        "CAST(v AS text)",
        "CAST(ch AS text)",
        "CAST(v AS varchar(3))",
        "CAST(i4 AS numeric(5,2))",
        "CAST('x' AS uuid)",
        "CAST(NULL AS uuid)",
        "CAST('{1}' AS integer[])",
        "CAST(abs(i4) AS integer)",
        "CAST(i4 + 1 AS text)",
        "CAST(id AS integer)",
        "CAST(abs(i4) AS date)",
    ] {
        let sql = format!("SELECT {expr} FROM c WHERE id = 1 LIMIT 1");
        let got = match select(&core, &sql) {
            Ok(_) => "ok",
            Err(e) => e.wire_code(),
        };
        // 種別不明の数値の date への変換は規則表の全数値行で `×`。
        let want = if expr == "CAST(abs(i4) AS date)" {
            "42846"
        } else {
            "0A000"
        };
        assert_eq!(got, want, "{expr}");
    }
}

#[test]
fn unknown_type_names_are_rejected_fail_closed() {
    let (core, _g) = setup("cast-unknown");
    assert_eq!(
        code(&core, "SELECT CAST(i4 AS nosuchtype) FROM c LIMIT 1"),
        "42704"
    );
    assert_eq!(
        code(&core, "SELECT CAST(i4 AS DOUBLE) FROM c LIMIT 1"),
        "42704"
    );
    // ENUM が実在してもカタログを引かずに `42704`（存在オラクルを作らない）
    ok(&core, "alice", "CREATE TYPE mood AS ENUM ('happy', 'sad')");
    assert_eq!(
        code(&core, "SELECT CAST(i4 AS mood) FROM c LIMIT 1"),
        "42704"
    );
    assert_eq!(
        code(&core, "SELECT CAST(i4 AS timetz) FROM c LIMIT 1"),
        "0A000"
    );
    assert_eq!(
        code(&core, "SELECT CAST(i4 AS vector(3)) FROM c LIMIT 1"),
        "0A000"
    );
}

// --- 構文エラー・上限 ---

#[test]
fn malformed_cast_syntax_is_a_syntax_error() {
    let (core, _g) = setup("cast-syntax");
    for sql in [
        "SELECT CAST(i4 integer) FROM c",
        "SELECT CAST(i4 AS) FROM c",
        "SELECT CAST(i4 AS integer FROM c",
        "SELECT CAST(i4) FROM c",
        "SELECT CAST() FROM c",
        "SELECT CAST(i4 AS integer, 1) FROM c",
        "SELECT int4[] '{1}'",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn nesting_depth_and_node_count_limits_apply_to_cast() {
    let (core, _g) = setup("cast-limits");
    let nest = |n: usize| {
        let mut e = "i4".to_string();
        for _ in 0..n {
            e = format!("CAST({e} AS integer)");
        }
        format!("SELECT {e} FROM c WHERE id = 1 LIMIT 1")
    };
    // 深さ上限（`MAX_EXPR_DEPTH`）の境界の両側。受理される最大段数 N があり、N+1 段は 54000。
    // CAST 1 段は丸括弧で包んだ部分式 1 段と同じ再帰（値式 → 加減算 → 乗除算 → 一次式）を消費する
    // ため、括弧の入れ子と最大段数が一致する（CAST が上限の計算に余分な深さを足さない）。
    let max_accepted = |build: &dyn Fn(usize) -> String| {
        let mut max_ok = 0;
        for n in 1..=60 {
            match select(&core, &build(n)) {
                Ok(_) => {
                    assert_eq!(max_ok, n - 1, "acceptance must be monotonic at {n}");
                    max_ok = n;
                }
                Err(e) => assert_eq!(e.wire_code(), "54000", "nest {n}"),
            }
        }
        max_ok
    };
    let nest_parens = |n: usize| {
        let mut e = "i4".to_string();
        for _ in 0..n {
            e = format!("({e})");
        }
        format!("SELECT {e} + 0 FROM c WHERE id = 1 LIMIT 1")
    };
    let nest_cast_plus = |n: usize| {
        let mut e = "i4".to_string();
        for _ in 0..n {
            e = format!("CAST({e} AS integer)");
        }
        format!("SELECT {e} + 0 FROM c WHERE id = 1 LIMIT 1")
    };
    let max_ok = max_accepted(&nest_cast_plus);
    assert_eq!(
        max_ok,
        max_accepted(&nest_parens),
        "CAST costs one paren level"
    );
    assert_eq!(max_ok, MAX_CAST_NESTING, "accepted CAST nesting");
    assert_eq!(code(&core, &nest(max_ok + 1)), "54000");
    // ノード数上限（1024）を超える CAST の連鎖
    let wide = (0..600)
        .map(|_| "CAST(i4 AS integer)")
        .collect::<Vec<_>>()
        .join(" + ");
    assert_eq!(
        code(&core, &format!("SELECT {wide} FROM c WHERE id = 1 LIMIT 1")),
        "54000"
    );
    let few = (0..20)
        .map(|_| "CAST(i4 AS integer)")
        .collect::<Vec<_>>()
        .join(" + ");
    assert!(select(&core, &format!("SELECT {few} FROM c WHERE id = 1 LIMIT 1")).is_ok());
}

// --- 従来の拒否の維持 ---

#[test]
fn legacy_rejections_are_kept() {
    let (core, _g) = setup("cast-legacy");
    // `::` は #1593 で CAST と同一の木へ脱糖される（`tests/sql_typecast_operator.rs`）。
    // 定数式（INSERT VALUES・DEFAULT）の CAST
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, embedding, i4) VALUES (9, '[0.1,0.2]', CAST(1 AS integer)) \
             USING OPERATION_ID 'cast-const'"
        ),
        "0A000"
    );
    // 列 DEFAULT
    assert_eq!(
        code(
            &core,
            "CREATE TABLE df (embedding VECTOR(2) NOT NULL, a INTEGER DEFAULT CAST(1 AS integer))"
        ),
        "0A000"
    );
    // CHECK 本体
    assert_eq!(
        code(
            &core,
            "CREATE TABLE ck (embedding VECTOR(2) NOT NULL, a INTEGER, CHECK (CAST(a AS integer) > 0))"
        ),
        "0A000"
    );
    // `cast` という UDF は登録できない
    assert_eq!(code(&core, "CREATE FUNCTION cast(x) AS x + 1"), "42723");
}

/// UDF 本体の CAST は定義時に受理され、呼び出し時に実引数の型で束縛される。本体の展開中は
/// スキーマを持たないため、整数列の実引数は種別不明の数値として扱い、数値への変換は `0A000`
/// で拒否する（誤った恒等・`42846` を作らない）。
#[test]
fn cast_in_function_body_is_validated_and_bound_at_call() {
    let (core, _g) = setup("cast-udf");
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("alice");
    let mut exec = |sql: &str| core.execute_sql_in_session(&c, &mut session, sql);
    exec("CREATE FUNCTION f(x) AS CAST(x AS text)").expect("define text cast");
    exec("CREATE FUNCTION g(x) AS CAST(x AS integer)").expect("define integer cast");
    exec("CREATE FUNCTION h(x) AS CAST(x AS date)").expect("define date cast");
    let rows = |out: SqlOutcome| match out {
        SqlOutcome::Query(r) => r,
        other => panic!("expected rows, got {other:?}"),
    };
    let r = rows(exec("SELECT f(t) FROM c WHERE id = 1 LIMIT 1").expect("text identity"));
    assert_eq!(r.rows[0].cells[0], Cell::Text("abc".into()));
    assert_eq!(
        exec("SELECT g(i4) FROM c WHERE id = 1 LIMIT 1")
            .unwrap_err()
            .wire_code(),
        "0A000"
    );
    assert_eq!(
        exec("SELECT h(i4) FROM c WHERE id = 1 LIMIT 1")
            .unwrap_err()
            .wire_code(),
        "42846"
    );
    // 未定義のパラメータ参照は定義時に拒否される
    assert!(exec("CREATE FUNCTION k(x) AS CAST(y AS text)").is_err());
}

/// 拡張クエリの `$n` は CAST の被演算子・比較の相手のどちらに置いても、型を推論せず構文段で
/// fail-closed に拒否される（`CAST($n AS t)` の型推論は未対応。誤った型を公告しない）。
#[test]
fn placeholders_around_cast_fail_closed_in_extended_query() {
    let (core, _g) = setup("cast-params");
    for sql in [
        "SELECT CAST($1 AS date) FROM c LIMIT 1",
        "SELECT id FROM c WHERE dt = CAST($1 AS date) LIMIT 1",
        "SELECT id FROM c WHERE CAST(i4 AS integer) = $1 LIMIT 1",
    ] {
        let err = core
            .parse_sql_prepared(sql)
            .expect_err("placeholder next to CAST must be rejected");
        assert!(
            ["42601", "42P18", "0A000"].contains(&err.wire_code()),
            "{sql}: {}",
            err.wire_code()
        );
    }
}

// --- テナント境界 ---

#[test]
fn cast_never_exposes_other_tenants_rows() {
    let (core, _g) = setup("cast-tenant");
    let r = select(
        &core,
        "SELECT CAST(t AS text) FROM c WHERE CAST(i4 AS integer) = 2 LIMIT 10",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    let bob = core
        .execute_sql(&ctx("carol"), "SELECT CAST(t AS text) FROM c LIMIT 10")
        .unwrap();
    assert!(bob.rows.is_empty());
    // エラー文に値・他テナントの情報を含めない
    let e = select(&core, "SELECT CAST(t AS uuid) FROM c LIMIT 1").unwrap_err();
    let msg = e.to_string();
    assert_eq!(e.wire_code(), "0A000");
    assert!(!msg.contains("abc"), "{msg}");
    let e = select(&core, "SELECT CAST(dt AS time) FROM c LIMIT 1").unwrap_err();
    assert_eq!(e.wire_code(), "42846");
    assert!(!e.to_string().contains("2026"), "{e}");
}
