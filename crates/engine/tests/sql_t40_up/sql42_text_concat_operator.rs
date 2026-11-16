//! text／varchar の `||` 連結と NULL 伝播（SQL-42 のポインタのみ。Issue #3276、親 #3275）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! FROM なし／列投影・NULL 伝播（`CONCAT()` との違い）・結果長上限の 54000・文脈のスモーク・
//! 未対応の組の fail-closed・jsonb `||` の不変・テナント境界を固定する。
//! 非 text オペランドの暗黙連結（Issue #3278。`CAST(.. AS text)` と同じ変換）・曖昧な型の組の
//! fail-closed・多段連鎖の線形束縛もここで固定する。非 text 同士の拒否 SQLSTATE の統一は #3277 の担当。

use engine::catalog::{
    ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema, MAX_ARRAY_ELEMENTS,
};
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
                ColumnDef::new("f", ColumnType::Double, true),
                ColumnDef::new(
                    "nm",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("d", ColumnType::Date, true),
                ColumnDef::new("tz", ColumnType::TimestampTz, true),
                ColumnDef::new("u", ColumnType::Uuid, true),
                ColumnDef::new("j", ColumnType::Json, true),
                ColumnDef::new("bt", ColumnType::Bytea, true),
                ColumnDef::new(
                    "arr",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Integer, MAX_ARRAY_ELEMENTS)
                            .expect("array type"),
                    ),
                    true,
                ),
                ColumnDef::new("doc", ColumnType::Json, true),
                ColumnDef::new(
                    "ai",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Integer, engine::catalog::MAX_ARRAY_ELEMENTS)
                            .expect("array type"),
                    ),
                    true,
                ),
                ColumnDef::new(
                    "bv",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Bit(4), engine::catalog::MAX_ARRAY_ELEMENTS)
                            .expect("bit array type"),
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
                "INSERT INTO t (id, embedding, lang, body, vc, ch, n, docb, f, nm, flag, d, tz, u, \
                 j, bt, arr) VALUES \
                 ({id}, '[0.{id},0.1]', {lang}, {body}, {vc}, {ch}, 1, '{{\"a\":1}}', \
                 {id}.5, {id}.25, TRUE, '2026-01-0{id}', '2026-01-0{id} 10:00:00+00', \
                 '00000000-0000-0000-0000-00000000000{id}', '{{\"k\": {id}}}', '\\x0{id}', \
                 '{{1,{id}}}') USING OPERATION_ID 'seed-{id}'"
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
// intentional: docs/design/json-expr-type-mapping.md
#[test]
fn syntax_and_name_errors_keep_their_codes() {
    let (core, ctx, _g) = setup();
    // jsonb と型付き text の組・自前の `||` を持つ型との組は曖昧なため拒否のまま（Issue #3278）。
    for sql in [
        "SELECT docb || lang FROM t",
        "SELECT lang || docb FROM t",
        "SELECT embedding || 'x' FROM t",
        // 種別を静的に決められない数値は CAST と同じく拒否。
        "SELECT 'x' || id FROM t",
        "SELECT 'x' || length(lang) FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
    // PG に演算子が無い組は 42883（#3277）。
    assert_eq!(code(&core, &ctx, "SELECT n || n FROM t"), "42883");
    assert_eq!(code(&core, &ctx, "SELECT lang || FROM t"), "42601");
    assert_eq!(code(&core, &ctx, "SELECT lang || nope_col FROM t"), "42703");
    // CAST が拒否する形と一致する。
    assert_eq!(code(&core, &ctx, "SELECT CAST(id AS text) FROM t"), "0A000");
    // 反対側の束縛エラーは型の組の判定より先に出る（json 列が左でも `42703`）。
    assert_eq!(code(&core, &ctx, "SELECT doc || nope_col FROM t"), "42703");
    assert_eq!(code(&core, &ctx, "SELECT docb || nope_col FROM t"), "42703");
    assert_eq!(code(&core, &ctx, "SELECT docb || '{' FROM t"), "22P02");
}

#[test]
fn non_text_operands_concat_like_cast() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT 'n=' || 5");
    assert_eq!(one(&r), s("n=5"));
    assert_eq!(first_ty(&r), Some(ColumnType::Text));
    for (sql, want) in [
        ("SELECT 'n=' || 5;", "n=5"),
        ("SELECT 5 || 'x'", "5x"),
        ("SELECT 'x' || 1.5", "x1.5"),
    ] {
        assert_eq!(one(&run(&core, &ctx, sql)), s(want), "{sql}");
    }
    let r = run(&core, &ctx, "SELECT lang || n FROM t");
    assert_eq!(texts(&r), vec![s("ja1"), None, s("en1")]);
    let r = run(&core, &ctx, "SELECT n || lang FROM t");
    assert_eq!(texts(&r), vec![s("1ja"), None, s("1en")]);
    let r = run(&core, &ctx, "SELECT lang || '-' || n || '-' || body FROM t");
    assert_eq!(texts(&r), vec![s("ja-1--a"), None, s("en-1--c")]);
    // 型ごとに `CAST(c AS text)` と同じ値になる。
    for c in ["n", "f", "nm", "flag", "d", "tz", "u", "j"] {
        let lhs = run(&core, &ctx, &format!("SELECT 'x' || {c} FROM t"));
        let rhs = run(
            &core,
            &ctx,
            &format!("SELECT 'x' || CAST({c} AS text) FROM t"),
        );
        assert_eq!(texts(&lhs), texts(&rhs), "column {c}");
        assert_eq!(texts(&lhs).len(), 3, "column {c}");
        assert!(texts(&lhs).iter().all(Option::is_some), "column {c}");
    }
    // json は原文のまま text 連結になる。
    let r = run(&core, &ctx, "SELECT j || 'x' FROM t");
    assert_eq!(texts(&r)[0], s("{\"k\": 1}x"));
    // NULL は NULL。
    let r = run(&core, &ctx, "SELECT CAST(NULL AS integer) || 'x'");
    assert_eq!(texts(&r), vec![None]);
}

