//! `x::T` 演算子（対象ビヘイビア: SQL-47。Issue #1593）の結合テスト。
//!
//! `::` は構文段で `CAST(x AS T)` と同一の `Expr::Cast` へ脱糖されるため、どの文脈でも
//! 成功時は出力全体（行・列型・既定列名）が、失敗時は SQLSTATE が `CAST` 形と一致することを
//! 表駆動で固定する。あわせて PG の優先順位（`::` は単項 `+`/`-`・`*` `/`・`+` `-`・`COLLATE`・
//! `AT TIME ZONE`・比較より強く左結合）、負リテラルとの結合、WHERE 構造葉との共存、
//! `$n::T` の型推論・位置検証、連鎖の上限、テナント境界を `EngineCore` の production 経路で確かめる。
//! 値の変換そのもの（`i`・`a`・`e`・`io/*`）は #1591／#1592 の担当でここでは固定しない。

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::params::PreparedParamType;
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

const CREATE: &str = "CREATE TABLE c (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, s2 SMALLINT, i4 INTEGER, \
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

/// 成功時は出力全体の Debug 表現、失敗時は SQLSTATE。CAST 形との同値比較に使う。
fn outcome(core: &EngineCore, sql: &str) -> Result<String, &'static str> {
    match run(core, "alice", sql) {
        Ok(o) => Ok(format!("{o:?}")),
        Err(e) => Err(e.wire_code()),
    }
}

/// `CAST(op AS ty)` と `op::ty` の結果が一致すること。`{}` は何度現れてもよい。
fn assert_equivalent(core: &EngineCore, template: &str, op: &str, ty: &str) {
    let cast = template.replace("{}", &format!("CAST({op} AS {ty})"));
    // 括弧付き被演算子（ORDER BY 先頭の `(` は式キーとして未対応のため対象外。NULL は列参照互換の
    // ため直後が `::` のときだけ型付き NULL になる）
    let paren = if template.contains("ORDER BY") || op == "NULL" {
        template.replace("{}", &format!("CAST({op} AS {ty})"))
    } else {
        template.replace("{}", &format!("({op})::{ty}"))
    };
    let bare = template.replace("{}", &format!("{op}::{ty}"));
    // 同じ operation-id の再送は重複扱いになるため、形ごとに別の ID を使う。
    let a = outcome(core, &cast.replace('@', "cast"));
    assert_eq!(
        a,
        outcome(core, &paren.replace('@', "paren")),
        "{cast} vs {paren}"
    );
    assert_eq!(
        a,
        outcome(core, &bare.replace('@', "bare")),
        "{cast} vs {bare}"
    );
}

/// (被演算子, 型名)。恒等・型なしリテラル・型付き NULL・42846・0A000・未知の型・多語の型・配列・
/// `numeric` の修飾なしを含む。
const OPERANDS_TYPES: &[(&str, &str)] = &[
    ("i4", "integer"),
    ("i4", "int4"),
    ("t", "text"),
    ("v", "varchar"),
    ("v", "character varying(5)"),
    ("d", "double precision"),
    ("tz", "timestamp with time zone"),
    ("'2026-10-10'", "date"),
    ("'1 day'", "interval"),
    ("'{\"a\":1}'", "jsonb"),
    ("'x'", "text"),
    ("NULL", "int"),
    ("dt", "integer"),
    ("i4", "bigint"),
    ("'1'", "int"),
    ("i4", "no_such_type"),
    ("'{1,2}'", "int[]"),
    ("i4", "numeric"),
    ("j", "json"),
];

#[test]
fn typecast_matches_cast_in_projection_contexts() {
    let (core, _g) = setup("typecast-equiv-proj");
    for (op, ty) in OPERANDS_TYPES {
        for template in [
            "SELECT {} FROM c WHERE id = 1 LIMIT 1",
            "SELECT {}, i4 FROM c WHERE id = 1 LIMIT 1",
            "SELECT i4, {} FROM c WHERE id = 1 LIMIT 1",
            "SELECT {} AS x FROM c WHERE id = 1 LIMIT 1",
        ] {
            assert_equivalent(&core, template, op, ty);
        }
    }
}

