//! 式の内側に埋め込んだ相関スカラーサブクエリの結合テスト（SQL-61。`docs/spec/04-behavior/` の
//! ポインタのみ。Issue #1848、親 #1845。テナント境界は RLS-10 ポインタ）。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、期待値は
//! PostgreSQL 17 の意味論から手計算した固定値（各ケースのコメントに根拠）。算術・比較・
//! `COALESCE`・`CASE` の内側で、投影・WHERE・ORDER BY の各位置に相関スカラー（集計形・非集計形）を
//! 置いたときに、該当なしが NULL として式の中で伝播すること、結果列の型が PostgreSQL と一致する
//! こと、`21000`（非集計形で 2 件以上）・`54000`（再評価の上限）・使えない位置の `0A000`、
//! 他テナントの行が値・基数に影響しないことを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::correlated_limits::CorrelatedLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn vec_col() -> ColumnDef {
    ColumnDef::new("embedding", ColumnType::Vector(2), false)
}

fn col(name: &str, ty: ColumnType) -> ColumnDef {
    ColumnDef::new(name, ty, true)
}

fn a_schema() -> TableSchema {
    TableSchema::new(
        "a",
        vec![
            vec_col(),
            col("k", ColumnType::BigInt),
            col("t", ColumnType::Text),
            col("v", ColumnType::Integer),
            col("r", ColumnType::Real),
        ],
    )
}

