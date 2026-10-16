//! jsonb の jsonpath 演算子 `@?`・`@@`（対象ビヘイビア: TABLE-25。Issue #2186）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-25。
//!
//! 固定する契約: 演算子の結果が silent=true の関数形（`jsonb_path_exists`／`jsonb_path_match`
//! の第 4 引数 true・vars なし）と常に一致すること、抑止されないエラー（`54000`・`42704`）、
//! NULL の扱い、使える位置（投影・WHERE）、拒否される位置（NOT・CHECK）、型エラー、
//! 束縛時の path 構文エラー、テナント境界。期待値の PostgreSQL 17 実機との
//! 突き合わせは未実施。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::jsonpath_limits::JsonPathLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "jpops";
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

/// `docs` の各要素を id = 1.. で挿入する（`None` は docb が NULL）。
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

const DOCS: [Option<&str>; 6] = [
    Some(DOC),
    None,
    Some(r#"[1,2,3]"#),
    Some(r#"42"#),
    Some(r#"{"a":"s","b":[{"k":1},{"k":5}]}"#),
    Some(r#"{}"#),
];

fn setup() -> (EngineCore, CleanupGuard, PolicyContext) {
    let (core, path) = open_core("json-jsonpath-operators", None);
    let guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed(&core, &alice, &DOCS);
    (core, guard, alice)
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql(ctx, sql)
        .expect_err("statement should fail")
        .wire_code()
        .to_string()
}

fn err_code(core: &EngineCore, ctx: &PolicyContext, expr: &str, id: u64) -> String {
    code(
        core,
        ctx,
        &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
    )
}

/// `expr` を id 1..=n の全行で評価し、行ごとの結果（または SQLSTATE）を返す。
fn per_row(core: &EngineCore, ctx: &PolicyContext, expr: &str) -> Vec<Result<Cell, String>> {
    (1..=DOCS.len() as u64)
        .map(|id| {
            core.execute_sql(
                ctx,
                &format!("SELECT {expr} FROM {TABLE} WHERE id = {id} LIMIT 1"),
            )
            .map(|r| {
                assert_eq!(r.rows.len(), 1);
                r.rows[0].cells[0].clone()
            })
            .map_err(|e| e.wire_code().to_string())
        })
        .collect()
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

const PATHS: [&str; 14] = [
    "$.a",
    "$.zzz",
    "strict $.zzz",
    "lax $.a[*]",
    "strict $.a[*]",
    "$.a[*] ? (@ > 1)",
    "$.a[0] == 1",
    "$.a[0] > 5",
    "$.n",
    "$.b[*].k ? (@ > 2)",
    "$.a + 1",
    "$[0]",
    "$ == 42",
    "strict $.a.b",
];

/// 受け入れ条件の本体: 演算子は silent=true の関数形と全行・全 path で一致する。
#[test]
fn operators_match_the_silent_function_forms() {
    let (core, _g, alice) = setup();
    for p in PATHS {
        let op_e = per_row(&core, &alice, &format!("docb @? '{p}'"));
        let fn_e = per_row(
            &core,
            &alice,
            &format!("jsonb_path_exists(docb, '{p}', '{{}}', true)"),
        );
        assert_eq!(op_e, fn_e, "@? {p}");
        let op_m = per_row(&core, &alice, &format!("docb @@ '{p}'"));
        let fn_m = per_row(
            &core,
            &alice,
            &format!("jsonb_path_match(docb, '{p}', '{{}}', true)"),
        );
        assert_eq!(op_m, fn_m, "@@ {p}");
    }
}

#[test]
fn representative_values() {
    let (core, _g, alice) = setup();
    let one = |e: &str, id: usize| per_row(&core, &alice, e)[id - 1].clone();
    assert_eq!(one("docb @? '$.a'", 1), Ok(Cell::Bool(true)));
    assert_eq!(one("docb @? '$.zzz'", 1), Ok(Cell::Bool(false)));
    // strict のキー欠落のエラーは silent で抑止され NULL（関数形と同じ）。
    assert_eq!(one("docb @? 'strict $.zzz'", 1), Ok(Cell::Null));
    assert_eq!(one("docb @@ '$.a[0] == 1'", 1), Ok(Cell::Bool(true)));
    assert_eq!(one("docb @@ '$.a[0] > 5'", 1), Ok(Cell::Bool(false)));
    // 非 bool の結果は silent で NULL。
    assert_eq!(one("docb @@ '$.a[*]'", 1), Ok(Cell::Null));
    // NULL 行は結果 NULL。
    assert_eq!(one("docb @? '$.a'", 2), Ok(Cell::Null));
}

#[test]
fn unsuppressed_errors_match_the_function_form() {
    let (core, _g, alice) = setup();
    // 未定義変数は silent でも 42704（関数形と同じ）。
    assert_eq!(err_code(&core, &alice, "docb @? '$x'", 1), "42704");
    assert_eq!(
        err_code(
            &core,
            &alice,
            "jsonb_path_exists(docb, '$x', '{}', true)",
            1
        ),
        "42704"
    );
}

#[test]
fn startup_limits_apply_to_operators() {
    let alice = ctx_for("alice");
    let (small, path) = open_core(
        "json-jsonpath-operators-small",
        Some(JsonPathLimits::new(5, 4)),
    );
    let _g = CleanupGuard(path);
    seed(&small, &alice, &[Some(DOC)]);
    assert_eq!(err_code(&small, &alice, "docb @? '$.**.**.**'", 1), "54000");
    assert_eq!(
        err_code(&small, &alice, "docb @@ '$.**.**.** == 1'", 1),
        "54000"
    );
}

#[test]
fn usable_in_where_and_projection() {
    let (core, _g, alice) = setup();
    assert_eq!(matching(&core, &alice, "docb @? '$.a'"), vec![1, 5]);
    assert_eq!(
        matching(&core, &alice, "docb @@ '$.a[0] == 1' AND n < 10"),
        vec![1]
    );
    assert_eq!(
        matching(&core, &alice, "(docb @? '$.b' OR docb @? '$[0]') AND n > 0"),
        vec![1, 3, 4, 5, 6]
    );
    assert_eq!(
        matching(&core, &alice, "docb -> 'b' @? '$.c'"),
        vec![1],
        "left associative: (docb -> 'b') @? '$.c'"
    );
}

#[test]
fn rejected_positions_fail_closed() {
    let (core, _g, alice) = setup();
    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE NOT docb @? '$.a' LIMIT 10")
        ),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            &alice,
            "CREATE TABLE ck (embedding VECTOR(2), docb JSONB, CHECK (docb @? '$.a'))"
        ),
        "42601"
    );
    // 未知の組み合わせは字句・構文段で拒否される。
    assert_eq!(
        code(
            &core,
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE docb @?| '$.a' LIMIT 10")
        ),
        "42601"
    );
}

#[test]
fn operand_type_errors_are_42883() {
    let (core, _g, alice) = setup();
    for e in [
        "doc @? '$.a'",
        "lang @? '$.a'",
        "n @? '$.a'",
        "docb @? lang",
        "docb @? n",
        "docb @@ (1 + 1)",
    ] {
        assert_eq!(err_code(&core, &alice, e, 1), "42883", "{e}");
    }
}

#[test]
fn path_syntax_errors_occur_at_bind_time() {
    let (core, _g, alice) = setup();
    let q = format!("SELECT docb @? '$.a[' FROM {TABLE} WHERE id = 999 LIMIT 1");
    let op_code = code(&core, &alice, &q);
    let fn_code = code(
        &core,
        &alice,
        &format!("SELECT jsonb_path_exists(docb, '$.a[') FROM {TABLE} WHERE id = 999 LIMIT 1"),
    );
    assert_eq!(op_code, fn_code);
}

#[test]
fn other_tenants_rows_never_leak() {
    let (core, _g, alice) = setup();
    let bob = ctx_for("bob");
    assert_eq!(matching(&core, &bob, "docb @? '$.a'"), Vec::<i64>::new());
    assert_eq!(matching(&core, &alice, "docb @? '$.a'"), vec![1, 5]);
}