#[test]
fn typecast_matches_cast_in_predicate_order_and_update_contexts() {
    let (core, _g) = setup("typecast-equiv-pred");
    for (op, ty) in OPERANDS_TYPES {
        for template in [
            "SELECT id FROM c WHERE {} IS NULL LIMIT 1",
            "SELECT id FROM c WHERE {} = {} LIMIT 1",
            "SELECT id FROM c ORDER BY {} LIMIT 1",
            "UPDATE c SET i4 = {} WHERE id = 1 USING OPERATION_ID 'op-@'",
        ] {
            assert_equivalent(&core, template, op, ty);
        }
    }
}

#[test]
fn typecast_matches_cast_around_aggregates_and_function_heads() {
    // 集計呼び出しの直後の `::` は CAST と同じく未対応（42601）で、両者の結果が一致する。
    let (core, _g) = setup("typecast-equiv-agg");
    for ty in ["bigint", "integer", "text", "no_such_type"] {
        let pairs = [
            (
                format!("SELECT CAST(count(*) AS {ty}) FROM c"),
                format!("SELECT count(*)::{ty} FROM c"),
            ),
            (
                format!("SELECT CAST(i4 AS {ty}) + sum(i8) FROM c GROUP BY i4"),
                format!("SELECT i4::{ty} + sum(i8) FROM c GROUP BY i4"),
            ),
            (
                format!("SELECT CAST(lower(t) AS {ty}) FROM c WHERE id = 1 LIMIT 1"),
                format!("SELECT lower(t)::{ty} FROM c WHERE id = 1 LIMIT 1"),
            ),
        ];
        for (cast, sugar) in pairs {
            assert_eq!(outcome(&core, &cast), outcome(&core, &sugar), "{sugar}");
        }
    }
}

#[test]
fn typecast_chain_is_left_associative() {
    let (core, _g) = setup("typecast-chain");
    let a = outcome(
        &core,
        "SELECT CAST(CAST(t AS text) AS varchar) FROM c WHERE id = 1 LIMIT 1",
    );
    let b = outcome(&core, "SELECT t::text::varchar FROM c WHERE id = 1 LIMIT 1");
    assert_eq!(a, b);
}

#[test]
fn typecast_binds_tighter_than_unary_minus_and_arithmetic() {
    let (core, _g) = setup("typecast-prec");
    for (sugar, cast) in [
        ("-i4::integer", "-CAST(i4 AS integer)"),
        ("-1::int", "-CAST(1 AS int)"),
        ("2 * 3::int", "2 * CAST(3 AS int)"),
        ("1 + 2::int", "1 + CAST(2 AS int)"),
        ("t || 1::text", "t || CAST(1 AS text)"),
        ("t::text COLLATE \"C\"", "CAST(t AS text) COLLATE \"C\""),
        (
            "ts AT TIME ZONE 'UTC'::text",
            "ts AT TIME ZONE CAST('UTC' AS text)",
        ),
        ("i4::bigint + 1", "CAST(i4 AS bigint) + 1"),
        ("jb -> 'a'::text", "jb -> CAST('a' AS text)"),
        ("-i4::bigint", "-CAST(i4 AS bigint)"),
    ] {
        let a = outcome(&core, &format!("SELECT {cast} FROM c WHERE id = 1"));
        let b = outcome(&core, &format!("SELECT {sugar} FROM c WHERE id = 1"));
        assert_eq!(a, b, "{sugar}");
    }
    // 括弧付きの負リテラルは従来どおり畳まれ、`(-1)::int` になる。
    let a = outcome(&core, "SELECT CAST(-1 AS int) FROM c WHERE id = 1");
    let b = outcome(&core, "SELECT (-1)::int FROM c WHERE id = 1");
    assert_eq!(a, b);
    // `-2147483648::int` は `-(2147483648::int)`（畳み込まない）。
    let a = outcome(&core, "SELECT -CAST(2147483648 AS int) FROM c WHERE id = 1");
    let b = outcome(&core, "SELECT -2147483648::int FROM c WHERE id = 1");
    assert_eq!(a, b);
}

