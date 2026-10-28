//! `JOIN ... USING (...)`（INNER／LEFT／RIGHT／FULL [OUTER]）の結合テスト
//! （Issue #1826。ポインタ: SQL-60 (1)・SQL-28・RLS-10）。
//!
//! `tests/sql28_outer_join.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。
//! 検証対象は (a) 等価 ON への脱糖、(b) 結合列の統合規則（値・列順・非修飾名の解決）、
//! (c) 外部結合での NULL 補完、(d) 未対応形の fail-closed な拒否、(e) テナント境界。
//! 期待値は PostgreSQL 17 の結果に合わせた手計算値。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// `(k BIGINT NULL, <text_col> TEXT)` の 2 列表。
fn kv_schema(name: &str, text_col: &str, key_ty: ColumnType) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("k", key_ty, true),
            ColumnDef::new(text_col, ColumnType::Text, false),
        ],
    )
}

/// `(k BIGINT, j TEXT, <text_col> TEXT)` の 3 列表（複数列 USING 用）。
fn kjv_schema(name: &str, text_col: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("k", ColumnType::BigInt, false),
            ColumnDef::new("j", ColumnType::Text, false),
            ColumnDef::new(text_col, ColumnType::Text, false),
        ],
    )
}

fn insert(storage: &Storage, table: &str, tenant: &str, id: u64, vis: Visibility, v: &[Value]) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "seed-{table}-{tenant}-{id}"
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(storage, table, &ctx(tenant), id, vis, v, &op_id)
        .expect("insert row");
}

fn insert_kv(storage: &Storage, table: &str, id: u64, k: Option<i64>, text: &str) {
    insert_kv_as(storage, table, "tenant-a", id, k, text, Visibility::Public);
}

fn insert_kv_as(
    storage: &Storage,
    table: &str,
    tenant: &str,
    id: u64,
    k: Option<i64>,
    text: &str,
    vis: Visibility,
) {
    insert(
        storage,
        table,
        tenant,
        id,
        vis,
        &[
            k.map_or(Value::Null, Value::BigInt),
            Value::Text(text.to_string()),
        ],
    );
}

fn insert_text_kv(storage: &Storage, table: &str, id: u64, k: &str, text: &str) {
    insert(
        storage,
        table,
        "tenant-a",
        id,
        Visibility::Public,
        &[Value::Text(k.to_string()), Value::Text(text.to_string())],
    );
}

fn insert_kjv(storage: &Storage, table: &str, id: u64, k: i64, j: &str, text: &str) {
    insert(
        storage,
        table,
        "tenant-a",
        id,
        Visibility::Public,
        &[
            Value::BigInt(k),
            Value::Text(j.to_string()),
            Value::Text(text.to_string()),
        ],
    );
}

/// a: (1,a1)(2,a2)(3,a3)(NULL,a-null)、b: (2,b2)(3,b3)(4,b4)(2,b2x)、c: (2,c2)(4,c4)、
/// d/e: 複数列 USING 用、f(k TEXT)・g(k INTEGER)・h/i(embedding VECTOR) は型検証用の空表。
fn seeded() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("join-using");
    let storage = Storage::open(&path).expect("open storage");
    for (name, col) in [("a", "v"), ("b", "w"), ("c", "x")] {
        storage
            .create_table(&kv_schema(name, col, ColumnType::BigInt))
            .expect("create table");
    }
    storage
        .create_table(&kv_schema("f", "fv", ColumnType::Text))
        .expect("create f");
    storage
        .create_table(&kv_schema("g", "gv", ColumnType::Integer))
        .expect("create g");
    for name in ["h", "i"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create vector table");
    }
    // WHERE の文字列リテラル検証用（JOIN の WHERE リテラルは文字列キーで書く）。
    for (name, col) in [("s", "sv"), ("t", "tv")] {
        storage
            .create_table(&kv_schema(name, col, ColumnType::Text))
            .expect("create text-key table");
    }
    for (id, k, t) in [(1, "p", "s-p"), (2, "q", "s-q")] {
        insert_text_kv(&storage, "s", id, k, t);
    }
    for (id, k, t) in [(1, "q", "t-q"), (2, "r", "t-r")] {
        insert_text_kv(&storage, "t", id, k, t);
    }
    storage
        .create_table(&kjv_schema("d", "p"))
        .expect("create d");
    storage
        .create_table(&kjv_schema("e", "q"))
        .expect("create e");

    for (id, k, t) in [
        (1, Some(1), "a1"),
        (2, Some(2), "a2"),
        (3, Some(3), "a3"),
        (4, None, "a-null"),
    ] {
        insert_kv(&storage, "a", id, k, t);
    }
    for (id, k, t) in [
        (1, Some(2), "b2"),
        (2, Some(3), "b3"),
        (3, Some(4), "b4"),
        (4, Some(2), "b2x"),
    ] {
        insert_kv(&storage, "b", id, k, t);
    }
    for (id, k, t) in [(1, Some(2), "c2"), (2, Some(4), "c4")] {
        insert_kv(&storage, "c", id, k, t);
    }
    for (id, k, j, t) in [(1, 1, "x", "d1x"), (2, 1, "y", "d1y"), (3, 2, "x", "d2x")] {
        insert_kjv(&storage, "d", id, k, j, t);
    }
    for (id, k, j, t) in [(1, 1, "x", "e1x"), (2, 2, "y", "e2y"), (3, 1, "y", "e1y")] {
        insert_kjv(&storage, "e", id, k, j, t);
    }
    (storage, path)
}

