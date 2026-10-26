//! `stddev` / `stddev_pop` / `stddev_samp` の結合テスト（SQL-57・Issue #1804。関連: SQL-13・
//! RLS-7/RLS-8・TABLE-13）。分散系（Issue #1803、`sql57_variance.rs`）の平方根として振る舞う。
//!
//! numeric の期待値は PostgreSQL 17 の `numeric_stddev_internal` の手順（丸め済みの分散を
//! 同じ除算 scale で平方根にして half-up 丸め）を Python の整数演算で独立に再現して導出した
//! 固定文字列で完全一致を、浮動小数は相対誤差 1e-12 以内で比較する。`unique_db_path`＋
//! `CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{ArrayValue, Value};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";
const OVF: &str = "o";

/// 2^53 + 1（f64 では正確に表せない bigint）。
const BIG_INT: i64 = 9_007_199_254_740_993;

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType| ColumnDef::new(name, ty, true);
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("g", ColumnType::Text),
            col("k", ColumnType::Integer),
            col("bg", ColumnType::BigInt),
            col("sm", ColumnType::SmallInt),
            col("r", ColumnType::Real),
            col("d", ColumnType::Double),
            col(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
            col(
                "nw",
                ColumnType::Numeric {
                    precision: 30,
                    scale: 20,
                },
            ),
            col("s", ColumnType::Text),
            col("bo", ColumnType::Boolean),
            col("dt", ColumnType::Date),
            col("ts", ColumnType::Timestamp),
            col("tm", ColumnType::Time),
            col("u", ColumnType::Uuid),
            col("b", ColumnType::Bytea),
            col("j", ColumnType::Json),
            col(
                "arr",
                ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 4).expect("array type")),
            ),
            // 関数名と同名の列（括弧を伴わない参照は従来どおり列として解釈される）。
            col("stddev", ColumnType::Integer),
        ],
    )
}

