//! `string_agg(<値>, <区切り> [ORDER BY ...])`（集計内 `ORDER BY`・`DISTINCT` 対応）の結合テスト
//! （SQL-57・Issue #1800。関連: SQL-13・SQL-14・SQL-25 (c)・SQL-56・RLS-7/RLS-8）。
//!
//! 期待値は PostgreSQL 17 の意味論（NULL の値は飛ばす・行ごとの区切り・NULL の区切りは何も置かない・
//! `DISTINCT` は引数の組で重複排除し昇順・`DISTINCT` で引数にない `ORDER BY` は `42P10`・
//! 引数の型違いは `42883`）に従う固定値。`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql`
//! の production 経路で検証し、他テナントの Private 行が結果・`54000` の有無へ影響しないこと
//! （RLS）と、`wire_code` のエラー契約（`22012`〜`54000`）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";
const BIG: &str = "big";
const MIB: usize = 1024 * 1024;

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("g", ColumnType::Text, None),
            col("s", ColumnType::Text, None),
            col("sep", ColumnType::Text, None),
            col("k", ColumnType::Integer, None),
            col("ic", ColumnType::Text, Some(Collation::UndIcu)),
            col("b", ColumnType::Bytea, None),
            col("v", ColumnType::Varchar(Some(20)), None),
            col("ch", ColumnType::Bpchar(4), None),
        ],
    )
}

fn big_schema() -> TableSchema {
    TableSchema::new(
        BIG,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("s", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn opt(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::Text(s.to_string()))
}

/// `(id, g, s, sep, k, ic)`。`s`・`sep`・`k` の NULL を含む（PG 17 の意味論の固定値用）。
type Seed = (
    u64,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<i32>,
    Option<&'static str>,
);

const SEEDS: [Seed; 8] = [
    (1, "a", Some("x"), Some(","), Some(3), Some("b")),
    (2, "a", None, Some(","), Some(1), Some("a")),
    (3, "a", Some("y"), Some("-"), Some(2), Some("C")),
    (4, "b", Some("z"), None, Some(5), None),
    (5, "b", Some("w"), Some(";"), None, Some("a")),
    (6, "c", None, Some(","), Some(1), Some("B")),
    (7, "b", Some("x"), Some(","), Some(4), Some("c")),
    (8, "a", Some("x"), Some(","), Some(9), Some("A")),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, s, sep, k, ic) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            opt(s),
            opt(sep),
            k.map_or(Value::Null, Value::Integer),
            opt(ic),
            Value::Bytes(vec![1, 2]),
            opt(s),
            Value::Text("ab  ".to_string()),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn insert_big(storage: &Storage, tenant: &str, id: u64, g: &str, len: usize, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        BIG,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            Value::Text("q".repeat(len)),
        ],
        &OperationId::parse(&format!("big-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert big row");
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage.create_table(&big_schema()).expect("create big");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない）。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some("SECRET"), Some("!"), Some(0), Some("0")),
        Visibility::Private,
    );
    // 結果長の上限（4 MiB）の境界: `ok` は 2 MiB + 区切り 1 + (2 MiB - 1) = ちょうど 4 MiB、
    // `over` は 2 MiB + 1 + 2 MiB で 1 バイト超過、`three` は 3 MiB の単一行。他テナントの Private 行
    // （`ok` の 3 MiB）は結果にも `54000` の有無にも影響してはならない。
    insert_big(&storage, "tenant-a", 1, "ok", 2 * MIB, Visibility::Public);
    insert_big(
        &storage,
        "tenant-a",
        2,
        "ok",
        2 * MIB - 1,
        Visibility::Public,
    );
    insert_big(&storage, "tenant-a", 3, "over", 2 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 4, "over", 2 * MIB, Visibility::Public);
    insert_big(
        &storage,
        "tenant-a",
        5,
        "three",
        3 * MIB,
        Visibility::Public,
    );
    // `dup`: 同一の 3 MiB が 2 行。`DISTINCT` では捨てられる重複行が予算で 54000 にならないこと。
    insert_big(&storage, "tenant-a", 7, "dup", 3 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 8, "dup", 3 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-b", 6, "ok", 3 * MIB, Visibility::Private);
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

/// 単一行・単一列の結果の text（NULL は `None`）。
fn one(core: &EngineCore, sql: &str) -> Option<String> {
    let r = run(core, sql);
    assert_eq!(r.rows.len(), 1, "{sql}");
    cell_text(&r.rows[0].cells[0])
}

fn cell_text(c: &Cell) -> Option<String> {
    match c {
        Cell::Text(s) => Some(s.clone()),
        Cell::Null => None,
        other => panic!("expected text or NULL, got {other:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn null_values_are_skipped_and_delimiter_is_per_row() {
    let (core, path) = build_core("sql57-basic");
    let _g = CleanupGuard(path);
    // 値 NULL の行は区切りごと飛ばす。最初の値の区切りは捨てる。
    assert_eq!(
        one(&core, "SELECT string_agg(s, ',') FROM t"),
        s("x,y,z,w,x,x")
    );
    // 行ごとの区切り（式でも列でもよい）。区切りが NULL の行（id=4）は何も置かない。
    assert_eq!(
        one(&core, "SELECT string_agg(s, sep) FROM t"),
        s("x-yz;w,x,x")
    );
    // 区切りは式でもよい。
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(s, concat('[', g, ']')) FROM t WHERE g = 'a'"
        ),
        s("x[a]y[a]x")
    );
    // 空集合・全行 NULL は NULL。
    assert_eq!(
        one(&core, "SELECT string_agg(s, ',') FROM t WHERE g = 'zz'"),
        None
    );
    assert_eq!(
        one(&core, "SELECT string_agg(s, ',') FROM t WHERE g = 'c'"),
        None
    );
    // 空文字列は値（NULL ではない）なので、1 行なら空文字列が結果になる。
    assert_eq!(
        one(&core, "SELECT string_agg('', ',') FROM t WHERE g = 'c'"),
        s("")
    );
}

#[test]
fn other_tenant_private_rows_never_appear() {
    let (core, path) = build_core("sql57-rls");
    let _g = CleanupGuard(path);
    let r = one(&core, "SELECT string_agg(s, ',' ORDER BY s) FROM t").expect("non-null");
    assert!(!r.contains("SECRET"), "{r}");
    // 他テナントだけが持つ g='a' の行があっても結果は自テナントのみ。
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(s, ',' ORDER BY s) FROM t WHERE g = 'a'"
        ),
        s("x,x,y")
    );
}
/// `tenant` の視点で実行する（他テナントの Private 行が自分の視点では見えることの対照用）。
fn one_as(core: &EngineCore, tenant: &str, sql: &str) -> Option<String> {
    let r = core
        .execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"));
    assert_eq!(r.rows.len(), 1, "{sql}");
    cell_text(&r.rows[0].cells[0])
}

