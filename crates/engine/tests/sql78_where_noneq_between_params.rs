//! `SELECT` の `WHERE` の非等価比較（`<`・`>`・`<=`・`>=`・`<>`／`!=`）と `[NOT] BETWEEN` で
//! `$n` を受理し、比較相手の列の型を推論する契約（Issue #2430。述語形 UPDATE／DELETE は Issue #2431 の別ファイル）の結合テスト。
//!
//! ポインタ: SQL-78（主）・SQL-24・WIRE-12、TASK-336。`parse_sql_prepared*` → `param_types` →
//! Describe → `bind_prepared` → `execute_parsed_in_session` の本番経路を実 `Storage` 越しに通し、
//! 同じ値をリテラルで書いた簡易クエリの結果行・`wire_code` と一致することを固定する。
//! wire-server は `PreparedParamType` から ParameterDescription を導くだけなので、ここでは
//! engine 側の契約（受理位置・型推論・`42P18`・Describe 用ダミーフラグの序数）だけを検証する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::ColumnMeta;
use engine::sql::mode::SessionState;
use engine::sql::params::{DeclaredParamType as D, PreparedParamType};
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
        "CREATE TABLE t (n INTEGER, f DOUBLE PRECISION, s TEXT, day DATE, at TIMESTAMP, \
         price NUMERIC(10,2), u UUID, b BOOLEAN)",
    )
    .expect("create table");
    let rows = [
        "(1, 1.5, 'a', '2024-01-01', '2024-01-01 00:00:00', 1.10, '00000000-0000-0000-0000-000000000001', true)",
        "(5, 2.5, 'm', '2024-06-15', '2024-06-15 12:00:00', 5.50, '00000000-0000-0000-0000-000000000005', false)",
        "(9, 9.5, 'z', '2024-12-31', '2024-12-31 23:59:59', 9.90, '00000000-0000-0000-0000-000000000009', true)",
    ];
    for (i, row) in rows.iter().enumerate() {
        core.execute_sql_in_session(
            &c,
            &mut session,
            &format!(
                "INSERT INTO t (n, f, s, day, at, price, u, b) VALUES {row} \
                 USING OPERATION_ID 'sql78-{i}'"
            ),
        )
        .expect("insert");
    }
    (core, session, CleanupGuard(path))
}

