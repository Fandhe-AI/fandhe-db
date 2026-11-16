//! text／varchar の `||` 連結と NULL 伝播（SQL-42 のポインタのみ。Issue #3276、親 #3275）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! FROM なし／列投影・NULL 伝播（`CONCAT()` との違い）・結果長上限の 54000・文脈のスモーク・
//! 未対応の組の fail-closed・jsonb `||` の不変・テナント境界を固定する。
//! 非 text オペランドの暗黙連結は兄弟 Issue #3278、拒否 SQLSTATE の統一は #3277 の担当。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run_opt(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run_opt(core, ctx, sql).unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}")) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match run_opt(core, ctx, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// `id` 昇順で先頭列のセルを集める。
fn cells0(r: &QueryResult) -> Vec<Cell> {
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    rows.iter().map(|row| row.cells[0].clone()).collect()
}

fn texts(r: &QueryResult) -> Vec<Option<String>> {
    cells0(r)
        .into_iter()
        .map(|c| match c {
            Cell::Text(s) => Some(s),
            Cell::Null => None,
            other => panic!("expected Text or Null, got {other:?}"),
        })
        .collect()
}

fn one(r: &QueryResult) -> Option<String> {
    let v = texts(r);
    assert_eq!(v.len(), 1, "expected a single row");
    v.into_iter().next().flatten()
}

fn first_ty(r: &QueryResult) -> Option<ColumnType> {
    match &r.columns[0] {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        other => panic!("expected Computed, got {other:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-text-concat");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
                ColumnDef::new("vc", ColumnType::Varchar(Some(10)), true),
                ColumnDef::new("ch", ColumnType::Bpchar(5), true),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("docb", ColumnType::Jsonb, true),
                ColumnDef::new("doc", ColumnType::Json, true),
                ColumnDef::new(
                    "ai",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Integer, engine::catalog::MAX_ARRAY_ELEMENTS)
                            .expect("array type"),
                    ),
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    // (id, lang, body, vc, ch)。id 2 は lang が NULL。
    for (id, lang, body, vc, ch) in [
        (1u64, "'ja'", "'-a'", "'v1'", "'c'"),
        (2, "NULL", "'-b'", "'v2'", "'dd'"),
        (3, "'en'", "'-c'", "'v3'", "'eee'"),
    ] {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, lang, body, vc, ch, n, docb) VALUES \
                 ({id}, '[0.{id},0.1]', {lang}, {body}, {vc}, {ch}, 1, '{{\"a\":1}}') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    // 他テナントの行（結果・エラーに現れてはならない）。
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, lang, body, vc, ch, n, docb) VALUES \
         (9, '[0.9,0.1]', 'secret', '-z', 'vz', 'z', 1, '{}') USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn from_less_literal_concat_is_text() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT 'a' || 'b'",
        "SELECT 'a' || 'b';",
        "SELECT 'a' || 'b' || ''",
    ] {
        let r = run(&core, &ctx, sql);
        assert_eq!(one(&r), s("ab"), "{sql}");
        assert_eq!(first_ty(&r), Some(ColumnType::Text), "{sql}");
    }
}

#[test]
fn column_concat_values_and_chain() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT lang || body FROM t");
    assert_eq!(texts(&r), vec![s("ja-a"), None, s("en-c")]);
    assert_eq!(first_ty(&r), Some(ColumnType::Text));
    let r = run(&core, &ctx, "SELECT lang || '-' || body FROM t");
    assert_eq!(texts(&r), vec![s("ja--a"), None, s("en--c")]);
    // リテラル先頭（構文段の先読み）・別名付き。
    let r = run(&core, &ctx, "SELECT 'x' || lang AS j FROM t");
    assert_eq!(texts(&r), vec![s("xja"), None, s("xen")]);
    // 末尾 `;` 付きの単独送信。
    let r = run(&core, &ctx, "SELECT lang || 'x' FROM t LIMIT 1;");
    assert_eq!(texts(&r).len(), 1);
}

#[test]
fn varchar_and_bpchar_operands() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT vc || lang FROM t");
    assert_eq!(texts(&r), vec![s("v1ja"), None, s("v3en")]);
    // char(n) は text へ変換される際に末尾空白が除かれる（PostgreSQL の bpchar→text と同じ）。
    let r = run(&core, &ctx, "SELECT ch || '|' FROM t");
    assert_eq!(texts(&r), vec![s("c|"), s("dd|"), s("eee|")]);
}

