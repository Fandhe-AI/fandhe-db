//! SQL/JSON 問い合わせ関数 `JSON_EXISTS`・`JSON_VALUE`・`JSON_QUERY`（対象ビヘイビア: TABLE-25・
//! TASK-273。Issue #2187）の結合テスト。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25。
//!
//! `tests/json_path_functions.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql` を production 経路として検証）。固定する契約: 各関数の既定動作と
//! 句（`PASSING`・`RETURNING`・ラッパー・引用符・`ON EMPTY`／`ON ERROR`）、`ON ERROR` が捕捉する
//! エラーと捕捉しないエラー（`42704`・`0A000`・`54000`）、受理しない句・型の `0A000`、使える位置、
//! 拡張クエリの WHERE 領域、起動時上限の反映、テナント境界、エラー文に入力の断片が出ないこと。
//! 期待値は PostgreSQL 17 のドキュメント・ソース由来で、実機との突き合わせは未実施（推定を含む）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::jsonpath_limits::JsonPathLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "sjdocs";
const DOC: &str = r#"{"a":[1,2,3],"b":{"c":"x"},"n":null,"t":true}"#;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

fn open_core(tag: &str, limits: Option<JsonPathLimits>) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_jsonpath_limits(l),
        None => core,
    };
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(core: &EngineCore, ctx: &PolicyContext, docs: &[Option<&str>]) {
    for (i, d) in docs.iter().enumerate() {
        let id = i + 1;
        let docb = match d {
            Some(d) => format!("'{d}'"),
            None => "NULL".to_string(),
        };
        core.execute_sql_in_session(
            ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, doc, docb, n) \
                 VALUES ({id}, '[0.1,0.2]', 'k{id}', '{{}}', {docb}, {id}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .expect("insert should succeed");
    }
}

fn setup() -> (EngineCore, CleanupGuard, PolicyContext) {
    let (core, path) = open_core("sql-json-query-functions", None);
    let guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &[Some(DOC), None]);
    (core, guard, alice)
}

fn cell(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .unwrap_or_else(|e| panic!("select {expr} should succeed: {}", e.wire_code()));
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

fn err_code(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
    )
    .expect_err("statement should fail")
    .wire_code()
    .to_string()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

fn json(s: &str) -> Cell {
    Cell::Json(s.to_string())
}

fn matching(core: &EngineCore, ctx: &PolicyContext, pred: &str) -> Vec<i64> {
    let r = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("{pred} should succeed: {}", e.wire_code()));
    let mut v: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Integer(x) => i64::try_from(*x).expect("fits"),
            Cell::SignedInteger(x) => *x,
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

// --- JSON_EXISTS ---

#[test]
fn json_exists_defaults_and_on_error() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(c("JSON_EXISTS(docb, '$.a')"), Cell::Bool(true));
    assert_eq!(c("JSON_EXISTS(docb, '$.zzz')"), Cell::Bool(false));
    assert_eq!(
        c("JSON_EXISTS(docb, '$.a[*] ? (@ > 5)')"),
        Cell::Bool(false)
    );
    // strict の欠落キーは評価エラー。既定は FALSE ON ERROR。
    assert_eq!(c("JSON_EXISTS(docb, 'strict $.zzz')"), Cell::Bool(false));
    assert_eq!(
        c("JSON_EXISTS(docb, 'strict $.zzz' TRUE ON ERROR)"),
        Cell::Bool(true)
    );
    assert_eq!(
        c("JSON_EXISTS(docb, 'strict $.zzz' UNKNOWN ON ERROR)"),
        Cell::Null
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_EXISTS(docb, 'strict $.zzz' ERROR ON ERROR)",
            1
        ),
        "2203A"
    );
    // NULL の context・path は NULL。
    assert_eq!(
        cell(&core, &alice, "JSON_EXISTS(docb, '$.a')", 2),
        Cell::Null
    );
    assert_eq!(c("JSON_EXISTS(NULL, '$.a')"), Cell::Null);
    assert_eq!(c("JSON_EXISTS(docb, NULL)"), Cell::Null);
    // 型なし文字列リテラルは jsonb として解析される。
    assert_eq!(c("JSON_EXISTS('{\"k\":1}', '$.k')"), Cell::Bool(true));
}

