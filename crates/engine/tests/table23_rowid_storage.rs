//! 行ストアの物理 rowid（TABLE-23・Issue #2193。設計記録:
//! `docs/design/rowid-primary-key-separation.md`）の非露出と既存 DB 互換の結合テスト。
//!
//! 役割: 物理キーを非公開の rowid として扱う内部変更が、SQL 応答・EXPLAIN・エラー文面に
//! rowid（`ctid`／`rowid` 疑似列）を一切出さないこと、および従来方式の既存表が変換なしで
//! 同じ値のまま読めることを公開 API から固定する。採番器・ゲート・カタログ属性の単体検証は
//! `src/catalog/rowid_alloc.rs`・`src/catalog.rs`・`src/catalog/rowid_gate_tests.rs` が持つ
//! （採番方式の表を作る手段は crate 内部に限るため）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::Storage;
use engine::storage::Visibility;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public]).expect("valid tenant")
}

fn core_with_rows(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("kind", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let c = ctx("tenant-a");
    for i in [3u64, 10, 4_000_000_000_000] {
        let op_id = OperationId::parse(&format!("seed-{i}")).expect("op id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &c,
            i,
            Visibility::Public,
            &[
                Value::Vector(vec![i as f32, 0.0, 0.0, 0.0]),
                Value::Text("a".to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .map_err(|e| format!("{}|{e}|{e:?}", e.wire_code()))
}

/// TABLE-23: `ctid`／`rowid` は疑似列として存在せず、参照は列不在（`42703`）で拒否される。
#[test]
fn table23_rowid_pseudo_columns_are_not_exposed() {
    let (core, _guard) = core_with_rows("table23-no-pseudo-col");
    for sql in [
        "SELECT ctid FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
        "SELECT rowid FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
        "SELECT id FROM docs WHERE ctid = 1 ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
        "SELECT id FROM docs WHERE rowid = 1 ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
    ] {
        let err = run(&core, sql).expect_err(sql);
        assert!(err.starts_with("42703|"), "{sql}: {err}");
    }
}

/// TABLE-23: `SELECT *`／`id` の応答は従来どおり利用者の `id` 列値で、列名にも rowid 系は無い。
#[test]
fn table23_select_star_returns_legacy_id_values_without_rowid_column() {
    let (core, _guard) = core_with_rows("table23-select-star");
    let out = run(
        &core,
        "SELECT * FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
    )
    .expect("select");
    let SqlOutcome::Query(result) = out else {
        panic!("expected rows");
    };
    for col in &result.columns {
        let name = format!("{col:?}").to_ascii_lowercase();
        assert!(!name.contains("rowid") && !name.contains("ctid"), "{name}");
    }
    let mut ids: Vec<String> = result
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect();
    ids.sort();
    assert_eq!(ids.len(), 3);
    let out = run(
        &core,
        "SELECT id FROM docs WHERE id = 4000000000000 ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
    )
    .expect("select id");
    let SqlOutcome::Query(result) = out else {
        panic!("expected rows");
    };
    assert_eq!(result.rows.len(), 1);
    assert!(matches!(
        result.rows[0].cells[0],
        Cell::Integer(4_000_000_000_000)
    ));
}

/// TABLE-23: `EXPLAIN` の出力に rowid／ctid の語は現れない。
#[test]
fn table23_explain_output_does_not_mention_rowid() {
    let (core, _guard) = core_with_rows("table23-explain");
    for sql in [
        "EXPLAIN SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[0,0,0,0]' LIMIT 2",
        "EXPLAIN SELECT * FROM docs ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
        "EXPLAIN SELECT id FROM docs WHERE id = 10 ORDER BY embedding <=> '[0,0,0,0]' LIMIT 10",
    ] {
        let SqlOutcome::Explain(result) = run(&core, sql).expect(sql) else {
            panic!("expected explain for {sql}");
        };
        for row in &result.rows {
            let text = format!("{:?}", row.cells).to_ascii_lowercase();
            assert!(!text.contains("rowid") && !text.contains("ctid"), "{text}");
        }
    }
}

/// TABLE-23: 既存 DB 互換。再オープンしても全行が同じ `id` 値のまま読める（変換不要）。
#[test]
fn table23_existing_rows_are_readable_after_reopen_with_same_ids() {
    let path = unique_db_path("table23-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("kind", ColumnType::Text, false)],
            ))
            .expect("create table");
        let c = ctx("tenant-a");
        for i in [1u64, 2, 99] {
            let op_id = OperationId::parse(&format!("seed-{i}")).expect("op id");
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &c,
                i,
                Visibility::Public,
                &[Value::Text(format!("k{i}"))],
                &op_id,
            )
            .expect("insert row");
        }
    }
    let storage = Storage::open(&path).expect("reopen");
    for i in [1u64, 2, 99] {
        let row = storage
            .get_row_from_table("docs", "tenant-a", i)
            .expect("row readable");
        assert_eq!(row.id, i);
    }
    let missing = storage
        .get_row_from_table("docs", "tenant-a", 3)
        .expect_err("absent id");
    assert!(missing.to_string().contains('3'), "{missing}");
}