fn b_schema() -> TableSchema {
    TableSchema::new(
        "b",
        vec![
            vec_col(),
            col("aid", ColumnType::BigInt),
            col("t", ColumnType::Text),
            col("v", ColumnType::Integer),
            col("flag", ColumnType::Boolean),
            col("x", ColumnType::Real),
            col(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"));
}

/// `INSERT INTO <table> (id, embedding, <cols>) VALUES (...)`。省略した列は NULL。
fn ins(core: &EngineCore, ctx: &PolicyContext, table: &str, id: u64, cols: &[(&str, &str)]) {
    let mut names = vec!["id".to_string(), "embedding".to_string()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".to_string()];
    for (c, v) in cols {
        names.push((*c).to_string());
        values.push((*v).to_string());
    }
    run(
        core,
        ctx,
        &format!(
            "INSERT INTO {table} ({}) VALUES ({}) USING OPERATION_ID '{table}-{id}'",
            names.join(", "),
            values.join(", ")
        ),
    );
}

fn query(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("sql={sql:?} should succeed: {e:?}"))
}

/// 結果の `id` を出現順のまま返す（ORDER BY の順序検証用）。
fn ids_in_order(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    query(core, ctx, sql).rows.iter().map(|r| r.id).collect()
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut v = ids_in_order(core, ctx, sql);
    v.sort_unstable();
    v
}

/// 数値セルを比較しやすい `i128` へ（`NULL` は `None`）。
fn num(cell: &Cell) -> Option<i128> {
    match cell {
        Cell::Null => None,
        Cell::Integer(n) => Some(i128::from(*n)),
        Cell::SignedInteger(n) => Some(i128::from(*n)),
        Cell::Float(f) => {
            assert!(f.fract() == 0.0, "non-integral float {f}");
            Some(*f as i128)
        }
        other => panic!("unexpected numeric cell {other:?}"),
    }
}

/// `SELECT id, <式> ... FROM a` の「最後の列」の数値を `id` 昇順で返す。
fn last_col_by_id(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<(u64, Option<i128>)> {
    let mut rows: Vec<(u64, Option<i128>)> = query(core, ctx, sql)
        .rows
        .iter()
        .map(|r| (r.id, num(r.cells.last().expect("cells"))))
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// 最後の列の結果列型（RowDescription の OID の根拠）。
fn last_col_type(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Option<ColumnType> {
    match query(core, ctx, sql).columns.last().expect("columns") {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        ColumnMeta::Scalar { ty, .. } => Some(ty.clone()),
        other => panic!("unexpected column meta {other:?}"),
    }
}

/// `(wire_code, message)`。
fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql) {
        Ok(_) => panic!("sql={sql:?} should be rejected"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn assert_no_reserved_names(sql: &str, msg: &str) {
    assert!(
        !msg.contains('\u{1}') && !msg.contains('\u{2}') && !msg.contains('\u{3}'),
        "reserved name leaked: sql={sql:?} msg={msg:?}"
    );
}

fn open_core(name: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    (storage, CleanupGuard(path))
}

fn seed(core: &EngineCore, ctx: &PolicyContext) {
    // a: id1 {k1, t'x', v10, r1.5}、id2 {k2, t'y', v20, r2.5}、id3 {k3, t NULL, v30}、id4 {k NULL, t'x', v5}
    ins(
        core,
        ctx,
        "a",
        1,
        &[("k", "1"), ("t", "'x'"), ("v", "10"), ("r", "1.5")],
    );
    ins(
        core,
        ctx,
        "a",
        2,
        &[("k", "2"), ("t", "'y'"), ("v", "20"), ("r", "2.5")],
    );
    ins(core, ctx, "a", 3, &[("k", "3"), ("v", "30")]);
    ins(core, ctx, "a", 4, &[("t", "'x'"), ("v", "5")]);
    // b: id1 {aid1, t'x', v5, flag true, x1.5}、id2 {aid1, t'z', v50, flag false, x2.5}、
    //    id3 {aid2, t'y', v20, flag true, x4.5}、id4 {aid NULL, v1, flag true}
    ins(
        core,
        ctx,
        "b",
        1,
        &[
            ("aid", "1"),
            ("t", "'x'"),
            ("v", "5"),
            ("flag", "true"),
            ("x", "1.5"),
            ("n", "1.25"),
        ],
    );
    ins(
        core,
        ctx,
        "b",
        2,
        &[
            ("aid", "1"),
            ("t", "'z'"),
            ("v", "50"),
            ("flag", "false"),
            ("x", "2.5"),
            ("n", "2.50"),
        ],
    );
    ins(
        core,
        ctx,
        "b",
        3,
        &[
            ("aid", "2"),
            ("t", "'y'"),
            ("v", "20"),
            ("flag", "true"),
            ("x", "4.5"),
            ("n", "3.00"),
        ],
    );
    ins(core, ctx, "b", 4, &[("v", "1"), ("flag", "true")]);
}

fn create_tables(storage: &Storage) {
    for schema in [a_schema(), b_schema()] {
        storage.create_table(&schema).expect("create table");
    }
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let (storage, guard) = open_core("sql61-correlated-scalar-expr");
    create_tables(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    (core, ctx, guard)
}

/// 集計形: キー別の `max(v)`（a1→max(5,50)=50、a2→20、a3→一致なしで NULL、a4→キー NULL で NULL）。
const MAXV: &str = "(SELECT max(b.v) FROM b WHERE b.aid = a.k)";
/// 集計形: キー別の件数（a1→2、a2→1、a3→0、a4→0。`count` は空集合で 0）。
const CNT: &str = "(SELECT count(*) FROM b WHERE b.aid = a.k)";
/// 非集計形: 等価キーのみ・一致は `flag = true` の行（a1→b1 の 5、a2→b3 の 20、a3 は無し、a4 はキー NULL）。
const KEY_ONLY: &str = "(SELECT b.v FROM b WHERE b.aid = a.k AND b.flag = true)";

// --- 投影位置: 算術・COALESCE・CASE ---------------------------------------------------------------

#[test]
fn projection_arithmetic_propagates_null_and_count_is_zero() {
    let (core, ctx, _g) = setup();
    // max(int4) + 1: 該当なしの NULL が式の中で伝播する（NULL + 1 は NULL）。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, {MAXV} + 1 FROM a")),
        vec![(1, Some(51)), (2, Some(21)), (3, None), (4, None)]
    );
    // count(*) * 2: 空集合の count は 0 なので 0 * 2 = 0（NULL にならない）。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, {CNT} * 2 FROM a")),
        vec![(1, Some(4)), (2, Some(2)), (3, Some(0)), (4, Some(0))]
    );
    // 左辺が外側の列: v - (非集計形)。a1: 10-5=5、a2: 20-20=0、a3・a4 は NULL。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, v - {KEY_ONLY} FROM a")),
        vec![(1, Some(5)), (2, Some(0)), (3, None), (4, None)]
    );
    // 除算（整数）: 100 / max(v)。a1: 100/50=2、a2: 100/20=5。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, 100 / {MAXV} FROM a")),
        vec![(1, Some(2)), (2, Some(5)), (3, None), (4, None)]
    );
}

#[test]
fn projection_coalesce_and_case_use_the_embedded_value() {
    let (core, ctx, _g) = setup();
    // COALESCE(NULL, 0) は 0。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, COALESCE({MAXV}, 0) FROM a")
        ),
        vec![(1, Some(50)), (2, Some(20)), (3, Some(0)), (4, Some(0))]
    );
    // COALESCE(非集計形, -1)。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, COALESCE({KEY_ONLY}, 0) + 100 FROM a")
        ),
        vec![
            (1, Some(105)),
            (2, Some(120)),
            (3, Some(100)),
            (4, Some(100))
        ]
    );
    // CASE WHEN count > 0 THEN 1 ELSE 0。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, CASE WHEN {CNT} > 0 THEN 1 ELSE 0 END FROM a")
        ),
        vec![(1, Some(1)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
}

#[test]
fn projection_case_with_embedded_comparison_treats_unknown_as_not_true() {
    let (core, ctx, _g) = setup();
    // CASE WHEN v > 非集計形 THEN 1 ELSE 0: a1 10>5 真、a2 20>20 偽、a3・a4 は UNKNOWN で ELSE 側。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, CASE WHEN v > {KEY_ONLY} THEN 1 ELSE 0 END FROM a")
        ),
        vec![(1, Some(1)), (2, Some(0)), (3, Some(0)), (4, Some(0))]
    );
    // 比較の右辺側の算術: v + 1 > max(v) / 2。a1 11>25 偽、a2 21>10 真、a3・a4 は UNKNOWN。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, CASE WHEN v + 1 > {MAXV} / 2 THEN 1 ELSE 0 END FROM a")
        ),
        vec![(1, Some(0)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
}

// --- 結果列の型（RowDescription の型 OID の根拠） ------------------------------------------------

#[test]
fn result_column_types_follow_the_inner_column_type() {
    let (core, ctx, _g) = setup();
    // max(int4) + 1 は int4。
    assert_eq!(
        last_col_type(&core, &ctx, &format!("SELECT id, {MAXV} + 1 FROM a")),
        Some(ColumnType::Integer)
    );
    // count(*) + 1 は bigint。
    assert_eq!(
        last_col_type(&core, &ctx, &format!("SELECT id, {CNT} + 1 FROM a")),
        Some(ColumnType::BigInt)
    );
    // real × 整数リテラルは、通常の real 列（`r * 2`）と同じく double precision（既存の式の型規則に
    // 一致する）。値は max(x) * 2。a1: 2.5*2=5、a2: 4.5*2=9。
    assert_eq!(
        last_col_type(
            &core,
            &ctx,
            "SELECT id, (SELECT max(b.x) FROM b WHERE b.aid = a.k) * 2 FROM a"
        ),
        last_col_type(&core, &ctx, "SELECT id, r * 2 FROM a")
    );
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT max(b.x) FROM b WHERE b.aid = a.k) * 2 FROM a"
        ),
        vec![(1, Some(5)), (2, Some(9)), (3, None), (4, None)]
    );
    // real 列 + max(real) は real（単精度どうしの型付き算術）。a1: 1.5+2.5=4、a2: 2.5+4.5=7、
    // a3・a4 は NULL。
    assert_eq!(
        last_col_type(
            &core,
            &ctx,
            "SELECT id, r + (SELECT max(b.x) FROM b WHERE b.aid = a.k) FROM a"
        ),
        Some(ColumnType::Real)
    );
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, r + (SELECT max(b.x) FROM b WHERE b.aid = a.k) FROM a"
        ),
        vec![(1, Some(4)), (2, Some(7)), (3, None), (4, None)]
    );
    // sum(int4) は bigint（PostgreSQL と同じ）。sum + 1 は bigint で、a1: 55+1=56、a2: 20+1=21。
    let sum = "(SELECT sum(b.v) FROM b WHERE b.aid = a.k)";
    assert_eq!(
        last_col_type(&core, &ctx, &format!("SELECT id, {sum} + 1 FROM a")),
        Some(ColumnType::BigInt)
    );
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, {sum} + 1 FROM a")),
        vec![(1, Some(56)), (2, Some(21)), (3, None), (4, None)]
    );
    // COALESCE(int4, int4) は int4、COALESCE(bigint, 0) は bigint。
    assert_eq!(
        last_col_type(
            &core,
            &ctx,
            &format!("SELECT id, COALESCE({MAXV}, 0) FROM a")
        ),
        Some(ColumnType::Integer)
    );
    assert_eq!(
        last_col_type(
            &core,
            &ctx,
            &format!("SELECT id, COALESCE({CNT}, 0) FROM a")
        ),
        Some(ColumnType::BigInt)
    );
    // 非集計形の int4 + int4 は int4。
    assert_eq!(
        last_col_type(&core, &ctx, &format!("SELECT id, v + {KEY_ONLY} FROM a")),
        Some(ColumnType::Integer)
    );
    // real の値の確認: max(x) * 2。a1: max(1.5, 2.5)*2=5、a2: 4.5*2=9。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT max(b.x) FROM b WHERE b.aid = a.k) * 2 FROM a"
        ),
        vec![(1, Some(5)), (2, Some(9)), (3, None), (4, None)]
    );
}