fn ovf_schema() -> TableSchema {
    TableSchema::new(
        OVF,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("d", ColumnType::Double, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, k)`。`k` の NULL を含む。
type Seed = (u64, &'static str, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some(3)),
    (2, "a", Some(1)),
    (3, "a", Some(2)),
    (4, "b", Some(5)),
    (5, "b", None),
    (6, "c", Some(1)),
    (7, "b", Some(4)),
    (8, "a", Some(9)),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, k) = *seed;
    let idn = id as i64;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            k.map_or(Value::Null, Value::Integer),
            if id == 2 {
                Value::Null
            } else if id == 1 {
                Value::BigInt(BIG_INT)
            } else {
                Value::BigInt(idn * 10)
            },
            Value::Integer(id as i32),
            Value::Real(id as f32 * 0.5),
            Value::Double(id as f64 * 1.5),
            Value::Numeric(Decimal::from_parts(i128::from(id) * 100 + 5, 2).expect("decimal")),
            Value::Numeric(
                Decimal::from_parts(i128::from(id) * 100_000_000_000_000_000_000, 20)
                    .expect("decimal"),
            ),
            Value::Text("x".to_string()),
            Value::Bool(id % 2 == 0),
            Value::Date(19_000 + id as i32),
            Value::Timestamp(1_700_000_000_000_000 + idn),
            Value::Time(idn),
            Value::Uuid(Uuid::from_bytes([id as u8; 16])),
            Value::Bytes(vec![id as u8]),
            Value::Json(format!("{{\"a\": {id}}}")),
            Value::Array(ArrayValue::Integer(vec![Some(id as i32)])),
            Value::Integer(id as i32),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn insert_ovf(storage: &Storage, id: u64, d: f64) {
    engine::tenant::insert_typed_row(
        storage,
        OVF,
        &ctx("tenant-a"),
        id,
        Visibility::Public,
        &[Value::Vector(vec![id as f32, 0.0]), Value::Double(d)],
        &OperationId::parse(&format!("ovf-{id}")).expect("valid operation id"),
    )
    .expect("insert ovf row");
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage.create_table(&ovf_schema()).expect("create ovf");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない）。値・あふれの有無に影響しない。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some(100_000)),
        Visibility::Private,
    );
    // `o` は double の極大値 2 行（和が非有限化して 22003）と、他テナントの 1 行。
    insert_ovf(&storage, 1, f64::MAX);
    insert_ovf(&storage, 2, f64::MAX);
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

/// セルを比較しやすい表現へ写す。numeric は表示文字列、double は値、NULL は `Null`。
#[derive(Debug, Clone, PartialEq)]
enum V {
    Null,
    Num(String),
    Float(f64),
    Text(String),
}

fn v_of(cell: &Cell) -> V {
    match cell {
        Cell::Null => V::Null,
        Cell::Numeric(d) => V::Num(d.to_string()),
        Cell::Float(f) => V::Float(*f),
        Cell::Text(t) => V::Text(t.clone()),
        other => panic!("unexpected cell: {other:?}"),
    }
}

fn one(core: &EngineCore, tenant: &str, sql: &str) -> V {
    let r = core
        .execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"));
    assert_eq!(r.rows.len(), 1, "{sql}");
    v_of(&r.rows[0].cells[0])
}

fn val(core: &EngineCore, sql: &str) -> V {
    one(core, "tenant-a", sql)
}

fn num(s: &str) -> V {
    V::Num(s.to_string())
}

/// 浮動小数の期待値との相対誤差 1e-12 以内の一致。
fn assert_close(got: &V, want: f64, what: &str) {
    match got {
        V::Float(g) => {
            let scale = want.abs().max(1.0);
            assert!(
                (g - want).abs() <= 1e-12 * scale,
                "{what}: got {g}, want {want}"
            );
        }
        other => panic!("{what}: expected a double, got {other:?}"),
    }
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

/// 端点: 0 行（空・全 NULL）は 3 関数とも NULL、1 行は標本が NULL・母集団が scale 0 の `0`。
#[test]
fn endpoints_and_null_inputs() {
    let (core, path) = build_core("sql57-sd-endpoints");
    let _g = CleanupGuard(path);
    for f in ["stddev", "stddev_pop", "stddev_samp"] {
        for sql in [
            format!("SELECT {f}(k) FROM t WHERE g = 'zzz'"),
            format!("SELECT {f}(k) FROM t WHERE id = 5"),
            format!("SELECT {f}(d) FROM t WHERE g = 'zzz'"),
        ] {
            assert_eq!(val(&core, &sql), V::Null, "{sql}");
        }
    }
    for sql in [
        "SELECT stddev(k) FROM t WHERE id = 1",
        "SELECT stddev_samp(k) FROM t WHERE id = 1",
        "SELECT stddev_samp(d) FROM t WHERE id = 1",
    ] {
        assert_eq!(val(&core, sql), V::Null, "{sql}");
    }
    assert_eq!(
        val(&core, "SELECT stddev_pop(k) FROM t WHERE id = 1"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT stddev_pop(n) FROM t WHERE id = 1"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT stddev_pop(d) FROM t WHERE id = 1"),
        V::Float(0.0)
    );
    // 全値が等しい場合は scale 0 の 0。
    assert_eq!(
        val(&core, "SELECT stddev_pop(k) FROM t WHERE id IN (2, 6)"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT stddev_samp(k) FROM t WHERE id IN (2, 6)"),
        num("0")
    );
    // NULL 行は無視される。
    assert_eq!(
        val(&core, "SELECT stddev_pop(k) FROM t"),
        val(&core, "SELECT stddev_pop(k) FROM t WHERE id <> 5")
    );
}

/// 整数系・疑似列 `id`・numeric 列は numeric 結果（PG 17 の手順で導出した固定値）。
#[test]
fn integer_and_numeric_inputs_return_numeric() {
    let (core, path) = build_core("sql57-sd-int");
    let _g = CleanupGuard(path);
    for (sql, want) in [
        ("SELECT stddev_pop(k) FROM t", "2.6108095546424379"),
        ("SELECT stddev_samp(k) FROM t", "2.8199966227605579"),
        ("SELECT stddev(k) FROM t", "2.8199966227605579"),
        ("SELECT stddev_pop(id) FROM t", "2.2912878474779200"),
        ("SELECT stddev_samp(id) FROM t", "2.4494897427831781"),
        ("SELECT stddev_pop(sm) FROM t", "2.2912878474779200"),
        ("SELECT stddev_samp(n) FROM t", "2.4494897427831781"),
        ("SELECT stddev_pop(n) FROM t", "2.2912878474779200"),
        (
            "SELECT stddev_pop(id) FROM t WHERE id <= 3",
            "0.81649658092772603273",
        ),
        (
            "SELECT stddev_samp(id) FROM t WHERE id <= 3",
            "1.00000000000000000000",
        ),
        (
            "SELECT stddev_samp(id) FROM t WHERE id <= 4",
            "1.2909944487358056",
        ),
        (
            "SELECT stddev_pop(id) FROM t WHERE id <= 4",
            "1.1180339887498948",
        ),
        // 2^53 + 1 を含む bigint も正確演算（scale 0）。
        ("SELECT stddev_pop(bg) FROM t", "3151863169384602"),
        ("SELECT stddev_samp(bg) FROM t", "3404401319607263"),
    ] {
        assert_eq!(val(&core, sql), num(want), "{sql}");
    }
    // 関数名と同名の列の裸参照は従来どおり列。
    let r = run(&core, "SELECT stddev FROM t WHERE id = 3");
    assert_eq!(r.rows.len(), 1);
}

/// 浮動小数列・数値スカラー式は double 結果。
#[test]
fn float_inputs_and_scalar_expressions_return_double() {
    let (core, path) = build_core("sql57-sd-float");
    let _g = CleanupGuard(path);
    // d = 1.5 * id: 母分散 11.8125・標本分散 13.5。r = 0.5 * id: 1.3125・1.5。
    assert_close(
        &val(&core, "SELECT stddev_pop(d) FROM t"),
        11.8125f64.sqrt(),
        "stddev_pop(d)",
    );
    assert_close(
        &val(&core, "SELECT stddev_samp(d) FROM t"),
        13.5f64.sqrt(),
        "stddev_samp(d)",
    );
    assert_close(
        &val(&core, "SELECT stddev(d) FROM t"),
        13.5f64.sqrt(),
        "stddev(d)",
    );
    assert_close(
        &val(&core, "SELECT stddev_pop(r) FROM t"),
        1.3125f64.sqrt(),
        "stddev_pop(r)",
    );
    assert_close(
        &val(&core, "SELECT stddev_samp(k + 1) FROM t"),
        (334.0f64 / 42.0).sqrt(),
        "stddev_samp(k + 1)",
    );
    // 極大値の累積が非有限になる形は 22003。
    assert_eq!(code_of(&core, "SELECT stddev_pop(d) FROM o"), "22003");
}

/// 分散段階の範囲超過（numeric(30,20) の結果 scale 超過）は stddev でも 22003。
#[test]
fn out_of_range_is_inherited_from_variance_stage() {
    let (core, path) = build_core("sql57-sd-range");
    let _g = CleanupGuard(path);
    assert_eq!(code_of(&core, "SELECT stddev_pop(nw) FROM t"), "22003");
    assert_eq!(code_of(&core, "SELECT stddev_samp(nw) FROM t"), "22003");
    assert_eq!(
        val(&core, "SELECT stddev_pop(nw) FROM t WHERE id = 1"),
        num("0")
    );
}

/// GROUP BY・HAVING（別名と直接呼び出し）・ORDER BY・既定列名・結果列型。
#[test]
fn group_by_having_order_by_and_metadata() {
    let (core, path) = build_core("sql57-sd-group");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, stddev_pop(k) AS p, stddev_samp(k) AS s FROM t GROUP BY g ORDER BY g",
    );
    let rows: Vec<(V, V, V)> = r
        .rows
        .iter()
        .map(|row| {
            (
                v_of(&row.cells[0]),
                v_of(&row.cells[1]),
                v_of(&row.cells[2]),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                V::Text("a".into()),
                num("3.1124748994971831"),
                num("3.5939764421413041")
            ),
            (
                V::Text("b".into()),
                num("0.50000000000000000000"),
                num("0.70710678118654752440")
            ),
            (V::Text("c".into()), num("0"), V::Null),
        ]
    );
    let names: Vec<String> = run(
        &core,
        "SELECT stddev(k), stddev_pop(k), stddev_samp(k) FROM t",
    )
    .columns
    .iter()
    .map(|c| match c {
        ColumnMeta::Computed { name, .. } => name.clone(),
        other => panic!("unexpected column meta: {other:?}"),
    })
    .collect();
    assert_eq!(names, ["stddev", "stddev_pop", "stddev_samp"]);
    // HAVING は別名経由と呼び出しを直接書く形の両方。
    for sql in [
        "SELECT g FROM t GROUP BY g HAVING stddev_pop(d) > 1 ORDER BY g",
        "SELECT g, stddev_pop(d) AS v FROM t GROUP BY g HAVING v > 1 ORDER BY g",
    ] {
        let r = run(&core, sql);
        let groups: Vec<V> = r.rows.iter().map(|row| v_of(&row.cells[0])).collect();
        assert_eq!(groups, [V::Text("a".into()), V::Text("b".into())], "{sql}");
    }
    let r = run(
        &core,
        "SELECT g, stddev_pop(k) AS p FROM t GROUP BY g ORDER BY p DESC",
    );
    let groups: Vec<V> = r.rows.iter().map(|row| v_of(&row.cells[0])).collect();
    assert_eq!(
        groups,
        [
            V::Text("a".into()),
            V::Text("b".into()),
            V::Text("c".into())
        ]
    );
    for (sql, numeric) in [
        ("SELECT stddev_pop(k) FROM t", true),
        ("SELECT stddev_samp(id) FROM t", true),
        ("SELECT stddev(bg) FROM t", true),
        ("SELECT stddev_pop(n) FROM t", true),
        ("SELECT stddev_pop(d) FROM t", false),
        ("SELECT stddev_pop(r) FROM t", false),
        ("SELECT stddev_pop(k + 1) FROM t", false),
    ] {
        let r = run(&core, sql);
        match &r.columns[0] {
            ColumnMeta::Computed { ty: Some(ty), .. } => {
                assert_eq!(matches!(ty, ColumnType::Numeric { .. }), numeric, "{sql}");
                assert_eq!(matches!(ty, ColumnType::Double), !numeric, "{sql}");
            }
            other => panic!("{sql}: unexpected column meta: {other:?}"),
        }
    }
}

/// 非数値の引数は `42883`。
#[test]
fn non_numeric_arguments_are_rejected() {
    let (core, path) = build_core("sql57-sd-reject");
    let _g = CleanupGuard(path);
    for f in ["stddev", "stddev_pop", "stddev_samp"] {
        for col in [
            "g",
            "s",
            "bo",
            "dt",
            "ts",
            "tm",
            "u",
            "b",
            "j",
            "arr",
            "embedding",
        ] {
            assert_eq!(
                code_of(&core, &format!("SELECT {f}({col}) FROM t")),
                "42883",
                "{f}({col})"
            );
        }
    }
    assert_eq!(
        code_of(&core, "SELECT stddev_pop(upper(g)) FROM t"),
        "42883"
    );
}

/// 対象外の形は fail-closed。
#[test]
fn unsupported_forms_are_rejected_explicitly() {
    let (core, path) = build_core("sql57-sd-contexts");
    let _g = CleanupGuard(path);
    for sql in [
        "SELECT stddev_pop(DISTINCT k) FROM t",
        "SELECT stddev_pop(k ORDER BY k) FROM t",
        "SELECT stddev_pop(*) FROM t",
        "SELECT stddev_pop() FROM t",
        "SELECT stddev_pop(k, k) FROM t",
        "SELECT stddev_pop(k) OVER () FROM t",
    ] {
        let c = code_of(&core, sql);
        assert!(
            matches!(c, "42601" | "0A000" | "42883"),
            "{sql}: unexpected code {c}"
        );
    }
    assert_eq!(code_of(&core, "SELECT stddev_pop(1)"), "0A000");
    assert_eq!(
        code_of(
            &core,
            "SELECT stddev_pop(a.k) FROM t a JOIN t b ON a.g = b.g"
        ),
        "0A000"
    );
}

/// NoSQL 表層の直接構築入口は stddev 系を `42601` で拒否する。
#[test]
fn nosql_direct_bind_rejects_stddev() {
    use engine::sql::allowlist::AggregateFunc;
    use engine::sql::parser::{AggregateTarget, BoundAggregateItem};
    let schema_val = schema();
    for func in [
        AggregateFunc::Stddev,
        AggregateFunc::StddevPop,
        AggregateFunc::StddevSamp,
    ] {
        let err =
            BoundAggregateItem::bind(func, AggregateTarget::Column("k".to_string()), &schema_val)
                .expect_err("stddev must not bind through the direct entry");
        assert_eq!(err.wire_code(), "42601");
    }
}

/// ビュー: 通常のビュー越しは元表と一致し、集計ビュー本体は fail-closed。
#[test]
fn views_keep_results_and_aggregate_bodies_are_rejected() {
    let (core, path) = build_core("sql57-sd-view");
    let _g = CleanupGuard(path);
    ddl(&core, "CREATE VIEW v AS SELECT g, k, d FROM t");
    assert_eq!(
        val(&core, "SELECT stddev_pop(k) FROM v"),
        val(&core, "SELECT stddev_pop(k) FROM t")
    );
    let c = code_of(&core, "CREATE VIEW vb AS SELECT stddev_pop(k) AS p FROM t");
    assert!(matches!(c, "0A000" | "42601"), "{c}");
    ddl(&core, "CREATE VIEW vl AS SELECT g, k FROM t LIMIT 5");
    let c = code_of(&core, "SELECT stddev_pop(k) FROM vl");
    assert!(matches!(c, "0A000" | "42601"), "{c}");
}

/// RLS: 他テナントの Private 行は結果に影響しない。
#[test]
fn other_tenant_private_rows_never_affect_results() {
    let (core, path) = build_core("sql57-sd-rls");
    let _g = CleanupGuard(path);
    assert_eq!(
        val(&core, "SELECT stddev_pop(k) FROM t"),
        num("2.6108095546424379")
    );
    assert_ne!(
        one(&core, "tenant-b", "SELECT stddev_pop(k) FROM t"),
        val(&core, "SELECT stddev_pop(k) FROM t")
    );
    assert_eq!(
        one(&core, "tenant-c", "SELECT stddev_pop(k) FROM t"),
        val(&core, "SELECT stddev_pop(k) FROM t")
    );
}