#[test]
fn rls_visibility_is_per_tenant() {
    let (core, path) = build_core("sql57-rls-pos");
    let _g = CleanupGuard(path);
    // tenant-b の視点では自分の Private 行（SECRET）が見える。tenant-a の視点では見えない。
    assert_eq!(
        one_as(
            &core,
            "tenant-b",
            "SELECT string_agg(s, ',' ORDER BY s) FROM t"
        ),
        s("SECRET,w,x,x,x,y,z")
    );
    assert_eq!(
        one_as(
            &core,
            "tenant-a",
            "SELECT string_agg(s, ',' ORDER BY s) FROM t"
        ),
        s("w,x,x,x,y,z")
    );
}

#[test]
fn inner_order_by_directions_nulls_and_keys() {
    let (core, path) = build_core("sql57-order");
    let _g = CleanupGuard(path);
    let q = |tail: &str| {
        one(
            &core,
            &format!("SELECT string_agg(s, ',' ORDER BY {tail}) FROM t"),
        )
    };
    // 既定: ASC は NULL 末尾・DESC は NULL 先頭（SQL-56）。
    assert_eq!(q("k"), s("y,x,x,z,x,w"));
    assert_eq!(q("k DESC"), s("w,x,z,x,x,y"));
    assert_eq!(q("k DESC NULLS LAST"), s("x,z,x,x,y,w"));
    assert_eq!(q("k ASC NULLS FIRST"), s("w,y,x,x,z,x"));
    // 複数キー・式キー・疑似列 `id`・値そのもの。
    assert_eq!(q("s, k DESC"), s("w,x,x,x,y,z"));
    assert_eq!(q("k + 1"), s("y,x,x,z,x,w"));
    assert_eq!(q("id DESC"), s("x,x,w,z,y,x"));
    assert_eq!(q("id"), s("x,y,z,w,x,x"));
    // 同値のキーは入力順を保つ（安定ソート）。
    assert_eq!(q("g"), s("x,y,x,z,w,x"));
    // 集計の内側では整数リテラルは定数式（出力列の位置ではない）なので、入力順のまま。
    assert_eq!(q("1"), s("x,y,z,w,x,x"));
    // 区切りも ORDER BY キーにできる（NULL の区切りは末尾）。区切りの昇順は "," "," "," "-" ";" NULL:
    // x(id1) x(id7) x(id8) y(id3) w(id5) z(id4) → 最初の値の区切りは捨て、以降は各行の区切り。
    assert_eq!(
        one(&core, "SELECT string_agg(s, sep ORDER BY sep, s) FROM t"),
        s("x,x,x-y;wz")
    );
}

