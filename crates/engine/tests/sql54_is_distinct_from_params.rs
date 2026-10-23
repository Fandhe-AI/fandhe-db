//! `WHERE <列> IS [NOT] DISTINCT FROM $n`（拡張クエリのバインドパラメータ。Issue #1783）の結合テスト。
//!
//! ポインタ: SQL-54（主）・WIRE-12。`parse_sql_prepared*` → `param_types` → Describe →
//! `bind_prepared` → `execute_parsed_in_session` の本番経路を実 `Storage` 越しに通し、同じ値を
//! リテラル（`NULL` を含む）で書いた簡易クエリの結果行・`wire_code` と一致することを固定する。
//! 受理位置は `sql::params` のパターン 10（左辺が列・右辺が `$n` 単独）だけで、逆向き・両辺 `$n`・
//! 式の内側の `$n` は `42601` のまま。雛形は `sql78_where_noneq_between_params.rs`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::ColumnMeta;
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

fn setup(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let c = ctx("sys");
    core.execute_sql_in_session(
        &c,
        &mut session,
        "CREATE TABLE t (n INTEGER, s TEXT, day DATE, u UUID)",
    )
    .expect("create table");
    let rows = [
        "(1, 'a', '2024-01-01', '00000000-0000-0000-0000-000000000001')",
        "(5, 'm', '2024-06-15', '00000000-0000-0000-0000-000000000005')",
        "(NULL, NULL, NULL, NULL)",
        "(9, 'z', '2024-12-31', NULL)",
    ];
    for (i, row) in rows.iter().enumerate() {
        core.execute_sql_in_session(
            &c,
            &mut session,
            &format!("INSERT INTO t (n, s, day, u) VALUES {row} USING OPERATION_ID 'sql54-{i}'"),
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

/// `template` の `{}` を、`$1` と同じ値を書いたリテラル（`NULL`・引用付き文字列・数値）で置換した
/// 簡易クエリの結果と、拡張クエリの結果が一致することを確かめる。`quote` が偽なら数値としてそのまま書く。
fn assert_same_as_literal(
    core: &EngineCore,
    session: &mut SessionState,
    template: &str,
    value: Option<&str>,
    quote: bool,
) {
    let prepared_sql = template.replace("{}", "$1");
    let lit = match value {
        None => "NULL".to_string(),
        Some(v) if quote => format!("'{v}'"),
        Some(v) => v.to_string(),
    };
    let literal_sql = template.replace("{}", &lit);
    let prepared = run_prepared(core, session, &prepared_sql, &[value]);
    let literal = run_literal(core, session, &literal_sql);
    // 構文・列名の誤りで両方が同じエラーになる空振りを防ぐ（値の不正による SQLSTATE は一致で確かめる）。
    if let Err(code) = &literal {
        assert!(
            !matches!(code.as_str(), "42601" | "42703" | "42P01"),
            "{literal_sql}: unexpected {code}"
        );
    }
    assert_eq!(prepared, literal, "{prepared_sql} vs {literal_sql}");
}

fn param_types(core: &EngineCore, sql: &str) -> Vec<PreparedParamType> {
    core.parse_sql_prepared(sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
        .param_types()
        .to_vec()
}

fn type_name(t: &PreparedParamType) -> String {
    match t {
        PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => format!("{ty:?}"),
        PreparedParamType::Column(ColumnMeta::Id) => "id".to_string(),
        PreparedParamType::Text => "text".to_string(),
        other => format!("{other:?}"),
    }
}

/// SQL-54: `$n` の型は左辺の列の型（ParameterDescription の元）。
#[test]
fn param_type_is_the_left_column_type() {
    let (core, _s, _g) = setup("sql54p-types");
    for (col, expected) in [
        ("n", "Integer"),
        ("s", "Text"),
        ("day", "Date"),
        ("u", "Uuid"),
    ] {
        for kw in ["IS DISTINCT FROM", "IS NOT DISTINCT FROM"] {
            let t = param_types(
                &core,
                &format!("SELECT n, s FROM t WHERE {col} {kw} $1 LIMIT 5"),
            );
            assert!(
                type_name(&t[0]).starts_with(expected),
                "{col} {kw}: {:?}",
                t[0]
            );
        }
    }
}

/// SQL-54: 述語形 `UPDATE`／`DELETE` の `$n` も左辺の列の型で公告される（`text` へ落ちない）。
/// `DISTINCT FROM` の `FROM` が表名の手掛かりと取り違えられないことの回帰確認を兼ねる。
#[test]
fn predicate_dml_param_type_is_the_left_column_type() {
    let (core, _s, _g) = setup("sql54p-dml-types");
    for (sql, expected) in [
        (
            "UPDATE t SET s = 'x' WHERE n IS DISTINCT FROM $1 USING OPERATION_ID 'ty-1'",
            "Integer",
        ),
        (
            "DELETE FROM t WHERE day IS NOT DISTINCT FROM $1 USING OPERATION_ID 'ty-2'",
            "Date",
        ),
    ] {
        let t = param_types(&core, sql);
        assert!(type_name(&t[0]).starts_with(expected), "{sql}: {:?}", t[0]);
    }
}

/// SQL-54: 実行結果（行）が、同じ値をリテラルで書いた簡易クエリと一致する（NULL パラメータを含む）。
#[test]
fn prepared_matches_literal_for_each_type() {
    let (core, mut session, _g) = setup("sql54p-exec");
    // INTEGER（数値リテラル）。
    for v in [Some("1"), Some("5"), Some("-3"), None] {
        for kw in ["IS DISTINCT FROM", "IS NOT DISTINCT FROM"] {
            assert_same_as_literal(
                &core,
                &mut session,
                &format!("SELECT n, s FROM t WHERE n {kw} {{}} LIMIT 10"),
                v,
                false,
            );
        }
    }
    // TEXT。
    for v in [Some("a"), Some("zz"), None] {
        for kw in ["IS DISTINCT FROM", "IS NOT DISTINCT FROM"] {
            assert_same_as_literal(
                &core,
                &mut session,
                &format!("SELECT n, s FROM t WHERE s {kw} {{}} LIMIT 10"),
                v,
                true,
            );
        }
    }
    // DATE（文字列リテラルから列型で解釈。不正な値は同じ SQLSTATE）。
    for v in [
        Some("2024-01-01"),
        Some("2000-02-02"),
        Some("not-a-date"),
        None,
    ] {
        for kw in ["IS DISTINCT FROM", "IS NOT DISTINCT FROM"] {
            assert_same_as_literal(
                &core,
                &mut session,
                &format!("SELECT n, s FROM t WHERE day {kw} {{}} LIMIT 10"),
                v,
                true,
            );
        }
    }
    // UUID（式経路の `=` が未対応の型は、リテラルと同じ結果・SQLSTATE になる）。
    for v in [Some("00000000-0000-0000-0000-000000000001"), None] {
        assert_same_as_literal(
            &core,
            &mut session,
            "SELECT n, s FROM t WHERE u IS NOT DISTINCT FROM {} LIMIT 10",
            v,
            true,
        );
    }
}

/// SQL-54: 他の述語・`NOT` 前置・AND／OR との併用。
#[test]
fn works_inside_compound_predicates() {
    let (core, mut session, _g) = setup("sql54p-compound");
    for (template, value, quote) in [
        (
            "SELECT n, s FROM t WHERE n > 0 AND s IS DISTINCT FROM {} LIMIT 10",
            Some("a"),
            true,
        ),
        (
            "SELECT n, s FROM t WHERE NOT n IS DISTINCT FROM {} LIMIT 10",
            Some("5"),
            false,
        ),
        (
            "SELECT n, s FROM t WHERE (n IS NOT DISTINCT FROM {} OR s = 'z') LIMIT 10",
            None,
            false,
        ),
    ] {
        assert_same_as_literal(&core, &mut session, template, value, quote);
    }
}

/// SQL-54: 述語形 `UPDATE`／`DELETE` でも同じ位置を受理する。
#[test]
fn predicate_dml_accepts_the_position() {
    let (core, mut session, _g) = setup("sql54p-dml");
    let c = ctx("sys");
    for sql in [
        "UPDATE t SET s = 'x' WHERE n IS NOT DISTINCT FROM $1 USING OPERATION_ID 'p-1'",
        "DELETE FROM t WHERE s IS DISTINCT FROM $1 USING OPERATION_ID 'p-2'",
    ] {
        let prepared = core.parse_sql_prepared(sql).expect("parse");
        core.describe_prepared_in_session(&session, &prepared)
            .expect("describe");
    }
    // Execute まで通し、NULL パラメータで NULL 行だけが対象になる。
    let upd = core
        .parse_sql_prepared(
            "UPDATE t SET s = 'x' WHERE n IS NOT DISTINCT FROM $1 USING OPERATION_ID 'p-3'",
        )
        .expect("parse");
    let bound = core.bind_prepared(&upd, &[None]).expect("bind");
    core.execute_parsed_in_session(&c, &mut session, &bound)
        .expect("execute");
    let after = run_literal(
        &core,
        &mut session,
        "SELECT n, s FROM t WHERE s IS NOT DISTINCT FROM 'x' LIMIT 10",
    )
    .expect("select");
    assert_eq!(after.matches("ResultRow").count(), 1, "{after}");
}

/// SQL-54: 受理位置外の `$n` は従来どおり fail-closed（`42601`／型を決められない形は `42P18`）。
#[test]
fn other_positions_stay_rejected() {
    let (core, _s, _g) = setup("sql54p-rejected");
    for sql in [
        // 逆向き。
        "SELECT n, s FROM t WHERE $1 IS DISTINCT FROM n LIMIT 5",
        // 式の内側。
        "SELECT n, s FROM t WHERE n + 1 IS DISTINCT FROM $1 LIMIT 5",
        "SELECT n, s FROM t WHERE n IS DISTINCT FROM $1 + 1 LIMIT 5",
        // 右辺が `$n` の後ろに式の続く形・キャスト。
        "SELECT n, s FROM t WHERE n IS DISTINCT FROM $1::int LIMIT 5",
        // 集合演算文。
        "SELECT n, s FROM t WHERE n IS DISTINCT FROM $1 UNION SELECT n, s FROM t",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert!(
            matches!(err.wire_code(), "42601" | "42P18"),
            "{sql}: unexpected code {}",
            err.wire_code()
        );
    }
    // 両辺 `$n` は左辺が `$n` の逆向き形として拒否される。
    let err = core
        .parse_sql_prepared("SELECT n, s FROM t WHERE $1 IS DISTINCT FROM $2 LIMIT 5")
        .expect_err("both params");
    assert!(matches!(err.wire_code(), "42601" | "42P18"));
}