#[test]
fn string_literal_typecast_in_where_is_an_expression_predicate() {
    let (core, _g) = setup("typecast-where-leaf");
    for (sugar, cast) in [
        ("id = '1'::bigint", "id = CAST('1' AS bigint)"),
        ("t = 'abc'::text", "t = CAST('abc' AS text)"),
        ("t < 'zzz'::text", "t < CAST('zzz' AS text)"),
        ("(i4 + 1)::bigint > 1", "CAST(i4 + 1 AS bigint) > 1"),
        ("i4::bigint = 2", "CAST(i4 AS bigint) = 2"),
    ] {
        let a = outcome(&core, &format!("SELECT id FROM c WHERE {cast} LIMIT 5"));
        let b = outcome(&core, &format!("SELECT id FROM c WHERE {sugar} LIMIT 5"));
        assert_eq!(a, b, "{sugar}");
    }
    // boolean 語の直後の `::` は BoolEquality にせず式として解析する（CAST 形式と同じ結果）。
    for (sugar, cast) in [
        ("id = true::bool", "id = CAST(true AS bool)"),
        ("id = FALSE::boolean", "id = CAST(FALSE AS boolean)"),
    ] {
        let a = outcome(&core, &format!("SELECT id FROM c WHERE {cast} LIMIT 5"));
        let b = outcome(&core, &format!("SELECT id FROM c WHERE {sugar} LIMIT 5"));
        assert_eq!(a, b, "{sugar}");
        assert_ne!(b, Err("42601"), "{sugar}");
    }
    // IN／BETWEEN／LIKE の中の `::` は受理しない（fail-closed）。
    for sql in [
        "SELECT id FROM c WHERE t IN ('a'::text) LIMIT 5",
        "SELECT id FROM c WHERE t LIKE 'a'::text LIMIT 5",
        "SELECT id FROM c WHERE t BETWEEN 'a'::text AND 'z' LIMIT 5",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn typecast_is_rejected_where_cast_is_rejected() {
    let (core, _g) = setup("typecast-legacy");
    for (sugar, cast) in [
        (
            "INSERT INTO c (id, embedding, i4) VALUES (9, '[0.1,0.2]', 1::integer) \
             USING OPERATION_ID 'k1'",
            "INSERT INTO c (id, embedding, i4) VALUES (9, '[0.1,0.2]', CAST(1 AS integer)) \
             USING OPERATION_ID 'k1'",
        ),
        (
            "CREATE TABLE df (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, a INTEGER DEFAULT 1::integer)",
            "CREATE TABLE df (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, a INTEGER DEFAULT CAST(1 AS integer))",
        ),
        (
            "CREATE TABLE ck (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, a INTEGER, CHECK (a::integer > 0))",
            "CREATE TABLE ck (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, a INTEGER, \
             CHECK (CAST(a AS integer) > 0))",
        ),
        (
            "SELECT id FROM c WHERE flag::bool LIMIT 1",
            "SELECT id FROM c WHERE CAST(flag AS bool) LIMIT 1",
        ),
    ] {
        assert_eq!(code(&core, sugar), code(&core, cast), "{sugar}");
    }
}

#[test]
fn typecast_chain_depth_is_bounded() {
    let (core, _g) = setup("typecast-depth");
    let chain = |n: usize| format!("SELECT t{} FROM c WHERE id = 1 LIMIT 1", "::text".repeat(n));
    assert!(outcome(&core, &chain(5)).is_ok());
    assert_eq!(code(&core, &chain(40)), "54000");
    assert_eq!(code(&core, &chain(2000)), "54000");
}

#[test]
fn typecast_view_and_tenant_isolation() {
    let (core, _g) = setup("typecast-view");
    // ビュー本文での扱いも CAST 形と一致する（現状の受理可否は CAST に従う）。
    let a = code_or_ok(
        &core,
        "CREATE VIEW va AS SELECT CAST(i4 AS text) AS s FROM c LIMIT 10",
    );
    let b = code_or_ok(
        &core,
        "CREATE VIEW vb AS SELECT i4::text AS s FROM c LIMIT 10",
    );
    assert_eq!(a, b);
    // bob の行は alice から見えない（`::` を含む式でも自テナントの行だけ）。
    let r = select(&core, "SELECT id, t::text FROM c LIMIT 10").expect("select");
    assert_eq!(r.rows.len(), 1);
    // 42846 のメッセージに値は出ない。
    let err = run(
        &core,
        "alice",
        "SELECT dt::integer FROM c WHERE id = 1 LIMIT 1",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "42846");
    assert!(!err.to_string().contains("2026"), "{err}");
}

#[test]
fn typecast_and_cast_share_operation_id_content() {
    let (core, _g) = setup("typecast-opid");
    let first = run(
        &core,
        "alice",
        "UPDATE c SET t = t::text WHERE id = 1 USING OPERATION_ID 'same-op'",
    );
    let second = run(
        &core,
        "alice",
        "UPDATE c SET t = CAST(t AS text) WHERE id = 1 USING OPERATION_ID 'same-op'",
    );
    // 内容ハッシュが同一なら再送は「内容不一致（22023）」ではなく重複（23505）として扱われる。
    assert!(first.is_ok(), "{first:?}");
    assert_eq!(second.unwrap_err().wire_code(), "23505");
}

fn code_or_ok(core: &EngineCore, sql: &str) -> Result<(), &'static str> {
    run(core, "alice", sql)
        .map(|_| ())
        .map_err(|e| e.wire_code())
}

fn parse_code(core: &EngineCore, sql: &str) -> String {
    core.parse_sql_prepared(sql)
        .map(|_| panic!("{sql} must fail to parse"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

#[test]
fn param_placeholder_with_typecast_infers_target_type() {
    let (core, _g) = setup("typecast-param");
    for (sql, text) in [
        (
            "UPDATE c SET i4 = $1::integer WHERE id = 1 USING OPERATION_ID $2",
            false,
        ),
        (
            "UPDATE c SET i4 = CAST($1 AS integer) WHERE id = 1 USING OPERATION_ID $2",
            false,
        ),
        (
            "UPDATE c SET t = $1::text WHERE id = 1 USING OPERATION_ID $2",
            true,
        ),
    ] {
        let p = core.parse_sql_prepared(sql).expect("parse");
        match (p.param_types().first(), text) {
            (Some(PreparedParamType::Text), true) => {}
            (
                Some(PreparedParamType::Column(ColumnMeta::Scalar {
                    ty: ColumnType::Integer,
                    ..
                })),
                false,
            ) => {}
            (other, _) => panic!("{sql}: unexpected {other:?}"),
        }
    }
    // 推論対象外の型は不定（42P18）。
    assert_eq!(
        parse_code(
            &core,
            "UPDATE c SET dt = $1::date WHERE id = 1 USING OPERATION_ID $2"
        ),
        "42P18"
    );
    // 数値間の変換は未実装のため、smallint・bigint・real・double precision 宛ての `$n` も
    // 不定（42P18）に留める（置換後のリテラルが int4／numeric となり束縛で 0A000 になるのを避ける）。
    for ty in ["smallint", "bigint", "real", "double precision"] {
        assert_eq!(
            parse_code(
                &core,
                &format!("UPDATE c SET i4 = $1::{ty} WHERE id = 1 USING OPERATION_ID $2")
            ),
            "42P18",
            "{ty}"
        );
    }
    // 許可位置外の `$n::T` は 42601。
    for sql in [
        "SELECT id FROM c WHERE id = $1::int LIMIT 1",
        "SELECT $1::int FROM c LIMIT 1",
        "SELECT id FROM c ORDER BY embedding <=> $1::vector LIMIT 1",
    ] {
        assert_eq!(parse_code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn negative_param_value_before_typecast_is_parenthesized() {
    let (core, _g) = setup("typecast-negparam");
    for value in ["-5", "-2147483648"] {
        let p = core
            .parse_sql_prepared("UPDATE c SET i4 = $1::integer WHERE id = 1 USING OPERATION_ID $2")
            .expect("parse");
        let binds = vec![
            Some(value.as_bytes().to_vec()),
            Some(format!("neg{value}").into_bytes()),
        ];
        let parsed = core
            .bind_prepared(&p, &binds)
            .unwrap_or_else(|e| panic!("{value}: {e:?}"));
        let mut s = SessionState::default();
        core.execute_parsed_in_session(&ctx("alice"), &mut s, &parsed)
            .unwrap_or_else(|e| panic!("{value}: {e:?}"));
    }
}