fn try_run(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<QueryResult, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    try_run(core, "tenant-a", sql)
        .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}"))
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    match try_run(core, "tenant-a", sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => assert_eq!(e.wire_code(), wire_code, "sql={sql:?} err={e:?}"),
    }
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(render).collect())
        .collect()
}

fn sorted_rows(result: &QueryResult) -> Vec<Vec<String>> {
    let mut rows = rows_of(result);
    rows.sort();
    rows
}

fn col_names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::Record { name, .. } => name.clone(),
        })
        .collect()
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| (*s).to_string()).collect())
        .collect()
}

// ---------- INNER ----------

#[test]
fn inner_using_merges_join_column_and_orders_star() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(&core, "SELECT * FROM a JOIN b USING (k) LIMIT 20");
    // 統合列 → 左の残り → 右の残り（疑似列 id は各表に残る）。
    assert_eq!(col_names(&result), ["k", "id", "v", "id", "w"]);
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["2", "2", "a2", "1", "b2"],
            &["2", "2", "a2", "4", "b2x"],
            &["3", "3", "a3", "2", "b3"],
        ])
    );
}

#[test]
fn inner_using_unqualified_name_is_unambiguous_and_qualified_names_stay_original() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT k, v, w, a.k, b.k FROM a JOIN b USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["2", "a2", "b2", "2", "2"],
            &["2", "a2", "b2x", "2", "2"],
            &["3", "a3", "b3", "3", "3"],
        ])
    );
    // 統合されていない同名列（疑似列 id）は非修飾だと曖昧のまま。
    assert_rejected(&core, "SELECT id FROM a JOIN b USING (k) LIMIT 5", "42702");
}

// ---------- LEFT / RIGHT / FULL ----------

#[test]
fn left_using_keeps_left_rows_and_takes_left_value() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT k, v, w, a.k, b.k FROM a LEFT JOIN b USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["1", "a1", "NULL", "1", "NULL"],
            &["2", "a2", "b2", "2", "2"],
            &["2", "a2", "b2x", "2", "2"],
            &["3", "a3", "b3", "3", "3"],
            &["NULL", "a-null", "NULL", "NULL", "NULL"],
        ])
    );
}

#[test]
fn right_using_keeps_right_rows_and_takes_right_value() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT k, v, w, a.k, b.k FROM a RIGHT JOIN b USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["2", "a2", "b2", "2", "2"],
            &["2", "a2", "b2x", "2", "2"],
            &["3", "a3", "b3", "3", "3"],
            &["4", "NULL", "b4", "NULL", "4"],
        ])
    );
}

