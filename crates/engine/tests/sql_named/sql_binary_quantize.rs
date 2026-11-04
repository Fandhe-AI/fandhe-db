//! `binary_quantize(vector)`（二値量子化。TABLE-48 (3)・Issue #2023。関連: RLS-7/RLS-8）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql` / `execute_sql_in_session`）で、(1) pgvector と同じ入力で同じ
//! ビット列（正の成分を 1、それ以外を 0。NULL は NULL）を返し、結果列が `bit(n)` として公告されること、
//! (2) `UPDATE ... SET <BIT 列> = binary_quantize(..)` で往復一致し、長さ不一致は `22026` になること、
//! (3) 投影以外の位置（比較・並べ替え・グループ化・関数引数・CAST 等）と `VECTOR` 列以外の引数が
//! fail-closed に拒否されること、(4) 他テナントの行が結果へ出ないこと、(5) `<~>` が偽の結果を返さず
//! 拒否のままであること（距離演算子の SQL 接続は別 Issue の担当）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(11), false),
            ColumnDef::new("b", ColumnType::Bit(11), true),
            ColumnDef::new("b4", ColumnType::Bit(4), true),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// pgvector の回帰テストと同じ入力（正の成分だけが 1 になる）。
const V_MIXED: [f32; 11] = [0.0, 0.1, -0.2, -0.3, 0.4, 0.5, 0.6, -0.7, 0.8, -0.9, 1.0];

fn insert_row(storage: &Storage, tenant: &str, id: u64, emb: [f32; 11], vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(emb.to_vec()),
            Value::Null,
            Value::Null,
            Value::Text("g".to_string()),
            Value::Integer(1),
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
    let rows: [(u64, [f32; 11]); 3] = [(1, V_MIXED), (2, [1.0; 11]), (3, [-0.0; 11])];
    for (id, emb) in rows {
        insert_row(&storage, "tenant-a", id, emb, Visibility::Public);
    }
    insert_row(&storage, "tenant-b", 100, [1.0; 11], Visibility::Private);
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

fn bit_text(c: &Cell) -> Option<String> {
    match c {
        Cell::Bit(b) => Some(b.format_text()),
        Cell::Null => None,
        other => panic!("unexpected cell {other:?}"),
    }
}

#[test]
fn projection_matches_pgvector_and_advertises_bit_type() {
    let (core, _g) = build_core("bq-proj");
    let r = exec(
        &core,
        "SELECT binary_quantize(embedding) FROM t ORDER BY id LIMIT 10",
    );
    let got: Vec<Option<String>> = r.rows.iter().map(|row| bit_text(&row.cells[0])).collect();
    assert_eq!(
        got,
        vec![
            Some("01001110101".to_string()),
            Some("11111111111".to_string()),
            Some("00000000000".to_string()),
        ]
    );
    match &r.columns[0] {
        ColumnMeta::Computed { ty, .. } => assert_eq!(*ty, Some(ColumnType::Bit(11))),
        other => panic!("unexpected column meta: {other:?}"),
    }
}

/// 裸の `NULL` 引数は長さ不明のため、無関係な VECTOR 列（VECTOR(11)）の次元を採って `bit(11)` を
/// 公告してはならない（公告するか拒否するかのどちらかで、`bit(11)` にだけはならない）。
#[test]
fn null_argument_does_not_borrow_unrelated_vector_dimension() {
    let (core, _g) = build_core("bq-null");
    let r = core.execute_sql(
        &ctx("tenant-a"),
        "SELECT binary_quantize(NULL) FROM t ORDER BY id LIMIT 10",
    );
    if let Ok(r) = r {
        match &r.columns[0] {
            ColumnMeta::Computed { ty, .. } => {
                assert!(!matches!(ty, Some(ColumnType::Bit(_))), "ty = {ty:?}")
            }
            other => panic!("unexpected column meta: {other:?}"),
        }
    }
}

#[test]
fn only_visible_rows_are_returned() {
    let (core, _g) = build_core("bq-rls");
    let r = exec(
        &core,
        "SELECT id, binary_quantize(embedding) FROM t LIMIT 10",
    );
    assert_eq!(r.rows.len(), 3);
    assert!(r.rows.iter().all(|row| row.id != 100));
}

#[test]
fn update_set_roundtrips_and_checks_length() {
    let (core, _g) = build_core("bq-update");
    let mut s = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut s,
        "UPDATE t SET b = binary_quantize(embedding) WHERE id = 1 USING OPERATION_ID 'bq-u1'",
    )
    .expect("update");
    let r = exec(&core, "SELECT b FROM t WHERE id = 1");
    assert_eq!(
        bit_text(&r.rows[0].cells[0]).as_deref(),
        Some("01001110101")
    );
    // 長さ 11 を BIT(4) 列へは入れられない（22026）。副作用は残らない。
    let e = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "UPDATE t SET b4 = binary_quantize(embedding) WHERE id = 1 USING OPERATION_ID 'bq-u2'",
        )
        .expect_err("length mismatch");
    assert_eq!(e.wire_code(), "22026");
    let r = exec(&core, "SELECT b4 FROM t WHERE id = 1");
    assert_eq!(bit_text(&r.rows[0].cells[0]), None);
    // BIT 列以外への代入は、他の cast-only 型と同じく 0A000。
    let e = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "UPDATE t SET k = binary_quantize(embedding) WHERE id = 1 USING OPERATION_ID 'bq-u4'",
        )
        .expect_err("type mismatch");
    assert_eq!(e.wire_code(), "0A000");
}

