//! 投影位置のスカラーサブクエリ `SELECT <列>, (SELECT ...) [AS <alias>] FROM ...` の
//! 結合テスト（Issue #1352・SQL-29 (a)・RLS-10 (b)・TASK-213）。
//! `tests/sql29_subquery_scalar.rs` と同じ流儀（実 `Storage`＋`CpuScalarProvider`、
//! `EngineCore::execute_sql*` を production 経路として検証）。期待値はテスト側で
//! 素朴に求めた独立オラクル。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const ITEMS: &str = "items";
const REFS: &str = "refs_t";

fn cols() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ColumnDef::new("name", ColumnType::Text, true),
        ColumnDef::new("qty", ColumnType::BigInt, true),
    ]
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql29-projection-subquery");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(ITEMS, cols()))
        .expect("items");
    storage
        .create_table(&TableSchema::new(REFS, cols()))
        .expect("refs");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ins(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, name: &str, qty: Option<i64>) {
    // NULL リテラルは VALUES に書けないため、NULL にしたい列は省略する。
    let (qty_col, qty_val) = match qty {
        Some(q) => (", qty", format!(", {q}")),
        None => ("", String::new()),
    };
    let sql = format!(
        "INSERT INTO {table} (id, embedding, name{qty_col}) VALUES ({id}, '[0.1,0.2]', '{name}'{qty_val}) \
         USING OPERATION_ID '{table}-{id}'"
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .unwrap_or_else(|e| panic!("insert failed sql={sql:?}: {e:?}"));
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"))
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .expect_err(&format!("sql={sql:?} should be rejected"))
        .wire_code()
        .to_string()
}

fn col_name(meta: &ColumnMeta) -> String {
    match meta {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name.clone(),
    }
}

fn sorted_rows(result: &QueryResult) -> Vec<(u64, Vec<Cell>)> {
    let mut rows: Vec<(u64, Vec<Cell>)> = result
        .rows
        .iter()
        .map(|r| (r.id, r.cells.clone()))
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

fn seed(core: &EngineCore, ctx: &PolicyContext) {
    ins(core, ctx, ITEMS, 1, "a", Some(10));
    ins(core, ctx, ITEMS, 2, "b", Some(20));
    ins(core, ctx, ITEMS, 3, "c", None);
    ins(core, ctx, REFS, 1, "only", Some(7));
}

#[test]
fn projection_subquery_single_row_is_repeated_and_columns_keep_position() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    // 先頭・中間・末尾・サブクエリのみ。
    let r = run(
        &core,
        &ctx,
        &format!(
            "SELECT (SELECT qty FROM {REFS} LIMIT 5) AS first_v, name, \
             (SELECT name FROM {REFS} LIMIT 5) AS mid_v, qty, \
             (SELECT qty FROM {REFS} LIMIT 5) FROM {ITEMS} LIMIT 100"
        ),
    );
    let names: Vec<String> = r.columns.iter().map(col_name).collect();
    assert_eq!(names, vec!["first_v", "name", "mid_v", "qty", "qty"]);
    let rows = sorted_rows(&r);
    assert_eq!(rows.len(), 3);
    for (id, cells) in &rows {
        assert_eq!(cells.len(), 5);
        assert_eq!(cells[0], Cell::SignedInteger(7));
        assert_eq!(cells[2], Cell::Text("only".to_string()));
        assert_eq!(cells[4], Cell::SignedInteger(7));
        let expected_name = ["a", "b", "c"][(*id - 1) as usize];
        assert_eq!(cells[1], Cell::Text(expected_name.to_string()));
    }
    assert_eq!(rows[0].1[3], Cell::SignedInteger(10));
    assert_eq!(rows[2].1[3], Cell::Null);

    // サブクエリのみの投影（内側の列名が既定名）。
    let r = run(
        &core,
        &ctx,
        &format!("SELECT (SELECT qty FROM {REFS} LIMIT 5) FROM {ITEMS} LIMIT 100"),
    );
    assert_eq!(
        r.columns.iter().map(col_name).collect::<Vec<_>>(),
        vec!["qty"]
    );
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn projection_subquery_zero_rows_is_null_and_aggregate_inner_works() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    let r = run(
        &core,
        &ctx,
        &format!(
            "SELECT name, (SELECT qty FROM {REFS} WHERE qty > 99 LIMIT 5) AS none_v, \
             (SELECT COUNT(*) FROM {REFS}) AS n FROM {ITEMS} LIMIT 100"
        ),
    );
    for row in &r.rows {
        assert_eq!(row.cells[1], Cell::Null);
        assert_eq!(row.cells[2], Cell::Integer(1));
    }
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn projection_subquery_works_with_where_nesting_order_by_and_offset() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    // 内側 WHERE に IN／EXISTS のネスト、外側に WHERE・ORDER BY・OFFSET。
    let r = run(
        &core,
        &ctx,
        &format!(
            "SELECT name, (SELECT qty FROM {REFS} WHERE id IN (SELECT id FROM {REFS} LIMIT 5) \
             AND EXISTS (SELECT id FROM {REFS} LIMIT 1) LIMIT 5) AS v \
             FROM {ITEMS} WHERE qty IN (SELECT qty FROM {ITEMS} WHERE qty >= 10 LIMIT 10) \
             ORDER BY qty DESC LIMIT 1 OFFSET 1"
        ),
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].id, 1);
    assert_eq!(r.rows[0].cells[0], Cell::Text("a".to_string()));
    assert_eq!(r.rows[0].cells[1], Cell::SignedInteger(7));
}

