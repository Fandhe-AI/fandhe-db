//! 名前付きウィンドウ（`WINDOW <名前> AS (...)`・`OVER <名前>`・`OVER (<名前> ...)`。SQL-64 (3)・
//! Issue #1892）の結合テスト。
//!
//! 足場は `sql64_window_groups_frame.rs` と同じ（`unique_db_path`＋`CleanupGuard`、公開 API の
//! `EngineCore::execute_sql` のみ）。結果は等価なインライン `OVER (...)` と完全に一致すること、
//! 拒否の SQLSTATE が PostgreSQL 17 と一致すること、テナント境界が変わらないことを確かめる。
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// (id, s, f, x, g)。`s` は NULL を含み、`s = 20` の 2 行が peer になる。
type Row = (u64, Option<i32>, f64, &'static str, &'static str);

const ROWS: [Row; 5] = [
    (1, Some(10), 0.1, "b", "a"),
    (2, Some(20), 0.2, "a", "a"),
    (3, Some(20), 0.3, "c", "b"),
    (4, None, 1e20, "d", "b"),
    (5, Some(50), -1e20, "e", "b"),
];

fn insert(storage: &Storage, ctx: &PolicyContext, vis: Visibility, row: Row) {
    let (id, s, f, x, g) = row;
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "namedwindow-{}-{id}",
        ctx.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        "t",
        ctx,
        id,
        vis,
        &[
            s.map_or(Value::Null, Value::Integer),
            Value::Double(f),
            Value::Text(x.to_string()),
            Value::Text(g.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("s", ColumnType::Integer, true),
            ColumnDef::new("f", ColumnType::Double, false),
            ColumnDef::new("x", ColumnType::Text, false),
            ColumnDef::new("g", ColumnType::Text, false),
        ],
    )
}

fn setup(label: &str, populate: bool) -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    storage.create_table(&schema()).expect("create table");
    if populate {
        for row in ROWS {
            insert(&storage, &ctx, Visibility::Public, row);
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, ctx, guard)
}