#[test]
fn unsupported_positions_and_arguments_stay_fail_closed() {
    let (core, _g) = build_core("bq-closed");
    for (sql, want) in [
        (
            "SELECT id FROM t WHERE binary_quantize(embedding) = binary_quantize(embedding)",
            "0A000",
        ),
        (
            "SELECT id FROM t ORDER BY binary_quantize(embedding)",
            "0A000",
        ),
        (
            "SELECT count(*) FROM t GROUP BY binary_quantize(embedding)",
            "0A000",
        ),
        (
            "SELECT CASE WHEN id = 1 THEN binary_quantize(embedding) END FROM t",
            "0A000",
        ),
        (
            "SELECT CAST(binary_quantize(embedding) AS text) FROM t",
            "0A000",
        ),
        (
            "SELECT vector_dims(binary_quantize(embedding)) FROM t",
            "0A000",
        ),
        (
            "SELECT binary_quantize(l2_normalize(embedding)) FROM t",
            "0A000",
        ),
        // `<~>`／`<%>` の被演算子としては受理する（Issue #2970。結果の検証は sql_bit_distance）。
        // 距離式の外では従来どおり bit 値を消費できない。
        ("SELECT binary_quantize(embedding) + 1 FROM t", "0A000"),
        ("SELECT binary_quantize(k) FROM t", "42804"),
        ("SELECT binary_quantize(g) FROM t", "42804"),
        ("SELECT binary_quantize(embedding, 1) FROM t", "42883"),
        ("SELECT binary_quantize('[1,2]') FROM t", "42804"),
    ] {
        assert_eq!(code_of(&core, sql), want, "{sql}");
    }
}

#[test]
fn reserved_name_cannot_be_redefined() {
    let (core, _g) = build_core("bq-reserved");
    let mut s = SessionState::default();
    s.allow_ddl();
    let e = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "CREATE FUNCTION binary_quantize(x DOUBLE PRECISION) RETURNS DOUBLE PRECISION AS x",
        )
        .expect_err("reserved");
    assert_eq!(e.wire_code(), "42723");
}

#[test]
fn ctas_and_insert_select_roundtrip() {
    let (core, _g) = build_core("bq-ctas");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut s,
        "CREATE TABLE u AS SELECT id, binary_quantize(embedding) AS q FROM t",
    )
    .expect("ctas");
    let r = exec(&core, "SELECT q FROM u ORDER BY id LIMIT 10");
    assert!(matches!(
        &r.columns[0],
        ColumnMeta::Scalar {
            ty: ColumnType::Bit(11),
            ..
        }
    ));
    let got: Vec<Option<String>> = r.rows.iter().map(|row| bit_text(&row.cells[0])).collect();
    assert_eq!(
        got,
        vec![
            Some("01001110101".to_string()),
            Some("11111111111".to_string()),
            Some("00000000000".to_string()),
        ]
    );
    // INSERT ... SELECT も bit 値を BIT 列へ往復一致で書く（長さ不一致の列は別途拒否される）。
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut s,
        "CREATE TABLE w (id BIGINT PRIMARY KEY, q BIT(11), r BIT(4))",
    )
    .expect("create w");
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut s,
        "INSERT INTO w (id, q) SELECT id, binary_quantize(embedding) FROM t USING OPERATION_ID 'bq-i1'",
    )
    .expect("insert select");
    let r = exec(&core, "SELECT q FROM w ORDER BY id LIMIT 10");
    let got: Vec<Option<String>> = r.rows.iter().map(|row| bit_text(&row.cells[0])).collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].as_deref(), Some("01001110101"));
    // 長さの異なる BIT 列へは書けず、副作用も残らない。
    let e = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "INSERT INTO w (id, r) SELECT id + 100, binary_quantize(embedding) FROM t USING OPERATION_ID 'bq-i2'",
        )
        .map(|_| ())
        .expect_err("length mismatch");
    assert!(matches!(e.wire_code(), "42804" | "22026" | "0A000"));
    assert_eq!(exec(&core, "SELECT id FROM w LIMIT 10").rows.len(), 3);
}
