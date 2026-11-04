//! 距離関数 `l2_distance`／`cosine_distance`／`inner_product`／`l1_distance` の結合テスト
//! （SQL-72 (1)(2)(5)(6)・SQL-33 (3)(4)・Issue #2011。関連: RLS-7/RLS-8）。
//!
//! 期待値はテスト内で pgvector と同じ式を f64 で組み立てて照合する（フィクスチャは整数成分で、
//! L2・内積・L1 は正確、コサインは `1 - dot / sqrt(na * nb)` を 1e-12 の許容で比べる）。
//! `unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証し、
//! 他テナントの Private 行が投影・WHERE・ORDER BY・集計に現れないこと（RLS）と `wire_code` の
//! エラー契約を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";
const Q: [f32; 3] = [1.0, 2.0, 3.0];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), true),
            ColumnDef::new("k", ColumnType::Integer, true),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// tenant-a の Public 行。VECTOR 列が NULL の行は表層から作れないため、NULL 伝播は引数の `NULL`
/// で検証し、行側の扱い（embedding 空）は `udf_call` の単体テストが担う。
const ROWS_A: [(u64, Option<[f32; 3]>); 4] = [
    (1, Some([1.0, 2.0, 3.0])),
    (2, Some([4.0, 5.0, 6.0])),
    (3, Some([3.0, 4.0, 0.0])),
    (4, Some([0.0, 0.0, 0.0])),
];

/// 他テナントの Private 行。クエリ `Q` との距離が 0（漏れれば最小値・先頭になる）。
const ROWS_B: [(u64, [f32; 3]); 2] = [(100, [1.0, 2.0, 3.0]), (101, [1.0, 2.0, 3.0])];

fn insert(storage: &Storage, tenant: &str, id: u64, emb: Option<[f32; 3]>, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            emb.map_or(Value::Null, |e| Value::Vector(e.to_vec())),
            Value::Integer(i32::try_from(id).expect("small id")),
            Value::Text("x".to_string()),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for (id, emb) in ROWS_A {
        insert(&storage, "tenant-a", id, emb, Visibility::Public);
    }
    for (id, emb) in ROWS_B {
        insert(&storage, "tenant-b", id, Some(emb), Visibility::Private);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn exec(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Integer(n) => *n,
            other => panic!("unexpected id cell: {other:?}"),
        })
        .collect()
}

fn float(c: &Cell) -> f64 {
    match c {
        Cell::Float(f) => *f,
        other => panic!("expected float cell: {other:?}"),
    }
}

// pgvector と同じ式（f64）。
fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}
fn l2(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum::<f64>()
        .sqrt()
}
fn l1(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
        .sum()
}
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (na, nb) = (dot(a, a), dot(b, b));
    if na * nb == 0.0 {
        return f64::NAN;
    }
    1.0 - (dot(a, b) / (na * nb).sqrt()).clamp(-1.0, 1.0)
}

fn expected(name: &str, v: &[f32]) -> f64 {
    match name {
        "l2_distance" => l2(v, &Q),
        "cosine_distance" => cosine(v, &Q),
        "inner_product" => dot(v, &Q),
        "l1_distance" => l1(v, &Q),
        _ => unreachable!(),
    }
}

const FNS: [&str; 4] = [
    "l2_distance",
    "cosine_distance",
    "inner_product",
    "l1_distance",
];

fn close(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= 1e-12
}

/// SQL-72 (1)(2): 投影の値が pgvector の式と一致し、列型は float8、既定の列名は関数名。
#[test]
fn projection_matches_pgvector_formulas() {
    let (core, _g) = build_core("sql72-dist-proj");
    for name in FNS {
        let r = exec(
            &core,
            &format!("SELECT id, {name}(embedding, '[1,2,3]') FROM t ORDER BY id"),
        );
        assert_eq!(r.rows.len(), 4, "{name}");
        for (row, (id, emb)) in r.rows.iter().zip(ROWS_A) {
            match emb {
                Some(v) => {
                    let got = float(&row.cells[1]);
                    assert!(close(got, expected(name, &v)), "{name} id={id}: {got}");
                }
                None => assert_eq!(row.cells[1], Cell::Null, "{name} id={id}"),
            }
        }
        match &r.columns[1] {
            ColumnMeta::Computed { name: n, ty, .. } => {
                assert_eq!(n, name);
                assert_eq!(*ty, Some(ColumnType::Double));
            }
            other => panic!("unexpected column meta: {other:?}"),
        }
    }
}

