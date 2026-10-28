//! `variance` / `var_pop` / `var_samp` の結合テスト（SQL-57・Issue #1803。関連: SQL-13・SQL-14・
//! RLS-7/RLS-8・WIRE-13・TABLE-13）。
//!
//! 期待値は PostgreSQL 17 の意味論（行数 0 は NULL、行数 1 は `var_samp`/`variance` が NULL・
//! `var_pop` が 0、NULL 入力は無視、整数系・numeric 列は numeric 結果で結果 scale は
//! `select_div_scale` 規則、浮動小数列・数値式は double 結果）に従う固定値。numeric の期待値は
//! 手順を独立に実装して導出した文字列で完全一致を、浮動小数は相対誤差 1e-12 以内で比較する
//! （累積順序による丸めの揺れを許容するため）。`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路で検証し、他テナントの Private 行が結果・
//! `22003` の有無へ影響しないこと（RLS）と `wire_code` のエラー契約を固定する。

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

use crate::temp_db;
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
            col("variance", ColumnType::Integer),
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

/// 端点: 0 行（空・全除外）は 3 関数とも NULL、1 行は標本が NULL・母集団が scale 0 の `0`、
/// 全 NULL は NULL。NULL 行は件数にも和にも入らない。
#[test]
fn endpoints_and_null_inputs() {
    let (core, path) = build_core("sql57-var-endpoints");
    let _g = CleanupGuard(path);
    for f in ["variance", "var_pop", "var_samp"] {
        assert_eq!(
            val(&core, &format!("SELECT {f}(k) FROM t WHERE g = 'zzz'")),
            V::Null
        );
        // 全 NULL（id 5 だけの k は NULL）。
        assert_eq!(
            val(&core, &format!("SELECT {f}(k) FROM t WHERE id = 5")),
            V::Null
        );
        assert_eq!(
            val(&core, &format!("SELECT {f}(d) FROM t WHERE g = 'zzz'")),
            V::Null
        );
    }
    // 1 行: 標本は NULL、母集団は 0（numeric は scale 0）。
    assert_eq!(
        val(&core, "SELECT variance(k) FROM t WHERE id = 1"),
        V::Null
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k) FROM t WHERE id = 1"),
        V::Null
    );
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t WHERE id = 1"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT var_pop(n) FROM t WHERE id = 1"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT var_pop(d) FROM t WHERE id = 1"),
        V::Float(0.0)
    );
    assert_eq!(
        val(&core, "SELECT var_samp(d) FROM t WHERE id = 1"),
        V::Null
    );
    // 全値が等しい場合も scale 0 の 0（`0.00...` ではない）。
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t WHERE id IN (2, 6)"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k) FROM t WHERE id IN (2, 6)"),
        num("0")
    );
    // NULL 行（id 5）を除いた結果と、含めた結果が一致する。
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t"),
        val(&core, "SELECT var_pop(k) FROM t WHERE id <> 5")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k) FROM t"),
        val(&core, "SELECT var_samp(k) FROM t WHERE id <> 5")
    );
}

/// 整数系・疑似列 `id` は numeric 結果で、結果 scale は PostgreSQL の `select_div_scale` 規則。
#[test]
fn integer_inputs_return_numeric_with_pg_scales() {
    let (core, path) = build_core("sql57-var-int");
    let _g = CleanupGuard(path);
    // k（NULL 1 件を除く 7 件: 3,1,2,5,1,4,9）
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t"),
        num("6.8163265306122449")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k) FROM t"),
        num("7.9523809523809524")
    );
    assert_eq!(
        val(&core, "SELECT variance(k) FROM t"),
        num("7.9523809523809524")
    );
    // id 1..8（疑似列）と smallint 列 sm（= id）は同じ値。
    assert_eq!(
        val(&core, "SELECT var_pop(id) FROM t"),
        num("5.2500000000000000")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(id) FROM t"),
        num("6.0000000000000000")
    );
    assert_eq!(
        val(&core, "SELECT var_pop(sm) FROM t"),
        num("5.2500000000000000")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(sm) FROM t"),
        num("6.0000000000000000")
    );
    // 分子 6 / 分母 9・6 は先頭桁が 6 以下になり scale 20。
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t WHERE id IN (1, 2, 3)"),
        num("0.66666666666666666667")
    );
    assert_eq!(
        val(&core, "SELECT var_pop(id) FROM t WHERE id <= 3"),
        num("0.66666666666666666667")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(id) FROM t WHERE id <= 3"),
        num("1.00000000000000000000")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(id) FROM t WHERE id <= 4"),
        num("1.6666666666666667")
    );
    assert_eq!(
        val(&core, "SELECT var_pop(id) FROM t WHERE id <= 4"),
        num("1.2500000000000000")
    );
    // 列名と同名の `variance` の裸参照は従来どおり列（関数呼び出しと共存する）。
    assert_eq!(
        val(&core, "SELECT variance(variance) FROM t"),
        num("6.0000000000000000")
    );
    let r = run(&core, "SELECT variance FROM t WHERE id = 3");
    assert_eq!(r.rows.len(), 1);
}

