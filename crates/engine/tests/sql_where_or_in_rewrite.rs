//! 同じ列への等価 `OR` を束縛時に `IN` フィルタへ書き換える最適化（Issue #1305・
//! TASK-208・SQL-24・SQL-2・RLS-10 ポインタ）の結合テスト。
//!
//! 固定する契約: (1) 同じ TEXT／ENUM 列への等価 `OR`（`IN` との混在含む）は
//! `scalar_plan: index_in_list` になり、索引経路（信頼マスク走査）を通って
//! 可視行の複製を行わない。(2) 書き換え前後（OR 形・IN 形・独立オラクル）で結果集合・
//! 順序・エラー・RLS 境界が一致する。(3) 畳めない形は従来どおり `plain_scan` に縮退し
//! 結果は不変。(4) 式述語を含む文はエラー順序保持のため畳まない。
//! `tests/sql_where_or.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn text_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("kind", ColumnType::Text, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-where-or-in-rewrite");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&text_schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql} should succeed: {e:?}"));
}

/// `lang` が `None` の行は NULL として挿入する。
fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, lang: Option<&str>, kind: &str) {
    // NULL リテラルは INSERT で受理されないため、列を省略して NULL にする。
    let (lang_col, lang_val) = lang.map_or((String::new(), String::new()), |l| {
        (", lang".to_string(), format!(", '{l}'"))
    });
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {TABLE} (id, embedding{lang_col}, kind, qty, flag) VALUES \
             ({id}, '[0.{id},0.1]'{lang_val}, '{kind}', {id}, true) USING OPERATION_ID 'seed-{id}'"
        ),
    );
}

/// 8 行: 1=a 2=b 3=c 4=d 5=a 6=NULL 7=e 8=b。
fn seed(core: &EngineCore, ctx: &PolicyContext) {
    let langs = [
        Some("a"),
        Some("b"),
        Some("c"),
        Some("d"),
        Some("a"),
        None,
        Some("e"),
        Some("b"),
    ];
    for (i, l) in langs.iter().enumerate() {
        insert(
            core,
            ctx,
            i as u64 + 1,
            *l,
            if i % 2 == 0 { "x" } else { "y" },
        );
    }
}