#[test]
fn full_using_coalesces_and_keeps_both_unmatched_sides() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT k, v, w FROM a FULL OUTER JOIN b USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["1", "a1", "NULL"],
            &["2", "a2", "b2"],
            &["2", "a2", "b2x"],
            &["3", "a3", "b3"],
            &["4", "NULL", "b4"],
            &["NULL", "a-null", "NULL"],
        ])
    );

    // `SELECT *` の列順と、右側のみの行の統合列（右の値）。
    let star = run(&core, "SELECT * FROM a FULL JOIN b USING (k) LIMIT 20");
    assert_eq!(col_names(&star), ["k", "id", "v", "id", "w"]);
    assert!(sorted_rows(&star).contains(&strs(&[&["4", "NULL", "NULL", "3", "b4"]])[0]));
}

#[test]
fn full_using_merged_column_orders_by_name_and_ordinal() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let firsts = |result: &QueryResult| -> Vec<String> {
        rows_of(result).into_iter().map(|r| r[0].clone()).collect()
    };
    let by_name = run(
        &core,
        "SELECT k, w FROM a FULL JOIN b USING (k) ORDER BY k LIMIT 20",
    );
    assert_eq!(firsts(&by_name), ["1", "2", "2", "3", "4", "NULL"]);
    let by_ordinal_desc = run(
        &core,
        "SELECT k, w FROM a FULL JOIN b USING (k) ORDER BY 1 DESC LIMIT 20",
    );
    assert_eq!(firsts(&by_ordinal_desc), ["NULL", "4", "3", "2", "2", "1"]);
    // `*` の序数 1 は統合列。
    let star_ordinal = run(
        &core,
        "SELECT * FROM a FULL JOIN b USING (k) ORDER BY 1 LIMIT 20",
    );
    assert_eq!(firsts(&star_ordinal), ["1", "2", "2", "3", "4", "NULL"]);
}

// ---------- 複数列・連鎖・ON との混在 ----------

#[test]
fn multi_column_using_follows_list_order() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(&core, "SELECT * FROM d JOIN e USING (k, j) LIMIT 20");
    assert_eq!(col_names(&result), ["k", "j", "id", "p", "id", "q"]);
    assert_eq!(
        sorted_rows(&result),
        strs(&[
            &["1", "x", "1", "d1x", "1", "e1x"],
            &["1", "y", "2", "d1y", "3", "e1y"],
        ])
    );
    let swapped = run(&core, "SELECT * FROM d JOIN e USING (j, k) LIMIT 20");
    assert_eq!(col_names(&swapped), ["j", "k", "id", "p", "id", "q"]);
}

#[test]
fn chained_using_merges_column_across_steps() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let star = run(
        &core,
        "SELECT * FROM a JOIN b USING (k) JOIN c USING (k) LIMIT 20",
    );
    assert_eq!(col_names(&star), ["k", "id", "v", "id", "w", "id", "x"]);
    let result = run(
        &core,
        "SELECT k, v, w, x FROM a JOIN b USING (k) JOIN c USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&result),
        strs(&[&["2", "a2", "b2", "c2"], &["2", "a2", "b2x", "c2"]])
    );

    let left = run(
        &core,
        "SELECT k, w, x FROM a LEFT JOIN b USING (k) LEFT JOIN c USING (k) LIMIT 20",
    );
    assert_eq!(
        sorted_rows(&left),
        strs(&[
            &["1", "NULL", "NULL"],
            &["2", "b2", "c2"],
            &["2", "b2x", "c2"],
            &["3", "b3", "NULL"],
            &["NULL", "NULL", "NULL"],
        ])
    );
}

// ---------- WHERE・集計 ----------

#[test]
fn where_on_merged_column_resolves_to_the_correct_side() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    // JOIN の WHERE リテラルは文字列で検証する（text の結合列 `k`）。
    let left = run(
        &core,
        "SELECT k, sv, tv FROM s LEFT JOIN t USING (k) WHERE k = 'p' LIMIT 20",
    );
    assert_eq!(rows_of(&left), strs(&[&["p", "s-p", "NULL"]]));
    let right = run(
        &core,
        "SELECT k, sv, tv FROM s RIGHT JOIN t USING (k) WHERE k = 'r' LIMIT 20",
    );
    assert_eq!(rows_of(&right), strs(&[&["r", "NULL", "t-r"]]));
    let inner = run(
        &core,
        "SELECT k, sv, tv FROM s JOIN t USING (k) WHERE k = 'q' LIMIT 20",
    );
    assert_eq!(rows_of(&inner), strs(&[&["q", "s-q", "t-q"]]));
}

