//! `GROUP BY` の `ROLLUP`・`CUBE`・`GROUPING SETS` の実行と結果合成の結合テスト
//! （Issue #1822、対象ビヘイビア: SQL-59。ポインタ: `docs/spec/04-behavior/` の SQL-59）。
//!
//! `EngineCore::execute_sql`（wire-server と NoSQL 表層が共有する production 経路）で、
//! (1) セットごとの集約と、セットに含まれないキー列の NULL 出力、(2) 小計 NULL とデータ上の
//! NULL が別の行として出ること、(3) 空入力でも総計行が出ること、(4) RLS（他テナントの行・
//! キーがどのセットにも現れない）、(5) グループ数上限が全セット合計に効くこと、(6) 未対応の
//! 文脈（ビュー外側集計）が fail-closed のままであること、を固定する。期待値は PostgreSQL 17
//! の意味論で手計算した固定値。`GROUPING()` 関数は #1823、他機能との網羅的な併用は #1824 の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("kind", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// tenant-a は 6 行（`lang` が NULL の行・同じ (lang, kind) の重複行を含む）、tenant-b は Private で（他テナントから不可視）
/// tenant-a に無い `lang` 値（`fr`）を持つ 2 行。
fn build(name: &str, limits: Option<GroupByLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let rows: [(&str, u64, Option<&str>, &str, i32); 8] = [
        ("tenant-a", 1, Some("ja"), "a", 1),
        ("tenant-a", 2, Some("ja"), "a", 2),
        ("tenant-a", 3, Some("ja"), "b", 3),
        ("tenant-a", 4, Some("en"), "a", 4),
        ("tenant-a", 5, None, "a", 5),
        ("tenant-a", 6, None, "b", 6),
        ("tenant-b", 7, Some("fr"), "a", 100),
        ("tenant-b", 8, Some("ja"), "a", 1000),
    ];
    for (tenant, id, lang, kind, n) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            if tenant == "tenant-a" {
                Visibility::Public
            } else {
                Visibility::Private
            },
            &[
                Value::Vector(vec![id as f32, 0.0]),
                match lang {
                    Some(l) => Value::Text(l.to_string()),
                    None => Value::Null,
                },
                Value::Text(kind.to_string()),
                Value::Integer(n),
            ],
            &OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_group_by_limits(l),
        None => core,
    };
    (core, guard)
}

fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(i) => i.to_string(),
        Cell::SignedInteger(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

/// 結果行を `|` 区切りの文字列へ（行の順序を保つ）。
fn rows_as_text(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let result = core
        .execute_sql(&ctx_for(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(cell_text).collect::<Vec<_>>().join("|"))
        .collect()
}

/// 結果行の集合を比較するための昇順ソート版（結果行の順序は契約にしない）。
fn sorted_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut rows = rows_as_text(core, tenant, sql);
    rows.sort();
    rows
}

fn expect_sorted(mut expected: Vec<&str>) -> Vec<String> {
    expected.sort();
    expected.into_iter().map(str::to_string).collect()
}

fn err_code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    core.execute_sql(&ctx_for(tenant), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

#[test]
fn rollup_emits_subtotals_and_grand_total_with_nulls() {
    let (core, _g) = build("sql59-exec-rollup", None);
    let got = sorted_rows(
        &core,
        "tenant-a",
        "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP(lang, kind)",
    );
    // (lang, kind) 5 行・(lang) 3 行・() 1 行。`lang` がデータ上 NULL の行（NULL|a|1 等）と
    // 小計・総計の NULL は別の行として出る（NULL|NULL|2 は lang=NULL の小計、NULL|NULL|6 が総計）。
    assert_eq!(
        got,
        expect_sorted(vec![
            "en|a|1",
            "ja|a|2",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1",
            "ja|NULL|3",
            "NULL|NULL|2",
            "NULL|NULL|6",
        ])
    );
}

#[test]
fn cube_emits_every_subset() {
    let (core, _g) = build("sql59-exec-cube", None);
    let got = sorted_rows(
        &core,
        "tenant-a",
        "SELECT lang, kind, count(*) FROM docs GROUP BY CUBE(lang, kind)",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "en|a|1",
            "ja|a|2",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1",
            "ja|NULL|3",
            "NULL|NULL|2",
            "NULL|a|4",
            "NULL|b|2",
            "NULL|NULL|6",
        ])
    );
}

