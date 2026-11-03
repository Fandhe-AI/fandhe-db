//! 距離演算子 4 種の kNN `ORDER BY` 受け入れテスト（Issue #1977。ビヘイビア: SQL-33 (2)(4)(5)・
//! WIRE-12・SQL-74・SQL-75 (4)。ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 固定する契約:
//! - `<=>`（コサイン距離）と `<#>`（負の内積）の kNN は行を返す（Issue #1991 で接続済み）。
//! - `<->`／`<+>` は構文・束縛を通り、実行段で `0A000`（カーネル未接続の fail-closed）。
//! - バインド時の型エラーの判定順序・コードは `EXPLAIN` の有無で変わらない。
//! - `0A000` の応答は固定文言で、他テナントの情報を含まない。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// SQL 実行経路に未接続の演算子（Issue #1991 時点で L2／L1）。
const OPS: [&str; 2] = ["<->", "<+>"];
/// バインド時の検査が 4 種で同じであることを確かめる全演算子。
const ALL_OPS: [&str; 4] = ["<=>", "<->", "<#>", "<+>"];

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    for (tenant, id, v) in [
        ("tenant-a", 1, "[1,0,0]"),
        ("tenant-a", 2, "[0,1,0]"),
        ("tenant-b", 3, "[1,1,0]"),
    ] {
        let mut session = SessionState::default();
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({id}, '{v}', 'secret-{tenant}') \
             USING OPERATION_ID 'seed-{id}'"
        );
        core.execute_sql_in_session(&ctx(tenant), &mut session, &sql)
            .unwrap_or_else(|e| panic!("seed {sql}: {e:?}"));
    }
    core
}

/// 実行結果を (wire_code, メッセージ) か成功の行 id 列で返す。
fn outcome(core: &EngineCore, tenant: &str, sql: &str) -> Result<usize, (String, String)> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(SqlOutcome::Query(r)) => Ok(r.rows.len()),
        Ok(SqlOutcome::Explain(r)) => Ok(r.rows.len()),
        Ok(other) => panic!("unexpected outcome for {sql}: {other:?}"),
        Err(e) => Err((e.wire_code().to_string(), e.to_string())),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    outcome(core, "tenant-a", sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} must fail"))
        .0
}

#[test]
fn sql33_cosine_knn_is_unchanged() {
    let path = unique_db_path("sql33-kind-cosine");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let n = outcome(
        &core,
        "tenant-a",
        "SELECT id FROM documents ORDER BY embedding <=> '[1,0,0]' LIMIT 5",
    )
    .expect("cosine runs");
    assert_eq!(n, 2);
}

#[test]
fn sql33_negative_inner_product_knn_runs() {
    let path = unique_db_path("sql33-kind-negip");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    for tail in ["LIMIT 3", "DESC LIMIT 3", "LIMIT 3 OFFSET 1", "LIMIT 5"] {
        let sql = format!("SELECT id FROM documents ORDER BY embedding <#> '[1,0,0]' {tail}");
        assert!(outcome(&core, "tenant-a", &sql).is_ok(), "{sql}");
    }
}

#[test]
fn sql33_unconnected_knn_binds_but_execution_is_0a000() {
    let path = unique_db_path("sql33-kind-exec");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    for op in OPS {
        for tail in ["LIMIT 3", "DESC LIMIT 3", "LIMIT 3 OFFSET 1"] {
            let sql = format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' {tail}");
            assert_eq!(code(&core, &sql), "0A000", "{sql}");
        }
        // 空表同然（他テナントしか行を持たない）でも同じコード・同じ文言（存在情報を漏らさない）。
        let sql = format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3");
        let a = outcome(&core, "tenant-a", &sql).expect_err("a");
        let c = outcome(&core, "tenant-none", &sql).expect_err("none");
        assert_eq!(a, c, "{op}");
        assert!(!a.1.contains("secret"), "{op}: {}", a.1);
    }
}

#[test]
fn sql33_bind_errors_are_identical_with_and_without_explain() {
    let path = unique_db_path("sql33-kind-explain");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    for op in ALL_OPS {
        for tail in [
            "ORDER BY embedding {op} '[1,0]' LIMIT 3",
            "ORDER BY nope {op} '[1,0,0]' LIMIT 3",
            "ORDER BY body {op} '[1,0,0]' LIMIT 3",
            "ORDER BY embedding {op} '[1,0,0]' LIMIT 0",
            "ORDER BY embedding {op} '[1,0,0]' DESC LIMIT 3 USING MODE 'precision'",
        ] {
            let tail = tail.replace("{op}", op);
            let plain = code(&core, &format!("SELECT id FROM documents {tail}"));
            let explain = code(&core, &format!("EXPLAIN SELECT id FROM documents {tail}"));
            assert_eq!(plain, explain, "{op} {tail}");
            assert_ne!(plain, "42601", "{op} {tail}");
        }
        // 正しい文: EXPLAIN は成功し、実行は未接続の演算子だけ 0A000。
        let ok = format!("SELECT id FROM documents ORDER BY embedding {op} '[1,0,0]' LIMIT 3");
        assert!(
            outcome(&core, "tenant-a", &format!("EXPLAIN {ok}")).is_ok(),
            "{op}"
        );
        if OPS.contains(&op) {
            assert_eq!(code(&core, &ok), "0A000", "{op}");
        } else {
            assert!(outcome(&core, "tenant-a", &ok).is_ok(), "{op}");
        }
    }
}
