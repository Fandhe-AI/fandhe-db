//! `trunc(x)`／`trunc(x, n)` 組み込み関数の結合テスト（Issue #1908、対象ビヘイビア:
//! SQL-65。ポインタ: `docs/spec/04-behavior/` 配下）。
//!
//! 0 方向への切り捨て（負の桁数含む）・WHERE/ORDER BY での利用・
//! arity `42883`・予約名 `42723`・型不一致 `42804`・桁数 `22000`/`22003`・
//! RLS 境界の維持を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("label", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    for (tenant, id, label) in [
        ("tenant-a", 1u64, Some("abc")),
        ("tenant-a", 2, Some("def")),
        // 別テナントの行は tenant-a から不可視。
        ("tenant-b", 5, Some("abc")),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}")).expect("op");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            if tenant == "tenant-b" {
                Visibility::Private
            } else {
                Visibility::Public
            },
            &[
                Value::Vector(vec![0.1, 0.2]),
                label.map_or(Value::Null, |s| Value::Text(s.to_string())),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn code_of(core: &EngineCore, sql: &str) -> String {
    core.execute_sql(&ctx_for("tenant-a"), sql)
        .expect_err("expected error")
        .wire_code()
        .to_string()
}

fn floats(core: &EngineCore, sql: &str) -> Vec<Cell> {
    let r = core
        .execute_sql(&ctx_for("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    r.rows[0].cells.clone()
}

#[test]
fn projection_truncates_toward_zero() {
    let path = unique_db_path("sql65-trunc-proj");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let cells = floats(
        &core,
        "SELECT trunc(42.8), trunc(-42.8), trunc(42.4382, 2), trunc(1234.5678, -2), \
         trunc(-1234.5678, -2), trunc(2.675, 2), trunc(99.99, 1) FROM docs LIMIT 1",
    );
    let want = [42.0, -42.0, 42.43, 1200.0, -1200.0, 2.67, 99.9];
    for (c, w) in cells.iter().zip(want) {
        assert_eq!(c, &Cell::Float(w));
    }
    assert_eq!(cells.len(), want.len());
}

#[test]
fn where_and_order_by_use_trunc_and_respect_rls() {
    let path = unique_db_path("sql65-trunc-where");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, trunc(id * 1.5) FROM docs WHERE trunc(id * 1.5) = 1 \
             ORDER BY trunc(id * 1.5) LIMIT 10",
        )
        .expect("where trunc");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[1], Cell::Float(1.0));
    // tenant-b の行（id=5）は tenant-a から不可視。
    let r = core
        .execute_sql(&ctx, "SELECT trunc(id) FROM docs WHERE id = 5 LIMIT 10")
        .expect("zero rows");
    assert!(r.rows.is_empty());
}

#[test]
fn contract_errors() {
    let path = unique_db_path("sql65-trunc-err");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    assert_eq!(code_of(&core, "SELECT trunc() FROM docs LIMIT 1"), "42883");
    assert_eq!(
        code_of(&core, "SELECT trunc(1.5, 1, 2) FROM docs LIMIT 1"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT trunc(embedding) FROM docs LIMIT 1"),
        "42804"
    );
    assert_eq!(
        code_of(&core, "SELECT trunc(label) FROM docs LIMIT 1"),
        "42804"
    );
    assert_eq!(
        code_of(&core, "SELECT trunc(1.5, 0.5) FROM docs LIMIT 1"),
        "22000"
    );
    assert_eq!(
        code_of(&core, "SELECT trunc(1.5, 3000000000) FROM docs LIMIT 1"),
        "22003"
    );
    let mut session = engine::sql::mode::SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx_for("tenant-a"),
            &mut session,
            "CREATE FUNCTION trunc(x) AS x",
        )
        .expect_err("trunc is reserved");
    assert_eq!(err.wire_code(), "42723");
}