#[test]
fn numeric_results_cannot_be_used_in_arithmetic_and_are_rejected_with_0a000() {
    let (core, ctx, _g) = setup();
    // PostgreSQL では numeric + int は numeric。本実装は numeric 値を式の被演算子に使えない
    // （既存の規則。`CAST` で整数へ落とせば使える）ため、黙って丸めず 0A000。
    let sql = "SELECT id, (SELECT max(b.n) FROM b WHERE b.aid = a.k) + 1 FROM a";
    let (code, msg) = err(&core, &ctx, sql);
    assert_eq!(code, "0A000", "{msg}");
    assert_no_reserved_names(sql, &msg);
}

// --- WHERE 位置 -----------------------------------------------------------------------------------

#[test]
fn where_expression_predicates_drop_unknown_rows() {
    let (core, ctx, _g) = setup();
    // v + 1 > 非集計形: a1 11>5 真、a2 21>20 真、a3・a4 は UNKNOWN で落ちる。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v + 1 > {KEY_ONLY}")
        ),
        vec![1, 2]
    );
    // 逆向き（サブクエリが左辺）: 5 < 10 真、20 < 20 偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE {KEY_ONLY} < v")
        ),
        vec![1]
    );
    // 閉じ括弧の直後に算術が続く形: max(v) * 2 > v。a1 100>10 真、a2 40>20 真。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE {MAXV} * 2 > v")
        ),
        vec![1, 2]
    );
    // 列が左辺で右辺の埋め込みに算術が続く形: v > max(v) * 2 は a1 10>100 偽、a2 20>40 偽。
    assert!(ids(
        &core,
        &ctx,
        &format!("SELECT id FROM a WHERE v > {MAXV} * 2")
    )
    .is_empty());
    // COALESCE で NULL を 0 にする: max(v) が NULL の行（a3・a4）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE COALESCE({MAXV}, 0) = 0")
        ),
        vec![3, 4]
    );
    // count + 1 = 2 は count = 1 の a2 だけ。AND と併用する。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v >= 10 AND {CNT} + 1 = 2")
        ),
        vec![2]
    );
    // OR と併用する（count = 0 の a3・a4 または a1）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE {CNT} + 0 = 0 OR id = 1")
        ),
        vec![1, 3, 4]
    );
    // `<>`: 非集計形 + 0 <> 5。a1 は 5<>5 偽、a2 は 20<>5 真、a3・a4 は UNKNOWN。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE {KEY_ONLY} + 0 <> 5")
        ),
        vec![2]
    );
    // 従来形（列 <op> (SELECT ...) が述語の終端）は従来どおりの経路で同じ結果になる。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v = {KEY_ONLY}")
        ),
        vec![2]
    );
}