#[test]
fn timestamptz_concat_follows_session_time_zone() {
    let (core, ctx, _g) = setup();
    let mut sess = SessionState::default();
    for zone in ["UTC", "Asia/Tokyo"] {
        core.execute_sql_in_session(&ctx, &mut sess, &format!("SET TIME ZONE '{zone}'"))
            .unwrap_or_else(|e| panic!("set time zone: {e:?}"));
        let q = |sql: &str, sess: &mut SessionState| match core
            .execute_sql_in_session(&ctx, sess, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        {
            SqlOutcome::Query(r) => texts(&r),
            other => panic!("expected Query, got {other:?}"),
        };
        let a = q("SELECT 'x' || tz FROM t", &mut sess);
        let b = q("SELECT 'x' || CAST(tz AS text) FROM t", &mut sess);
        assert_eq!(a, b, "{zone}");
    }
}

#[test]
fn deep_chain_with_non_text_leaves_binds_in_linear_time() {
    let (core, ctx, _g) = setup();
    let mut sql = String::from("SELECT lang");
    for i in 0..40 {
        sql.push_str(&format!(" || {i}"));
    }
    sql.push_str(" FROM t WHERE id = 1");
    let started = std::time::Instant::now();
    let r = run(&core, &ctx, &sql);
    let want: String = (0..40).map(|i| i.to_string()).collect();
    assert_eq!(one(&r), Some(format!("ja{want}")));
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[test]
fn non_text_concat_in_where_update_and_limit() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT body FROM t WHERE 'n=' || n = 'n=1' AND id = 1",
    );
    assert_eq!(texts(&r), vec![s("-a")]);
    run_opt(
        &core,
        &ctx,
        "UPDATE t SET body = 'n=' || n WHERE id = 1 USING OPERATION_ID 'upd-3278'",
    )
    .unwrap_or_else(|e| panic!("update should succeed: {e:?}"));
    let r = run(&core, &ctx, "SELECT body FROM t WHERE id = 1");
    assert_eq!(texts(&r), vec![s("n=1")]);
    assert_eq!(
        code(&core, &ctx, "SELECT lpad('x', 4194304, 'y') || 1"),
        "54000"
    );
    let r = run(&core, &ctx, "SELECT lpad('x', 4194303, 'y') || 1");
    assert_eq!(one(&r).map(|v| v.len()), Some(4_194_304));
}