#[test]
fn projection_subquery_multiple_rows_errors_only_when_outer_has_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    ins(&core, &ctx, REFS, 2, "second", Some(8));

    let multi = format!("SELECT name, (SELECT qty FROM {REFS} LIMIT 10) FROM {ITEMS} LIMIT 100");
    assert_eq!(code(&core, &ctx, &multi), "22000");
    // 外側が 0 行なら発生しない。
    let none = format!(
        "SELECT name, (SELECT qty FROM {REFS} LIMIT 10) FROM {ITEMS} WHERE qty > 999 LIMIT 100"
    );
    assert!(run(&core, &ctx, &none).rows.is_empty());
    // 内側の LIMIT 1 は先頭 1 行の切り詰めとして許容される。
    let limited = format!("SELECT name, (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 100");
    assert_eq!(run(&core, &ctx, &limited).rows.len(), 3);
}

#[test]
fn projection_subquery_static_and_context_rejections() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    for sql in [
        // 投影 2 列・全列・LIMIT 省略・相関・内側の投影サブクエリ。
        format!("SELECT (SELECT qty, name FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        format!("SELECT (SELECT * FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        format!("SELECT (SELECT qty FROM {REFS}) FROM {ITEMS} LIMIT 10"),
        format!("SELECT (SELECT qty FROM {REFS} WHERE qty = 1 AND name = outer_only LIMIT 1) FROM {ITEMS} LIMIT 10"),
        format!("SELECT (SELECT (SELECT qty FROM {REFS} LIMIT 1) FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        // 式の内側への埋め込み・演算。
        format!("SELECT upper((SELECT name FROM {REFS} LIMIT 1)) FROM {ITEMS} LIMIT 10"),
        format!("SELECT (SELECT qty FROM {REFS} LIMIT 1) + 1 FROM {ITEMS} LIMIT 10"),
        // ランキング付き検索・集計・DISTINCT・ウィンドウ併用。
        format!("SELECT (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10"),
        format!("SELECT COUNT(*), (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS}"),
        format!("SELECT DISTINCT (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS}"),
        format!("SELECT ROW_NUMBER() OVER () AS rn, (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        // EXPLAIN・カーソル・ビュー本体・CTE・集合演算の枝。
        format!("EXPLAIN SELECT (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        format!("CREATE VIEW pv AS SELECT (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10"),
        format!("WITH c AS (SELECT (SELECT qty FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10) SELECT * FROM c LIMIT 10"),
        format!("SELECT name FROM {ITEMS} UNION ALL SELECT (SELECT name FROM {REFS} LIMIT 1) FROM {ITEMS}"),
    ] {
        let c = code(&core, &ctx, &sql);
        assert!(c == "42601" || c == "22000", "sql={sql} code={c}");
    }
    // 未知列は 22000、深さ超過は 54000。
    assert_eq!(
        code(
            &core,
            &ctx,
            &format!("SELECT (SELECT nope FROM {REFS} LIMIT 1) FROM {ITEMS} LIMIT 10")
        ),
        "22000"
    );
    let deep = format!(
        "SELECT (SELECT qty FROM {REFS} WHERE qty IN (SELECT qty FROM {REFS} WHERE qty IN \
         (SELECT qty FROM {REFS} WHERE qty IN (SELECT qty FROM {REFS} WHERE qty IN \
         (SELECT qty FROM {REFS} LIMIT 1) LIMIT 1) LIMIT 1) LIMIT 1) LIMIT 1) FROM {ITEMS} LIMIT 10"
    );
    assert_eq!(code(&core, &ctx, &deep), "54000");
    // 実行回数上限（16 を超える項目数）は 54000。
    let many = (0..17)
        .map(|_| format!("(SELECT qty FROM {REFS} LIMIT 1)"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        code(&core, &ctx, &format!("SELECT {many} FROM {ITEMS} LIMIT 10")),
        "54000"
    );
}

#[test]
fn projection_subquery_extended_protocol_parameters_are_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let err = core
        .parse_sql_prepared(&format!(
            "SELECT (SELECT qty FROM {REFS} WHERE name = $1 LIMIT 1) FROM {ITEMS} LIMIT 10"
        ))
        .expect_err("projection subquery with $n must be rejected");
    assert_eq!(err.wire_code(), "42601");
    // 投影位置ではない通常の `$n`（WHERE のパラメータ）は従来どおり受理される。
    core.parse_sql_prepared(&format!(
        "SELECT name FROM {ITEMS} WHERE name = $1 LIMIT 10"
    ))
    .expect("plain parameterized SELECT must still parse");
}

/// RLS-10 (b): 他テナントの行の有無・値が、投影位置のスカラーサブクエリの値・エラー・
/// `wire_code` を変えない。
#[test]
fn projection_subquery_ignores_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    ins(&core, &a, ITEMS, 1, "a", Some(1));
    // tenant-a の refs は 0 行 → NULL。
    let q = format!("SELECT name, (SELECT qty FROM {REFS} LIMIT 10) AS v FROM {ITEMS} LIMIT 10");
    let before = run(&core, &a, &q);
    assert_eq!(before.rows[0].cells[1], Cell::Null);
    for i in 0..20u64 {
        ins(&core, &b, REFS, 100 + i, "x", Some(i as i64));
        ins(&core, &b, ITEMS, 200 + i, "y", Some(1));
    }
    let after = run(&core, &a, &q);
    assert_eq!(after, before);
    // 陽性対照: tenant-b 自身には refs が 20 行見えるため複数行エラー。
    assert_eq!(code(&core, &b, &q), "22000");
}
#[test]
fn projection_subquery_runtime_error_is_deferred_until_outer_has_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);

    // 内側の集計の数値あふれ（22003）は、外側が 0 行なら発生せず、行があれば返る。
    ins(&core, &ctx, REFS, 5, "big1", Some(i64::MAX));
    ins(&core, &ctx, REFS, 6, "big2", Some(i64::MAX));
    let inner = format!("(SELECT SUM(qty) FROM {REFS} WHERE qty > 100)");
    let none = format!("SELECT name, {inner} FROM {ITEMS} WHERE qty > 999 LIMIT 100");
    assert!(run(&core, &ctx, &none).rows.is_empty());
    let some = format!("SELECT name, {inner} FROM {ITEMS} LIMIT 100");
    assert_eq!(code(&core, &ctx, &some), "22003");
}

#[test]
fn projection_subquery_nested_where_runtime_error_is_deferred() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    ins(&core, &ctx, REFS, 5, "big1", Some(i64::MAX));
    ins(&core, &ctx, REFS, 6, "big2", Some(i64::MAX));
    // 入れ子 WHERE サブクエリ内の数値あふれ（22003）も、外側 0 行なら発生しない。
    let inner = format!(
        "(SELECT name FROM {REFS} WHERE qty = (SELECT SUM(qty) FROM {REFS} WHERE qty > 100) LIMIT 1)"
    );
    let none = format!("SELECT name, {inner} FROM {ITEMS} WHERE qty > 999 LIMIT 100");
    assert!(run(&core, &ctx, &none).rows.is_empty());
    let some = format!("SELECT name, {inner} FROM {ITEMS} LIMIT 100");
    assert_eq!(code(&core, &ctx, &some), "22003");
}

#[test]
fn projection_subquery_alias_of_id_keeps_numeric_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let q = format!("SELECT name, (SELECT id FROM {REFS} LIMIT 1) AS ref_id FROM {ITEMS} LIMIT 10");
    let r = run(&core, &ctx, &q);
    assert_eq!(
        r.columns[1],
        ColumnMeta::Computed {
            name: "ref_id".to_string(),
            ty: Some(ColumnType::Numeric {
                precision: 20,
                scale: 0
            }),
        }
    );
}