#[test]
fn passing_values_bind_by_type_and_columns_are_marked() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    // 数値
    assert_eq!(
        c("JSON_VALUE(docb, '$.a[*] ? (@ > $min)' PASSING 2 AS min)"),
        text("3")
    );
    // 整数列（行ごとの値。マスク漏れがあると評価時に Internal になる）
    assert_eq!(
        c("JSON_VALUE(docb, '$.a[*] ? (@ == $m)' PASSING n AS m)"),
        text("1")
    );
    // text
    assert_eq!(
        c("JSON_EXISTS(docb, '$.b.c ? (@ == $v)' PASSING 'x' AS v)"),
        Cell::Bool(true)
    );
    // jsonb 列の値
    assert_eq!(
        c("JSON_EXISTS(docb, '$.a ? (@[0] == $j.a[0])' PASSING docb AS j)"),
        Cell::Bool(true)
    );
    // SQL の NULL は JSON の null
    assert_eq!(
        c("JSON_EXISTS(docb, '$.n ? (@ == $z)' PASSING NULL AS z)"),
        Cell::Bool(true)
    );
    // 名前の畳み込みと重複
    assert_eq!(
        c("JSON_EXISTS(docb, '$.a[*] ? (@ == $x)' PASSING 1 AS X)"),
        Cell::Bool(true)
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_EXISTS(docb, '$' PASSING 1 AS x, 2 AS x)",
            1
        ),
        "0A000"
    );
    // 未定義変数は抑止されない
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_EXISTS(docb, '$.a[0] ? (@ == $zz)' TRUE ON ERROR)",
            1
        ),
        "42704"
    );
}

// --- JSON_VALUE ---

#[test]
fn json_value_maps_scalars_and_applies_on_empty_and_on_error() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(c("JSON_VALUE(docb, '$.b.c')"), text("x"));
    assert_eq!(c("JSON_VALUE(docb, '$.a[1]')"), text("2"));
    assert_eq!(c("JSON_VALUE(docb, '$.t')"), text("true"));
    assert_eq!(c("JSON_VALUE(docb, '$.n')"), Cell::Null);
    assert_eq!(c("JSON_VALUE(docb, '$.zzz')"), Cell::Null);
    assert_eq!(
        c("JSON_VALUE(docb, '$.zzz' DEFAULT 'd' ON EMPTY)"),
        text("d")
    );
    assert_eq!(c("JSON_VALUE(docb, '$.zzz' DEFAULT 7 ON EMPTY)"), text("7"));
    assert_eq!(
        c("JSON_VALUE(docb, '$.zzz' DEFAULT 'd' ON EMPTY DEFAULT 'e' ON ERROR)"),
        text("d")
    );
    assert_eq!(
        err_code(&core, &alice, "JSON_VALUE(docb, '$.zzz' ERROR ON EMPTY)", 1),
        "22035"
    );
    // 2 件以上（22034）・非スカラー（2203F）は ON ERROR の対象（既定は NULL）。
    assert_eq!(c("JSON_VALUE(docb, '$.a[*]')"), Cell::Null);
    assert_eq!(c("JSON_VALUE(docb, '$.b')"), Cell::Null);
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_VALUE(docb, '$.a[*]' ERROR ON ERROR)",
            1
        ),
        "22034"
    );
    assert_eq!(
        err_code(&core, &alice, "JSON_VALUE(docb, '$.b' ERROR ON ERROR)", 1),
        "2203F"
    );
    assert_eq!(c("JSON_VALUE(docb, '$.b' DEFAULT 'e' ON ERROR)"), text("e"));
    // 抑止できる評価エラー（strict の欠落キー）も ON ERROR の対象。
    assert_eq!(
        c("JSON_VALUE(docb, 'strict $.zzz' DEFAULT 'e' ON ERROR)"),
        text("e")
    );
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_VALUE(docb, 'strict $.zzz' ERROR ON ERROR)",
            1
        ),
        "2203A"
    );
    // RETURNING text／長さなし varchar
    assert_eq!(c("JSON_VALUE(docb, '$.a[0]' RETURNING text)"), text("1"));
    assert_eq!(c("JSON_VALUE(docb, '$.a[0]' RETURNING varchar)"), text("1"));
    assert_eq!(
        cell(&core, &alice, "JSON_VALUE(docb, '$.a[0]')", 2),
        Cell::Null
    );
}