#[test]
fn explicit_grouping_sets_and_composites() {
    let (core, _g) = build("sql59-exec-sets", None);
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, kind, count(*) FROM docs GROUP BY GROUPING SETS ((lang), (kind), ())",
        ),
        expect_sorted(vec![
            "en|NULL|1",
            "ja|NULL|3",
            "NULL|NULL|2",
            "NULL|a|4",
            "NULL|b|2",
            "NULL|NULL|6",
        ])
    );
    // 複合: 通常キー + ROLLUP は (lang, kind) と (lang) の 2 セット。
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, kind, count(*) FROM docs GROUP BY lang, ROLLUP(kind)",
        ),
        expect_sorted(vec![
            "en|a|1",
            "ja|a|2",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1",
            "ja|NULL|3",
            "NULL|NULL|2",
        ])
    );
    // 括弧つき複合要素は 1 要素として扱う（(lang, kind) と ()）。
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP((lang, kind))",
        ),
        expect_sorted(vec![
            "en|a|1",
            "ja|a|2",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "NULL|NULL|6",
        ])
    );
    // 集計値（sum）もセットごとに計算される。
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, sum(n) FROM docs GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec!["en|4", "ja|6", "NULL|11", "NULL|21"])
    );
}

#[test]
fn duplicate_sets_are_not_deduplicated() {
    let (core, _g) = build("sql59-exec-dup", None);
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, count(*) FROM docs GROUP BY GROUPING SETS ((lang), (lang))",
        ),
        expect_sorted(vec!["en|1", "en|1", "ja|3", "ja|3", "NULL|2", "NULL|2"])
    );
}

#[test]
fn empty_input_still_emits_the_grand_total_row() {
    let (core, _g) = build("sql59-exec-empty", None);
    assert_eq!(
        rows_as_text(
            &core,
            "tenant-a",
            "SELECT lang, count(*), sum(n) FROM docs WHERE n > 100000 GROUP BY ROLLUP(lang)",
        ),
        vec!["NULL|0|NULL".to_string()]
    );
    // 空のセットだけの指定: `GROUP BY ()` は常に 1 行、`GROUPING SETS ((), ())` は 2 行。
    assert_eq!(
        rows_as_text(&core, "tenant-a", "SELECT count(*) FROM docs GROUP BY ()"),
        vec!["6".to_string()]
    );
    assert_eq!(
        rows_as_text(
            &core,
            "tenant-a",
            "SELECT count(*) FROM docs GROUP BY GROUPING SETS ((), ())",
        ),
        vec!["6".to_string(), "6".to_string()]
    );
    assert_eq!(
        rows_as_text(
            &core,
            "tenant-a",
            "SELECT count(*) FROM docs WHERE n > 100000 GROUP BY ()",
        ),
        vec!["0".to_string()]
    );
    // キーが 0 個のグループ化では裸の列は参照できない。
    assert_eq!(
        err_code(
            &core,
            "tenant-a",
            "SELECT lang, count(*) FROM docs GROUP BY ()"
        ),
        "42803"
    );
}