fn some(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

/// 拡張クエリ経路（Parse → Describe → Bind → Execute）の結果を `Ok(行の文字列表現)`／
/// `Err(wire_code)` へ畳む。
fn run_prepared(
    core: &EngineCore,
    session: &mut SessionState,
    c: &PolicyContext,
    sql: &str,
    values: &[&str],
) -> Result<String, String> {
    let prepared = core
        .parse_sql_prepared(sql)
        .map_err(|e| e.wire_code().to_string())?;
    core.describe_prepared_in_session(session, &prepared)
        .map_err(|e| e.wire_code().to_string())?;
    let binds: Vec<Option<Vec<u8>>> = values.iter().map(|v| some(v)).collect();
    let bound = core
        .bind_prepared(&prepared, &binds)
        .map_err(|e| e.wire_code().to_string())?;
    match core.execute_parsed_in_session(c, session, &bound) {
        Ok(SqlOutcome::Query(r)) => Ok(format!("{:?}", r.rows)),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

fn run_literal(
    core: &EngineCore,
    session: &mut SessionState,
    c: &PolicyContext,
    sql: &str,
) -> Result<String, String> {
    match core.execute_sql_in_session(c, session, sql) {
        Ok(SqlOutcome::Query(r)) => Ok(format!("{:?}", r.rows)),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code().to_string()),
    }
}

/// `template` の `{}` を `values` で順に置換した文（リテラル形。文字列は `'..'`）の結果と
/// 拡張クエリ形（`$1`...）の結果を比較する。
fn assert_same_as_literal(
    core: &EngineCore,
    session: &mut SessionState,
    template: &str,
    values: &[&str],
) {
    let c = ctx("sys");
    let mut prepared_sql = template.to_string();
    let mut literal_sql = template.to_string();
    for (i, v) in values.iter().enumerate() {
        prepared_sql = prepared_sql.replacen("{}", &format!("${}", i + 1), 1);
        let lit = if *v == "true" || *v == "false" {
            v.to_string()
        } else if v.parse::<f64>().is_ok() && !v.contains('-')
            || v.starts_with('-') && v[1..].parse::<f64>().is_ok()
        {
            // 数値は SQL 上の数値リテラル（`{}` が引用符なしで来る列の場合）
            v.to_string()
        } else {
            format!("'{v}'")
        };
        literal_sql = literal_sql.replacen("{}", &lit, 1);
    }
    let prepared = run_prepared(core, session, &c, &prepared_sql, values);
    let literal = run_literal(core, session, &c, &literal_sql);
    eprintln!("{literal_sql} => {literal:?}");
    assert_eq!(prepared, literal, "{prepared_sql} vs {literal_sql}");
}

fn param_types(core: &EngineCore, sql: &str) -> Vec<PreparedParamType> {
    core.parse_sql_prepared(sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
        .param_types()
        .to_vec()
}

fn scalar_ty(t: &PreparedParamType) -> String {
    match t {
        PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => format!("{ty:?}"),
        PreparedParamType::Column(ColumnMeta::Id) => "id".to_string(),
        PreparedParamType::Text => "text".to_string(),
        other => format!("{other:?}"),
    }
}

// --- 型推論 -----------------------------------------------------------------

#[test]
fn param_type_is_the_compared_column_type() {
    let (core, _s, _g) = setup("sql78-types");
    let cols = [
        ("n", "Integer"),
        ("f", "Double"),
        ("s", "Text"),
        ("day", "Date"),
        ("at", "Timestamp"),
        ("u", "Uuid"),
    ];
    for (col, expected) in cols {
        for op in [">", "<", ">=", "<=", "<>", "!="] {
            let t = param_types(
                &core,
                &format!("SELECT id FROM t WHERE {col} {op} $1 LIMIT 5"),
            );
            assert!(
                scalar_ty(&t[0]).starts_with(expected),
                "{col} {op}: {:?}",
                t[0]
            );
        }
        for kw in ["BETWEEN", "NOT BETWEEN"] {
            let t = param_types(
                &core,
                &format!("SELECT id FROM t WHERE {col} {kw} $1 AND $2 LIMIT 5"),
            );
            assert!(
                scalar_ty(&t[0]).starts_with(expected),
                "{col} {kw}: {:?}",
                t[0]
            );
            assert_eq!(t[0], t[1], "{col} {kw}");
        }
    }
}

#[test]
fn compound_predicates_infer_each_column() {
    let (core, _s, _g) = setup("sql78-compound");
    let t = param_types(
        &core,
        "SELECT id FROM t WHERE (n > $1 AND day <= $2) OR s BETWEEN $3 AND $4 LIMIT 5",
    );
    let names: Vec<String> = t.iter().map(scalar_ty).collect();
    assert!(names[0].starts_with("Integer"), "{names:?}");
    assert!(names[1].starts_with("Date"), "{names:?}");
    assert!(
        names[2].starts_with("Text") && names[3].starts_with("Text"),
        "{names:?}"
    );
}

#[test]
fn unknown_table_or_column_falls_back_to_text_without_42p18() {
    let (core, _s, _g) = setup("sql78-unknown");
    for sql in [
        "SELECT id FROM nosuch WHERE zz > $1 LIMIT 5",
        "SELECT id FROM t WHERE zz > $1 LIMIT 5",
        "SELECT id FROM nosuch WHERE zz BETWEEN $1 AND $2 LIMIT 5",
    ] {
        let prepared = core.parse_sql_prepared(sql);
        // 表・列の存在は応答コードで観測させない（Parse では 42P18 にならない）。
        match prepared {
            Ok(p) => assert!(p
                .param_types()
                .iter()
                .all(|t| *t == PreparedParamType::Text)),
            Err(e) => assert_ne!(e.wire_code(), "42P18", "{sql}"),
        }
    }
}

// --- 42P18 ------------------------------------------------------------------

#[test]
fn param_to_param_comparison_is_indeterminate() {
    let (core, _s, _g) = setup("sql78-42p18");
    for sql in [
        "SELECT id FROM t WHERE $1 < $2 LIMIT 5",
        "SELECT id FROM t WHERE $1 <> $2 LIMIT 5",
        "SELECT id FROM t WHERE $1 >= $2 AND n = 1 LIMIT 5",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42P18", "{sql}");
    }
}

#[test]
fn param_resolved_elsewhere_is_not_indeterminate() {
    let (core, _s, _g) = setup("sql78-partial");
    let err = core
        .parse_sql_prepared("SELECT id FROM t WHERE $1 < $2 AND n = $1 LIMIT 5")
        .expect_err("$2 stays indeterminate");
    assert_eq!(err.wire_code(), "42P18");
}

#[test]
fn declared_type_resolves_param_pair() {
    let (core, _s, _g) = setup("sql78-declared");
    let r = core.parse_sql_prepared_with_declared(
        "SELECT id FROM t WHERE $1 < $2 LIMIT 5",
        &[Some(D::Int4), Some(D::Int4)],
    );
    match r {
        Ok(p) => assert_eq!(p.param_types()[0], PreparedParamType::Declared(D::Int4)),
        Err(e) => assert_ne!(e.wire_code(), "42P18"),
    }
}

#[test]
fn syntax_error_wins_over_indeterminate() {
    let (core, _s, _g) = setup("sql78-syntax-first");
    let err = core
        .parse_sql_prepared("SELECT id FROM t WHERE $1 < $2 AND (( LIMIT 5")
        .expect_err("syntax");
    assert_eq!(err.wire_code(), "42601");
}

// --- リテラル形との一致 --------------------------------------------------------

#[test]
fn results_match_literal_form() {
    let (core, mut session, _g) = setup("sql78-literal");
    let cases: &[(&str, &[&str])] = &[
        ("SELECT n FROM t WHERE n > {} LIMIT 100", &["1"]),
        ("SELECT n FROM t WHERE n >= {} LIMIT 100", &["5"]),
        ("SELECT n FROM t WHERE n < {} LIMIT 100", &["9"]),
        ("SELECT n FROM t WHERE n <= {} LIMIT 100", &["5"]),
        ("SELECT n FROM t WHERE n <> {} LIMIT 100", &["5"]),
        ("SELECT n FROM t WHERE n != {} LIMIT 100", &["5"]),
        ("SELECT n FROM t WHERE n > {} LIMIT 100", &["-5"]),
        (
            "SELECT n FROM t WHERE n BETWEEN {} AND {} LIMIT 100",
            &["1", "5"],
        ),
        (
            "SELECT n FROM t WHERE n NOT BETWEEN {} AND {} LIMIT 100",
            &["1", "5"],
        ),
        (
            "SELECT n FROM t WHERE n BETWEEN {} AND {} LIMIT 100",
            &["-5", "5"],
        ),
        ("SELECT n FROM t WHERE f > {} LIMIT 100", &["2.5"]),
        ("SELECT n FROM t WHERE day > {} LIMIT 100", &["2024-03-01"]),
        ("SELECT n FROM t WHERE day <= {} LIMIT 100", &["2024-06-15"]),
        ("SELECT n FROM t WHERE day <> {} LIMIT 100", &["2024-06-15"]),
        (
            "SELECT n FROM t WHERE day BETWEEN {} AND {} LIMIT 100",
            &["2024-01-01", "2024-06-15"],
        ),
        ("SELECT n FROM t WHERE day > {} LIMIT 100", &["not-a-date"]),
        (
            "SELECT n FROM t WHERE at > {} LIMIT 100",
            &["2024-03-01 00:00:00"],
        ),
        (
            "SELECT n FROM t WHERE at NOT BETWEEN {} AND {} LIMIT 100",
            &["2024-01-01 00:00:00", "2024-07-01 00:00:00"],
        ),
        ("SELECT n FROM t WHERE price > {} LIMIT 100", &["5.50"]),
        (
            "SELECT n FROM t WHERE price BETWEEN {} AND {} LIMIT 100",
            &["1.10", "5.50"],
        ),
        (
            "SELECT n FROM t WHERE u > {} LIMIT 100",
            &["00000000-0000-0000-0000-000000000005"],
        ),
        (
            "SELECT n FROM t WHERE u <> {} LIMIT 100",
            &["00000000-0000-0000-0000-000000000005"],
        ),
        ("SELECT n FROM t WHERE s > {} LIMIT 100", &["a"]),
        ("SELECT n FROM t WHERE s <> {} LIMIT 100", &["m"]),
        (
            "SELECT n FROM t WHERE s BETWEEN {} AND {} LIMIT 100",
            &["a", "m"],
        ),
        ("SELECT n FROM t WHERE b > {} LIMIT 100", &["false"]),
        ("SELECT n FROM t WHERE b <> {} LIMIT 100", &["true"]),
        (
            "SELECT n FROM t WHERE n > {} AND day < {} LIMIT 100",
            &["0", "2024-12-31"],
        ),
        (
            "SELECT n FROM t WHERE n > {} OR day < {} LIMIT 100",
            &["8", "2024-02-01"],
        ),
    ];
    for (template, values) in cases {
        assert_same_as_literal(&core, &mut session, template, values);
    }
}

// --- Describe 用ダミーフラグの序数 ------------------------------------------------

#[test]
fn describe_dummy_value_does_not_fail_for_typed_columns() {
    let (core, session, _g) = setup("sql78-describe");
    for sql in [
        "SELECT n FROM t WHERE day > $1 LIMIT 5",
        "SELECT n FROM t WHERE at <> $1 LIMIT 5",
        "SELECT n FROM t WHERE u > $1 LIMIT 5",
        "SELECT n FROM t WHERE price BETWEEN $1 AND $2 LIMIT 5",
        "SELECT n FROM t WHERE day NOT BETWEEN $1 AND $2 LIMIT 5",
        "SELECT n FROM t WHERE n > $1 AND day < $2 AND s > $3 AND b = $4 LIMIT 5",
    ] {
        let prepared = core
            .parse_sql_prepared(sql)
            .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()));
        core.describe_prepared_in_session(&session, &prepared)
            .unwrap_or_else(|e| panic!("describe {sql}: {}", e.wire_code()));
    }
}

#[test]
fn real_literal_before_param_is_still_validated_at_describe() {
    let (core, session, _g) = setup("sql78-ordinal");
    // 実リテラルの `Compare` の後ろに `$n` の等価が来ても、実リテラルの検証が省略されない
    // （序数がずれない）。簡易クエリと同じエラーになる。
    let literal = core.parse_sql("SELECT n FROM t WHERE day > 'not-a-date' AND s = 'x'");
    let prepared = core
        .parse_sql_prepared("SELECT n FROM t WHERE day > 'not-a-date' AND s = $1")
        .and_then(|p| core.describe_prepared_in_session(&session, &p).map(|_| ()));
    assert_eq!(
        prepared.map_err(|e| e.wire_code().to_string()),
        literal.map(|_| ()).map_err(|e| e.wire_code().to_string()),
    );
}

// --- 範囲外の形（42601 のまま） -------------------------------------------------

#[test]
fn out_of_scope_forms_stay_42601() {
    let (core, _s, _g) = setup("sql78-reject");
    for sql in [
        // 逆向き
        "SELECT id FROM t WHERE $1 < n LIMIT 5",
        // 片側だけが `$n` の BETWEEN
        "SELECT id FROM t WHERE day BETWEEN '2024-01-01' AND $1 LIMIT 5",
        "SELECT id FROM t WHERE day BETWEEN $1 AND '2024-01-01' LIMIT 5",
        // `$n` を左辺に置いた BETWEEN
        "SELECT id FROM t WHERE $1 BETWEEN 1 AND 2 LIMIT 5",
        // 型変換が続く
        "SELECT id FROM t WHERE n > $1::int LIMIT 5",
        // 式の一部
        "SELECT id FROM t WHERE 1 + n > $1 LIMIT 5",
        "SELECT id FROM t WHERE n > $1 + 1 LIMIT 5",
        // 相手が数値リテラル・関数
        "SELECT id FROM t WHERE $1 < 5 LIMIT 5",
        // UPDATE／DELETE は #2431 で受理済み。ここでは範囲外の形（逆向き）の拒否を維持する
        "UPDATE t SET n = 1 WHERE $1 < n USING OPERATION_ID 'x'",
        "DELETE FROM t WHERE n BETWEEN 1 AND $1 USING OPERATION_ID 'x'",
        // 集合演算
        "SELECT id FROM t WHERE n > $1 UNION SELECT id FROM t WHERE n < 3 LIMIT 5",
        // GROUP BY の後ろ
        "SELECT n FROM t GROUP BY n HAVING n > $1",
        // LIKE・IN
        "SELECT id FROM t WHERE s LIKE $1 LIMIT 5",
        "SELECT id FROM t WHERE n IN ($1, $2) LIMIT 5",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

// --- RLS --------------------------------------------------------------------

#[test]
fn rls_applies_to_noneq_and_between_params() {
    let (core, mut session, _g) = setup("sql78-rls");
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    session.allow_ddl();
    core.execute_sql_in_session(&a, &mut session, "CREATE TABLE r (n INTEGER)")
        .expect("create a");

    for (c, n, op) in [(&a, 7, "a-1"), (&b, 8, "b-1"), (&b, 9, "b-2")] {
        core.execute_sql_in_session(
            c,
            &mut session,
            &format!("INSERT INTO r (n) VALUES ({n}) USING OPERATION_ID '{op}'"),
        )
        .expect("insert");
    }
    // テナント A の Prepared は A の可視行（7）だけを返し、B の行（8・9）の存在は現れない。
    for (sql, lit, values) in [
        (
            "SELECT n FROM r WHERE n > $1 LIMIT 10",
            "SELECT n FROM r WHERE n > 0 LIMIT 10",
            vec!["0"],
        ),
        (
            "SELECT n FROM r WHERE n BETWEEN $1 AND $2 LIMIT 10",
            "SELECT n FROM r WHERE n BETWEEN 0 AND 100 LIMIT 10",
            vec!["0", "100"],
        ),
    ] {
        let got = run_prepared(&core, &mut session, &a, sql, &values).expect("prepared");
        assert_eq!(Ok(got.clone()), run_literal(&core, &mut session, &a, lit));
        assert!(got.contains("SignedInteger(7)"), "{got}");
        assert!(
            !got.contains("SignedInteger(8)") && !got.contains("SignedInteger(9)"),
            "{got}"
        );
    }
}
// PR #2631 指摘: BOOLEAN 列の `[NOT] BETWEEN $1 AND $2` は境界が文字列リテラルへ置換され、
// 文字列境界を書いた簡易クエリと同じ結果・`wire_code` になる（Parse/Describe/Bind が 42601 にならない）。
#[test]
fn boolean_between_bounds_match_string_literal_form() {
    let (core, mut session, _g) = setup("sql78-bool-between");
    let c = ctx("sys");
    for (op, a, b) in [
        ("BETWEEN", "false", "true"),
        ("NOT BETWEEN", "false", "false"),
        ("BETWEEN", "t", "f"),
    ] {
        let prepared = run_prepared(
            &core,
            &mut session,
            &c,
            &format!("SELECT n FROM t WHERE b {op} $1 AND $2 LIMIT 100"),
            &[a, b],
        );
        assert!(prepared.is_ok(), "{op} {a} {b}: {prepared:?}");
        // 束縛値は正規形（true／false）の文字列リテラルへ置換される。
        fn canon(v: &str) -> &str {
            match v {
                "t" => "true",
                "f" => "false",
                other => other,
            }
        }
        let literal = run_literal(
            &core,
            &mut session,
            &c,
            &format!(
                "SELECT n FROM t WHERE b {op} '{}' AND '{}' LIMIT 100",
                canon(a),
                canon(b)
            ),
        );
        assert_eq!(prepared, literal, "{op} {a} {b}");
    }
}

// PR #2631 指摘: `intersect` 等の名前の別名があっても集合演算とみなさず、パターン 9 は無効化されない。
#[test]
fn set_operator_named_identifiers_do_not_disable_noneq_params() {
    let (core, mut session, _g) = setup("sql78-setop-name");
    let c = ctx("sys");
    for sql in [
        "SELECT n AS intersect FROM t WHERE n > $1 LIMIT 5",
        "SELECT n FROM t WHERE n > $1 ORDER BY n LIMIT 5",
    ] {
        let r = run_prepared(&core, &mut session, &c, sql, &["1"]);
        assert!(r.is_ok(), "{sql}: {r:?}");
    }
}

// PR #2631 指摘: 集合演算でも枝ごとにダミーフラグを持つ。第 1 枝の `day = $1` のダミー値は
// 日付検証を省略され、第 2 枝の実リテラルは検証されたまま Describe が成功する。
#[test]
fn set_op_describe_keeps_param_dummy_per_branch() {
    let (core, session, _g) = setup("sql78-setop-flags");
    let ok = core
        .parse_sql_prepared(
            "SELECT day FROM t WHERE day = $1 UNION SELECT day FROM t WHERE day = '2024-01-01' LIMIT 5",
        )
        .and_then(|p| core.describe_prepared_in_session(&session, &p).map(|_| ()));
    assert!(
        ok.is_ok(),
        "{:?}",
        ok.map_err(|e| e.wire_code().to_string())
    );
    // 実リテラルの不正値は第 2 枝でも拒否される（省略されない）。
    let bad = core
        .parse_sql_prepared(
            "SELECT day FROM t WHERE day = $1 UNION SELECT day FROM t WHERE day = 'not-a-date' LIMIT 5",
        )
        .and_then(|p| core.describe_prepared_in_session(&session, &p).map(|_| ()));
    assert!(bad.is_err());
}