#[test]
fn aggregate_over_merged_column_groups_correctly() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT k, count(*) FROM a JOIN b USING (k) GROUP BY k ORDER BY k",
    );
    assert_eq!(rows_of(&result), strs(&[&["2", "2"], &["3", "1"]]));
}

#[test]
fn using_on_pseudo_id_on_both_sides_is_accepted_and_merged() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    let result = run(&core, "SELECT * FROM a JOIN b USING (id) LIMIT 20");
    assert_eq!(col_names(&result), ["id", "k", "v", "k", "w"]);
    assert_eq!(result.columns.first(), Some(&ColumnMeta::Id));
    assert_eq!(result.rows.len(), 4);
}

// ---------- 疑似列 id と実カラム id の共存 ----------

/// 疑似列 `id`（行キー）と実カラム `id`（TEXT）を併せ持つ表を USING 結合した `SELECT *` と
/// 序数 `ORDER BY` は、疑似列と実カラムを取り違えない（セル位置はスロットから決める）。
#[test]
fn using_star_and_ordinal_order_keep_pseudo_id_distinct_from_real_id_column() {
    let path = unique_db_path("join-using-real-id");
    let storage = Storage::open(&path).expect("open storage");
    let _guard = CleanupGuard(path);
    storage
        .create_table(&TableSchema::new(
            "p",
            vec![
                ColumnDef::new("k", ColumnType::BigInt, false),
                ColumnDef::new("id", ColumnType::Text, false),
            ],
        ))
        .expect("create p");
    storage
        .create_table(&kv_schema("q", "w", ColumnType::BigInt))
        .expect("create q");
    for (rid, real) in [(100_u64, "real-b"), (101, "real-a")] {
        insert(
            &storage,
            "p",
            "tenant-a",
            rid,
            Visibility::Public,
            &[Value::BigInt(1), Value::Text(real.to_string())],
        );
    }
    insert_kv(&storage, "q", 200, Some(1), "w1");
    let core = new_core(storage);

    let result = run(
        &core,
        "SELECT * FROM p JOIN q USING (k) ORDER BY 2 LIMIT 20",
    );
    assert_eq!(col_names(&result), ["k", "id", "id", "id", "w"]);
    assert_eq!(result.columns.get(1), Some(&ColumnMeta::Id));
    assert!(matches!(
        result.columns.get(2),
        Some(ColumnMeta::Scalar { .. })
    ));
    assert_eq!(
        rows_of(&result),
        strs(&[
            &["1", "100", "real-b", "200", "w1"],
            &["1", "101", "real-a", "200", "w1"],
        ])
    );

    // 序数 3（実カラム `id`）で並べ替えると real-a（行キー 101）が先頭になる。
    let result = run(
        &core,
        "SELECT * FROM p JOIN q USING (k) ORDER BY 3 LIMIT 20",
    );
    assert_eq!(
        rows_of(&result),
        strs(&[
            &["1", "101", "real-a", "200", "w1"],
            &["1", "100", "real-b", "200", "w1"],
        ])
    );
}

// ---------- 拒否（fail-closed） ----------