#[test]
fn distinct_dedups_on_argument_pairs_and_sorts() {
    let (core, path) = build_core("sql57-distinct");
    let _g = CleanupGuard(path);
    assert_eq!(
        one(&core, "SELECT string_agg(DISTINCT s, ',') FROM t"),
        s("w,x,y,z")
    );
    // 重複判定は（値, 区切り）の組。ORDER BY なしの出力は引数の昇順（NULL 末尾）。
    assert_eq!(
        one(&core, "SELECT string_agg(DISTINCT s, sep) FROM t"),
        s("w,x-yz")
    );
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(DISTINCT s, ',' ORDER BY s DESC) FROM t"
        ),
        s("z,y,x,w")
    );
    // 引数にない ORDER BY は 42P10。
    assert_eq!(
        code_of(
            &core,
            "SELECT string_agg(DISTINCT s, ',' ORDER BY k) FROM t"
        ),
        "42P10"
    );
    // GROUP BY との併用。
    let r = run(
        &core,
        "SELECT g, string_agg(DISTINCT s, ',') FROM t GROUP BY g ORDER BY g",
    );
    let got: Vec<(Option<String>, Option<String>)> = r
        .rows
        .iter()
        .map(|row| (cell_text(&row.cells[0]), cell_text(&row.cells[1])))
        .collect();
    assert_eq!(
        got,
        vec![(s("a"), s("x,y")), (s("b"), s("w,x,z")), (s("c"), None)]
    );
}

#[test]
fn group_by_with_and_without_order_by() {
    let (core, path) = build_core("sql57-group");
    let _g = CleanupGuard(path);
    let rows = |sql: &str| -> Vec<(Option<String>, Option<String>)> {
        run(&core, sql)
            .rows
            .iter()
            .map(|row| (cell_text(&row.cells[0]), cell_text(&row.cells[1])))
            .collect()
    };
    assert_eq!(
        rows("SELECT g, string_agg(s, ',' ORDER BY s) FROM t GROUP BY g ORDER BY g"),
        vec![(s("a"), s("x,x,y")), (s("b"), s("w,x,z")), (s("c"), None)]
    );
    // ORDER BY なしは行の処理順（物理行順）。索引の列挙形を使わず全走査と同じ順になる。
    assert_eq!(
        rows("SELECT g, string_agg(s, ',') FROM t GROUP BY g ORDER BY g"),
        vec![(s("a"), s("x,y,x")), (s("b"), s("z,w,x")), (s("c"), None)]
    );
    // WHERE 付き。
    assert_eq!(
        rows(
            "SELECT g, string_agg(s, ',' ORDER BY s DESC) FROM t WHERE k < 5 GROUP BY g ORDER BY g"
        ),
        vec![(s("a"), s("y,x")), (s("b"), s("x")), (s("c"), None)]
    );
}