fn explain(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let outcome = core
        .execute_sql_in_session(ctx, &mut SessionState::default(), &format!("EXPLAIN {sql}"))
        .unwrap_or_else(|e| panic!("EXPLAIN {sql} should succeed: {e:?}"));
    match outcome {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("unexpected cell {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn scalar_plan_token(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    explain(core, ctx, sql)
        .iter()
        .find_map(|l| l.strip_prefix("scalar_plan: "))
        .and_then(|t| t.split_whitespace().next())
        .unwrap_or_else(|| panic!("no scalar_plan line for {sql}"))
        .to_string()
}

fn search_sql(predicate: &str) -> String {
    format!("SELECT id FROM {TABLE} WHERE {predicate} ORDER BY embedding <=> '[0.5,0.1]' LIMIT 100")
}

/// ランキング順を保ったままの id 列。
fn ranked_ids(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    core.execute_sql(ctx, &search_sql(predicate))
        .unwrap_or_else(|e| panic!("search {predicate:?} should succeed: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

fn scan_ids(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("scan {predicate:?} should succeed: {e:?}"));
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
}

fn count_where(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> u64 {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE {predicate}"),
        )
        .unwrap_or_else(|e| panic!("count {predicate:?} should succeed: {e:?}"));
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Integer, got {other:?}"),
    }
}

fn group_by(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<(String, u64)> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT lang, COUNT(*) FROM {TABLE} WHERE {predicate} GROUP BY lang"),
        )
        .unwrap_or_else(|e| panic!("group by {predicate:?} should succeed: {e:?}"));
    let mut rows: Vec<(String, u64)> = result
        .rows
        .iter()
        .map(|r| match (&r.cells[0], &r.cells[1]) {
            (Cell::Text(t), Cell::Integer(n)) => (t.clone(), *n),
            other => panic!("unexpected cells {other:?}"),
        })
        .collect();
    rows.sort();
    rows
}

/// OR 形・IN 形が全経路（検索・scan・COUNT・GROUP BY）で同一結果を返し、
/// 独立オラクル `expected`（scan の id 昇順）とも一致する。
fn assert_or_equals_in(
    core: &EngineCore,
    ctx: &PolicyContext,
    or_pred: &str,
    in_pred: &str,
    expected: &[u64],
) {
    assert_eq!(scan_ids(core, ctx, or_pred), expected, "scan {or_pred}");
    assert_eq!(scan_ids(core, ctx, in_pred), expected, "scan {in_pred}");
    assert_eq!(
        ranked_ids(core, ctx, or_pred),
        ranked_ids(core, ctx, in_pred),
        "ranked order {or_pred}"
    );
    let mut sorted = ranked_ids(core, ctx, or_pred);
    sorted.sort_unstable();
    assert_eq!(sorted, expected, "search set {or_pred}");
    assert_eq!(count_where(core, ctx, or_pred), expected.len() as u64);
    assert_eq!(count_where(core, ctx, in_pred), expected.len() as u64);
    assert_eq!(group_by(core, ctx, or_pred), group_by(core, ctx, in_pred));
}

// --- EXPLAIN と経路 ----------------------------------------------------------------

#[test]
fn same_column_or_explains_as_index_in_list_like_the_in_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    for pred in [
        "lang = 'a' OR lang = 'b'",
        "lang = 'a' OR lang IN ('b', 'c')",
        "lang IN ('a') OR lang IN ('b')",
        "lang = 'a' OR lang = 'a'",
        "lang = 'a' OR lang = 'b' OR lang = 'c'",
    ] {
        assert_eq!(
            scalar_plan_token(&core, &ctx, &search_sql(pred)),
            "index_in_list",
            "{pred}"
        );
    }
    assert_eq!(
        explain(&core, &ctx, &search_sql("lang = 'a' OR lang = 'b'")),
        explain(&core, &ctx, &search_sql("lang IN ('a', 'b')"))
    );
}

#[test]
fn rewritten_or_takes_the_index_path_without_row_copies() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let sql = search_sql("lang = 'a' OR lang = 'b'");
    // 1 回目はキャッシュを温める。
    core.execute_sql(&ctx, &sql).expect("warm-up");
    let (i0, c0) = (
        core.scalar_index_cache_stats(),
        core.sql_arena_cache_stats(),
    );
    core.execute_sql(&ctx, &sql).expect("measured");
    let (i1, c1) = (
        core.scalar_index_cache_stats(),
        core.sql_arena_cache_stats(),
    );
    assert!(
        i1.index_scans > i0.index_scans
            || i1.index_trusted_mask_scans > i0.index_trusted_mask_scans
    );
    assert_eq!(c1.full_rebuild_copies, c0.full_rebuild_copies);
}

// --- 結果の同値 ----------------------------------------------------------------------

#[test]
fn rewritten_or_matches_in_form_and_oracle_on_all_paths() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    assert_or_equals_in(
        &core,
        &ctx,
        "lang = 'a' OR lang = 'b'",
        "lang IN ('a', 'b')",
        &[1, 2, 5, 8],
    );
    assert_or_equals_in(
        &core,
        &ctx,
        "lang = 'a' OR lang IN ('c', 'e')",
        "lang IN ('a', 'c', 'e')",
        &[1, 3, 5, 7],
    );
    // 重複リテラルは単一値と同じ。NULL 行（id=6）はどちらの形でも不一致。
    assert_or_equals_in(
        &core,
        &ctx,
        "lang = 'a' OR lang = 'a'",
        "lang = 'a'",
        &[1, 5],
    );
    // 値が存在しない OR は 0 件。
    assert_or_equals_in(
        &core,
        &ctx,
        "lang = 'zz' OR lang = 'yy'",
        "lang IN ('zz', 'yy')",
        &[],
    );
    // 他の AND 条件との併用。
    assert_or_equals_in(
        &core,
        &ctx,
        "kind = 'x' AND (lang = 'a' OR lang = 'b')",
        "kind = 'x' AND lang IN ('a', 'b')",
        &[1, 5],
    );
}

#[test]
fn predicate_delete_and_update_with_rewritten_or() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let out = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!("UPDATE {TABLE} SET kind = 'z' WHERE lang = 'a' OR lang = 'c' USING OPERATION_ID 'op-u'"),
        )
        .expect("update");
    match out {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 3),
        other => panic!("expected Update, got {other:?}"),
    }
    assert_eq!(scan_ids(&core, &ctx, "kind = 'z'"), vec![1, 3, 5]);
    let out = core
        .execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "DELETE FROM {TABLE} WHERE lang = 'b' OR lang = 'e' USING OPERATION_ID 'op-d'"
            ),
        )
        .expect("delete");
    match out {
        SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 3),
        other => panic!("expected Delete, got {other:?}"),
    }
    assert_eq!(
        scan_ids(&core, &ctx, "lang IN ('b', 'e')"),
        Vec::<u64>::new()
    );
}

