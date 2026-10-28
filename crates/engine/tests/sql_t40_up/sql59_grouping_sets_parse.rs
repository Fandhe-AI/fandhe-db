//! `GROUP BY` の `ROLLUP`・`CUBE`・`GROUPING SETS` の構文解析と展開の結合テスト
//! （Issue #1821、対象ビヘイビア: SQL-59。ポインタ: `docs/spec/04-behavior/` の SQL-59）。
//!
//! `EngineCore::execute_sql` の production 経路で、(1) 構文を受理して展開した上で、実行段
//! （Issue #1822）が実行して行を返すこと（結果の網羅は `sql59_grouping_sets_exec`）、(2) 上限超過の
//! エラー契約（`54011`・`54001`・`54000`。`wire_code`）とその判定順、(3) 従来形の `GROUP BY`
//! が変わらないこと、(4) トップレベル以外の文脈での拒否を固定する。展開結果そのものの網羅は
//! `sql::grouping_sets` と `sql::allowlist` の単体テストが担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

fn build(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    let rows = [
        (1u64, "ja", "a", 1),
        (2, "ja", "b", 2),
        (3, "en", "a", 3),
        (4, "en", "b", 4),
    ];
    for (id, lang, kind, n) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::Text(kind.to_string()),
                Value::Integer(n),
            ],
            &OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn err_code(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx_for("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

#[test]
fn valid_grouping_syntax_is_accepted_and_executed() {
    let (core, _g) = build("sql59-parse-gate");
    for gb in [
        "ROLLUP(lang, kind)",
        "CUBE(lang, kind)",
        "GROUPING SETS ((lang, kind), (lang), ())",
        "lang, ROLLUP(kind)",
        "GROUPING SETS (lang, ROLLUP(kind), CUBE(lang))",
        "ROLLUP((lang, kind), n)",
        "()",
    ] {
        let sql = format!("SELECT count(*) FROM docs GROUP BY {gb}");
        // 構文を受理して展開し、実行段（Issue #1822）がセットごとの集計を返す。
        let result = core
            .execute_sql(&ctx_for("tenant-a"), &sql)
            .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
        assert!(!result.rows.is_empty(), "{sql}");
    }
}

#[test]
fn malformed_grouping_syntax_is_a_syntax_error() {
    let (core, _g) = build("sql59-parse-syntax");
    for gb in [
        "ROLLUP()",
        "CUBE()",
        "GROUPING SETS ()",
        "ROLLUP(CUBE(lang))",
        "(lang, kind)",
    ] {
        let sql = format!("SELECT count(*) FROM docs GROUP BY {gb}");
        assert_eq!(err_code(&core, &sql), "42601", "{sql}");
    }
}

#[test]
fn limits_are_checked_before_execution_in_a_fixed_order() {
    let (core, _g) = build("sql59-parse-limits");
    let names = |n: usize| {
        (0..n)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cube12_dup = ["lang"; 12].join(", ");
    let cases = [
        // CUBE は 13 要素で 54011（別々の列でも 54000 より先）。
        (format!("CUBE({})", names(13)), "54011"),
        // 展開後 8,192 セット。
        (format!("CUBE({cube12_dup}), ROLLUP(lang)"), "54001"),
        // 展開後 65,536 セット（オーバーフローしない）。
        (["CUBE(lang, kind, n, lang)"; 4].join(", "), "54001"),
        // 一意キー 9 個は 54000。
        (format!("ROLLUP({})", names(9)), "54000"),
    ];
    for (gb, code) in cases {
        let sql = format!("SELECT count(*) FROM docs GROUP BY {gb}");
        assert_eq!(err_code(&core, &sql), code, "{sql}");
    }
    // ちょうど 4,096 セットは上限内。2 値の lang で 4,095 × 2 + 1 = 8,191 グループとなり、
    // 既定のグループ数上限（10,000）以内なので実行も成功する。
    let sql = format!("SELECT count(*) FROM docs GROUP BY CUBE({cube12_dup})");
    let result = core
        .execute_sql(&ctx_for("tenant-a"), &sql)
        .expect("exactly 4096 sets within the group limit must succeed");
    assert_eq!(result.rows.len(), 8_191);
}

#[test]
fn plain_group_by_is_unchanged() {
    let (core, _g) = build("sql59-parse-regression");
    let ctx = ctx_for("tenant-a");
    for (sql, rows) in [
        ("SELECT lang, count(*) FROM docs GROUP BY lang", 2),
        (
            "SELECT lang, kind, count(*) FROM docs GROUP BY lang, kind",
            4,
        ),
        (
            "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang)",
            2,
        ),
    ] {
        let r = core.execute_sql(&ctx, sql).expect(sql);
        assert_eq!(r.rows.len(), rows, "{sql}");
    }
    // 構文を含まない重複キーは従来どおり 42601。
    assert_eq!(
        err_code(&core, "SELECT count(*) FROM docs GROUP BY lang, lang"),
        "42601"
    );
}

#[test]
fn grouping_syntax_outside_top_level_aggregate_select_is_rejected() {
    let (core, _g) = build("sql59-parse-context");
    for sql in [
        "SELECT id FROM docs WHERE id IN (SELECT id FROM docs GROUP BY ROLLUP(lang))",
        "WITH c AS (SELECT lang FROM docs GROUP BY ROLLUP(lang)) SELECT count(*) FROM docs",
    ] {
        assert_eq!(err_code(&core, sql), "42601", "{sql}");
    }
}