#[test]
fn icu_collated_column_keys_and_values() {
    let (core, path) = build_core("sql57-icu");
    let _g = CleanupGuard(path);
    // ICU（und）の順序: 小文字が大文字より先・文字ごとにまとまる（バイト列順なら A,B,C,a,a,b,c）。
    assert_eq!(
        one(&core, "SELECT string_agg(ic, ',' ORDER BY ic) FROM t"),
        s("a,a,A,b,B,c,C")
    );
    assert_eq!(
        one(&core, "SELECT string_agg(DISTINCT ic, ',') FROM t"),
        s("a,A,b,B,c,C")
    );
    // 値としての連結は照合に依存しない（入力順）。
    assert_eq!(
        one(&core, "SELECT string_agg(ic, ',') FROM t"),
        s("b,a,C,a,B,c,A")
    );
}

#[test]
fn varchar_and_char_columns_are_text() {
    let (core, path) = build_core("sql57-types");
    let _g = CleanupGuard(path);
    assert_eq!(
        one(&core, "SELECT string_agg(v, ',') FROM t"),
        s("x,y,z,w,x,x")
    );
    // CHAR(n) は text へ変換（末尾の半角空白を除く）して連結する。
    assert_eq!(
        one(&core, "SELECT string_agg(ch, '|') FROM t"),
        s("ab|ab|ab|ab|ab|ab|ab|ab")
    );
    // 結果列は text で、別名と既定名が効く。
    let r = run(
        &core,
        "SELECT STRING_AGG(s, ',') AS names, string_agg(s, ',') FROM t",
    );
    match (&r.columns[0], &r.columns[1]) {
        (ColumnMeta::Computed { name: n0, ty: t0 }, ColumnMeta::Computed { name: n1, ty: t1 }) => {
            assert_eq!((n0.as_str(), n1.as_str()), ("names", "string_agg"));
            assert_eq!(t0, &Some(ColumnType::Text));
            assert_eq!(t1, &Some(ColumnType::Text));
        }
        other => panic!("unexpected column metas: {other:?}"),
    }
}

#[test]
fn argument_shape_and_type_errors_are_rejected_fail_closed() {
    let (core, path) = build_core("sql57-errors");
    let _g = CleanupGuard(path);
    // 該当シグネチャが無い（PostgreSQL と同じ 42883）。
    for sql in [
        "SELECT string_agg(k, ',') FROM t",
        "SELECT string_agg(s, 1) FROM t",
        "SELECT string_agg(s) FROM t",
        "SELECT string_agg(s, ',', ',') FROM t",
        "SELECT string_agg(id, ',') FROM t",
    ] {
        assert_eq!(code_of(&core, sql), "42883", "{sql}");
    }
    assert_eq!(code_of(&core, "SELECT string_agg(*) FROM t"), "42601");
    // bytea は対象外（0A000）。
    assert_eq!(code_of(&core, "SELECT string_agg(b, b) FROM t"), "0A000");
    // 他の集計に 2 引数・ORDER BY・DISTINCT を許さない。
    for sql in [
        "SELECT sum(k, 1) FROM t",
        "SELECT count(s ORDER BY s) FROM t",
        "SELECT sum(DISTINCT k) FROM t",
        "SELECT string_agg(max(s), ',') FROM t",
    ] {
        assert_eq!(code_of(&core, sql), "42601", "{sql}");
    }
    // ORDER BY のキー数の上限（8）は確保前に検査する。
    let keys = (0..9).map(|_| "s").collect::<Vec<_>>().join(", ");
    assert_eq!(
        code_of(
            &core,
            &format!("SELECT string_agg(s, ',' ORDER BY {keys}) FROM t")
        ),
        "54000"
    );
    // 未知列。
    assert_eq!(
        code_of(&core, "SELECT string_agg(nope, ',') FROM t"),
        "42703"
    );
}

#[test]
fn unsupported_contexts_are_rejected_explicitly() {
    let (core, path) = build_core("sql57-unsupported");
    let _g = CleanupGuard(path);
    // FROM なし。
    assert_eq!(code_of(&core, "SELECT string_agg('a', ',')"), "0A000");
    // ウィンドウ関数（OVER）は対象外（成功してはならない）。
    let c = code_of(&core, "SELECT string_agg(s, ',') OVER () FROM t");
    assert!(c == "42601" || c == "0A000", "{c}");
    // JOIN。
    let c = code_of(
        &core,
        "SELECT string_agg(a.s, ',') FROM t a JOIN t b ON a.g = b.g",
    );
    assert_eq!(c, "0A000");
}

