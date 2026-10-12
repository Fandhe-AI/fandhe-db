//! FROM ありの SELECT／DML が存在しない列を参照したときの分類（Issue #2332。
//! ポインタ: `docs/spec/04-behavior/` ERR-2・ERR-6、SQL 表層のビヘイビア ID 群）の
//! 結合テスト。
//!
//! 従来は束縛箇所ごとに `22000`（InvalidInput）で拒否していた未定義列を、
//! `SqlSurfaceError::UndefinedColumn`（`42703`）へ揃えた。`EngineCore::execute_sql_in_session`
//! （production 経路）で、投影・WHERE・ORDER BY・GROUP BY・INSERT 列リスト・UPDATE SET・
//! DELETE WHERE・JOIN・IN サブクエリ外側列が同じ分類になること、拒否時に副作用が無いこと、
//! テナントが違っても応答（`wire_code`・文言）が同一であること、型不正（VECTOR 列の
//! GROUP BY）は従来どおり `22000` のままであることを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql-undefined-column-42703");
    let storage = Storage::open(&path).expect("open storage");
    for (name, cols) in [
        (
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("author_id", ColumnType::Text, false),
            ],
        ),
        (
            "authors",
            vec![ColumnDef::new("name", ColumnType::Text, false)],
        ),
    ] {
        storage
            .create_table(&TableSchema::new(name, cols))
            .expect("create table");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<engine::sql::SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx_for(tenant), &mut SessionState::default(), sql)
}

fn count_docs(core: &EngineCore, tenant: &str) -> u64 {
    let result = core
        .execute_sql(&ctx_for(tenant), "SELECT COUNT(*) FROM docs")
        .expect("count(*) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn seed(core: &EngineCore, tenant: &str) {
    run(
        core,
        tenant,
        "INSERT INTO docs (id, embedding, lang, author_id) VALUES (1, '[0.1,0.2]', 'ja', 'a') \
         USING OPERATION_ID 'seed-1'",
    )
    .expect("seed insert");
}

/// 受け入れ条件の全経路（SELECT の投影・WHERE・ORDER BY・GROUP BY、INSERT の列リスト、
/// UPDATE の SET、DELETE の WHERE、JOIN の修飾付き列、IN サブクエリの外側列）が
/// `42703` かつ `UndefinedColumn` で拒否され、書き込み系は副作用を残さない。
#[test]
fn undefined_column_is_42703_on_every_from_path_without_side_effects() {
    let (core, _guard) = new_core();
    seed(&core, "tenant-a");
    let before = count_docs(&core, "tenant-a");

    for sql in [
        "SELECT ghost FROM docs LIMIT 10",
        "SELECT id FROM docs WHERE ghost = 'x' LIMIT 10",
        "SELECT id FROM docs ORDER BY ghost LIMIT 10",
        "SELECT ghost, COUNT(*) FROM docs GROUP BY ghost",
        "INSERT INTO docs (id, embedding, ghost) VALUES (2, '[0.1,0.2]', 'x') \
         USING OPERATION_ID 'op-ins'",
        "UPDATE docs SET ghost = 'x' WHERE id = 1 USING OPERATION_ID 'op-upd'",
        "DELETE FROM docs WHERE ghost = 'x' USING OPERATION_ID 'op-del'",
        "SELECT docs.ghost FROM docs JOIN authors ON docs.id = authors.id LIMIT 10",
        "SELECT id FROM docs WHERE ghost IN (SELECT name FROM authors LIMIT 10) LIMIT 10",
    ] {
        let err = run(&core, "tenant-a", sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42703", "sql={sql} err={err:?}");
        assert!(
            matches!(err, SqlSurfaceError::UndefinedColumn { .. }),
            "sql={sql} err={err:?}"
        );
    }
    assert_eq!(count_docs(&core, "tenant-a"), before, "no side effects");
}

/// 文言はクライアントが指定した識別子だけを含み、他テナントの有無で応答が変わらない
/// （テナント境界: 列の存在はスキーマだけで決まり行データに依存しない）。
#[test]
fn undefined_column_response_is_identical_across_tenants() {
    let (core, _guard) = new_core();
    seed(&core, "tenant-a");
    let sql = "SELECT ghost FROM docs LIMIT 10";
    let a = run(&core, "tenant-a", sql).expect_err("tenant-a");
    let b = run(&core, "tenant-b", sql).expect_err("tenant-b (no rows)");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(a.to_string(), b.to_string());
    assert_eq!(a.to_string(), "column does not exist: ghost");
}

/// 型不正（並べ替え不能な VECTOR 列の GROUP BY）は未定義列ではないため `22000` のまま。
#[test]
fn unorderable_group_by_type_stays_22000() {
    let (core, _guard) = new_core();
    seed(&core, "tenant-a");
    let err = run(
        &core,
        "tenant-a",
        "SELECT embedding, COUNT(*) FROM docs GROUP BY embedding",
    )
    .expect_err("vector group by");
    assert_eq!(err.wire_code(), "22000", "err={err:?}");
}