/// `numeric(p,s)` 列は入力 scale を反映した numeric 結果になる。
#[test]
fn numeric_columns_return_numeric() {
    let (core, path) = build_core("sql57-var-numeric");
    let _g = CleanupGuard(path);
    // n = (id * 100 + 5) / 100（scale 2）なので id と同じ分散。
    assert_eq!(
        val(&core, "SELECT var_pop(n) FROM t"),
        num("5.2500000000000000")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(n) FROM t"),
        num("6.0000000000000000")
    );
    assert_eq!(
        val(&core, "SELECT variance(n) FROM t"),
        num("6.0000000000000000")
    );
    // numeric(30,20): 結果 scale が 38 を超える形は 22003（fail-closed。PG の任意精度との差分）。
    assert_eq!(code_of(&core, "SELECT var_pop(nw) FROM t"), "22003");
    assert_eq!(code_of(&core, "SELECT var_samp(nw) FROM t"), "22003");
    // 1 行・0 行は演算に入らないため成功する。
    assert_eq!(
        val(&core, "SELECT var_pop(nw) FROM t WHERE id = 1"),
        num("0")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(nw) FROM t WHERE id = 1"),
        V::Null
    );
}

/// 浮動小数列と数値スカラー式は double 結果（Youngs-Cramer 更新）。
#[test]
fn float_inputs_and_scalar_expressions_return_double() {
    let (core, path) = build_core("sql57-var-float");
    let _g = CleanupGuard(path);
    // d = 1.5 * id（1..8）→ 母 11.8125・標本 13.5。r = 0.5 * id → 母 1.3125・標本 1.5。
    assert_close(
        &val(&core, "SELECT var_pop(d) FROM t"),
        11.8125,
        "var_pop(d)",
    );
    assert_close(
        &val(&core, "SELECT var_samp(d) FROM t"),
        13.5,
        "var_samp(d)",
    );
    assert_close(
        &val(&core, "SELECT variance(d) FROM t"),
        13.5,
        "variance(d)",
    );
    assert_close(
        &val(&core, "SELECT var_pop(r) FROM t"),
        1.3125,
        "var_pop(r)",
    );
    assert_close(&val(&core, "SELECT var_samp(r) FROM t"), 1.5, "var_samp(r)");
    // 数値式: k + 1 の分散は k の分散（NULL 行は式が NULL なので無視される）。
    assert_close(
        &val(&core, "SELECT var_pop(k + 1) FROM t"),
        334.0 / 49.0,
        "var_pop(k + 1)",
    );
    assert_close(
        &val(&core, "SELECT var_samp(k + 1) FROM t"),
        334.0 / 42.0,
        "var_samp(k + 1)",
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k + 1) FROM t WHERE id = 1"),
        V::Null
    );
    // 極大値の累積が非有限になる形は 22003。
    assert_eq!(code_of(&core, "SELECT var_pop(d) FROM o"), "22003");
}

/// 大きな値: `N * Σx²` が i128 を超える bigint でも正確に計算できる（PG は成功する）。
#[test]
fn bigint_values_use_wide_arithmetic() {
    let (core, path) = build_core("sql57-var-bigint");
    let _g = CleanupGuard(path);
    // bg = [2^53 + 1, 30, 40, 50, 60, 70, 80]（id 2 は NULL）。結果は 31 桁の整数部で scale 0。
    assert_eq!(
        val(&core, "SELECT var_pop(bg) FROM t"),
        num("9934241438523148030808509361864")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(bg) FROM t"),
        num("11589948344943672702609927588841")
    );
}