// --- ORDER BY --------------------------------------------------------------------------------------

#[test]
fn order_by_expression_keys_use_the_embedded_value() {
    let (core, ctx, _g) = setup();
    // COALESCE(max(v), 0) 昇順、同値は id 昇順: a3(0)・a4(0)・a2(20)・a1(50)。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a ORDER BY COALESCE({MAXV}, 0), id")
        ),
        vec![3, 4, 2, 1]
    );
    // count 降順、同値は id 昇順: a1(2)・a2(1)・a3(0)・a4(0)。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a ORDER BY {CNT} DESC, id")
        ),
        vec![1, 2, 3, 4]
    );
    // 括弧始まりのキー: ORDER BY (SELECT ...)。count 昇順、同値は id 昇順。
    assert_eq!(
        ids_in_order(&core, &ctx, &format!("SELECT id FROM a ORDER BY {CNT}, id")),
        vec![3, 4, 2, 1]
    );
    // LIMIT と併用: 先頭 2 件。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a ORDER BY {CNT} + 0 DESC, id LIMIT 2")
        ),
        vec![1, 2]
    );
}

// --- 式の形のバリエーション --------------------------------------------------------------------------

#[test]
fn embedded_values_compose_with_nested_expressions_casts_functions_and_ordinals() {
    let (core, ctx, _g) = setup();
    // 入れ子の括弧・乗算: (max(v) + 1) * 2。a1 (50+1)*2=102、a2 (20+1)*2=42。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, ({MAXV} + 1) * 2 FROM a")),
        vec![(1, Some(102)), (2, Some(42)), (3, None), (4, None)]
    );
    // 1 つの式に 2 つの埋め込み: max(v) + count(*)。int4 + int8 は bigint。a1 50+2=52、a2 20+1=21、
    // a3・a4 は max が NULL なので NULL。
    let two = format!("SELECT id, {MAXV} + {CNT} FROM a");
    assert_eq!(
        last_col_by_id(&core, &ctx, &two),
        vec![(1, Some(52)), (2, Some(21)), (3, None), (4, None)]
    );
    assert_eq!(last_col_type(&core, &ctx, &two), Some(ColumnType::BigInt));
    // 関数の引数（abs）: abs(0 - max(v))。a1 50、a2 20。
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, abs(0 - {MAXV}) FROM a")),
        vec![(1, Some(50)), (2, Some(20)), (3, None), (4, None)]
    );
    // CAST の被演算子: max(int4) の bigint 変換は bigint。
    let cast = format!("SELECT id, CAST({MAXV} AS bigint) + 1 FROM a");
    assert_eq!(
        last_col_by_id(&core, &ctx, &cast),
        vec![(1, Some(51)), (2, Some(21)), (3, None), (4, None)]
    );
    assert_eq!(last_col_type(&core, &ctx, &cast), Some(ColumnType::BigInt));
    // TEXT の値は関数の引数にできる（upper(max(t))）。a1 は max('x','z')='z'、a2 は 'y'。
    let r = query(
        &core,
        &ctx,
        "SELECT id, upper((SELECT max(b.t) FROM b WHERE b.aid = a.k)) FROM a",
    );
    let mut got: Vec<(u64, Cell)> = r
        .rows
        .iter()
        .map(|row| (row.id, row.cells.last().expect("cells").clone()))
        .collect();
    got.sort_by_key(|(id, _)| *id);
    assert_eq!(
        got,
        vec![
            (1, Cell::Text("Z".to_string())),
            (2, Cell::Text("Y".to_string())),
            (3, Cell::Null),
            (4, Cell::Null),
        ]
    );
    // 出力列の序数での並べ替え（NULL は昇順で最後）: a2(21)・a1(51)・a3・a4。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id, {MAXV} + 1 FROM a ORDER BY 2, id")
        ),
        vec![2, 1, 3, 4]
    );
    // 別名での並べ替え（式への別名。同じ結果）。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id, {MAXV} + 1 AS s FROM a ORDER BY s, id")
        ),
        vec![2, 1, 3, 4]
    );
    // LIMIT／OFFSET と併用する。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {MAXV} + 1 FROM a ORDER BY id LIMIT 1 OFFSET 1")
        ),
        vec![(2, Some(21))]
    );
    // 否定との併用: NOT (max(v) + 1 > 30)。a2 だけ（a1 は 51>30 で除外、a3・a4 は UNKNOWN）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE NOT ({MAXV} + 1 > 30)")
        ),
        vec![2]
    );
}