// --- RLS ---------------------------------------------------------------------------

#[test]
fn rewritten_or_respects_tenant_boundary() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    // 同じ lang 値を 2 テナントへ置く（id は共有空間のため重ならない値を使う）。
    for (id, lang) in [(1u64, "a"), (2, "b"), (3, "c")] {
        insert(&core, &a, id, Some(lang), "x");
    }
    for (id, lang) in [(11u64, "a"), (12, "b"), (13, "d")] {
        insert(&core, &b, id, Some(lang), "x");
    }
    assert_or_equals_in(
        &core,
        &a,
        "lang = 'a' OR lang = 'b'",
        "lang IN ('a', 'b')",
        &[1, 2],
    );
    assert_or_equals_in(
        &core,
        &b,
        "lang = 'a' OR lang = 'b'",
        "lang IN ('a', 'b')",
        &[11, 12],
    );
    // Public のみの可視集合でも、他テナント行が混ざらない。
    let a_pub = PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("ctx");
    let pub_or = scan_ids(&core, &a_pub, "lang = 'a' OR lang = 'b'");
    let pub_in = scan_ids(&core, &a_pub, "lang IN ('a', 'b')");
    assert_eq!(pub_or, pub_in);
    assert!(pub_or.iter().all(|id| *id < 10));
}

// --- 上限 --------------------------------------------------------------------------

#[test]
fn merged_value_limit_keeps_plain_scan_but_same_result() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let list = |n: usize| {
        (0..n)
            .map(|i| format!("'v{i:04}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // 255 + 1 = 256 件ちょうどは畳む。
    let ok = format!("lang IN ({}) OR lang = 'a'", list(255));
    assert_eq!(
        scalar_plan_token(&core, &ctx, &search_sql(&ok)),
        "index_in_list"
    );
    assert_eq!(scan_ids(&core, &ctx, &ok), vec![1, 5]);
    // 256 + 1 = 257 件は畳まず plain_scan のまま。結果は正しい。
    let over = format!("lang IN ({}) OR lang = 'a'", list(256));
    assert_eq!(
        scalar_plan_token(&core, &ctx, &search_sql(&over)),
        "plain_scan"
    );
    assert_eq!(scan_ids(&core, &ctx, &over), vec![1, 5]);
}

// --- 縮退のまま ----------------------------------------------------------------------

#[test]
fn non_foldable_ors_stay_plain_scan_with_unchanged_results() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let cases: [(&str, Vec<u64>); 7] = [
        ("lang = 'a' OR kind = 'y'", vec![1, 2, 4, 5, 6, 8]),
        ("lang = 'a' OR lang LIKE 'b%'", vec![1, 2, 5, 8]),
        ("lang = 'a' OR lang IS NULL", vec![1, 5, 6]),
        ("lang = 'a' OR NOT lang = 'b'", vec![1, 3, 4, 5, 7]),
        ("lang = 'a' OR lang < 'b'", vec![1, 5]),
        ("flag = true OR flag = false", vec![1, 2, 3, 4, 5, 6, 7, 8]),
        (
            "lang = 'a' OR (lang = 'b' AND kind = 'y')",
            vec![1, 2, 5, 8],
        ),
    ];
    for (pred, expected) in cases {
        assert_eq!(
            scalar_plan_token(&core, &ctx, &search_sql(pred)),
            "plain_scan",
            "{pred}"
        );
        assert_eq!(scan_ids(&core, &ctx, pred), expected, "{pred}");
    }
}

// --- エラー同値のゲート ------------------------------------------------------------------

#[test]
fn expression_predicates_keep_or_unfolded_and_error_unchanged() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let with_expr = "(lang = 'a' OR lang = 'b') AND qty / 0 > 1";
    assert_eq!(
        scalar_plan_token(&core, &ctx, &search_sql(with_expr)),
        "plain_scan"
    );
    let expr_first = "qty / 0 > 1 AND (lang = 'a' OR lang = 'b')";
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE {expr_first} LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22012");
    let err = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE {with_expr} LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22012");
}

#[test]
fn unknown_column_error_is_identical_for_or_and_in_forms() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let code = |pred: &str| {
        core.execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 10"),
        )
        .unwrap_err()
        .wire_code()
    };
    assert_eq!(code("nope = 'a' OR nope = 'b'"), code("nope IN ('a', 'b')"));
}

// --- HINT ORDER ---------------------------------------------------------------------------