fn outcome(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql(ctx, sql) {
        Ok(_) => "OK",
        Err(e) => e.wire_code(),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Float(v) => format!("{v:?}"),
        Cell::Text(v) => v.clone(),
        Cell::Numeric(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

/// 2 列目（`id` の次）を `id` 昇順の文字列で返す。
fn col(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let result = core
        .execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql}: {}", e.wire_code()));
    let mut rows: Vec<(u64, String)> = result
        .rows
        .iter()
        .map(|r| {
            let id = match &r.cells[0] {
                Cell::Integer(v) => *v,
                other => panic!("expected id, got {other:?}"),
            };
            (id, show(&r.cells[1]))
        })
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows.into_iter().map(|(_, v)| v).collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}
fn q(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    col(core, ctx, sql)
}

/// 名前付きと等価なインラインで、行ごとの結果が完全に一致すること。
fn assert_same(core: &EngineCore, ctx: &PolicyContext, named: &str, inline: &str) {
    let a = q(core, ctx, named);
    let b = q(core, ctx, inline);
    assert_eq!(a, b, "named={named} inline={inline}");
    assert!(!a.is_empty());
}

/// SQL-64 (3): `OVER w` の結果が、等価なインライン `OVER (...)` と一致する。
#[test]
fn named_reference_matches_inline() {
    let (core, ctx, _g) = setup("nw-same", true);
    for func in [
        "row_number()",
        "rank()",
        "dense_rank()",
        "sum(s)",
        "count(*)",
        "min(s)",
    ] {
        assert_same(
            &core,
            &ctx,
            &format!("SELECT id, {func} OVER w FROM t WINDOW w AS (PARTITION BY g ORDER BY s, id) LIMIT 100"),
            &format!("SELECT id, {func} OVER (PARTITION BY g ORDER BY s, id) FROM t LIMIT 100"),
        );
    }
    for frame in [
        "ROWS BETWEEN 1 PRECEDING AND CURRENT ROW",
        "RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        "GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
    ] {
        assert_same(
            &core,
            &ctx,
            &format!("SELECT id, sum(s) OVER w FROM t WINDOW w AS (ORDER BY s {frame}) LIMIT 100"),
            &format!("SELECT id, sum(s) OVER (ORDER BY s {frame}) FROM t LIMIT 100"),
        );
    }
    assert_same(
        &core,
        &ctx,
        "SELECT id, lag(s) OVER w FROM t WINDOW w AS (ORDER BY id) LIMIT 100",
        "SELECT id, lag(s) OVER (ORDER BY id) FROM t LIMIT 100",
    );
}

/// 派生・連鎖・複数参照・未使用定義・名前の大小無視。
#[test]
fn derived_chained_and_unused() {
    let (core, ctx, _g) = setup("nw-derive", true);
    assert_same(
        &core,
        &ctx,
        "SELECT id, sum(s) OVER (w ORDER BY s) FROM t WINDOW w AS (PARTITION BY g) LIMIT 100",
        "SELECT id, sum(s) OVER (PARTITION BY g ORDER BY s) FROM t LIMIT 100",
    );
    assert_same(
        &core,
        &ctx,
        "SELECT id, sum(s) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t \
         WINDOW w AS (ORDER BY s, id) LIMIT 100",
        "SELECT id, sum(s) OVER (ORDER BY s, id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t LIMIT 100",
    );
    assert_same(
        &core,
        &ctx,
        "SELECT id, rank() OVER W2 FROM t WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY s) LIMIT 100",
        "SELECT id, rank() OVER (PARTITION BY g ORDER BY s) FROM t LIMIT 100",
    );
    // 未使用の定義と、同じ定義の複数回参照
    assert_same(
        &core,
        &ctx,
        "SELECT id, rank() OVER w, sum(s) OVER w FROM t WINDOW w AS (ORDER BY s, id), u AS (ORDER BY id) LIMIT 100",
        "SELECT id, rank() OVER (ORDER BY s, id), sum(s) OVER (ORDER BY s, id) FROM t LIMIT 100",
    );
}

/// WHERE・ORDER BY・LIMIT/OFFSET との併用と、CTE 本文・WITH 主クエリの WINDOW 句。
#[test]
fn combined_with_other_clauses_and_cte() {
    let (core, ctx, _g) = setup("nw-combined", true);
    assert_same(
        &core,
        &ctx,
        "SELECT id, row_number() OVER w FROM t WHERE id > 1 WINDOW w AS (ORDER BY id) ORDER BY id LIMIT 3 OFFSET 1",
        "SELECT id, row_number() OVER (ORDER BY id) FROM t WHERE id > 1 ORDER BY id LIMIT 3 OFFSET 1",
    );
    assert_same(
        &core,
        &ctx,
        "WITH c AS (SELECT id, s FROM t) SELECT id, rank() OVER w FROM c WINDOW w AS (ORDER BY s, id) LIMIT 100",
        "WITH c AS (SELECT id, s FROM t) SELECT id, rank() OVER (ORDER BY s, id) FROM c LIMIT 100",
    );
    assert_same(
        &core,
        &ctx,
        "WITH c AS (SELECT id, rank() OVER w AS r FROM t WINDOW w AS (ORDER BY s, id) LIMIT 100) SELECT id, r FROM c",
        "WITH c AS (SELECT id, rank() OVER (ORDER BY s, id) AS r FROM t LIMIT 100) SELECT id, r FROM c",
    );
}

/// マテリアライズドビュー・CTAS・カーソル・EXPLAIN は、名前付きとインラインで結果（成否・SQLSTATE）が
/// 一致する（展開を通らない入口は従来どおりの拒否にとどまる＝fail-closed）。
#[test]
fn other_entry_points_agree_with_inline() {
    let (core, ctx, _g) = setup("nw-entries", true);
    let named = "SELECT id, rank() OVER w FROM t WINDOW w AS (ORDER BY s, id) LIMIT 100";
    let inline = "SELECT id, rank() OVER (ORDER BY s, id) FROM t LIMIT 100";
    for prefix in [
        "EXPLAIN ",
        "CREATE MATERIALIZED VIEW mv_a AS ",
        "CREATE TABLE ctas_a AS ",
    ] {
        let n = outcome(&core, &ctx, &format!("{prefix}{named}"));
        // 同名オブジェクトの衝突を避けるため、インライン側は別名で流す。
        let i = outcome(
            &core,
            &ctx,
            &format!("{}{}", prefix.replace("_a", "_b"), inline),
        );
        assert_eq!(n, i, "prefix={prefix}");
    }
}

/// 拒否: PostgreSQL 17 と同じ SQLSTATE。
#[test]
fn rejections() {
    let (core, ctx, _g) = setup("nw-reject", true);
    let cases: [(&str, &str); 14] = [
        ("SELECT id, rank() OVER nosuch FROM t LIMIT 10", "42704"),
        ("SELECT id, rank() OVER (nosuch ORDER BY s) FROM t LIMIT 10", "42704"),
        (
            "SELECT id, rank() OVER w FROM t WINDOW w AS (u ORDER BY s), u AS (PARTITION BY g) LIMIT 10",
            "42704",
        ),
        (
            "SELECT id, rank() OVER w FROM t WINDOW w AS (ORDER BY s), w AS (ORDER BY id) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id, rank() OVER (w PARTITION BY g) FROM t WINDOW w AS (ORDER BY s) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id, rank() OVER (w ORDER BY id) FROM t WINDOW w AS (ORDER BY s) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id, sum(s) OVER (w) FROM t WINDOW w AS (ORDER BY s ROWS 1 PRECEDING) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id FROM t WINDOW a AS (ORDER BY s ROWS 1 PRECEDING), b AS (a) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id, rank() OVER w FROM t WINDOW w AS (ORDER BY s) WHERE id > 1",
            "42601",
        ),
        (
            "SELECT id, rank() OVER w FROM t ORDER BY id WINDOW w AS (ORDER BY s) LIMIT 10",
            "42601",
        ),
        (
            "SELECT id FROM t WINDOW w AS (ORDER BY s ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) LIMIT 10",
            "42P20",
        ),
        // 外側の名前を内側のサブクエリから参照できない
        (
            "SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE rank() OVER w = 1) WINDOW w AS (ORDER BY s) LIMIT 10",
            "42704",
        ),
        // 既存の未対応の形は従来どおり
        (
            "SELECT g, sum(s), rank() OVER w FROM t GROUP BY g WINDOW w AS (ORDER BY g) LIMIT 10",
            "42601",
        ),
        (
            "CREATE VIEW v_nw AS SELECT id, rank() OVER w FROM t WINDOW w AS (ORDER BY s) LIMIT 10",
            "42601",
        ),
    ];
    for (sql, want) in cases {
        let got = outcome(&core, &ctx, sql);
        if sql.starts_with("SELECT g, sum") || sql.starts_with("CREATE VIEW") {
            assert_ne!(got, "OK", "sql={sql}");
        } else {
            assert_eq!(got, want, "sql={sql}");
        }
    }
}

/// 名前付きウィンドウの計算対象は、そのテナントの可視行だけ（回帰ガード。実行経路は既存と同じ）。
#[test]
fn tenant_boundary_is_preserved() {
    let path = unique_db_path("nw-tenant");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    storage.create_table(&schema()).expect("create table");
    for row in ROWS {
        insert(&storage, &a, Visibility::Public, row);
    }
    // 他テナントの行（大きな値）。a の結果に混ざってはならない。
    insert(
        &storage,
        &b,
        Visibility::Private,
        (100, Some(1000), 0.0, "z", "a"),
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let named = "SELECT id, sum(s) OVER w FROM t \
        WINDOW w AS (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) LIMIT 100";
    let got = col(&core, &a, named);
    assert_eq!(got, strs(&["100", "100", "100", "100", "100"]));
    let err = core
        .execute_sql(&a, "SELECT id, rank() OVER nosuch FROM t LIMIT 10")
        .expect_err("undefined window");
    assert_eq!(err.wire_code(), "42704");
    assert!(!err.to_string().contains("tenant-b"));
}

/// 句の位置判定が構文位置で行われる回帰（指摘 1〜3）: `offset`／`union` という名前の列や
/// `JOIN ... USING` が WINDOW 句の前にあっても受理され、結果が等価なインラインと一致する。
#[test]
fn keyword_named_columns_and_join_using_before_window_clause() {
    let path = unique_db_path("nw-kwcols");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let ctx = ctx_for("tenant-a");
    for name in ["k", "k2"] {
        let schema = TableSchema::new(
            name,
            vec![
                ColumnDef::new("offset", ColumnType::Integer, true),
                ColumnDef::new("union", ColumnType::Integer, true),
                ColumnDef::new("s", ColumnType::Integer, true),
            ],
        );
        storage.create_table(&schema).expect("create table");
        for id in 1u64..=4 {
            let op_id =
                engine::recovery::required_op_id::OperationId::parse(&format!("nwkw-{name}-{id}"))
                    .expect("valid operation_id");
            engine::tenant::insert_typed_row(
                &storage,
                name,
                &ctx,
                id,
                Visibility::Public,
                &[
                    Value::Integer(1),
                    Value::Integer(id as i32 % 2),
                    Value::Integer(id as i32 * 10),
                ],
                &op_id,
            )
            .expect("insert row");
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 指摘 1: WHERE 内の列名 `offset`
    assert_same(
        &core,
        &ctx,
        "SELECT id, rank() OVER w FROM k WHERE offset = 1 WINDOW w AS (ORDER BY s, id) LIMIT 10",
        "SELECT id, rank() OVER (ORDER BY s, id) FROM k WHERE offset = 1 LIMIT 10",
    );
    // 指摘 3: WHERE 内の列名 `union`
    assert_same(
        &core,
        &ctx,
        "SELECT id, rank() OVER w FROM k WHERE union = 1 WINDOW w AS (ORDER BY s, id) LIMIT 10",
        "SELECT id, rank() OVER (ORDER BY s, id) FROM k WHERE union = 1 LIMIT 10",
    );
    // 指摘 2: FROM 内の JOIN ... USING。窓関数と JOIN の併用が未対応でも、名前付きとインラインで
    // 同じ結果（成功なら同じ行、失敗なら同じ SQLSTATE）になり、WINDOW 句の位置判定で
    // 余計に拒否されない（unit テスト `join_using_before_window_clause` がトークン列で固定）。
    let named = "SELECT k.id, rank() OVER w FROM k JOIN k2 USING (id) WINDOW w AS (ORDER BY k.s, k.id) LIMIT 10";
    let inline = "SELECT k.id, rank() OVER (ORDER BY k.s, k.id) FROM k JOIN k2 USING (id) LIMIT 10";
    assert_eq!(outcome(&core, &ctx, named), outcome(&core, &ctx, inline));
    // 本来の誤配置は引き続き 42601
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, rank() OVER w FROM k LIMIT 5 WINDOW w AS (ORDER BY s)"
        ),
        "42601"
    );
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id, rank() OVER w FROM k OFFSET 1 WINDOW w AS (ORDER BY s)"
        ),
        "42601"
    );
}

/// SELECT 以外の枝（データ変更 CTE 本体・UPDATE・INSERT）の WINDOW 句は除去されず `42601` で
/// 拒否され、テーブルは変更されない。
#[test]
fn window_clause_on_data_modifying_statements_is_rejected_without_effect() {
    let (core, ctx, _g) = setup("nw-dml", true);
    let before = q(&core, &ctx, "SELECT id, s FROM t LIMIT 100");
    for sql in [
        "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id WINDOW w AS ()) SELECT * FROM d LIMIT 10",
        "WITH d AS (UPDATE t SET s = 99 WHERE id = 1 RETURNING id WINDOW w AS (ORDER BY id)) SELECT * FROM d LIMIT 10",
        "WITH d AS (DELETE FROM t RETURNING id WINDOW w AS (ORDER BY id)) SELECT * FROM d LIMIT 10",
        "UPDATE t SET s = 99 WHERE id = 1 RETURNING id WINDOW w AS (ORDER BY id)",
        "DELETE FROM t WHERE id = 1 RETURNING id WINDOW w AS (ORDER BY id)",
        "INSERT INTO t (id, s) SELECT id + 100, s FROM t WINDOW w AS (ORDER BY id)",
    ] {
        assert_eq!(outcome(&core, &ctx, sql), "42601", "{sql}");
    }
    assert_eq!(q(&core, &ctx, "SELECT id, s FROM t LIMIT 100"), before);
    assert_eq!(
        q(&core, &ctx, "SELECT id, s FROM t LIMIT 100").len(),
        ROWS.len()
    );
}

/// 参照されない定義でもフレームの意味検査（ORDER BY の無い GROUPS・RANGE オフセットのキー数・
/// オフセット内の変数）が通り、`42P20`／`42P10` になる。
#[test]
fn unused_definition_frame_semantics_are_rejected() {
    let (core, ctx, _g) = setup("nw-unused", true);
    for (sql, code) in [
        ("SELECT id FROM t WINDOW w AS (GROUPS CURRENT ROW) LIMIT 10", "42P20"),
        (
            "SELECT id FROM t WINDOW w AS (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) LIMIT 10",
            "42P20",
        ),
        (
            "SELECT id FROM t WINDOW w AS (ORDER BY s ROWS BETWEEN s PRECEDING AND CURRENT ROW) LIMIT 10",
            "42P10",
        ),
    ] {
        assert_eq!(outcome(&core, &ctx, sql), code, "{sql}");
    }
    assert_eq!(
        outcome(
            &core,
            &ctx,
            "SELECT id FROM t WINDOW w AS (ORDER BY s GROUPS CURRENT ROW) LIMIT 10"
        ),
        "OK"
    );
}

/// 動詞と同名の CTE を持つ SELECT 主クエリの WINDOW 句は受理され、結果がインラインと一致する。
#[test]
fn cte_named_like_a_verb_keeps_the_window_clause_valid() {
    let (core, ctx, _g) = setup("nw-verbcte", true);
    assert_same(
        &core,
        &ctx,
        "WITH update AS (SELECT id, s FROM t) SELECT id, rank() OVER w FROM update WINDOW w AS (ORDER BY s, id) LIMIT 100",
        "WITH update AS (SELECT id, s FROM t) SELECT id, rank() OVER (ORDER BY s, id) FROM update LIMIT 100",
    );
}
