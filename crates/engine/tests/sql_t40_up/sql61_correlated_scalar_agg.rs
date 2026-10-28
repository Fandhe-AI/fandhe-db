//! 集計形の相関スカラーサブクエリ（左外部結合＋集計と同値の実行）の結合テスト（SQL-61。
//! `docs/spec/04-behavior/` のポインタのみ。Issue #1846、親 #1845。テナント境界は RLS-10 ポインタ）。
//!
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql` 経由で使い、期待値は
//! PostgreSQL 17 の意味論から手計算した固定値（各ケースのコメントに根拠）。投影位置
//! （`SELECT ..., (SELECT agg ...)`）と WHERE 位置（`col <op> (SELECT agg ...)`）の両方で、
//! 該当する内側行が無い外側の行は `count` が 0、それ以外の集計は NULL になること、
//! 受理形の外は fail-closed（`42601`／`0A000`）で拒否されることを固定する。

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::nesting_limits::NestingLimits;
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
            col("n", ColumnType::Integer),
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

/// 結果の `id` を返す順のまま集める。
fn ids_in_order(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    query(core, ctx, sql).rows.iter().map(|r| r.id).collect()
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    let mut v = ids_in_order(core, ctx, sql);
    v.sort_unstable();
    v
}

/// 数値セルを比較しやすい `i128` へ（`NULL` は `None`）。整数・scale 0 の numeric を扱う。
fn num(cell: &Cell) -> Option<i128> {
    match cell {
        Cell::Null => None,
        Cell::Integer(n) => Some(i128::from(*n)),
        Cell::SignedInteger(n) => Some(i128::from(*n)),
        Cell::Numeric(d) if d.scale() == 0 => Some(d.unscaled()),
        other => panic!("unexpected numeric cell {other:?}"),
    }
}

/// `SELECT id, (<サブクエリ>) ... FROM a` の `id` 順の「最後の列」の数値（`id` 昇順）。
fn last_col_by_id(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<(u64, Option<i128>)> {
    let mut rows: Vec<(u64, Option<i128>)> = query(core, ctx, sql)
        .rows
        .iter()
        .map(|r| (r.id, num(r.cells.last().expect("cells"))))
        .collect();
    rows.sort_by_key(|(id, _)| *id);
    rows
}

/// `(wire_code, message)`。
fn err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> (String, String) {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql) {
        Ok(_) => panic!("sql={sql:?} should be rejected"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn col_name(meta: &ColumnMeta) -> String {
    match meta {
        ColumnMeta::Id => "id".to_string(),
        ColumnMeta::Scalar { name, .. }
        | ColumnMeta::Computed { name, .. }
        | ColumnMeta::AliasedId { name }
        | ColumnMeta::Record { name, .. } => name.clone(),
    }
}

fn open_core(name: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    (storage, CleanupGuard(path))
}

fn seed(core: &EngineCore, ctx: &PolicyContext) {
    // a: id1 {k1, t'x', v10, n2}、id2 {k2, t'y', v20, n1}、id3 {k3, t NULL, v30, n0}、
    //    id4 {k NULL, t'x', v5, n0}
    ins(
        core,
        ctx,
        "a",
        1,
        &[("k", "1"), ("t", "'x'"), ("v", "10"), ("n", "2")],
    );
    ins(
        core,
        ctx,
        "a",
        2,
        &[("k", "2"), ("t", "'y'"), ("v", "20"), ("n", "1")],
    );
    ins(core, ctx, "a", 3, &[("k", "3"), ("v", "30"), ("n", "0")]);
    ins(core, ctx, "a", 4, &[("t", "'x'"), ("v", "5"), ("n", "0")]);
    // b: id1 {aid1, t'x', v5, flag true}、id2 {aid1, t'z', v50, flag false}、
    //    id3 {aid2, t'y', v20, flag true}、id4 {aid NULL, v1, flag true}
    ins(
        core,
        ctx,
        "b",
        1,
        &[("aid", "1"), ("t", "'x'"), ("v", "5"), ("flag", "true")],
    );
    ins(
        core,
        ctx,
        "b",
        2,
        &[("aid", "1"), ("t", "'z'"), ("v", "50"), ("flag", "false")],
    );
    ins(
        core,
        ctx,
        "b",
        3,
        &[("aid", "2"), ("t", "'y'"), ("v", "20"), ("flag", "true")],
    );
    ins(core, ctx, "b", 4, &[("v", "1"), ("flag", "true")]);
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let (storage, guard) = open_core("sql61-correlated-scalar-agg");
    for schema in [a_schema(), b_schema()] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    (core, ctx, guard)
}

// --- 投影位置 --------------------------------------------------------------------------------

#[test]
fn projection_count_is_zero_for_unmatched_outer_rows_including_null_key() {
    let (core, ctx, _g) = setup();
    // aid=1 の b は 2 行、aid=2 は 1 行、k=3 は該当なし（0）、k が NULL の行も 0。
    // b4（aid NULL）はどの外側の行にも数えない（`=` は NULL で真にならない）。
    let r = last_col_by_id(
        &core,
        &ctx,
        "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) AS c FROM a LIMIT 100",
    );
    assert_eq!(
        r,
        vec![(1, Some(2)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
}

#[test]
fn projection_non_count_aggregates_are_null_for_unmatched_rows() {
    let (core, ctx, _g) = setup();
    let cases: [(&str, [Option<i128>; 4]); 5] = [
        ("max(b.v)", [Some(50), Some(20), None, None]),
        ("min(b.v)", [Some(5), Some(20), None, None]),
        ("sum(b.v)", [Some(55), Some(20), None, None]),
        // 内側のキーが NULL の b4（v=1）は a4 にも数えない。
        ("count(DISTINCT b.t)", [Some(2), Some(1), Some(0), Some(0)]),
        (
            "count(*) FILTER (WHERE b.flag)",
            [Some(1), Some(1), Some(0), Some(0)],
        ),
    ];
    for (agg, expected) in cases {
        let r = last_col_by_id(
            &core,
            &ctx,
            &format!("SELECT id, (SELECT {agg} FROM b WHERE b.aid = a.k) FROM a LIMIT 100"),
        );
        let got: Vec<Option<i128>> = r.iter().map(|(_, v)| *v).collect();
        assert_eq!(got, expected.to_vec(), "agg={agg}");
    }
}

#[test]
fn projection_text_and_bool_aggregates_are_null_for_unmatched_rows() {
    let (core, ctx, _g) = setup();
    let r = query(
        &core,
        &ctx,
        "SELECT id, (SELECT string_agg(b.t, ',' ORDER BY b.t) FROM b WHERE b.aid = a.k) AS s, \
         (SELECT bool_and(b.flag) FROM b WHERE b.aid = a.k) AS f FROM a LIMIT 100",
    );
    let mut rows: Vec<(u64, Cell, Cell)> = r
        .rows
        .iter()
        .map(|r| (r.id, r.cells[1].clone(), r.cells[2].clone()))
        .collect();
    rows.sort_by_key(|(id, _, _)| *id);
    assert_eq!(
        rows,
        vec![
            (1, Cell::Text("x,z".to_string()), Cell::Bool(false)),
            (2, Cell::Text("y".to_string()), Cell::Bool(true)),
            (3, Cell::Null, Cell::Null),
            (4, Cell::Null, Cell::Null),
        ]
    );
}

#[test]
fn projection_keeps_column_names_positions_and_types() {
    let (core, ctx, _g) = setup();
    let r = query(
        &core,
        &ctx,
        "SELECT (SELECT count(*) FROM b WHERE b.aid = a.k) AS c, v, \
         (SELECT max(b.v) FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
    );
    let names: Vec<String> = r.columns.iter().map(col_name).collect();
    assert_eq!(names, vec!["c", "v", "max"]);
    // 隠しキー列は結果へ漏れない。
    assert_eq!(r.columns.len(), 3);
    assert!(r.rows.iter().all(|row| row.cells.len() == 3));
    // 列型は外側の行数（0 行でも）に依らず束縛結果から決まる。
    let empty = query(
        &core,
        &ctx,
        "SELECT (SELECT count(*) FROM b WHERE b.aid = a.k) AS c, v FROM a WHERE v > 1000 LIMIT 100",
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.len(), 2);
    assert_eq!(col_name(&empty.columns[0]), "c");
}

#[test]
fn projection_works_with_order_by_limit_and_offset() {
    let (core, ctx, _g) = setup();
    // キー列 k での並べ替え（DESC は NULLS FIRST）。隠しキー列は並べ替えに影響しない。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a ORDER BY k DESC LIMIT 100"
        ),
        vec![4, 3, 2, 1]
    );
    // キー列名 k と同名の出力別名での並べ替えは出力列（v）を指す（PostgreSQL と同じ）。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT v AS k, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a ORDER BY k LIMIT 100"
        ),
        vec![4, 1, 2, 3]
    );
    // 並べ替え・LIMIT・OFFSET（v 降順は 30, 20, 10, 5 → 先頭 1 件を飛ばして 2 件）。
    assert_eq!(
        ids_in_order(
            &core,
            &ctx,
            "SELECT v, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a ORDER BY v DESC LIMIT 2 OFFSET 1"
        ),
        vec![2, 1]
    );
}

#[test]
fn projection_pseudo_id_keys_on_both_sides() {
    let (core, ctx, _g) = setup();
    // 外側の疑似列 id がキー: b.aid の個数は id1→2、id2→1、id3→0、id4→0（隠しキー列は不要）。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.id) AS c FROM a LIMIT 100"
        ),
        vec![(1, Some(2)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
    // 内側の疑似列 id がキー: b.id = a.k は k=1,2,3 で各 1 件、k NULL は 0。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.id = a.k) FROM a LIMIT 100"
        ),
        vec![(1, Some(1)), (2, Some(1)), (3, Some(1)), (4, Some(0))]
    );
}

#[test]
fn composite_and_cross_integer_class_keys() {
    let (core, ctx, _g) = setup();
    // 複合キー（aid = k AND t = t）。a1 は b1（aid1,'x'）、a2 は b3（aid2,'y'）に一致。
    // a3 は t NULL、a4 は k NULL のため一致なし。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k AND b.t = a.t) FROM a LIMIT 100"
        ),
        vec![(1, Some(1)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
    // INTEGER（b.v）×BIGINT（a.k）の整数クラス: b.v = a.k は b4（v=1）が a1 にだけ一致。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.v = a.k) FROM a LIMIT 100"
        ),
        vec![(1, Some(1)), (2, Some(0)), (3, Some(0)), (4, Some(0))]
    );
    // 内側だけの条件との併用: flag = true の b は b1（aid1）・b3（aid2）・b4。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k AND b.flag = true) FROM a LIMIT 100"
        ),
        vec![(1, Some(1)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
}

// --- WHERE 位置 ------------------------------------------------------------------------------

#[test]
fn where_compares_against_group_value_for_every_operator() {
    let (core, ctx, _g) = setup();
    let sub = "(SELECT max(b.v) FROM b WHERE b.aid = a.k)";
    // max(b.v) は a1=50、a2=20、a3・a4=NULL（比較は偽）。a.v は 10, 20, 30, 5。
    let cases: [(&str, Vec<u64>); 6] = [
        ("=", vec![2]),
        ("<>", vec![1]),
        ("<", vec![1]),
        ("<=", vec![1, 2]),
        (">", vec![]),
        (">=", vec![2]),
    ];
    for (op, expected) in cases {
        assert_eq!(
            ids(
                &core,
                &ctx,
                &format!("SELECT id FROM a WHERE v {op} {sub} LIMIT 100")
            ),
            expected,
            "op={op}"
        );
    }
    // `NOT (col = (...))` は構文段で `<>` へ反転される（NULL の行は UNKNOWN で偽のまま）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE NOT (v = {sub}) LIMIT 100")
        ),
        vec![1]
    );
    // OR の分岐内と他の条件との併用。
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v = {sub} OR id = 4 LIMIT 100")
        ),
        vec![2, 4]
    );
    assert_eq!(
        ids(
            &core,
            &ctx,
            &format!("SELECT id FROM a WHERE v <= {sub} AND t = 'y' LIMIT 100")
        ),
        vec![2]
    );
}

#[test]
fn where_count_is_zero_for_empty_groups_and_null_outer_keys() {
    let (core, ctx, _g) = setup();
    // n は期待する件数（a1=2、a2=1、a3=0、a4=0）。a3（該当なし）と a4（外側キー NULL）の
    // `count` は 0 なので `n = count` は全行で真（PostgreSQL と同じ）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE n = (SELECT count(*) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2, 3, 4]
    );
    // `>` は a1（2>2 偽）・a2（1>1 偽）を除き a3・a4 でも 0>0 は偽。`n < count` は全行偽。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE n < (SELECT count(*) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        Vec::<u64>::new()
    );
    // 外側キーが NULL の行（a4）・該当なしの行（a3）で max との比較は偽（count との差異）。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v = (SELECT max(b.v) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![2]
    );
    // sum は該当なしで NULL（比較は偽）。a3・a4 は返らない。
    assert_eq!(
        ids(
            &core,
            &ctx,
            "SELECT id FROM a WHERE v <= (SELECT sum(b.v) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2]
    );
}

#[test]
fn where_works_in_an_aggregate_outer_query() {
    let (core, ctx, _g) = setup();
    let r = query(
        &core,
        &ctx,
        "SELECT count(*) FROM a WHERE v <= (SELECT max(b.v) FROM b WHERE b.aid = a.k)",
    );
    assert_eq!(num(&r.rows[0].cells[0]), Some(2));
}

#[test]
fn where_and_projection_can_be_combined_in_one_statement() {
    let (core, ctx, _g) = setup();
    let r = last_col_by_id(
        &core,
        &ctx,
        "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a \
         WHERE v <= (SELECT max(b.v) FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(r, vec![(1, Some(2)), (2, Some(1))]);
}

// --- キー型 ------------------------------------------------------------------------------------

#[test]
fn key_classes_text_date_uuid_timestamp_bool_enum_and_smallint() {
    let (storage, _g) = open_core("sql61-correlated-scalar-agg-keys");
    let mood = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("enum");
    let cols = |with_w: bool| {
        let mut c = vec![
            vec_col(),
            col("d", ColumnType::Date),
            col("u", ColumnType::Uuid),
            col("ts", ColumnType::Timestamp),
            col("bl", ColumnType::Boolean),
            col("e", ColumnType::Enum(Arc::clone(&mood))),
            col("s", ColumnType::SmallInt),
            col("ty", ColumnType::Text),
        ];
        if with_w {
            c.push(col("w", ColumnType::Integer));
        }
        c
    };
    storage
        .create_table(&TableSchema::new("p", cols(false)))
        .expect("p");
    storage
        .create_table(&TableSchema::new("q", cols(true)))
        .expect("q");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let u1 = "'123e4567-e89b-12d3-a456-426614174000'";
    let u2 = "'223e4567-e89b-12d3-a456-426614174000'";
    ins(
        &core,
        &ctx,
        "p",
        1,
        &[
            ("d", "'2024-01-01'"),
            ("u", u1),
            ("ts", "'2024-01-01 10:00:00'"),
            ("bl", "true"),
            ("e", "'happy'"),
            ("s", "1"),
            ("ty", "'x'"),
        ],
    );
    ins(
        &core,
        &ctx,
        "p",
        2,
        &[
            ("d", "'2024-02-02'"),
            ("u", u2),
            ("ts", "'2024-02-02 10:00:00'"),
            ("bl", "false"),
            ("e", "'sad'"),
            ("s", "2"),
            ("ty", "'y'"),
        ],
    );
    ins(&core, &ctx, "p", 3, &[("s", "3")]);
    for (id, w) in [(1_u64, 10), (2, 20), (3, 30)] {
        ins(
            &core,
            &ctx,
            "q",
            id,
            &[
                ("d", "'2024-01-01'"),
                ("u", u1),
                ("ts", "'2024-01-01 10:00:00'"),
                ("bl", "true"),
                ("e", "'happy'"),
                ("s", "1"),
                ("ty", "'x'"),
                ("w", &w.to_string()),
            ],
        );
    }
    // どのキー列でも p1 に一致するのは q の 3 行（w 合計 60）、p2・p3 は該当なし（NULL）。
    for key in ["d", "u", "ts", "bl", "e", "s", "ty"] {
        let r = last_col_by_id(
            &core,
            &ctx,
            &format!(
                "SELECT id, (SELECT sum(q.w) FROM q WHERE q.{key} = p.{key}) FROM p LIMIT 100"
            ),
        );
        // `bl`: p2（false）は q に false が無く NULL、p3 は NULL キー。
        assert_eq!(r, vec![(1, Some(60)), (2, None), (3, None)], "key={key}");
        // WHERE 位置でも同じキー型が使える（60 = sum は w 列が無いため `s` で比較）。
        let hits = ids(
            &core,
            &ctx,
            &format!(
                "SELECT id FROM p WHERE s < (SELECT sum(q.w) FROM q WHERE q.{key} = p.{key}) LIMIT 100"
            ),
        );
        assert_eq!(hits, vec![1], "key={key}");
    }
}

// --- テナント境界（RLS-10） ----------------------------------------------------------------------

#[test]
fn other_tenants_rows_never_reach_groups_counts_or_errors() {
    let (core, t1, _g) = setup();
    let t2 = ctx_for("t2");
    // t2 の行: b に aid=1 を 5 行（v=1000）、aid=3 を 1 行。a に k=3 の行。
    for i in 0..5_u64 {
        ins(
            &core,
            &t2,
            "b",
            100 + i,
            &[("aid", "1"), ("v", "1000"), ("flag", "true")],
        );
    }
    ins(&core, &t2, "b", 200, &[("aid", "3"), ("v", "7")]);
    ins(&core, &t2, "a", 300, &[("k", "3"), ("v", "7"), ("n", "1")]);
    // t1 から見た結果は t2 の行を含めない。
    assert_eq!(
        last_col_by_id(
            &core,
            &t1,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a LIMIT 100"
        ),
        vec![(1, Some(2)), (2, Some(1)), (3, Some(0)), (4, Some(0))]
    );
    assert_eq!(
        ids(
            &core,
            &t1,
            "SELECT id FROM a WHERE v <= (SELECT max(b.v) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![1, 2]
    );
    // t2 は自分の行だけで集計される（k=3 の a300 は max=7、v=7 → `<=` が真）。
    assert_eq!(
        ids(
            &core,
            &t2,
            "SELECT id FROM a WHERE v <= (SELECT max(b.v) FROM b WHERE b.aid = a.k) LIMIT 100"
        ),
        vec![300]
    );
    assert_eq!(
        last_col_by_id(
            &core,
            &t2,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a LIMIT 100"
        ),
        vec![(300, Some(1))]
    );
}

// --- 上限・fail-closed ---------------------------------------------------------------------------

#[test]
fn group_count_limit_rejects_with_54000_in_both_positions() {
    let (storage, _g) = open_core("sql61-correlated-scalar-agg-groups");
    for schema in [a_schema(), b_schema()] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_group_by_limits(GroupByLimits::new(8, 1));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    // b のグループは aid=1・2・NULL の 3 つ。上限 1 を超えるため部分結果を返さず 54000。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
    );
    assert_eq!(code, "54000");
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE v <= (SELECT max(b.v) FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "54000");
}

#[test]
fn execution_budget_is_shared_across_the_statement() {
    let (storage, _g) = open_core("sql61-correlated-scalar-agg-budget");
    for schema in [a_schema(), b_schema()] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_nesting_limits(NestingLimits::new(4, 1, 16, 4));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    // 1 回なら通り、2 つ目の相関スカラーで実行回数の予算を超えて 54000。
    assert_eq!(
        last_col_by_id(
            &core,
            &ctx,
            "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a LIMIT 100"
        )
        .len(),
        4
    );
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k), \
         (SELECT max(b.v) FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
    );
    assert_eq!(code, "54000");
}

#[test]
fn unsupported_shapes_are_rejected_fail_closed() {
    let (core, ctx, _g) = setup();
    let sub_rejects = [
        // 集計引数・FILTER の外側参照。
        "SELECT id, (SELECT max(b.v + a.v) FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        "SELECT id, (SELECT count(*) FILTER (WHERE b.v > a.v) FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        // 内側の GROUP BY・HAVING・LIMIT・OFFSET。
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k GROUP BY b.aid) FROM a LIMIT 100",
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k HAVING count(*) > 0) FROM a LIMIT 100",
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k LIMIT 1) FROM a LIMIT 100",
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k OFFSET 1) FROM a LIMIT 100",
        // 式項目・SELECT DISTINCT。
        "SELECT id, (SELECT count(*) + 1 FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        "SELECT id, (SELECT DISTINCT b.v FROM b WHERE b.aid = a.k) FROM a LIMIT 100",
        // 非等価の相関・キーなし（行ごとの再評価は範囲外）。
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.v < a.v) FROM a LIMIT 100",
        "SELECT id, (SELECT max(b.v) FROM b WHERE b.aid = a.k OR b.v = a.v) FROM a LIMIT 100",
        // 非集計形で投影が式（Issue #1847 の受理形は単一の裸の列だけ）。
        "SELECT id, (SELECT b.v + 1 FROM b WHERE b.aid = a.k LIMIT 1) FROM a LIMIT 100",
        // 同じ内側列への等価の重複。
        "SELECT id, (SELECT count(*) FROM b WHERE b.aid = a.k AND b.aid = a.v) FROM a LIMIT 100",
    ];
    for sql in sub_rejects {
        let (code, msg) = err(&core, &ctx, sql);
        assert!(
            code == "42601" || code == "0A000",
            "sql={sql:?} code={code} msg={msg}"
        );
        assert!(
            !msg.contains('\u{2}') && !msg.contains('\u{3}'),
            "reserved name leaked: sql={sql:?} msg={msg:?}"
        );
    }
    // WHERE 位置の同じ形も拒否される（`correlated` を含む静的文言）。
    for sql in [
        "SELECT id FROM a WHERE v < (SELECT max(b.v) FROM b WHERE b.v < a.v) LIMIT 100",
        "SELECT id FROM a WHERE v < (SELECT max(b.v + a.v) FROM b WHERE b.aid = a.k) LIMIT 100",
        "SELECT id FROM a WHERE v < (SELECT b.v + 1 FROM b WHERE b.aid = a.k LIMIT 1) LIMIT 100",
    ] {
        let (code, msg) = err(&core, &ctx, sql);
        assert_eq!(code, "42601", "sql={sql:?} msg={msg}");
        assert!(msg.contains("correlated"), "sql={sql:?} msg={msg}");
    }
    // 述語形 UPDATE／DELETE の WHERE と集計 FILTER の相関スカラーは従来どおり拒否する。
    for sql in [
        "UPDATE a SET v = 1 WHERE v < (SELECT max(b.v) FROM b WHERE b.aid = a.k) USING OPERATION_ID 'u-1'",
        "DELETE FROM a WHERE v < (SELECT max(b.v) FROM b WHERE b.aid = a.k) USING OPERATION_ID 'd-1'",
        "SELECT count(*) FILTER (WHERE v < (SELECT max(b.v) FROM b WHERE b.aid = a.k)) FROM a",
    ] {
        let (code, _) = err(&core, &ctx, sql);
        // 述語形 DML の外側参照は従来から拒否される（修飾名の解決は 42P01 になる経路を含む）。
        assert!(
            matches!(code.as_str(), "42601" | "0A000" | "42P01"),
            "sql={sql:?} should stay rejected: {code}"
        );
    }
    // 外側が JOIN・カンマ区切り FROM の相関スカラーも受理しない。
    for sql in [
        "SELECT a.id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a, b LIMIT 100",
        "SELECT a.id, (SELECT count(*) FROM b WHERE b.aid = a.k) FROM a JOIN b ON a.k = b.aid LIMIT 100",
    ] {
        let (code, _) = err(&core, &ctx, sql);
        assert!(
            code == "42601" || code == "0A000" || code == "42703",
            "sql={sql:?} should stay rejected: {code}"
        );
    }
}

#[test]
fn levels_beyond_the_enclosing_query_are_rejected() {
    let (core, ctx, _g) = setup();
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE v < \
         (SELECT max(b.v) FROM b b2 WHERE b2.aid = a.k)) LIMIT 100",
    );
    assert!(code == "42601" || code == "0A000", "{code}");
}

#[test]
fn bytea_key_value_family_mismatch_and_unknown_target_column_codes() {
    let (storage, _g) = open_core("sql61-correlated-scalar-agg-codes");
    for schema in [
        a_schema(),
        b_schema(),
        TableSchema::new(
            "y",
            vec![
                vec_col(),
                col("bb", ColumnType::Bytea),
                col("w", ColumnType::Integer),
            ],
        ),
        TableSchema::new("z", vec![vec_col(), col("bb", ColumnType::Bytea)]),
    ] {
        storage.create_table(&schema).expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    seed(&core, &ctx);
    // BYTEA キーは GROUP BY で束ねられない。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id, (SELECT max(y.w) FROM y WHERE y.bb = z.bb) FROM z LIMIT 100",
    );
    assert_eq!(code, "0A000");
    // 値族の不一致（TEXT 列と整数の集計）と未知の対象列。
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE t = (SELECT count(*) FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "22000");
    let (code, _) = err(
        &core,
        &ctx,
        "SELECT id FROM a WHERE nope = (SELECT count(*) FROM b WHERE b.aid = a.k) LIMIT 100",
    );
    assert_eq!(code, "42703");
}

#[test]
fn deferred_runtime_error_is_raised_only_when_the_outer_query_has_rows() {
    let (core, ctx, _g) = setup();
    // 内側の集計が 0 除算（22012）になる。外側が 0 行なら内側は評価されずエラーにしない。
    let failing = "(SELECT max(b.v / (b.v - b.v)) FROM b WHERE b.aid = a.k)";
    let empty = query(
        &core,
        &ctx,
        &format!("SELECT id, {failing} FROM a WHERE v > 1000 LIMIT 100"),
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.len(), 2);
    let (code, _) = err(
        &core,
        &ctx,
        &format!("SELECT id, {failing} FROM a LIMIT 100"),
    );
    assert_eq!(code, "22012");
}