#[test]
fn other_tenants_rows_never_reach_any_set() {
    let (core, _g) = build("sql59-exec-rls", None);
    let a = sorted_rows(
        &core,
        "tenant-a",
        "SELECT lang, count(*), sum(n) FROM docs GROUP BY ROLLUP(lang)",
    );
    assert!(a.iter().all(|r| !r.starts_with("fr|")), "{a:?}");
    assert_eq!(
        a,
        expect_sorted(vec!["en|1|4", "ja|3|6", "NULL|2|11", "NULL|6|21"])
    );
    let b = sorted_rows(
        &core,
        "tenant-b",
        "SELECT lang, count(*), sum(n) FROM docs GROUP BY ROLLUP(lang)",
    );
    assert_eq!(
        b,
        expect_sorted(vec![
            "en|1|4",
            "fr|1|100",
            "ja|4|1006",
            "NULL|2|11",
            "NULL|8|1121",
        ])
    );
}

#[test]
fn expression_keys_and_having_and_order_apply_after_composition() {
    let (core, _g) = build("sql59-exec-expr", None);
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT upper(lang), count(*) FROM docs GROUP BY ROLLUP(upper(lang))",
        ),
        expect_sorted(vec!["EN|1", "JA|3", "NULL|2", "NULL|6"])
    );
    // HAVING は小計行・総計行にも適用される。
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-a",
            "SELECT lang, count(*) FROM docs GROUP BY ROLLUP(lang) HAVING count(*) > 2",
        ),
        expect_sorted(vec!["ja|3", "NULL|6"])
    );
    // ORDER BY・LIMIT・OFFSET は合成後の結果に適用される（6, 3, 2, 1 の降順から 2 件目以降 2 件）。
    assert_eq!(
        rows_as_text(
            &core,
            "tenant-a",
            "SELECT lang, count(*) FROM docs GROUP BY ROLLUP(lang) \
             ORDER BY count(*) DESC, lang LIMIT 2 OFFSET 1",
        ),
        vec!["ja|3".to_string(), "NULL|2".to_string()]
    );
}

#[test]
fn group_limit_applies_to_the_total_across_sets() {
    // ROLLUP(lang, kind) は全セット合計 9 グループ（5 + 3 + 1）。各セット単独は 5 以下。
    let sql = "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP(lang, kind)";
    let (exact, _g1) = build("sql59-exec-limit-exact", Some(GroupByLimits::new(8, 9)));
    assert_eq!(rows_as_text(&exact, "tenant-a", sql).len(), 9);
    let (over, _g2) = build("sql59-exec-limit-over", Some(GroupByLimits::new(8, 8)));
    assert_eq!(err_code(&over, "tenant-a", sql), "54000");
}

#[test]
fn explain_uses_a_plain_scan_for_a_single_text_key_rollup() {
    let (core, _g) = build("sql59-exec-explain", None);
    let mut s = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx_for("tenant-a"),
            &mut s,
            "EXPLAIN SELECT lang, count(*) FROM docs GROUP BY ROLLUP(lang)",
        )
        .expect("EXPLAIN of a grouping-sets aggregate");
    let text = format!("{outcome:?}").to_lowercase();
    assert!(!text.contains("enumerat"), "{text}");
}

#[test]
fn unsupported_contexts_stay_fail_closed() {
    let (core, _g) = build("sql59-exec-closed", None);
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx_for("tenant-a"),
        &mut s,
        "CREATE VIEW vdocs AS SELECT lang, kind FROM docs LIMIT 100",
    )
    .expect("create view");
    // ビュー経由の外側集計は構文段が受理しない（42601。誤った行を返さない）。
    assert_eq!(
        err_code(
            &core,
            "tenant-a",
            "SELECT lang, count(*) FROM vdocs GROUP BY ROLLUP(lang)"
        ),
        "42601"
    );
    for sql in [
        "SELECT id FROM docs WHERE id IN (SELECT id FROM docs GROUP BY ROLLUP(lang))",
        "WITH c AS (SELECT lang FROM docs GROUP BY ROLLUP(lang)) SELECT count(*) FROM docs",
    ] {
        assert_eq!(err_code(&core, "tenant-a", sql), "42601", "{sql}");
    }
}