// --- JSON_QUERY ---

#[test]
fn json_query_wrapper_quotes_and_behaviors() {
    let (core, _g, alice) = setup();
    let c = |e: &str| cell(&core, &alice, e, 1);
    assert_eq!(c("JSON_QUERY(docb, '$.b')"), json(r#"{"c":"x"}"#));
    assert_eq!(c("JSON_QUERY(docb, '$.b.c')"), json(r#""x""#));
    assert_eq!(c("JSON_QUERY(docb, '$.n')"), json("null"));
    assert_eq!(c("JSON_QUERY(docb, '$.zzz')"), Cell::Null);
    // 2 件以上はラッパーなしだと 22034（既定は NULL ON ERROR）。
    assert_eq!(c("JSON_QUERY(docb, '$.a[*]')"), Cell::Null);
    assert_eq!(
        err_code(
            &core,
            &alice,
            "JSON_QUERY(docb, '$.a[*]' ERROR ON ERROR)",
            1
        ),
        "22034"
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.a[*]' WITH WRAPPER)"),
        json("[1,2,3]")
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.b' WITH UNCONDITIONAL ARRAY WRAPPER)"),
        json(r#"[{"c":"x"}]"#)
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.a[*]' WITHOUT ARRAY WRAPPER EMPTY ARRAY ON ERROR)"),
        json("[]")
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.zzz' EMPTY ARRAY ON EMPTY)"),
        json("[]")
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.zzz' EMPTY OBJECT ON EMPTY)"),
        json("{}")
    );
    assert_eq!(
        c(r#"JSON_QUERY(docb, '$.zzz' DEFAULT '{"z":1}' ON EMPTY)"#),
        json(r#"{"z":1}"#)
    );
    assert_eq!(
        err_code(&core, &alice, "JSON_QUERY(docb, '$.zzz' ERROR ON EMPTY)", 1),
        "22035"
    );
    // RETURNING text と引用符
    assert_eq!(
        c("JSON_QUERY(docb, '$.b' RETURNING text)"),
        text(r#"{"c":"x"}"#)
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.b.c' RETURNING text)"),
        text(r#""x""#)
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.b.c' RETURNING text KEEP QUOTES ON SCALAR STRING)"),
        text(r#""x""#)
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.b.c' RETURNING text OMIT QUOTES)"),
        text("x")
    );
    assert_eq!(
        c("JSON_QUERY(docb, '$.a[0]' RETURNING text OMIT QUOTES ON SCALAR STRING)"),
        text("1")
    );
    // RETURNING json は jsonb の正準テキストを json として返す。
    let r = c("JSON_QUERY(docb, '$.b' RETURNING json)");
    assert!(
        matches!(r, Cell::Json(ref t) if t == r#"{"c":"x"}"#),
        "{r:?}"
    );
}

// --- 受理しない句・型は 0A000 ---

#[test]
fn unsupported_clauses_and_types_are_0a000() {
    let (core, _g, alice) = setup();
    let cases = [
        "JSON_VALUE(docb, '$.a[0]' RETURNING int)",
        "JSON_VALUE(docb, '$.a[0]' RETURNING integer)",
        "JSON_VALUE(docb, '$.a[0]' RETURNING numeric)",
        "JSON_VALUE(docb, '$.t' RETURNING boolean)",
        "JSON_VALUE(docb, '$.a[0]' RETURNING varchar(10))",
        "JSON_VALUE(docb, '$.a[0]' RETURNING character varying)",
        "JSON_VALUE(docb, '$.a[0]' RETURNING jsonb)",
        "JSON_QUERY(docb, '$.a' RETURNING int[])",
        "JSON_QUERY(docb, '$.a' RETURNING date)",
        "JSON_QUERY(docb, '$.b.c' RETURNING jsonb OMIT QUOTES)",
        "JSON_QUERY(docb, '$.a[*]' WITH CONDITIONAL WRAPPER)",
        "JSON_QUERY(docb, '$.a[*]' WITH CONDITIONAL ARRAY WRAPPER)",
        "JSON_QUERY(docb, '$.a' RETURNING jsonb FORMAT JSON)",
        "JSON_VALUE(docb FORMAT JSON, '$.a')",
        "JSON_EXISTS(docb, '$.a' RETURNING int)",
        // 非リテラルの path・DEFAULT
        "JSON_VALUE(docb, lang)",
        "JSON_EXISTS(docb, lang)",
        "JSON_VALUE(docb, '$.zzz' DEFAULT lang ON EMPTY)",
        "JSON_VALUE(docb, '$.zzz' DEFAULT 1.5 ON EMPTY)",
        "JSON_QUERY(docb, '$.zzz' DEFAULT 1 ON EMPTY)",
        // text／json の context item
        "JSON_VALUE(lang, '$.a')",
        "JSON_VALUE(doc, '$.a')",
        // datetime・vector の PASSING 値
        "JSON_EXISTS(docb, '$' PASSING embedding AS v)",
        // `x` フラグは未対応（Issue #2677）。束縛時の拒否は ON ERROR でも捕捉しない
        "JSON_EXISTS(docb, '$ ? (@.b.c like_regex \"x\" flag \"x\")' FALSE ON ERROR)",
    ];
    for expr in cases {
        assert_eq!(err_code(&core, &alice, expr, 1), "0A000", "{expr}");
    }
}

#[test]
fn invalid_clause_combinations_are_syntax_errors() {
    let (core, _g, alice) = setup();
    let cases = [
        "JSON_QUERY(docb, '$.a' WITH WRAPPER KEEP QUOTES)",
        "JSON_QUERY(docb, '$.a' WITH WRAPPER OMIT QUOTES)",
        "JSON_VALUE(docb, '$.a' EMPTY ARRAY ON ERROR)",
        "JSON_VALUE(docb, '$.a' TRUE ON ERROR)",
        "JSON_QUERY(docb, '$.a' UNKNOWN ON ERROR)",
        "JSON_EXISTS(docb, '$.a' NULL ON ERROR)",
        "JSON_EXISTS(docb, '$.a' ERROR ON EMPTY)",
        // 句の順序違反
        "JSON_VALUE(docb, '$.a' RETURNING text PASSING 1 AS x)",
        "JSON_VALUE(docb, '$.a' NULL ON ERROR NULL ON EMPTY)",
        "JSON_VALUE(docb, '$.a' PASSING 1)",
    ];
    for expr in cases {
        assert_eq!(err_code(&core, &alice, expr, 1), "42601", "{expr}");
    }
}

#[test]
fn path_literal_errors_are_raised_at_bind_time() {
    let (core, _g, alice) = setup();
    let e = core
        .execute_sql(
            &alice,
            "SELECT JSON_VALUE('{}', '$.a[') FROM sjdocs WHERE id = 999 LIMIT 1",
        )
        .expect_err("syntax error in a path literal even with no rows");
    assert_eq!(e.wire_code(), "42601");
}

// --- 捕捉しないエラー ---

#[test]
fn on_error_never_swallows_unsuppressible_errors() {
    let alice = ctx_for("alice");
    let (small, path) = open_core("sql-json-query-small", Some(JsonPathLimits::new(5, 4)));
    let _g = CleanupGuard(path);
    seed(&small, &alice, &[Some(DOC)]);
    for expr in [
        "JSON_EXISTS(docb, '$.**.**.**' TRUE ON ERROR)",
        "JSON_VALUE(docb, '$.**.**.**' DEFAULT 'e' ON ERROR)",
        "JSON_QUERY(docb, '$.**.**.**' EMPTY ARRAY ON ERROR)",
    ] {
        assert_eq!(err_code(&small, &alice, expr, 1), "54000", "{expr}");
    }
}

// --- 使える位置 ---

#[test]
fn usable_in_projection_where_order_by_and_const_select() {
    let (core, _g, alice) = setup();
    // 既定の列名は PostgreSQL と同じ小文字の関数名。
    let r = core
        .execute_sql(
            &alice,
            &format!(
                "SELECT JSON_EXISTS(docb, '$.a'), JSON_VALUE(docb, '$.t'), JSON_QUERY(docb, '$.b') \
                 FROM {TABLE} WHERE id = 1 LIMIT 1"
            ),
        )
        .expect("projection");
    let names: Vec<String> = r
        .columns
        .iter()
        .map(|c| match c {
            engine::sql::exec::ColumnMeta::Computed { name, .. } => name.clone(),
            other => panic!("unexpected column meta {other:?}"),
        })
        .collect();
    assert_eq!(names, ["json_exists", "json_value", "json_query"]);
    assert_eq!(matching(&core, &alice, "JSON_EXISTS(docb, '$.a')"), vec![1]);
    assert_eq!(
        matching(&core, &alice, "JSON_VALUE(docb, '$.b.c') = 'x'"),
        vec![1]
    );
    assert_eq!(
        matching(
            &core,
            &alice,
            "JSON_VALUE(docb, '$.b.c' RETURNING text) = 'x' AND n = 1"
        ),
        vec![1]
    );
    let e = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE NOT JSON_EXISTS(docb, '$.a') LIMIT 5"),
        )
        .expect_err("negated bare predicate");
    assert_eq!(e.wire_code(), "42601");
    let r = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} ORDER BY JSON_VALUE(docb, '$.a[0]') LIMIT 5"),
        )
        .expect("order by a JSON_VALUE");
    assert_eq!(r.rows.len(), 2);
    let r = core
        .execute_sql(&alice, "SELECT JSON_VALUE('{\"a\":1}', '$.a')")
        .expect("FROM-less select");
    assert_eq!(r.rows[0].cells[0], text("1"));
    let e = core
        .execute_sql(
            &alice,
            "CREATE TABLE chk (id BIGINT, v INTEGER, CHECK (JSON_EXISTS('{}', '$')))",
        )
        .expect_err("CHECK with JSON_EXISTS");
    // jsonb 関数・`jsonb_path_*` と同じく CHECK 本体には置けない。
    assert!(matches!(e.wire_code(), "0A000" | "42601"));
}

/// INSERT VALUES・UPDATE SET・CREATE VIEW 本文での扱いは、既存の jsonb 関数（Issue #1599）と
/// 同じ受理／SQLSTATE になる（新しい受理経路を黙って増やさない）。
#[test]
fn unsupported_contexts_match_existing_jsonb_functions() {
    let (core, _g, alice) = setup();
    let run = |sql: &str| {
        core.execute_sql_in_session(&alice, &mut SessionState::default(), sql)
            .map(|_| ())
            .map_err(|e| e.wire_code().to_string())
    };
    let pairs = [
        (
            format!("INSERT INTO {TABLE} (id, embedding, docb) VALUES (9, '[0.1,0.2]', JSON_QUERY('{{}}', '$')) USING OPERATION_ID 'v1'"),
            format!("INSERT INTO {TABLE} (id, embedding, docb) VALUES (9, '[0.1,0.2]', jsonb_build_array('{{}}')) USING OPERATION_ID 'v1'"),
        ),
        (
            format!("UPDATE {TABLE} SET docb = JSON_QUERY(docb, '$') WHERE id = 1 USING OPERATION_ID 'u1'"),
            format!("UPDATE {TABLE} SET docb = jsonb_build_array(docb) WHERE id = 1 USING OPERATION_ID 'u1'"),
        ),
        (
            format!("CREATE VIEW vv AS SELECT id FROM {TABLE} WHERE JSON_EXISTS(docb, '$.a')"),
            format!("CREATE VIEW vv2 AS SELECT id FROM {TABLE} WHERE jsonb_array_length(docb) > 0"),
        ),
    ];
    for (new, old) in pairs {
        let a = run(&new);
        let b = run(&old);
        assert_eq!(a.is_ok(), b.is_ok(), "{new}: {a:?} vs {b:?}");
        if let (Err(x), Err(y)) = (&a, &b) {
            assert_eq!(x, y, "{new}");
        }
    }
}

// --- 拡張クエリプロトコル ---

/// `RETURNING`／`ON` を括弧内の句語として持つ式が WHERE 領域の判定を壊さない（`$n` が領域内に
/// 残り、簡易形のリテラル SQL と同じ構文木になる）。
#[test]
fn prepared_where_region_survives_returning_and_on_inside_parentheses() {
    let (core, _g, _alice) = setup();
    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {TABLE} WHERE JSON_VALUE(docb, '$.b.c' RETURNING text NULL ON ERROR) = 'x' \
             AND lang = $1 LIMIT 5"
        ))
        .expect("parse_sql_prepared should succeed");
    assert_eq!(prepared.param_count(), 1);
    let bound = core
        .bind_prepared(&prepared, &[Some(b"k1".to_vec())])
        .expect("bind_prepared should succeed");
    let literal = core
        .parse_sql(&format!(
            "SELECT id FROM {TABLE} WHERE JSON_VALUE(docb, '$.b.c' RETURNING text NULL ON ERROR) = 'x' \
             AND lang = 'k1' LIMIT 5"
        ))
        .expect("parse_sql should succeed");
    assert_eq!(bound, literal);
}

// --- テナント境界・エラー文 ---

#[test]
fn other_tenants_rows_never_leak_into_results_or_errors() {
    let (core, _g, alice) = setup();
    let bob = ctx_for("bob");
    assert_eq!(
        matching(&core, &bob, "JSON_EXISTS(docb, '$.a')"),
        Vec::<i64>::new()
    );
    for expr in [
        "JSON_VALUE(docb, 'strict $.zzz' ERROR ON ERROR)",
        "JSON_VALUE(docb, '$.a[*]' ERROR ON ERROR)",
        "JSON_VALUE(docb, '$.b' ERROR ON ERROR)",
        "JSON_QUERY(docb, '$.a[*]' ERROR ON ERROR)",
        "JSON_VALUE(docb, '$.zzz' ERROR ON EMPTY)",
        "JSON_EXISTS(docb, '$.a[0] ? (@ == $qqq)')",
    ] {
        let e = core
            .execute_sql(
                &alice,
                &format!("SELECT {expr} FROM {TABLE} WHERE id = 1 LIMIT 1"),
            )
            .expect_err("should fail");
        let msg = e.to_string();
        for leaked in ["zzz", "qqq", "\"x\"", "1,2,3"] {
            assert!(!msg.contains(leaked), "{expr}: {msg}");
        }
    }
}

/// `like_regex` は評価される（Issue #2677。SQL-55・TABLE-25）。
#[test]
fn like_regex_filters_in_json_exists_and_json_value() {
    let (core, _g, alice) = setup();
    assert_eq!(
        cell(
            &core,
            &alice,
            "JSON_EXISTS(docb, '$ ? (@.b.c like_regex \"^x$\")' FALSE ON ERROR)",
            1
        ),
        Cell::Bool(true)
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            "JSON_EXISTS(docb, '$ ? (@.b.c like_regex \"y\")' FALSE ON ERROR)",
            1
        ),
        Cell::Bool(false)
    );
    assert_eq!(
        cell(
            &core,
            &alice,
            "JSON_VALUE(docb, '$.b ? (@.c like_regex \"X\" flag \"i\").c')",
            1
        ),
        text("x")
    );
}
