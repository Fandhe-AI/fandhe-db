//! `GROUPING()` 関数の結合テスト（Issue #1823、対象ビヘイビア: SQL-59 (1)。ポインタ:
//! `docs/spec/04-behavior/` の SQL-59、エラー分類は同 `error-format.md` の `54023`）。
//!
//! `EngineCore::execute_sql`（wire-server と NoSQL 表層が共有する production 経路）で、
//! (1) ビットの規則（引数 i はそのキーが現在のセットに無いとき 1、最初の引数が最上位ビット）、
//! (2) データ上の NULL と小計の NULL を区別できること、(3) `HAVING`・`ORDER BY`・式項目での
//! 利用とアキュムレータの共有、(4) エラー分類（`42803`・`42703`・`42601`・`54023`）、
//! (5) 非対応の文脈（ビュー外側）が fail-closed であること、を固定する。期待値は PostgreSQL 17 の
//! 意味論から手計算した固定値（PG 実機との照合は未実施）。他機能との網羅的な併用は #1824 の担当。
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta};
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

const T: &str = "tenant-a";

#[test]
fn rollup_bit_mask_follows_argument_order_with_first_argument_as_msb() {
    let (core, _g) = build("sql1823-rollup", None);
    // grouping(lang, kind): kind だけ無いセットは 1、両方無い総計は 3。
    // grouping(kind, lang): 最初の引数が最上位ビットなので kind だけ無いセットは 2。
    let got = sorted_rows(
        &core,
        T,
        "SELECT lang, kind, grouping(lang, kind), grouping(kind, lang), count(*) \
         FROM docs GROUP BY ROLLUP(lang, kind)",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "ja|a|0|0|2",
            "ja|b|0|0|1",
            "en|a|0|0|1",
            "NULL|a|0|0|1",
            "NULL|b|0|0|1",
            "ja|NULL|1|2|3",
            "en|NULL|1|2|1",
            "NULL|NULL|1|2|2",
            "NULL|NULL|3|3|6",
        ])
    );
}

#[test]
fn single_argument_values_tell_data_null_from_subtotal_null() {
    let (core, _g) = build("sql1823-null", None);
    // 集計関数なし（GROUPING のみ）の SELECT も受理する。`lang` がデータ上 NULL の行は
    // grouping(lang) = 0、ROLLUP の総計の NULL は 1。
    let got = sorted_rows(
        &core,
        T,
        "SELECT lang, kind, grouping(lang), grouping(kind) FROM docs GROUP BY ROLLUP(lang, kind)",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "ja|a|0|0",
            "ja|b|0|0",
            "en|a|0|0",
            "NULL|a|0|0",
            "NULL|b|0|0",
            "ja|NULL|0|1",
            "en|NULL|0|1",
            "NULL|NULL|0|1",
            "NULL|NULL|1|1",
        ])
    );
}

#[test]
fn cube_covers_all_four_masks() {
    let (core, _g) = build("sql1823-cube", None);
    let got = sorted_rows(
        &core,
        T,
        "SELECT grouping(lang, kind), count(*) FROM docs GROUP BY CUBE(lang, kind)",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "0|2", "0|1", "0|1", "0|1", "0|1", // (lang, kind)
            "1|3", "1|1", "1|2", // (lang)
            "2|4", "2|2", // (kind)
            "3|6", // ()
        ])
    );
}

#[test]
fn grouping_sets_and_composite_specs() {
    let (core, _g) = build("sql1823-sets", None);
    let got = sorted_rows(
        &core,
        T,
        "SELECT lang, kind, grouping(lang, kind) AS g FROM docs \
         GROUP BY GROUPING SETS ((lang), (kind), ())",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "ja|NULL|1",
            "en|NULL|1",
            "NULL|NULL|1",
            "NULL|a|2",
            "NULL|b|2",
            "NULL|NULL|3",
        ])
    );
    // 複合指定 `a, ROLLUP(b)`: lang は全セットに含まれるので kind だけがビットを持つ。
    let got = sorted_rows(
        &core,
        T,
        "SELECT lang, kind, grouping(lang), grouping(kind) FROM docs GROUP BY lang, ROLLUP(kind)",
    );
    assert_eq!(
        got,
        expect_sorted(vec![
            "ja|a|0|0",
            "ja|b|0|0",
            "en|a|0|0",
            "NULL|a|0|0",
            "NULL|b|0|0",
            "ja|NULL|0|1",
            "en|NULL|0|1",
            "NULL|NULL|0|1",
        ])
    );
}

