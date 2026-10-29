//! `md5(text)` 組み込み関数の結合テスト（Issue #1905、対象ビヘイビア: SQL-65・SQL-26。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`・`docs/spec/05-tasks.md` TASK-308）。
//!
//! 32 文字の小文字 16 進・strict（NULL 伝播）・WHERE/ORDER BY での利用・arity `42883`・
//! 予約名 `42723`・型不一致 `42804`・RLS 境界の維持を固定する。

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
const ABC: &str = "900150983cd24fb0d6963f7d28e17f72";
const MSG: &str = "f96b697d7cb7938d525a2f31aaf161d0";
const JA: &str = "291a033f45232d75458716bcc03d90a0";

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
        ("tenant-a", 2, Some("message digest")),
        ("tenant-a", 3, Some("日本語テキスト")),
        ("tenant-a", 4, None),
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

#[test]
fn projection_is_lower_hex_and_strict() {
    let path = unique_db_path("sql65-md5-proj");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let r = core
        .execute_sql(
            &ctx_for("tenant-a"),
            "SELECT id, md5(label) FROM docs ORDER BY id LIMIT 10",
        )
        .expect("md5 projection");
    let got: Vec<&Cell> = r.rows.iter().map(|row| &row.cells[1]).collect();
    assert_eq!(
        got,
        vec![
            &Cell::Text(ABC.into()),
            &Cell::Text(MSG.into()),
            &Cell::Text(JA.into()),
            &Cell::Null,
        ]
    );
    for s in got.iter().filter_map(|c| match c {
        Cell::Text(s) => Some(s),
        _ => None,
    }) {
        assert_eq!(s.len(), 32);
        assert!(s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
}

#[test]
fn where_and_order_by_use_md5() {
    let path = unique_db_path("sql65-md5-where");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            &format!("SELECT id FROM docs WHERE md5(label) = '{ABC}' LIMIT 10"),
        )
        .expect("where md5");
    // tenant-b の同値行は RLS で不可視。
    assert_eq!(r.rows.len(), 1);
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE md5(label) <> 'zz' ORDER BY md5(label) LIMIT 10",
        )
        .expect("order by md5");
    assert_eq!(r.rows.len(), 3);
    // 可視行 0 件でもエラーにならない。
    let r = core
        .execute_sql(&ctx, "SELECT md5(label) FROM docs WHERE id = 5 LIMIT 10")
        .expect("zero rows");
    assert!(r.rows.is_empty());
}

#[test]
fn contract_errors() {
    let path = unique_db_path("sql65-md5-err");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    assert_eq!(code_of(&core, "SELECT md5() FROM docs LIMIT 1"), "42883");
    assert_eq!(
        code_of(&core, "SELECT md5(label, label) FROM docs LIMIT 1"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT md5(vec_norm(embedding)) FROM docs LIMIT 1"),
        "42804"
    );
    let mut session = engine::sql::mode::SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx_for("tenant-a"),
            &mut session,
            "CREATE FUNCTION md5(x) AS x",
        )
        .expect_err("md5 is reserved");
    assert_eq!(err.wire_code(), "42723");
}