/// `GROUP BY` 併用: グループごとの端点・値、結果列名、`HAVING`・`ORDER BY`。
#[test]
fn group_by_having_and_order_by() {
    let (core, path) = build_core("sql57-var-group");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, var_pop(k) AS p, var_samp(k) AS s, variance(k) FROM t GROUP BY g ORDER BY g",
    );
    let rows: Vec<(String, V, V, V)> = r
        .rows
        .iter()
        .map(|row| {
            let V::Text(g) = v_of(&row.cells[0]) else {
                panic!("group key must be text")
            };
            (
                g,
                v_of(&row.cells[1]),
                v_of(&row.cells[2]),
                v_of(&row.cells[3]),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "a".to_string(),
                num("9.6875000000000000"),
                num("12.9166666666666667"),
                num("12.9166666666666667")
            ),
            (
                "b".to_string(),
                num("0.25000000000000000000"),
                num("0.50000000000000000000"),
                num("0.50000000000000000000")
            ),
            // 1 行のグループ: 母集団は 0、標本は NULL。
            ("c".to_string(), num("0"), V::Null, V::Null),
        ]
    );
    // 既定の結果列名は関数名。
    let names: Vec<String> = run(&core, "SELECT var_pop(k), var_samp(k), variance(k) FROM t")
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } => name.clone(),
            other => panic!("unexpected column meta: {other:?}"),
        })
        .collect();
    assert_eq!(names, ["var_pop", "var_samp", "variance"]);
    // NULL のみのグループ（k が NULL の行だけを残す）。
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t WHERE k IS NULL GROUP BY g"),
        V::Null
    );
    // numeric 結果の ORDER BY（scale が値ごとに異なっても厳密に比較される）。
    let r = run(
        &core,
        "SELECT g, var_pop(k) AS p FROM t GROUP BY g ORDER BY p DESC",
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
    // HAVING（double 結果）と ORDER BY（出力列名）。
    let r = run(
        &core,
        "SELECT g, var_pop(d) AS v FROM t GROUP BY g HAVING v > 1 ORDER BY g",
    );
    let groups: Vec<V> = r.rows.iter().map(|row| v_of(&row.cells[0])).collect();
    assert_eq!(groups, [V::Text("a".into()), V::Text("b".into())]);
    let r = run(
        &core,
        "SELECT g, var_pop(d) AS v FROM t GROUP BY g ORDER BY v DESC",
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
}

/// 結果列の型: 整数系・numeric 入力は numeric、浮動小数・数値式は double（wire の型 OID の根拠）。
#[test]
fn result_column_types_are_advertised() {
    let (core, path) = build_core("sql57-var-meta");
    let _g = CleanupGuard(path);
    for (sql, numeric) in [
        ("SELECT var_pop(k) FROM t", true),
        ("SELECT var_samp(id) FROM t", true),
        ("SELECT variance(bg) FROM t", true),
        ("SELECT var_pop(sm) FROM t", true),
        ("SELECT var_pop(n) FROM t", true),
        ("SELECT var_pop(d) FROM t", false),
        ("SELECT var_pop(r) FROM t", false),
        ("SELECT var_pop(k + 1) FROM t", false),
    ] {
        let r = run(&core, sql);
        match &r.columns[0] {
            ColumnMeta::Computed { ty: Some(ty), .. } => {
                assert_eq!(
                    matches!(ty, ColumnType::Numeric { .. }),
                    numeric,
                    "{sql}: {ty:?}"
                );
                assert_eq!(matches!(ty, ColumnType::Double), !numeric, "{sql}: {ty:?}");
            }
            other => panic!("{sql}: unexpected column meta: {other:?}"),
        }
    }
}

/// 非数値の引数は該当シグネチャの関数が無いとして `42883`（内部エラー `XX000` にならない）。
#[test]
fn non_numeric_arguments_are_rejected_with_undefined_function() {
    let (core, path) = build_core("sql57-var-reject");
    let _g = CleanupGuard(path);
    for f in ["variance", "var_pop", "var_samp"] {
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
    // 非数値の式。
    assert_eq!(code_of(&core, "SELECT var_pop(upper(g)) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT var_pop(missing) FROM t"), "42703");
}

/// 対象外の形は既存の拒否コードで fail-closed（成功も `XX000` もしない）。
#[test]
fn unsupported_forms_are_rejected_explicitly() {
    let (core, path) = build_core("sql57-var-contexts");
    let _g = CleanupGuard(path);
    for sql in [
        "SELECT var_pop(DISTINCT k) FROM t",
        "SELECT var_pop(k ORDER BY k) FROM t",
        "SELECT var_pop(*) FROM t",
        "SELECT var_pop() FROM t",
        "SELECT var_pop(k, k) FROM t",
        "SELECT var_pop(k) OVER () FROM t",
    ] {
        let c = code_of(&core, sql);
        assert!(
            matches!(c, "42601" | "0A000" | "42883"),
            "{sql}: unexpected code {c}"
        );
    }
    // FROM なしは明示的な 0A000。
    assert_eq!(code_of(&core, "SELECT var_pop(1)"), "0A000");
    // JOIN は 0A000。
    assert_eq!(
        code_of(&core, "SELECT var_pop(a.k) FROM t a JOIN t b ON a.g = b.g"),
        "0A000"
    );
}

/// NoSQL 表層の直接構築入口は分散系を `42601` で拒否する。
#[test]
fn nosql_direct_bind_rejects_variance() {
    use engine::sql::allowlist::AggregateFunc;
    use engine::sql::parser::{AggregateTarget, BoundAggregateItem};
    let schema_val = schema();
    for func in [
        AggregateFunc::Variance,
        AggregateFunc::VarPop,
        AggregateFunc::VarSamp,
    ] {
        let err =
            BoundAggregateItem::bind(func, AggregateTarget::Column("k".to_string()), &schema_val)
                .expect_err("variance must not bind through the direct entry");
        assert_eq!(err.wire_code(), "42601");
    }
}

/// ビュー: 通常のビュー越しの分散は元表と一致し、集計ビュー本体・評価後射影形ビューは fail-closed。
#[test]
fn views_keep_results_and_aggregate_bodies_are_rejected() {
    let (core, path) = build_core("sql57-var-view");
    let _g = CleanupGuard(path);
    ddl(&core, "CREATE VIEW v AS SELECT g, k, d FROM t");
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM v"),
        val(&core, "SELECT var_pop(k) FROM t")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(d) FROM v"),
        val(&core, "SELECT var_samp(d) FROM t")
    );
    // 公開されていない列は参照できない。
    assert_eq!(code_of(&core, "SELECT var_pop(bg) FROM v"), "42703");
    // 集計ビュー本体には含められない。
    let c = code_of(&core, "CREATE VIEW vb AS SELECT var_pop(k) AS p FROM t");
    assert!(matches!(c, "0A000" | "42601"), "{c}");
    // 集計 / LIMIT を持つビューへの外側集計も対象外。
    ddl(&core, "CREATE VIEW vl AS SELECT g, k FROM t LIMIT 5");
    let c = code_of(&core, "SELECT var_pop(k) FROM vl");
    assert!(matches!(c, "0A000" | "42601"), "{c}");
}

/// RLS: 他テナントの Private 行は結果にも `22003` の有無にも影響しない。
#[test]
fn other_tenant_private_rows_never_affect_results() {
    let (core, path) = build_core("sql57-var-rls");
    let _g = CleanupGuard(path);
    // tenant-b の行（k = 100000）が見えていれば値が大きく変わる。
    assert_eq!(
        val(&core, "SELECT var_pop(k) FROM t"),
        num("6.8163265306122449")
    );
    assert_eq!(
        val(&core, "SELECT var_samp(k) FROM t WHERE g = 'a'"),
        num("12.9166666666666667")
    );
    // tenant-b は自テナントの Private 行（k = 100000）が加わるため結果が変わる。
    // 行を持たない tenant-c は tenant-a と同じ Public 行だけを見る。
    assert_ne!(
        one(&core, "tenant-b", "SELECT var_pop(k) FROM t"),
        val(&core, "SELECT var_pop(k) FROM t")
    );
    assert_eq!(
        one(&core, "tenant-c", "SELECT var_pop(k) FROM t"),
        val(&core, "SELECT var_pop(k) FROM t")
    );
}