#[test]
fn duplicate_arguments_and_the_31_argument_limit() {
    let (core, _g) = build("sql1823-dup", None);
    // grouping(lang, lang, kind): lang のセットは kind 分の 1、kind のセットは 4 + 2 = 6。
    let got = sorted_rows(
        &core,
        T,
        "SELECT grouping(lang, lang, kind), count(*) FROM docs \
         GROUP BY GROUPING SETS ((lang), (kind))",
    );
    assert_eq!(got, expect_sorted(vec!["1|3", "1|1", "1|2", "6|4", "6|2"]));

    // 31 個の引数: lang の無いセットは全ビットが立ち int4 の上限（2^31 - 1）になる。
    let args = vec!["lang"; 31].join(", ");
    let got = sorted_rows(
        &core,
        T,
        &format!(
            "SELECT grouping({args}), count(*) FROM docs GROUP BY GROUPING SETS ((lang), (kind))"
        ),
    );
    assert_eq!(
        got,
        expect_sorted(vec!["0|3", "0|1", "0|2", "2147483647|4", "2147483647|2"])
    );

    // 32 個は `54023`。キーでない列を並べても個数の検査が先（確保・束縛より前）。
    let args32 = vec!["n"; 32].join(", ");
    assert_eq!(
        err_code(
            &core,
            T,
            &format!("SELECT grouping({args32}) FROM docs GROUP BY ROLLUP(lang)")
        ),
        "54023"
    );
}

#[test]
fn plain_group_by_always_returns_zero_on_every_scan_path() {
    let (core, _g) = build("sql1823-plain", None);
    // 同じ文を繰り返し、冷えた経路・索引の列挙形（単一 TEXT キー）のどちらでも同じ結果になる。
    for _ in 0..4 {
        assert_eq!(
            sorted_rows(
                &core,
                T,
                "SELECT lang, grouping(lang), count(*) FROM docs GROUP BY lang"
            ),
            expect_sorted(vec!["ja|0|3", "en|0|1", "NULL|0|2"])
        );
        assert_eq!(
            sorted_rows(
                &core,
                T,
                "SELECT lang, grouping(lang) FROM docs GROUP BY lang"
            ),
            expect_sorted(vec!["ja|0", "en|0", "NULL|0"])
        );
    }
}

#[test]
fn expression_key_is_matched_structurally() {
    let (core, _g) = build("sql1823-exprkey", None);
    let got = sorted_rows(
        &core,
        T,
        "SELECT upper(lang), grouping(upper(lang)), count(*) FROM docs GROUP BY ROLLUP(upper(lang))",
    );
    assert_eq!(
        got,
        expect_sorted(vec!["JA|0|3", "EN|0|1", "NULL|0|2", "NULL|1|6"])
    );
}

#[test]
fn having_order_by_and_expression_items_share_the_call() {
    let (core, _g) = build("sql1823-clauses", None);
    // 従来形の HAVING。
    assert_eq!(
        sorted_rows(
            &core,
            T,
            "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP(lang, kind) \
             HAVING grouping(lang) = 1"
        ),
        vec!["NULL|NULL|6".to_string()]
    );
    // 式の HAVING。
    assert_eq!(
        sorted_rows(
            &core,
            T,
            "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP(lang, kind) \
             HAVING grouping(kind) + 0 > 0"
        ),
        expect_sorted(vec!["ja|NULL|3", "en|NULL|1", "NULL|NULL|2", "NULL|NULL|6"])
    );
    // ORDER BY（SELECT に同じ呼び出しがあればアキュムレータを共有する）。
    assert_eq!(
        rows_as_text(
            &core,
            T,
            "SELECT lang, grouping(lang) AS g, count(*) FROM docs GROUP BY ROLLUP(lang) \
             ORDER BY grouping(lang) DESC, lang"
        ),
        vec!["NULL|1|6", "en|0|1", "ja|0|3", "NULL|0|2"]
    );
    assert_eq!(
        rows_as_text(
            &core,
            T,
            "SELECT lang, grouping(lang) AS g FROM docs GROUP BY ROLLUP(lang) ORDER BY g DESC, lang"
        )
        .first()
        .map(String::as_str),
        Some("NULL|1")
    );
    // 式の一部としての利用。
    assert_eq!(
        sorted_rows(
            &core,
            T,
            "SELECT grouping(lang) + 1, count(*) FROM docs GROUP BY ROLLUP(lang)"
        ),
        expect_sorted(vec!["1|3", "1|1", "1|2", "2|6"])
    );
}