#[test]
fn non_text_concat_in_insert_values() {
    let (core, ctx, _g) = setup();
    // リテラルの直後の `||` も INSERT VALUES では式として読む（Issue #3278）。
    run_opt(
        &core,
        &ctx,
        "INSERT INTO t (id, embedding, lang, body, vc, ch, n, docb) VALUES \
         (7, '[0.7,0.1]', 'n=' || 5, 5 || 'x', 'v7', 'c', 1, '{}') USING OPERATION_ID 'ins-3278'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    let r = run(&core, &ctx, "SELECT lang FROM t WHERE id = 7");
    assert_eq!(texts(&r), vec![s("n=5")]);
    let r = run(&core, &ctx, "SELECT body FROM t WHERE id = 7");
    assert_eq!(texts(&r), vec![s("5x")]);
}

#[test]
fn non_text_concat_respects_tenant_boundary() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT lang || n FROM t");
    assert_eq!(texts(&r).len(), 3);
    for v in texts(&r).into_iter().flatten() {
        assert!(!v.contains("secret"), "other tenant value leaked: {v}");
    }
    let r = run(&core, &ctx_for("t2"), "SELECT lang || n FROM t");
    assert_eq!(texts(&r), vec![s("secret1")]);
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

/// text と jsonb の組（曖昧な組）は、被演算子の形によらず `0A000`。text と変換できる非 text の組は
/// 連結になる（Issue #3278）。
// intentional: docs/design/json-expr-type-mapping.md
// 節「Issue #3278 での変更」の拒否する組の項を参照。
#[test]
fn text_with_jsonb_is_0a000_in_every_shape() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT docb || lang FROM t",
        "SELECT lang || docb FROM t",
        "SELECT docb || CAST('a' AS text) FROM t",
        "SELECT lang FROM t WHERE (docb || lang) IS NOT NULL",
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
        "SELECT ARRAY[1] || n FROM t",
        "SELECT ARRAY['a'] || lang FROM t",
        "SELECT embedding || embedding FROM t",
        "SELECT embedding || lang FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
}

/// 要素型が互換しない配列の `||` は PG に演算子が無く `42883`（解決できる組だけ `0A000`）。
// intentional: docs/design/json-expr-type-mapping.md
// 節「Issue #3277 での変更」の分類表（配列を含む組の行）を参照。
#[test]
fn incompatible_array_concat_is_42883() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT ARRAY[1] || ARRAY[CAST(1 AS boolean)]",
        "SELECT ARRAY[CAST(1 AS boolean)] || ARRAY[1]",
        "SELECT ARRAY[1] || CAST(1 AS boolean)",
        "SELECT lang || ARRAY[1] FROM t",
        "SELECT ARRAY[1] || lang FROM t",
        "SELECT doc || ARRAY[1] FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42883", "{sql}");
    }
}

/// 同じ要素型の json／jsonb／bytea 配列とスカラー、timestamp と timestamptz の配列連結は
/// PG で成立するため `0A000`（未実装）。要素型が違う組は `42883`。
// intentional: docs/design/json-expr-type-mapping.md
// 節「Issue #3277 での変更」の分類表（配列を含む組の行）を参照。
#[test]
fn compatible_array_concat_of_special_scalars_is_0a000() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT ARRAY[CAST('{}' AS jsonb)] || CAST('{}' AS jsonb)",
        "SELECT CAST('{}' AS jsonb) || ARRAY[CAST('{}' AS jsonb)]",
        "SELECT ARRAY[CAST('a' AS bytea)] || CAST('a' AS bytea)",
        "SELECT ARRAY[CAST('2024-01-01 00:00:00' AS timestamp)] || CAST('2024-01-01 00:00:00+00' AS timestamptz)",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
    for sql in [
        "SELECT ARRAY[CAST('a' AS bytea)] || CAST('{}' AS jsonb)",
        "SELECT ARRAY[1] || CAST('a' AS bytea)",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42883", "{sql}");
    }
}

/// BIT スカラーと配列の `||` は、BIT 配列を要素型つきで区別して判定する（Issue #3277 の PR 指摘）。
/// BIT 配列の値そのものは式の中で未解禁（`ARRAY[bit]` の構築・配列列の裸参照は fail-closed）のため、
/// 現状で到達できる組（BIT スカラーと非 BIT 配列は `42883`）と、未解禁の BIT 配列の構築が `0A000` の
/// ままであることを固定する。BIT 配列どうしの互換判定は `concat_array_pair_compatible` が担う。
// intentional: docs/design/json-expr-type-mapping.md
// 節「Issue #3277 での変更」の分類表（配列を含む組の行）を参照。
#[test]
fn bit_scalar_with_array_concat_sqlstate_is_consistent_across_operand_order() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT bv[1] || ARRAY[1] FROM t",
        "SELECT ARRAY[1] || bv[1] FROM t",
        "SELECT bv[1] || ARRAY['a'] FROM t",
        "SELECT ARRAY['a'] || bv[1] FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42883", "{sql}");
    }
    for sql in [
        "SELECT ARRAY[bv[1]] || ARRAY[1] FROM t",
        "SELECT bv[1] || ARRAY[bv[1]] FROM t",
        "SELECT ARRAY[bv[1]] || bv[1] FROM t",
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
        "SELECT lang || docb FROM t",
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
    // jsonb と数値は演算子なし（非 text 同士の組。統一は #3277）。
    assert_eq!(code(&core, &ctx, "SELECT docb || n FROM t"), "42883");
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