#[test]
fn unknown_columns_in_embedded_forms_report_user_written_names_only() {
    let (core, ctx, _g) = setup();
    for sql in [
        format!("SELECT id, nosuch + {MAXV} FROM a"),
        format!("SELECT id, {MAXV} + nosuch FROM a"),
        "SELECT id, (SELECT max(b.nosuch) FROM b WHERE b.aid = a.k) + 1 FROM a".to_string(),
    ] {
        let (code, msg) = err(&core, &ctx, &sql);
        assert_eq!(code, "42703", "sql={sql:?} msg={msg}");
        assert!(msg.contains("nosuch"), "{msg}");
        assert_no_reserved_names(&sql, &msg);
    }
    // 項目全体形の別名は ORDER BY から参照できない（従来どおり。内部名を出さず 42703）。
    let sql = format!("SELECT id, {MAXV} AS s FROM a ORDER BY s");
    let (code, msg) = err(&core, &ctx, &sql);
    assert_eq!(code, "42703", "{msg}");
    assert_no_reserved_names(&sql, &msg);
}

#[test]
fn embedded_predicates_work_with_literal_comparisons_and_secondary_indexes() {
    let (core, ctx, _g) = setup();
    // 定数との等価（左右どちらでも）。非集計形は a2 だけが 20。
    for sql in [
        format!("SELECT id FROM a WHERE {KEY_ONLY} + 0 = 20"),
        format!("SELECT id FROM a WHERE 20 = {KEY_ONLY} + 0"),
        format!("SELECT id FROM a WHERE ({KEY_ONLY}) * 1 = 20"),
    ] {
        assert_eq!(ids(&core, &ctx, &sql), vec![2], "{sql}");
    }
    // 二次索引を持つ列と併用しても、式述語は全走査へ縮退して同じ結果になる（索引を黙って使わない）。
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut ddl, "CREATE INDEX a_v_idx ON a (v)")
        .expect("create index");
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v >= 10 AND v + 1 > {KEY_ONLY}")
        ),
        vec![1, 2]
    );
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v > 0 ORDER BY COALESCE(v, 0), {CNT} DESC")
        ),
        vec![4, 1, 2, 3]
    );
}