#[test]
fn null_propagates_unlike_concat_function() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT lang || CAST(NULL AS text) FROM t");
    assert_eq!(texts(&r), vec![None, None, None]);
    let r = run(&core, &ctx, "SELECT 'a' || CAST(NULL AS text)");
    assert_eq!(texts(&r), vec![None]);
    assert_eq!(first_ty(&r), Some(ColumnType::Text));
    // CONCAT() は NULL を空文字として扱い、非 NULL のまま。
    let r = run(&core, &ctx, "SELECT concat(lang, body) FROM t");
    assert_eq!(texts(&r), vec![s("ja-a"), s("-b"), s("en-c")]);
}

#[test]
fn concat_works_in_where_order_by_and_update() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT body FROM t WHERE lang || 'x' = 'jax'");
    assert_eq!(texts(&r), vec![s("-a")]);
    run_opt(
        &core,
        &ctx,
        "UPDATE t SET body = lang || 'x' WHERE id = 1 USING OPERATION_ID 'upd-1'",
    )
    .unwrap_or_else(|e| panic!("update should succeed: {e:?}"));
    let r = run(&core, &ctx, "SELECT body FROM t WHERE id = 1");
    assert_eq!(texts(&r), vec![s("jax")]);
}

#[test]
fn result_length_limit_rejected_with_54000() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT lpad('x', 4194304, 'y') || 'z'"),
        "54000"
    );
    let r = run(&core, &ctx, "SELECT lpad('x', 4194303, 'y') || 'z'");
    assert_eq!(one(&r).map(|v| v.len()), Some(4_194_304));
}

/// 構文・名前解決の拒否（型の組の分類とは別段）。
#[test]
fn syntax_and_name_errors_keep_their_codes() {
    let (core, ctx, _g) = setup();
    assert_eq!(code(&core, &ctx, "SELECT lang || FROM t"), "42601");
    assert_eq!(code(&core, &ctx, "SELECT lang || nope_col FROM t"), "42703");
    // 反対側の束縛エラーは型の組の判定より先に出る（json 列が左でも `42703`）。
    assert_eq!(code(&core, &ctx, "SELECT doc || nope_col FROM t"), "42703");
    assert_eq!(code(&core, &ctx, "SELECT docb || nope_col FROM t"), "42703");
    assert_eq!(code(&core, &ctx, "SELECT docb || '{' FROM t"), "22P02");
}

/// PG に演算子が無い組は、被演算子の形（リテラル・列・CAST・json 列の左右）・句の位置
/// （FROM なし・通常投影・集計リスト・WHERE の式）によらず `42883` に揃う（Issue #3277）。
#[test]
fn pairs_without_an_operator_are_42883_in_every_shape() {
    let (core, ctx, _g) = setup();
    let wrap = |expr: &str| {
        [
            format!("SELECT {expr}"),
            format!("SELECT {expr} FROM t"),
            format!("SELECT {expr}, count(*) FROM t GROUP BY lang"),
            format!("SELECT lang FROM t WHERE ({expr}) IS NOT NULL"),
        ]
    };
    for expr in [
        "1 || 2",
        "CAST(1 AS int) || CAST(2 AS int)",
        "CAST('{}' AS json) || CAST(1 AS int)",
        "CAST('{}' AS jsonb) || CAST(1 AS int)",
        "CAST('{}' AS jsonb) || CAST('{}' AS json)",
        "CAST('{}' AS json) || CAST('{}' AS json)",
    ] {
        for sql in wrap(expr) {
            assert_eq!(code(&core, &ctx, &sql), "42883", "{sql}");
        }
    }
    for sql in [
        "SELECT n || n FROM t",
        "SELECT docb || n FROM t",
        "SELECT docb || doc FROM t",
        "SELECT doc || docb FROM t",
        "SELECT doc || doc FROM t",
        "SELECT doc || n FROM t",
        "SELECT n || CAST('a' AS bytea) FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42883", "{sql}");
    }
}