#[test]
fn distinct_duplicates_do_not_consume_limits() {
    let (core, path) = build_core("sql57-distinct-dup");
    let _g = CleanupGuard(path);
    for sql in [
        "SELECT string_agg(DISTINCT s, ',') FROM big WHERE g = 'dup'",
        "SELECT string_agg(DISTINCT s, ',' ORDER BY s) FROM big WHERE g = 'dup'",
    ] {
        let out = one(&core, sql).expect("non-null");
        assert_eq!(out.len(), 3 * MIB, "{sql}");
    }
    // DISTINCT なしでは 6 MiB + 区切りで結果長超過。
    assert_eq!(
        code_of(&core, "SELECT string_agg(s, ',') FROM big WHERE g = 'dup'"),
        "54000"
    );
}

#[test]
fn null_literal_arguments_are_bound_as_text() {
    let (core, path) = build_core("sql57-null-literal");
    let _g = CleanupGuard(path);
    // NULL 区切りは何も置かない。
    assert_eq!(
        one(&core, "SELECT string_agg(s, NULL) FROM t WHERE g = 'a'"),
        s("xyx")
    );
    // NULL 値は全行読み飛ばされ、結果は NULL。
    assert_eq!(one(&core, "SELECT string_agg(NULL, ',') FROM t"), None);
}

#[test]
fn result_length_limit_is_exact_and_independent_of_other_tenants() {
    let (core, path) = build_core("sql57-limit");
    let _g = CleanupGuard(path);
    // ちょうど 4 MiB（2 MiB + 区切り 1 + 2 MiB - 1）は成功する。他テナントの 3 MiB の
    // Private 行（g = 'ok'）は結果にも成否にも影響しない。
    for sql in [
        "SELECT string_agg(s, ',') FROM big WHERE g = 'ok'",
        "SELECT string_agg(s, ',' ORDER BY id) FROM big WHERE g = 'ok'",
        "SELECT string_agg(DISTINCT s, ',') FROM big WHERE g = 'ok'",
    ] {
        let out = one(&core, sql).expect("non-null");
        assert_eq!(out.len(), 4 * MIB, "{sql}");
    }
    // 1 バイト超過は 54000（部分的な結果は返さない）。
    for sql in [
        "SELECT string_agg(s, ',') FROM big WHERE g = 'over'",
        "SELECT string_agg(s, ',' ORDER BY id) FROM big WHERE g = 'over'",
        "SELECT g, string_agg(s, ',') FROM big GROUP BY g",
    ] {
        assert_eq!(code_of(&core, sql), "54000", "{sql}");
    }
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(s, ',') FROM big WHERE g = 'three'"
        )
        .expect("non-null")
        .len(),
        3 * MIB
    );
    // 他テナントの視点: 自分の 3 MiB の Private 行が加わると `ok` は超過する。
    let r = core.execute_sql(
        &ctx("tenant-b"),
        "SELECT string_agg(s, ',') FROM big WHERE g = 'ok'",
    );
    assert_eq!(r.expect_err("over the limit").wire_code(), "54000");
    // 文全体の状態の上限（16 MiB）: 3 MiB × 6 項目は超過、5 項目は成功する。
    let items = |n: usize| {
        (0..n)
            .map(|_| "string_agg(s, ',')")
            .collect::<Vec<_>>()
            .join(", ")
    };
    run(
        &core,
        &format!("SELECT {} FROM big WHERE g = 'three'", items(5)),
    );
    assert_eq!(
        code_of(
            &core,
            &format!("SELECT {} FROM big WHERE g = 'three'", items(6))
        ),
        "54000"
    );
}
/// NoSQL 表層の直接構築入口（`BoundAggregateItem::bind`）は `string_agg` を `42601` で拒否する
/// （SQL-57 (6)。NoSQL は対象外。2 引数・内側 ORDER BY を表せない入口で黙って通さない）。
#[test]
fn nosql_direct_bind_rejects_string_agg() {
    use engine::sql::allowlist::AggregateFunc;
    use engine::sql::parser::{AggregateTarget, BoundAggregateItem};
    let schema_val = schema();
    let err = BoundAggregateItem::bind(
        AggregateFunc::StringAgg,
        AggregateTarget::Column("s".to_string()),
        &schema_val,
    )
    .expect_err("string_agg must not bind through the direct entry");
    assert_eq!(err.wire_code(), "42601");
}