#[test]
fn index_ordered_scans_and_static_errors_coexist_with_embedded_leaves() {
    let (core, ctx, _g) = setup();
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut ddl, "CREATE INDEX a_v_idx ON a (v)")
        .expect("create index");
    // 索引順の走査でも、ORDER BY の式キーが参照する外側列（相関キー）がデコード対象に入る。
    // v 昇順（a4=5・a1=10・a2=20・a3=30）。同値は無い。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v > 0 ORDER BY v, {CNT} DESC")
        ),
        vec![4, 1, 2, 3]
    );
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a ORDER BY v DESC, {CNT} DESC LIMIT 2")
        ),
        vec![3, 2]
    );
    // 式キーが先頭: count 昇順（a3=0・a2=1・a1=2）。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v >= 10 ORDER BY {CNT}, v")
        ),
        vec![3, 2, 1]
    );
    // 同じ WHERE に、実行時データ例外（0 除算 22012）を遅延する非相関のスカラー比較と埋め込みの葉を
    // 混在させても、静的な束縛エラー（0A000）にならず 22012 が返る（順序の入れ替えも同じ）。
    for sql in [
        format!("SELECT id FROM a WHERE v > (SELECT 1/0 FROM b LIMIT 1) AND {MAXV} + 1 > 0"),
        format!("SELECT id FROM a WHERE {MAXV} + 1 > 0 AND v > (SELECT 1/0 FROM b LIMIT 1)"),
    ] {
        let (code, msg) = err(&core, &ctx, &sql);
        assert_eq!(code, "22012", "sql={sql:?} msg={msg}");
        assert_no_reserved_names(&sql, &msg);
    }
}

// --- 基数（21000）の遅延 -----------------------------------------------------------------------------

#[test]
fn non_aggregate_two_matches_raise_21000_only_when_an_outer_row_is_evaluated() {
    let (core, ctx, _g) = setup();
    // a1 はキー 1 の内側行が 2 行（b1・b2）。外側の行が評価されると 21000。
    let many = "(SELECT b.v FROM b WHERE b.aid = a.k)";
    for sql in [
        format!("SELECT id, {many} + 1 FROM a"),
        format!("SELECT id FROM a WHERE {many} + 1 > 0"),
    ] {
        let (code, msg) = err(&core, &ctx, &sql);
        assert_eq!(code, "21000", "sql={sql:?} msg={msg}");
        assert_no_reserved_names(&sql, &msg);
    }
    // 外側が 0 行なら評価されずエラーにならない（遅延評価）。
    assert!(query(
        &core,
        &ctx,
        &format!("SELECT id, {many} + 1 FROM a WHERE v > 1000")
    )
    .rows
    .is_empty());
    // 2 件以上一致する行（a1）を WHERE で除けば、残りの外側は評価できる（a2 は 20+1、a3・a4 は NULL）。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, {many} + 1 FROM a WHERE id >= 2")
        ),
        vec![(2, Some(21)), (3, None), (4, None)]
    );
}

// --- 上限・fail-closed -----------------------------------------------------------------------------

fn setup_with_limits(
    name: &str,
    limits: CorrelatedLimits,
) -> (EngineCore, PolicyContext, CleanupGuard) {
    let (storage, guard) = open_core(name);
    create_tables(&storage);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_correlated_limits(limits);
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    (core, ctx, guard)
}