/// text と非 text の組は、リテラル・列・CAST の形によらず `0A000`（暗黙連結は #3278）。
// tracked: #3278
#[test]
fn text_with_non_text_is_0a000_in_every_shape() {
    let (core, ctx, _g) = setup();
    let wrap = |expr: &str| {
        [
            format!("SELECT {expr}"),
            format!("SELECT {expr} FROM t"),
            format!("SELECT {expr}, count(*) FROM t GROUP BY lang"),
            format!("SELECT lang FROM t WHERE ({expr}) IS NOT NULL"),
        ]
    };
    for expr in [
        "'a' || 1",
        "1 || 'a'",
        "CAST('a' AS text) || 1",
        "CAST(1 AS int) || CAST('a' AS text)",
    ] {
        for sql in wrap(expr) {
            assert_eq!(code(&core, &ctx, &sql), "0A000", "{sql}");
        }
    }
    for sql in [
        "SELECT lang || 1 FROM t",
        "SELECT n || lang FROM t",
        "SELECT docb || lang FROM t",
        "SELECT lang || docb FROM t",
        "SELECT doc || lang FROM t",
        "SELECT lang || doc FROM t",
        "SELECT doc || '{}' FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
}

/// 配列・vector の `||` は PG／pgvector にあるが未実装のため `0A000`（fail-closed）。
// intentional: docs/design/json-expr-type-mapping.md
// 節「Issue #3277 での変更」の分類表（配列・bytea・vector・bit の行）を参照。
#[test]
fn array_and_vector_concat_stay_0a000() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT ARRAY[1] || '{2}'",
        "SELECT ARRAY[1] || ARRAY[2]",
        "SELECT 'x' || ARRAY[1]",
        "SELECT lang || ARRAY[1] FROM t",
        "SELECT ARRAY[1] || n FROM t",
        "SELECT embedding || embedding FROM t",
        "SELECT embedding || lang FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
}

/// 拒否の SQLSTATE と文言は静的な型だけで決まり、他テナントの行の有無で変わらない。
#[test]
fn rejection_does_not_depend_on_other_tenants_rows() {
    let (core, ctx, _g) = setup();
    let err = |ctx: &PolicyContext, sql: &str| match run_opt(&core, ctx, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => (e.wire_code().to_string(), format!("{e:?}")),
    };
    // t3 は行を持たない（空の表）。t1・t2 は行を持つ。
    for sql in [
        "SELECT lang || 1 FROM t",
        "SELECT n || n FROM t",
        "SELECT doc || n FROM t",
    ] {
        let base = err(&ctx, sql);
        assert_eq!(err(&ctx_for("t2"), sql), base, "{sql}");
        assert_eq!(err(&ctx_for("t3"), sql), base, "{sql}");
        assert!(!base.1.contains("secret"), "{sql}: {}", base.1);
    }
}

/// 集計 SELECT リストで文字列リテラルから始まる `||` も式として読める（Issue #3277）。
#[test]
fn aggregate_list_item_may_start_with_a_string_literal_concat() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT 'x' || lang, count(*) FROM t GROUP BY lang",
    );
    let mut got: Vec<Option<String>> = r
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(s) => Some(s.clone()),
            Cell::Null => None,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(got, vec![None, s("xen"), s("xja")]);
    // 他テナントの値は出ない。
    assert!(!got.iter().flatten().any(|v| v.contains("secret")));
    // GROUP BY に無い列を使う形は従来どおり拒否される。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT 'x' || body, count(*) FROM t GROUP BY lang"
        ),
        "42803"
    );
}

#[test]
fn jsonb_concat_is_unchanged() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT docb || '{\"z\":2}' FROM t WHERE id = 1",
    );
    assert_eq!(
        cells0(&r),
        vec![Cell::Json("{\"a\":1,\"z\":2}".to_string())]
    );
}

/// 左結合の jsonb `||` を多段に連ねても束縛時間が段数に対して線形に収まる
/// （オペランドを二重に束縛すると 2^段数になり、この試験は終わらない。PR #3453 指摘）。
#[test]
fn deep_jsonb_concat_chain_binds_in_linear_time() {
    let (core, ctx, _g) = setup();
    let mut sql = String::from("SELECT docb");
    for _ in 0..40 {
        sql.push_str(" || '{}'");
    }
    sql.push_str(" FROM t WHERE id = 1");
    let started = std::time::Instant::now();
    let r = run(&core, &ctx, &sql);
    assert_eq!(cells0(&r), vec![Cell::Json("{\"a\":1}".to_string())]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "jsonb || chain took too long"
    );
}

#[test]
fn tenant_boundary_holds() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT lang || 'x' FROM t");
    for v in texts(&r).into_iter().flatten() {
        assert!(!v.contains("secret"), "other tenant value leaked: {v}");
    }
    assert_eq!(texts(&r).len(), 3);
    let r = run(&core, &ctx_for("t2"), "SELECT lang || 'x' FROM t");
    assert_eq!(texts(&r), vec![s("secretx")]);
}