#[test]
fn hint_order_distance_first_stays_plain_scan_with_same_results() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let sql = |p: &str| format!("{} HINT ORDER(DISTANCE, SCALAR, RLS)", search_sql(p));
    assert_eq!(
        scalar_plan_token(&core, &ctx, &sql("lang = 'a' OR lang = 'b'")),
        "plain_scan"
    );
    let ids = |p: &str| -> Vec<u64> {
        core.execute_sql(&ctx, &sql(p))
            .expect("hint order search")
            .rows
            .iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids("lang = 'a' OR lang = 'b'"), ids("lang IN ('a', 'b')"));
}

// --- JOIN・IN サブクエリ（1 分岐ラッパーの回帰） -------------------------------------------------

#[test]
fn in_subquery_results_are_unchanged() {
    // `IN (SELECT ...)` は 1 チャンクでも `Or` 1 分岐で包まれる（`sql::subquery`）。
    // 書き換えは分岐 2 個以上のみ対象のため形状・結果とも不変であることを固定する。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let ids = |sub: &str| scan_ids(&core, &ctx, &format!("lang IN ({sub})"));
    assert_eq!(
        ids("SELECT lang FROM docs WHERE id = 1 LIMIT 10"),
        vec![1, 5]
    );
    assert_eq!(
        ids("SELECT lang FROM docs WHERE id <= 5 LIMIT 10"),
        vec![1, 2, 3, 4, 5, 8]
    );
}

// --- ENUM 列・Describe -------------------------------------------------------------------

fn new_enum_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-where-or-in-rewrite-enum");
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(
            "mood",
            vec!["happy".to_string(), "sad".to_string(), "calm".to_string()],
        )
        .expect("create enum");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("m", ColumnType::Enum(def), true),
            ],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

#[test]
fn enum_column_or_folds_and_matches_in_form() {
    let (core, path) = new_enum_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for (id, m) in [(1u64, "happy"), (2, "sad"), (3, "calm"), (4, "happy")] {
        run(
            &core,
            &ctx,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, m) VALUES ({id}, '[0.{id},0.1]', '{m}') USING OPERATION_ID 'seed-{id}'"
            ),
        );
    }
    let or_pred = "m = 'happy' OR m = 'sad'";
    assert_eq!(
        scalar_plan_token(&core, &ctx, &search_sql(or_pred)),
        "index_in_list"
    );
    assert_eq!(
        scan_ids(&core, &ctx, or_pred),
        scan_ids(&core, &ctx, "m IN ('happy', 'sad')")
    );
    assert_eq!(scan_ids(&core, &ctx, or_pred), vec![1, 2, 4]);
    // 語彙外ラベルは OR 形・IN 形とも同じ 22P02。
    let code = |pred: &str| {
        core.execute_sql(
            &ctx,
            &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 10"),
        )
        .unwrap_err()
        .wire_code()
    };
    assert_eq!(code("m = 'happy' OR m = 'bogus'"), "22P02");
    assert_eq!(code("m IN ('happy', 'bogus')"), "22P02");
}

#[test]
fn describe_of_enum_or_with_params_does_not_fail() {
    let (core, path) = new_enum_core();
    let _guard = CleanupGuard(path);
    let session = SessionState::default();
    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {TABLE} WHERE m = $1 OR m = $2 LIMIT 5"
        ))
        .expect("parse_sql_prepared");
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe must not fail with 22P02 on dummy labels");
}

#[test]
fn prepared_or_params_execute_like_the_literal_form() {
    // `$n` は実値のリテラルへ置換されてから再束縛される（`sql::params`）ため、
    // 畳み込み後も OR 形のリテラルと同じ結果になることを固定する。
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let prepared = core
        .parse_sql_prepared(&format!(
            "SELECT id FROM {TABLE} WHERE lang = $1 OR lang = $2 LIMIT 100"
        ))
        .expect("parse_sql_prepared");
    let bound = core
        .bind_prepared(&prepared, &[Some(b"a".to_vec()), Some(b"c".to_vec())])
        .expect("bind_prepared");
    let outcome = core
        .execute_parsed_in_session(&ctx, &mut SessionState::default(), &bound)
        .expect("execute");
    let mut ids: Vec<u64> = match outcome {
        SqlOutcome::Query(r) => r.rows.iter().map(|row| row.id).collect(),
        other => panic!("expected Query, got {other:?}"),
    };
    ids.sort_unstable();
    assert_eq!(ids, scan_ids(&core, &ctx, "lang = 'a' OR lang = 'c'"));
    assert_eq!(ids, vec![1, 3, 5]);
}