/// 索引経路（候補走査・列挙形の抑止）と全走査・cold/hot の結果が一致する。`ORDER BY` なしの
/// `string_agg` は処理順が連結順になるため、索引の選択で結果が変わってはならない。
#[test]
fn index_paths_match_full_scan_results() {
    let (core, path) = build_core("sql57-index");
    let _g = CleanupGuard(path);
    let cases = [
        // 索引対応述語（候補走査形）。
        "SELECT string_agg(s, ',') FROM t WHERE g = 'a'",
        "SELECT string_agg(s, sep ORDER BY k) FROM t WHERE g = 'b'",
        // WHERE なしの GROUP BY（列挙形は使わず物理行順）。
        "SELECT g, string_agg(s, ',') FROM t GROUP BY g ORDER BY g",
        "SELECT g, string_agg(s, ',') FROM t WHERE k > 0 GROUP BY g ORDER BY g",
    ];
    for sql in cases {
        let snapshot = |r: &QueryResult| -> Vec<Vec<Option<String>>> {
            r.rows
                .iter()
                .map(|row| {
                    row.cells
                        .iter()
                        .map(|c| match c {
                            Cell::Text(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .collect()
        };
        let cold = snapshot(&run(&core, sql));
        let hot1 = snapshot(&run(&core, sql));
        let hot2 = snapshot(&run(&core, sql));
        assert_eq!(cold, hot1, "{sql}");
        assert_eq!(hot1, hot2, "{sql}");
    }
    assert_eq!(
        one(&core, "SELECT string_agg(s, ',') FROM t WHERE g = 'a'"),
        s("x,y,x")
    );
    // EXPLAIN は列挙形（`scalar_index_group_enumeration`）を選ばない。
    let mut session = engine::sql::mode::SessionState::default();
    let plan = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT g, string_agg(s, ',') FROM t GROUP BY g",
        )
        .expect("explain");
    let text = format!("{plan:?}");
    assert!(text.contains("access_path"), "{text}");
    assert!(!text.contains("enumeration"), "{text}");
}
fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

/// ビュー越しの `string_agg`: 区切り・内側 `ORDER BY` の式も公開列に限定され（非公開列の参照は
/// `42703`）、別名は物理列へ写って連結結果が元表と一致する。評価後射影形ビュー・ビュー本体への
/// `string_agg` は fail-closed で拒否する。
#[test]
fn views_scope_string_agg_columns_and_keep_results() {
    let (core, path) = build_core("sql57-view");
    let _g = CleanupGuard(path);
    ddl(&core, "CREATE VIEW v AS SELECT g, s, k FROM t");
    ddl(&core, "CREATE VIEW va (grp, val) AS SELECT g, s FROM t");
    ddl(
        &core,
        "CREATE VIEW vb AS SELECT g, count(*) AS n FROM t GROUP BY g",
    );
    assert_eq!(
        one(&core, "SELECT string_agg(s, ',' ORDER BY k) FROM v"),
        s("y,x,x,z,x,w")
    );
    assert_eq!(
        one(&core, "SELECT string_agg(val, ',' ORDER BY val) FROM va"),
        s("w,x,x,x,y,z")
    );
    // 公開されていない列（`sep`）は区切りにも ORDER BY にも使えない。
    assert_eq!(code_of(&core, "SELECT string_agg(s, sep) FROM v"), "42703");
    assert_eq!(
        code_of(&core, "SELECT string_agg(s, ',' ORDER BY sep) FROM v"),
        "42703"
    );
    // 別名ビューの物理名は直接参照できない。
    assert_eq!(
        code_of(&core, "SELECT string_agg(val, ',' ORDER BY s) FROM va"),
        "42703"
    );
    // 集計・LIMIT を含む本体のビューの外側での `string_agg` は fail-closed。
    let c = code_of(&core, "SELECT string_agg(g, ',') FROM vb");
    assert!(c == "42601" || c == "0A000", "{c}");
}
