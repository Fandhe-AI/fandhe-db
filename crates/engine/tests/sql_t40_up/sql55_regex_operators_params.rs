//! `WHERE <列> ~ | ~* | !~ | !~* $n`（拡張クエリのバインドパラメータ。Issue #1787）の結合テスト。
//!
//! ポインタ: SQL-55（主）・WIRE-12。`parse_sql_prepared*` → `param_types` → Describe →
//! `bind_prepared` → `execute_parsed_in_session` の本番経路を実 `Storage` 越しに通し、同じ値を
//! リテラル（`NULL` を含む）で書いた簡易クエリの結果行・`wire_code` と一致することを固定する。
//! 受理位置は `sql::params` のパターン 11（左辺が列・右辺が `$n` 単独）だけで、逆向き・両辺 `$n`・
//! 式の内側の `$n` は `42601` のまま。雛形は `sql54_is_distinct_from_params.rs`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::params::PreparedParamType;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("sys");
    core.execute_sql_in_session(&c, &mut session, "CREATE TABLE t (n INTEGER, s TEXT)")
        .expect("create table");
    let rows = [
        "(1, 'Alpha')",
        "(5, 'alpine')",
        "(NULL, NULL)",
        "(9, 'zeta')",
    ];
    for (i, row) in rows.iter().enumerate() {
        core.execute_sql_in_session(
            &c,
            &mut session,
            &format!("INSERT INTO t (n, s) VALUES {row} USING OPERATION_ID 'sql55-{i}'"),
        )
        .expect("insert");
    }
    (core, session, CleanupGuard(path))
}

