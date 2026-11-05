//! `EngineCore::execute_bound_upsert_in_session`（NoSQL 表層の `upsert` op が呼ぶ束縛済み UPSERT の
//! セッション入口。Issue #2087・NOSQL-21 ポインタ）が engine クレート外から到達可能であることと、
//! 判定順（`23502` → `22000` → `42P01` → 束縛結果の整合）・SQL 表層の UPSERT との件数一致を
//! 固定する結合テスト。`sql_insert_batch_public_api.rs` と同じ流儀。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::{
    ConflictTargetSpec, InsertLiteral, OnConflictAction, OnConflictClause, SqlSurfaceError,
    UpsertValue, ValidatedInsert,
};
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind_insert_form, BoundInsertForm, BoundUpsert};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(TABLE, vec![ColumnDef::new("lang", ColumnType::Text, false)])
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn new_core(label: &str, mode: Option<LedgerMode>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(mode) = mode {
        core = core.with_ledger_mode(mode);
    }
    (core, guard)
}

fn validated(ids: &[u64], do_update: bool, op: Option<&str>) -> ValidatedInsert {
    ValidatedInsert {
        table_name: TABLE.to_string(),
        columns: vec!["id".to_string(), "lang".to_string()],
        columns_omitted: false,
        rows: ids
            .iter()
            .map(|id| {
                vec![
                    InsertLiteral::Number(id.to_string()),
                    InsertLiteral::String("ja".to_string()),
                ]
            })
            .collect(),
        operation_id: op.map(|s| OperationId::parse(s).expect("valid operation_id")),
        returning: None,
        on_conflict: Some(OnConflictClause {
            target: ConflictTargetSpec::Columns(vec!["id".to_string()]),
            action: if do_update {
                OnConflictAction::DoUpdate {
                    assignments: vec![(
                        "lang".to_string(),
                        UpsertValue::Excluded("lang".to_string()),
                    )],
                    condition: Vec::new(),
                }
            } else {
                OnConflictAction::DoNothing
            },
        }),
    }
}

fn bind(stmt: &ValidatedInsert, schema: &TableSchema) -> Result<BoundUpsert, SqlSurfaceError> {
    match bind_insert_form(stmt, schema)? {
        BoundInsertForm::Upsert(b) => Ok(b),
        _ => panic!("expected Upsert form"),
    }
}

fn run(
    core: &EngineCore,
    stmt: &ValidatedInsert,
    table: &str,
    row_count: usize,
    op: Option<&OperationId>,
) -> Result<u64, SqlSurfaceError> {
    core.execute_bound_upsert_in_session(
        &ctx(),
        &SessionState::default(),
        table,
        row_count,
        op,
        |schema| bind(stmt, schema),
    )
    .map(|o| o.rows_affected)
}

fn sql_count(core: &EngineCore, text: &str) -> u64 {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(), &mut session, text)
        .expect("sql ok")
    {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("unexpected outcome {other:?}"),
    }
}

#[test]
fn counts_match_the_sql_surface_for_new_conflicting_and_mixed_rows() {
    let (core, _g) = new_core("upsert-bound-count", None);
    let (sql_core, _g2) = new_core("upsert-bound-count-sql", None);
    assert_eq!(
        run(&core, &validated(&[1, 2], true, None), TABLE, 2, None).unwrap(),
        sql_count(
            &sql_core,
            "INSERT INTO docs (id, lang) VALUES (1, 'ja'), (2, 'ja') \
             ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang"
        )
    );
    // 衝突 + 新規の混在（DO UPDATE は更新も数える）。
    assert_eq!(
        run(&core, &validated(&[2, 3], true, None), TABLE, 2, None).unwrap(),
        sql_count(
            &sql_core,
            "INSERT INTO docs (id, lang) VALUES (2, 'ja'), (3, 'ja') \
             ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang"
        )
    );
    // DO NOTHING は飛ばした行を数えない。
    let nothing = run(&core, &validated(&[3, 4], false, None), TABLE, 2, None).unwrap();
    assert_eq!(nothing, 1);
    assert_eq!(
        nothing,
        sql_count(
            &sql_core,
            "INSERT INTO docs (id, lang) VALUES (3, 'ja'), (4, 'ja') ON CONFLICT (id) DO NOTHING"
        )
    );
}

#[test]
fn judgement_order_is_23502_then_22000_then_42p01() {
    let (core, _g) = new_core("upsert-bound-order", Some(LedgerMode::Ledgered));
    let stmt = validated(&[1], false, None);
    // operation_id 必須構成で省略 -> 23502（行数・カタログより前）。
    let err = run(&core, &stmt, "missing", 0, None).unwrap_err();
    assert_eq!(err.wire_code(), "23502");

    let op = OperationId::parse("op-order").expect("valid operation_id");
    let stmt = validated(&[1], false, Some("op-order"));
    // 空バッチ -> 22000（存在しない表でもカタログ参照より前）。
    let err = run(&core, &stmt, "missing", 0, Some(&op)).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    // 存在しない表 -> 42P01。
    let err = run(&core, &stmt, "missing", 1, Some(&op)).unwrap_err();
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn bound_plan_mismatch_is_rejected_with_22000_and_writes_nothing() {
    let (core, _g) = new_core("upsert-bound-mismatch", None);
    // 行数の不一致（引数 2・束縛 1）。
    let err = run(&core, &validated(&[1], false, None), TABLE, 2, None).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    // operation_id の不一致（引数なし・束縛あり）。
    let err = run(&core, &validated(&[1], false, Some("op-x")), TABLE, 1, None).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    // 何も書かれていない: 同じ id が新規挿入として 1 件数えられる。
    assert_eq!(
        run(&core, &validated(&[1], false, None), TABLE, 1, None).unwrap(),
        1
    );
}