#[test]
fn reevaluation_limit_rejects_with_54000_for_embedded_forms() {
    // キーなし（`b.v < a.v` だけ）は外側 1 行ごとに再評価 1 回を計上する（外側 4 行）。flag=false の
    // 内側は b2(50) だけで `50 < a.v` を満たす外側は無い（全行 NULL）。
    let inner = "(SELECT b.v FROM b WHERE b.v < a.v AND b.flag = false)";
    let proj = format!("SELECT id, {inner} + 1 FROM a");
    let wher = format!("SELECT id FROM a WHERE v + 1 > {inner}");
    let (core, ctx, _g) = setup_with_limits("sql61-expr-limit-ok", CorrelatedLimits::new(4));
    assert_eq!(
        last_col_by_id(&core, &ctx, &proj),
        vec![(1, None), (2, None), (3, None), (4, None)]
    );
    assert!(ids(&core, &ctx, &wher).is_empty());
    let (core, ctx, _g) = setup_with_limits("sql61-expr-limit-ng", CorrelatedLimits::new(3));
    for sql in [&proj, &wher] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "54000", "sql={sql:?} msg={msg}");
        assert_no_reserved_names(sql, &msg);
    }
    // 外側が 0 行なら葉が評価されないため上限にも達しない。
    assert!(query(
        &core,
        &ctx,
        &format!("SELECT id, {inner} + 1 FROM a WHERE v > 1000")
    )
    .rows
    .is_empty());
    // 等価キーのみ（残余なし）の集計形はハッシュ引きで再評価を計上しない（上限 1 でも成功する）。
    let (core, ctx, _g) = setup_with_limits("sql61-expr-limit-key", CorrelatedLimits::new(1));
    assert_eq!(
        last_col_by_id(&core, &ctx, &format!("SELECT id, {MAXV} + 1 FROM a")),
        vec![(1, Some(51)), (2, Some(21)), (3, None), (4, None)]
    );
}

#[test]
fn unsupported_positions_and_shapes_are_rejected_fail_closed() {
    let (core, ctx, _g) = setup();
    let rejects = [
        // 非相関のスカラーサブクエリを式の内側へ埋め込む形は従来どおり 42601。
        "SELECT id, (SELECT max(v) FROM b) + 1 FROM a",
        "SELECT id FROM a WHERE v > (SELECT max(v) FROM b) * 2",
        // GROUP BY キー・HAVING・外側が集計。
        "SELECT count(*) FROM a GROUP BY (SELECT 1) + k",
        "SELECT t, count(*) FROM a GROUP BY t HAVING count(*) > (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1",
        "SELECT count(*) FROM a WHERE v > (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1",
        // 外側が JOIN・カンマ区切り FROM。
        "SELECT a.id, (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 FROM a, b",
        "SELECT a.id, (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 FROM a JOIN b ON a.k = b.aid",
        // 括弧で囲んだだけの項目。
        "SELECT id, ((SELECT max(b.v) FROM b WHERE b.aid = a.k)) FROM a",
        // 値として運べない内側の型（VECTOR・JSON 系・CHAR(n)）は 0A000。
        "SELECT id, (SELECT b.embedding FROM b WHERE b.aid = a.k AND b.flag = true) IS NULL FROM a",
        // EXPLAIN・DISTINCT・DML。
        "EXPLAIN SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 FROM a",
        "SELECT DISTINCT v + (SELECT max(b.v) FROM b WHERE b.aid = a.k) FROM a",
        "UPDATE a SET v = (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 USING OPERATION_ID 'u-1'",
        "DELETE FROM a WHERE v > (SELECT max(b.v) FROM b WHERE b.aid = a.k) * 2 USING OPERATION_ID 'd-1'",
        "INSERT INTO a (id, embedding, v) VALUES (900, '[0.1,0.2]', (SELECT max(b.v) FROM b WHERE b.aid = 1) + 1) USING OPERATION_ID 'i-1'",
        // ビュー本文・CTE・集合演算の枝・UNION は式の内側への埋め込みを受理しない。
        "CREATE VIEW ev AS SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 AS m FROM a LIMIT 10",
        "WITH c AS (SELECT id, k FROM a) SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = c.k) + 1 FROM c",
        "SELECT id FROM a UNION ALL SELECT id FROM a WHERE v > (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1",
        // ウィンドウ関数・USING PLAN とは併用できない。
        "SELECT id, row_number() OVER (), (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1 FROM a",
    ];
    for sql in rejects {
        let (code, msg) = err(&core, &ctx, sql);
        assert!(
            matches!(code.as_str(), "42601" | "0A000" | "42703" | "42P01"),
            "sql={sql:?} code={code} msg={msg}"
        );
        assert_no_reserved_names(sql, &msg);
    }
    // GROUP BY のキーに使えない形（相関スカラーを含む）は 0A000（構文は正しいが未対応）。
    for sql in [
        "SELECT count(*) FROM a GROUP BY (SELECT 1) + k",
        "SELECT count(*) FROM a GROUP BY (SELECT max(b.v) FROM b WHERE b.aid = a.k)",
        "SELECT count(*) FROM a GROUP BY (SELECT max(b.v) FROM b WHERE b.aid = a.k) + 1",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "0A000", "sql={sql:?} msg={msg}");
        assert_no_reserved_names(sql, &msg);
    }
}