#[test]
fn projection_subquery_deferred_error_keeps_static_column_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    ins(&core, &ctx, REFS, 5, "big1", Some(i64::MAX));
    ins(&core, &ctx, REFS, 6, "big2", Some(i64::MAX));
    let inner = format!("(SELECT SUM(qty) FROM {REFS} WHERE qty > 100)");
    let none = run(
        &core,
        &ctx,
        &format!("SELECT name, {inner} AS s FROM {ITEMS} WHERE qty > 999 LIMIT 100"),
    );
    assert!(none.rows.is_empty());
    assert_eq!(
        none.columns[1],
        ColumnMeta::Computed {
            name: "s".to_string(),
            ty: Some(ColumnType::BigInt),
        }
    );
}

#[test]
fn projection_subquery_on_buffered_view_is_rejected_not_dropped() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    seed(&core, &ctx);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!("CREATE VIEW bv AS SELECT name FROM {ITEMS} LIMIT 10"),
    )
    .expect("create view");
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("SELECT name, (SELECT qty FROM {REFS} LIMIT 1) FROM bv LIMIT 10"),
        )
        .expect_err("must be rejected");
    assert_eq!(err.wire_code(), "42601");
}
/// PR #1384 codex-review P1 の回帰テスト: 疑似列 `id` が `2^53` 超（`u64` 全域）でも、
/// `IN`／`NOT IN`／スカラー比較のサブクエリが整数のまま厳密に照合できる
/// （式評価器の `f64` 写像による `22003` や境界値への丸めで落ちない）。
#[test]
fn id_in_subquery_matches_ids_beyond_2_pow_53() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let big1: u64 = (1u64 << 53) + 1;
    let big2: u64 = u64::MAX - 1;
    ins(&core, &ctx, ITEMS, 1, "small", Some(1));
    ins(&core, &ctx, ITEMS, big1, "big1", Some(2));
    ins(&core, &ctx, ITEMS, big2, "big2", Some(3));
    ins(&core, &ctx, REFS, big1, "r1", Some(1));
    ins(&core, &ctx, REFS, big2, "r2", Some(2));

    let ids = |sql: String| -> Vec<u64> {
        sorted_rows(&run(&core, &ctx, &sql))
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    };
    assert_eq!(
        ids(format!(
            "SELECT name FROM {ITEMS} WHERE id IN (SELECT id FROM {REFS} LIMIT 10) LIMIT 100"
        )),
        vec![big1, big2]
    );
    assert_eq!(
        ids(format!(
            "SELECT name FROM {ITEMS} WHERE id NOT IN (SELECT id FROM {REFS} LIMIT 10) LIMIT 100"
        )),
        vec![1]
    );
    assert_eq!(
        ids(format!(
            "SELECT name FROM {ITEMS} WHERE id = (SELECT id FROM {REFS} WHERE name = 'r2' LIMIT 10) LIMIT 100"
        )),
        vec![big2]
    );
    assert_eq!(
        ids(format!(
            "SELECT name FROM {ITEMS} WHERE id > (SELECT id FROM {REFS} WHERE name = 'r1' LIMIT 10) LIMIT 100"
        )),
        vec![big2]
    );
}