#[test]
fn result_column_is_named_grouping_and_typed_int4() {
    let (core, _g) = build("sql1823-meta", None);
    let result = core
        .execute_sql(
            &ctx_for(T),
            "SELECT grouping(lang), grouping(lang) AS g FROM docs GROUP BY ROLLUP(lang)",
        )
        .expect("query");
    let metas: Vec<(String, Option<ColumnType>)> = result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, ty } => (name.clone(), ty.clone()),
            other => panic!("unexpected column meta: {other:?}"),
        })
        .collect();
    assert_eq!(
        metas,
        vec![
            ("grouping".to_string(), Some(ColumnType::Integer)),
            ("g".to_string(), Some(ColumnType::Integer)),
        ]
    );
}

#[test]
fn other_tenants_rows_never_influence_the_values() {
    let (core, _g) = build("sql1823-rls", None);
    // tenant-b の `fr` 行は tenant-a から見えず、グループにも現れない。
    let got = sorted_rows(
        &core,
        T,
        "SELECT lang, grouping(lang), count(*) FROM docs GROUP BY ROLLUP(lang)",
    );
    assert!(got.iter().all(|r| !r.starts_with("fr|")), "{got:?}");
    assert!(got.contains(&"NULL|1|6".to_string()), "{got:?}");
}

#[test]
fn arguments_must_be_grouping_keys() {
    let (core, _g) = build("sql1823-errors", None);
    for sql in [
        // キーでない既存の列。
        "SELECT lang, grouping(kind) FROM docs GROUP BY lang",
        // 式がキーと構造一致しない。
        "SELECT lang, grouping(upper(kind)) FROM docs GROUP BY lang",
        // GROUP BY なし・GROUP BY ()・GROUP BY なしの HAVING。
        "SELECT grouping(lang) FROM docs",
        "SELECT grouping(lang) FROM docs GROUP BY ()",
        "SELECT count(*) FROM docs HAVING grouping(lang) = 0",
        // FROM なし。
        "SELECT grouping(1)",
    ] {
        assert_eq!(err_code(&core, T, sql), "42803", "{sql}");
    }
    // 未知の列は 42703。
    assert_eq!(
        err_code(&core, T, "SELECT grouping(zzz) FROM docs GROUP BY lang"),
        "42703"
    );
}

#[test]
fn malformed_calls_and_misplaced_calls_are_rejected() {
    let (core, _g) = build("sql1823-syntax", None);
    for sql in [
        "SELECT grouping() FROM docs GROUP BY lang",
        "SELECT grouping(*) FROM docs GROUP BY lang",
        "SELECT grouping(DISTINCT lang) FROM docs GROUP BY lang",
        "SELECT grouping(lang) FILTER (WHERE n > 1) FROM docs GROUP BY lang",
        // 入れ子の集計は既存規約どおり 42601。
        "SELECT sum(grouping(lang)) FROM docs GROUP BY lang",
        "SELECT grouping(count(*)) FROM docs GROUP BY lang",
    ] {
        assert_eq!(err_code(&core, T, sql), "42601", "{sql}");
    }
}

#[test]
fn unsupported_contexts_stay_fail_closed() {
    let (core, _g) = build("sql1823-closed", None);
    // WHERE に置いた GROUPING は構文段で拒否される（集計関数と同じ扱い）。
    let where_code = err_code(
        &core,
        T,
        "SELECT lang FROM docs WHERE grouping(lang) = 0 GROUP BY lang",
    );
    assert!(
        ["42601", "42803", "42883"].contains(&where_code),
        "{where_code}"
    );
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx_for(T),
        &mut s,
        "CREATE VIEW vdocs AS SELECT lang, kind FROM docs",
    )
    .expect("create view");
    assert_eq!(
        err_code(
            &core,
            T,
            "SELECT lang, grouping(lang) FROM vdocs GROUP BY lang"
        ),
        "0A000"
    );
    // JOIN 集計は fail-closed。
    let join_code = err_code(
        &core,
        T,
        "SELECT grouping(a.lang) FROM docs a INNER JOIN docs b ON a.id = b.id",
    );
    assert!(
        ["0A000", "42601", "42803"].contains(&join_code),
        "{join_code}"
    );
}

#[test]
fn direct_construction_entry_rejects_grouping() {
    use engine::sql::allowlist::AggregateFunc;
    use engine::sql::parser::{AggregateTarget, BoundAggregateItem};
    let err = BoundAggregateItem::bind(
        AggregateFunc::Grouping,
        AggregateTarget::Column("lang".to_string()),
        &schema(),
    )
    .expect_err("GROUPING must not be constructible without a GROUP BY");
    assert_eq!(err.wire_code(), "42601");
}