#[test]
fn reserved_internal_names_cannot_be_written_by_users() {
    let (core, ctx, _g) = setup();
    // 予約内部名は制御文字で始まり、字句段で利用者が書けない（引用識別子でも拒否される）。
    for sql in [
        "SELECT \"\u{1}ssq0\" + 1 FROM a",
        "SELECT id FROM a WHERE \"\u{1}ssq0\" > 1",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert!(
            matches!(code.as_str(), "42601" | "0A000" | "42703"),
            "sql={sql:?} code={code}"
        );
        assert_no_reserved_names(sql, &msg);
    }
}

// --- 従来形との共存 -------------------------------------------------------------------------------

#[test]
fn whole_item_and_embedded_forms_can_be_mixed_in_one_statement() {
    let (core, ctx, _g) = setup();
    // 項目全体形（max）と埋め込み形（max + 1）を同じ SELECT に置く。a1: (50, 51)、a2: (20, 21)。
    let r = query(
        &core,
        &ctx,
        &format!("SELECT id, {MAXV}, {MAXV} + 1 FROM a WHERE id <= 2"),
    );
    let mut got: Vec<(u64, Option<i128>, Option<i128>)> = r
        .rows
        .iter()
        .map(|row| (row.id, num(&row.cells[1]), num(&row.cells[2])))
        .collect();
    got.sort_by_key(|(id, _, _)| *id);
    assert_eq!(got, vec![(1, Some(50), Some(51)), (2, Some(20), Some(21))]);
}

// --- テナント境界（RLS-10） ----------------------------------------------------------------------

#[test]
fn other_tenants_rows_never_affect_values_cardinality_or_errors() {
    let (core, t1, _g) = setup();
    let t2 = ctx_for("t2");
    // t2 の行: b にキー 1 を 2 行（v1000・1001、flag true）、a に k=1 と k=2 の行。
    ins(
        &core,
        &t2,
        "b",
        100,
        &[("aid", "1"), ("v", "1000"), ("flag", "true")],
    );
    ins(
        &core,
        &t2,
        "b",
        101,
        &[("aid", "1"), ("v", "1001"), ("flag", "true")],
    );
    ins(&core, &t2, "a", 300, &[("k", "1"), ("v", "7")]);
    ins(&core, &t2, "a", 301, &[("k", "2"), ("v", "9")]);
    // t1 から見た値・件数は t2 の行を含めず、21000 も発生しない。
    assert_eq!(
        last_col_by_id(&core, &t1, &format!("SELECT id, {MAXV} + 1 FROM a")),
        vec![(1, Some(51)), (2, Some(21)), (3, None), (4, None)]
    );
    assert_eq!(
        last_col_by_id(&core, &t1, &format!("SELECT id, {CNT} * 2 FROM a")),
        vec![(1, Some(4)), (2, Some(2)), (3, Some(0)), (4, Some(0))]
    );
    assert_eq!(
        last_col_by_id(&core, &t1, &format!("SELECT id, {KEY_ONLY} + 1 FROM a")),
        vec![(1, Some(6)), (2, Some(21)), (3, None), (4, None)]
    );
    assert_eq!(
        ids(
            &core,
            &t1,
            &format!("SELECT id FROM a WHERE v + 1 > {KEY_ONLY}")
        ),
        vec![1, 2]
    );
    // t2 は自分の行だけで評価される（a300 はキー 1 に 2 件で max(v)=1001、count=2。a301 のキー 2 は内側無し）。
    assert_eq!(
        last_col_by_id(&core, &t2, &format!("SELECT id, {MAXV} + 1 FROM a")),
        vec![(300, Some(1002)), (301, None)]
    );
    assert_eq!(
        last_col_by_id(&core, &t2, &format!("SELECT id, {CNT} FROM a")),
        vec![(300, Some(2)), (301, Some(0))]
    );
    // t2 の非集計形はキー 1 に 2 件で 21000。t1 のエラー有無とは独立。
    let (code, _) = err(&core, &t2, &format!("SELECT id, {KEY_ONLY} + 1 FROM a"));
    assert_eq!(code, "21000");
}