/// 拡張クエリ経路（Parse → Describe → Bind → Execute）の結果を `Ok(行の文字列表現)`／
/// `Err(wire_code)` へ畳む。`None` の値は NULL パラメータ。
fn run_prepared(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
    values: &[Option<&str>],
) -> Result<String, String> {
    let prepared = core
        .parse_sql_prepared(sql)
        .map_err(|e| e.wire_code().to_string())?;
    core.describe_prepared_in_session(session, &prepared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values
        .iter()
        .map(|v| v.map(|s| s.as_bytes().to_vec()))
        .collect();
    let bound = core
        .bind_prepared(&prepared, &binds)
        .map_err(|e| e.wire_code().to_string())?;
    match core.execute_parsed_in_session(&ctx("sys"), session, &bound) {
        Ok(SqlOutcome::Query(r)) => Ok(format!("{:?}", r.rows)),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

fn run_literal(core: &EngineCore, session: &mut SessionState, sql: &str) -> Result<String, String> {
    match core.execute_sql_in_session(&ctx("sys"), session, sql) {
        Ok(SqlOutcome::Query(r)) => Ok(format!("{:?}", r.rows)),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

/// `template` の `{}` を、`$1` と同じ値を書いたリテラル（`NULL`・引用付き文字列）で置換した
/// 簡易クエリの結果と、拡張クエリの結果が一致することを確かめる。
fn assert_same_as_literal(
    core: &EngineCore,
    session: &mut SessionState,
    template: &str,
    value: Option<&str>,
) -> Result<String, String> {
    let prepared_sql = template.replace("{}", "$1");
    let lit = value.map_or("NULL".to_string(), |v| format!("'{v}'"));
    let literal_sql = template.replace("{}", &lit);
    let prepared = run_prepared(core, session, &prepared_sql, &[value]);
    let literal = run_literal(core, session, &literal_sql);
    if let Err(code) = &literal {
        assert!(
            !matches!(code.as_str(), "42601" | "42703" | "42P01"),
            "{literal_sql}: unexpected {code}"
        );
    }
    assert_eq!(prepared, literal, "{prepared_sql} vs {literal_sql}");
    prepared
}

/// SQL-55: `$n` は左辺の列の型に依らず `text` と公告される。
#[test]
fn param_type_is_always_text() {
    let (core, _s, _g) = setup("sql55p-types");
    for col in ["s", "n"] {
        for op in ["~", "~*", "!~", "!~*"] {
            let sql = format!("SELECT n, s FROM t WHERE {col} {op} $1 LIMIT 5");
            let t = core
                .parse_sql_prepared(&sql)
                .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
            assert!(
                matches!(t.param_types(), [PreparedParamType::Text]),
                "{sql}: {:?}",
                t.param_types()
            );
        }
    }
}

/// SQL-55: 実行結果（行）が、同じ値をリテラルで書いた簡易クエリと一致する（NULL パラメータ・
/// 不正パターンを含む）。
#[test]
fn prepared_matches_literal() {
    let (core, mut session, _g) = setup("sql55p-exec");
    for op in ["~", "~*", "!~", "!~*"] {
        for v in [Some("^al"), Some("a.*e$"), Some("("), Some("(a)\\1"), None] {
            assert_same_as_literal(
                &core,
                &mut session,
                &format!("SELECT n, s FROM t WHERE s {op} {{}} LIMIT 10"),
                v,
            )
            .ok();
        }
    }
    // 結果の具体値。
    let got = assert_same_as_literal(
        &core,
        &mut session,
        "SELECT n, s FROM t WHERE s ~* {} LIMIT 10",
        Some("^alp"),
    )
    .expect("ok");
    assert_eq!(got.matches("ResultRow").count(), 2, "{got}");
    // 不正パターンは 2201B、NULL パターンは 0 件。
    assert_eq!(
        run_prepared(
            &core,
            &mut session,
            "SELECT n FROM t WHERE s ~ $1 LIMIT 10",
            &[Some("(")]
        )
        .expect_err("invalid"),
        "2201B"
    );
    let none = run_prepared(
        &core,
        &mut session,
        "SELECT n FROM t WHERE s !~ $1 LIMIT 10",
        &[None],
    )
    .expect("null pattern");
    assert_eq!(none.matches("ResultRow").count(), 0, "{none}");
}

/// SQL-55: 他の述語・`NOT` 前置・AND／OR との併用。
#[test]
fn works_inside_compound_predicates() {
    let (core, mut session, _g) = setup("sql55p-compound");
    for (template, value) in [
        (
            "SELECT n, s FROM t WHERE n > 0 AND s ~ {} LIMIT 10",
            Some("a"),
        ),
        ("SELECT n, s FROM t WHERE NOT s !~* {} LIMIT 10", Some("^Z")),
        (
            "SELECT n, s FROM t WHERE (s ~ {} OR n = 9) LIMIT 10",
            Some("^Al"),
        ),
    ] {
        assert_same_as_literal(&core, &mut session, template, value).expect("ok");
    }
}

/// SQL-55: 述語形 `UPDATE`／`DELETE` でも同じ位置を受理し、実行できる。
#[test]
fn predicate_dml_accepts_the_position() {
    let (core, mut session, _g) = setup("sql55p-dml");
    let c = ctx("sys");
    let upd = core
        .parse_sql_prepared("UPDATE t SET n = 0 WHERE s ~* $1 USING OPERATION_ID 'p-1'")
        .expect("parse");
    core.describe_prepared_in_session(&session, &upd)
        .expect("describe");
    let bound = core
        .bind_prepared(&upd, &[Some(b"^al".to_vec())])
        .expect("bind");
    core.execute_parsed_in_session(&c, &mut session, &bound)
        .expect("execute");
    let after =
        run_literal(&core, &mut session, "SELECT n FROM t WHERE n = 0 LIMIT 10").expect("select");
    assert_eq!(after.matches("ResultRow").count(), 2, "{after}");
}

/// SQL-55: 受理位置外の `$n` は従来どおり fail-closed（`42601`／型を決められない形は `42P18`）。
#[test]
fn other_positions_stay_rejected() {
    let (core, _s, _g) = setup("sql55p-rejected");
    for sql in [
        "SELECT n, s FROM t WHERE $1 ~ s LIMIT 5",
        "SELECT n, s FROM t WHERE $1 ~ $2 LIMIT 5",
        "SELECT n, s FROM t WHERE lower(s) ~ $1 LIMIT 5",
        "SELECT n, s FROM t WHERE s ~ $1::text LIMIT 5",
        "SELECT n, s FROM t WHERE s ~ $1 || 'x' LIMIT 5",
        "SELECT n, s FROM t WHERE s ~ $1 UNION SELECT n, s FROM t",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert!(
            matches!(err.wire_code(), "42601" | "42P18"),
            "{sql}: unexpected code {}",
            err.wire_code()
        );
    }
}