/// 左右どちらにリテラルを置いても同じ値（距離は対称）。
#[test]
fn literal_may_come_first() {
    let (core, _g) = build_core("sql72-dist-sym");
    for name in FNS {
        let a = exec(
            &core,
            &format!("SELECT {name}(embedding, '[1,2,3]') FROM t ORDER BY id"),
        );
        let b = exec(
            &core,
            &format!("SELECT {name}('[1,2,3]', embedding) FROM t ORDER BY id"),
        );
        assert_eq!(format!("{:?}", a.rows), format!("{:?}", b.rows), "{name}");
    }
}

/// WHERE: 関数形は演算子形（`inner_product` は `<#>` の符号反転）と同じ行集合を返す。
#[test]
fn where_matches_operator_form() {
    let (core, _g) = build_core("sql72-dist-where");
    let pairs = [
        (
            "l2_distance(embedding, '[1,2,3]') < 6",
            "embedding <-> '[1,2,3]' < 6",
        ),
        (
            "cosine_distance(embedding, '[1,2,3]') < 0.1",
            "embedding <=> '[1,2,3]' < 0.1",
        ),
        (
            "inner_product(embedding, '[1,2,3]') > 10",
            "embedding <#> '[1,2,3]' < -10",
        ),
        (
            "l1_distance(embedding, '[1,2,3]') <= 6",
            "embedding <+> '[1,2,3]' <= 6",
        ),
    ];
    for (func, op) in pairs {
        let f = exec(&core, &format!("SELECT id FROM t WHERE {func} ORDER BY id"));
        let o = exec(&core, &format!("SELECT id FROM t WHERE {op} ORDER BY id"));
        assert_eq!(ids(&f), ids(&o), "{func}");
        assert!(!ids(&f).is_empty(), "{func}");
        assert!(
            ids(&f).iter().all(|i| *i < 100),
            "tenant-b row leaked: {func}"
        );
    }
}

/// ORDER BY: 全走査ソート。コサインのゼロノルム行（NaN）は ASC で末尾・DESC で先頭。
#[test]
fn order_by_function_expression() {
    let (core, _g) = build_core("sql72-dist-order");
    let r = exec(
        &core,
        "SELECT id FROM t ORDER BY l2_distance(embedding, '[1,2,3]') LIMIT 3",
    );
    assert_eq!(ids(&r), vec![1, 4, 3]);
    let r = exec(
        &core,
        "SELECT id FROM t ORDER BY inner_product(embedding, '[1,2,3]') DESC LIMIT 2",
    );
    assert_eq!(ids(&r), vec![2, 1]);
    let r = exec(
        &core,
        "SELECT id FROM t ORDER BY cosine_distance(embedding, '[1,2,3]')",
    );
    assert_eq!(ids(&r), vec![1, 2, 3, 4]);
    let r = exec(
        &core,
        "SELECT id FROM t ORDER BY cosine_distance(embedding, '[1,2,3]') DESC",
    );
    assert_eq!(ids(&r), vec![4, 3, 2, 1]);
}

/// NULL 引数は NULL（SQL-72 (2)）。
#[test]
fn null_arguments_return_null() {
    let (core, _g) = build_core("sql72-dist-null");
    for name in FNS {
        let r = exec(
            &core,
            &format!("SELECT {name}(embedding, NULL) FROM t WHERE id = 1"),
        );
        assert_eq!(r.rows[0].cells[0], Cell::Null, "{name}");
        let r = exec(
            &core,
            &format!("SELECT {name}(NULL, embedding) FROM t WHERE id = 1"),
        );
        assert_eq!(r.rows[0].cells[0], Cell::Null, "{name}");
    }
}

/// 式の引数（列どうし・ベクトルを返す式・CASE の中）。
#[test]
fn expression_arguments() {
    let (core, _g) = build_core("sql72-dist-expr");
    let r = exec(
        &core,
        "SELECT l2_distance(embedding, embedding) FROM t WHERE id = 2",
    );
    assert_eq!(float(&r.rows[0].cells[0]), 0.0);
    let r = exec(
        &core,
        "SELECT l2_distance(vec_div(embedding, 2), '[2,2.5,3]') FROM t WHERE id = 2",
    );
    assert_eq!(float(&r.rows[0].cells[0]), 0.0);
    let r = exec(
        &core,
        "SELECT CASE WHEN l1_distance(embedding, '[1,2,3]') = 0 THEN 'same' ELSE 'diff' END \
         FROM t WHERE id = 1",
    );
    assert_eq!(r.rows[0].cells[0], Cell::Text("same".into()));
}