#[test]
fn rejects_invalid_using_lists_and_unsupported_forms() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);

    // 列の存在・重複（USING リスト順に判定。`nosuch, nosuch` は 42703 が先）。
    for (sql, code) in [
        ("SELECT * FROM a JOIN b USING (nosuch) LIMIT 5", "42703"),
        ("SELECT * FROM a JOIN b USING (v) LIMIT 5", "42703"),
        ("SELECT * FROM a JOIN b USING (w) LIMIT 5", "42703"),
        ("SELECT * FROM a JOIN b USING (k, k) LIMIT 5", "42701"),
        (
            "SELECT * FROM a JOIN b USING (nosuch, nosuch) LIMIT 5",
            "42703",
        ),
        // 左に未統合の同名列が複数ある。
        (
            "SELECT * FROM a JOIN b ON a.k = b.k JOIN c USING (k) LIMIT 5",
            "42702",
        ),
        // 型: クラス不一致は ON と同じ 42804、同クラスで型が異なる組は 0A000、VECTOR は 42601。
        ("SELECT * FROM a JOIN f USING (k) LIMIT 5", "42804"),
        ("SELECT * FROM a JOIN g USING (k) LIMIT 5", "0A000"),
        ("SELECT * FROM h JOIN i USING (embedding) LIMIT 5", "42601"),
        // FULL の統合列は値を組み立てられない文脈（WHERE・GROUP BY・後続段の結合キー）で 0A000。
        (
            "SELECT sv FROM s FULL JOIN t USING (k) WHERE k = 'p' LIMIT 5",
            "0A000",
        ),
        (
            "SELECT k, count(*) FROM a FULL JOIN b USING (k) GROUP BY k",
            "0A000",
        ),
        (
            "SELECT * FROM a FULL JOIN b USING (k) JOIN c USING (k) LIMIT 5",
            "0A000",
        ),
        // 構文: 空リスト・修飾名・USING の別名。
        ("SELECT * FROM a JOIN b USING () LIMIT 5", "42601"),
        ("SELECT * FROM a JOIN b USING (a.k) LIMIT 5", "42601"),
        ("SELECT * FROM a JOIN b USING (k) AS j LIMIT 5", "42601"),
        (
            "SELECT * FROM a JOIN b USING (c1, c2, c3, c4, c5, c6, c7, c8, c9) LIMIT 5",
            "54000",
        ),
    ] {
        assert_rejected(&core, sql, code);
    }
}

#[test]
fn mixed_using_and_on_unqualified_merged_name_is_ambiguous() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage);
    assert_rejected(
        &core,
        "SELECT k FROM a JOIN b USING (k) JOIN c ON c.k = a.k LIMIT 5",
        "42702",
    );
    let result = run(
        &core,
        "SELECT x, c.k FROM a JOIN b USING (k) JOIN c ON c.k = a.k LIMIT 20",
    );
    assert_eq!(sorted_rows(&result), strs(&[&["c2", "2"], &["c2", "2"]]));
}

// ---------- 上限 ----------

#[test]
fn using_respects_input_and_output_row_limits() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = new_core(storage).with_join_limits(JoinSizeLimits::new(8, 3, 100));
    // a は可視 4 行 > 入力上限 3。
    assert_rejected(&core, "SELECT * FROM a JOIN b USING (k) LIMIT 5", "54000");

    let (storage, path) = seeded();
    let _guard2 = CleanupGuard(path);
    let core = new_core(storage).with_join_limits(JoinSizeLimits::new(8, 100, 2));
    // 一致ペア 3 件 > 出力上限 2。
    assert_rejected(&core, "SELECT * FROM a JOIN b USING (k) LIMIT 5", "54000");
}

// ---------- テナント境界（RLS-10） ----------

#[test]
fn using_results_never_include_other_tenants_rows() {
    let (storage, path) = seeded();
    let _guard = CleanupGuard(path);
    // 別テナントの非公開行（結合キーが一致しうる値と、統合列に現れうる値）。
    for (table, col_text) in [("a", "leak-a"), ("b", "leak-b")] {
        insert_kv_as(
            &storage,
            table,
            "tenant-b",
            100,
            Some(2),
            col_text,
            Visibility::Private,
        );
        insert_kv_as(
            &storage,
            table,
            "tenant-b",
            101,
            Some(9),
            col_text,
            Visibility::Private,
        );
    }
    let core = new_core(storage);

    for sql in [
        "SELECT k, v, w FROM a JOIN b USING (k) LIMIT 50",
        "SELECT k, v, w FROM a LEFT JOIN b USING (k) LIMIT 50",
        "SELECT k, v, w FROM a RIGHT JOIN b USING (k) LIMIT 50",
        "SELECT k, v, w FROM a FULL JOIN b USING (k) LIMIT 50",
        "SELECT * FROM a FULL JOIN b USING (k) LIMIT 50",
    ] {
        let result = run(&core, sql);
        for row in rows_of(&result) {
            assert!(
                row.iter().all(|c| !c.starts_with("leak") && c != "9"),
                "other tenant's row leaked: sql={sql:?} row={row:?}"
            );
        }
    }
    // 件数も baseline（他テナント行なし）と一致する。
    let full = run(
        &core,
        "SELECT k, v, w FROM a FULL JOIN b USING (k) LIMIT 50",
    );
    assert_eq!(full.rows.len(), 6);
}