/// 集計の引数。tenant-b の距離 0 の行は最小値に現れない。
#[test]
fn aggregate_argument_sees_only_own_tenant() {
    let (core, _g) = build_core("sql72-dist-agg");
    let r = exec(
        &core,
        "SELECT min(l2_distance(embedding, '[1,2,3]')) FROM t",
    );
    assert_eq!(float(&r.rows[0].cells[0]), 0.0);
    let r = exec(
        &core,
        "SELECT count(*) FROM t WHERE l2_distance(embedding, '[1,2,3]') = 0",
    );
    assert_eq!(r.rows[0].cells[0], Cell::Integer(1));
}

/// RLS: tenant-b から見ると自分の行だけが対象になる。
#[test]
fn rls_hides_other_tenant_rows() {
    let (core, _g) = build_core("sql72-dist-rls");
    let r = core
        .execute_sql(
            &ctx("tenant-b"),
            "SELECT id FROM t WHERE l2_distance(embedding, '[1,2,3]') = 0 ORDER BY id",
        )
        .expect("tenant-b query");
    // Public な tenant-a の行 1 は見え、tenant-a から見た tenant-b の Private 行は見えない。
    assert_eq!(ids(&r), vec![1, 100, 101]);
    let r = exec(
        &core,
        "SELECT id FROM t WHERE l2_distance(embedding, '[1,2,3]') = 0 ORDER BY id",
    );
    assert_eq!(ids(&r), vec![1]);
}

/// エラー契約（fail-closed）。
#[test]
fn error_classification() {
    let (core, _g) = build_core("sql72-dist-err");
    let cases = [
        ("SELECT l2_distance(embedding) FROM t", "42883"),
        (
            "SELECT l2_distance(embedding, embedding, embedding) FROM t",
            "42883",
        ),
        ("SELECT l2_distance(k, '[1,2,3]') FROM t", "42883"),
        ("SELECT l2_distance(label, '[1,2,3]') FROM t", "42883"),
        ("SELECT l2_distance(embedding, '[1,2]') FROM t", "22000"),
        ("SELECT l2_distance(embedding, 'abc') FROM t", "22000"),
        ("SELECT l2_distance(embedding, '[]') FROM t", "22000"),
        ("SELECT l2_distance('[1,2,3]', '[1,2,3]') FROM t", "0A000"),
        ("SELECT l2_distance(NULL, '[1,2,3]') FROM t", "0A000"),
        ("SELECT l2_distance(NULL, NULL) FROM t", "0A000"),
        (
            "SELECT l2_distance(embedding, vec_div(embedding, 2)) FROM t WHERE id = 4",
            "ok",
        ),
    ];
    for (sql, want) in cases {
        let got = match core.execute_sql(&ctx("tenant-a"), sql) {
            Ok(_) => "ok",
            Err(e) => e.wire_code(),
        };
        assert_eq!(got, want, "{sql}");
    }
    let long = format!("[{}]", "1,".repeat(40_000));
    assert_eq!(
        code_of(
            &core,
            &format!("SELECT l2_distance(embedding, '{long}') FROM t")
        ),
        "54000"
    );
}

/// エラー文言にベクトルリテラルの値を含めない。
#[test]
fn error_detail_does_not_echo_literal() {
    let (core, _g) = build_core("sql72-dist-leak");
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT l2_distance(embedding, '[7.654321,0]') FROM t",
        )
        .expect_err("dim mismatch");
    assert!(!format!("{err}").contains("7.654321"));
}

/// 式索引・CHECK・宣言的 UDF の本体には置けない（距離演算子と同じ `0A000`）。予約名の再定義も拒否する。
#[test]
fn ddl_positions_are_rejected() {
    use engine::sql::mode::SessionState;
    let (core, _g) = build_core("sql72-dist-ddl");
    let mut session = SessionState::default();
    session.allow_ddl();
    let cases = [
        (
            "CREATE INDEX i1 ON t ((l2_distance(embedding, '[1,2,3]')))",
            "0A000",
        ),
        (
            "ALTER TABLE t ADD CONSTRAINT c1 CHECK (l2_distance(embedding, '[1,2,3]') < 1)",
            "0A000",
        ),
        ("CREATE FUNCTION f_d(a) AS l2_distance(a, a)", "0A000"),
        ("CREATE FUNCTION l2_distance(a, b) AS a", "42723"),
    ];
    for (sql, want) in cases {
        let got = match core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql) {
            Ok(_) => "ok",
            Err(e) => e.wire_code(),
        };
        assert_eq!(got, want, "{sql}");
    }
}
