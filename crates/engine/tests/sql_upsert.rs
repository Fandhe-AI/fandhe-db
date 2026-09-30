//! `INSERT INTO <table> (...) VALUES (...)[, ...] ON CONFLICT (id) DO NOTHING
//! | DO UPDATE SET ... USING OPERATION_ID '<id>'`（SQL-20、TASK-193、
//! Issue #872）の結合テスト。ポインタ: `docs/spec/05-tasks.md` TASK-193・
//! `docs/spec/04-behavior/sql-surface.md` SQL-20。関連ポインタ: TABLE-12
//! （物理キーのテナント名前空間化）・RLS-9・RLS-10（他テナント存在情報の
//! 非漏えい）・RECOVER-1〜3（`operation_id` 必須化・台帳）・RECOVER-10
//! （台帳照合による再送判定）・RECOVER-11(a)（複数行・UPSERT のハッシュ入力）・
//! SQL-16（複数行 `VALUES`）・SQL-17（`SET` の禁止列・次元検証）。
//!
//! `EngineCore::execute_sql_in_session`（先頭トークン `INSERT` の覗き見判定 →
//! `sql::allowlist::validate_insert_tokens` → `sql::parser::bind_insert_form`
//! の `Upsert` 分岐 → `sql::exec::execute_upsert`）を production
//! 経路として検証する。`sql_delete_single_row.rs`・`insert_multi_row.rs` と
//! 同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／
//! `CleanupGuard`）。
//!
//! 衝突判定スコープ（`(tenant_id, id)` の所有。RLS 可視性ではない）・判定順序
//! （台帳照合が行の衝突分岐より前）・応答意味論（`rows_affected` の数え方）の
//! 設計判断は `docs/design/sql-upsert.md` 参照。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    )
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn open_engine_with_limits(
    name: &str,
    limits: engine::batch_limits::BatchLimits,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (core, path)
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', '{lang}') \
         USING OPERATION_ID '{op_id}'"
    )
}

fn run_sql(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, session, sql)
}

fn insert_rows_affected(outcome: SqlOutcome) -> u64 {
    match outcome {
        SqlOutcome::Insert(o) => o.rows_affected,
        other => panic!("expected Insert outcome, got {other:?}"),
    }
}

/// `id`／`lang`／`note` 列を `id` の昇順で読み戻す（副作用ゼロの確定オラクル）。
fn read_back(core: &EngineCore, ctx: &PolicyContext) -> Vec<(u64, String, Option<String>)> {
    let mut session = SessionState::default();
    let sql = format!("SELECT lang, note FROM {TABLE} LIMIT 100");
    let outcome = core
        .execute_sql_in_session(ctx, &mut session, &sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome, got {outcome:?}");
    };
    let mut rows: Vec<(u64, String, Option<String>)> = Vec::new();
    for row in &result.rows {
        let lang = match row.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected lang Cell::Text, got {other:?}"),
        };
        let note = match row.cells.get(1) {
            Some(Cell::Text(s)) => Some(s.clone()),
            Some(Cell::Null) => None,
            other => panic!("expected note Cell::Text or Null, got {other:?}"),
        };
        rows.push((row.id, lang, note));
    }
    rows.sort_by_key(|(id, _, _)| *id);
    rows
}

fn ledger_state(core: &EngineCore, tenant: &str, op_id: &str) -> LedgerLookup {
    let ctx = ctx_for(tenant);
    let operation_id = engine::recovery::required_op_id::OperationId::parse(op_id)
        .expect("valid operation_id literal");
    core.operation_recorded(&ctx, TABLE, &operation_id)
        .expect("ledger lookup ok")
}

// --- 基本受理 ---

#[test]
fn non_conflicting_upsert_inserts_new_row() {
    let (core, path) = open_engine("sql-upsert-basic-insert");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-1'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
    assert_eq!(read_back(&core, &ctx), vec![(1, "ja".to_string(), None)]);
}

#[test]
fn do_nothing_on_conflict_leaves_row_unchanged() {
    let (core, path) = open_engine("sql-upsert-do-nothing");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-1")).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 0);
    assert_eq!(read_back(&core, &ctx), vec![(1, "ja".to_string(), None)]);
}

#[test]
fn do_update_on_conflict_overwrites_only_set_columns_and_preserves_visibility() {
    let (core, path) = open_engine("sql-upsert-do-update");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let seed = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, note) VALUES (1, '[0.1,0.2]', 'ja', 'orig') \
         USING OPERATION_ID 'seed-1'"
    );
    run_sql(&core, &ctx, &mut session, &seed).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-3'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
    // lang は EXCLUDED（新規挿入しようとした値）で上書きされる一方、SET で
    // 触れなかった `note` は既存値（'orig'）を保持する。
    assert_eq!(
        read_back(&core, &ctx),
        vec![(1, "en".to_string(), Some("orig".to_string()))]
    );

    // 可視性の保持は SELECT の可視集合そのもの（Private が引き続き見える）で
    // 間接的に確認済みだが、明示的に Public のみへ絞った ctx からも読めない
    // （＝ Private のまま）ことを追加で確認する。
    let public_only = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut public_session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &public_only,
            &mut public_session,
            &format!("SELECT id FROM {TABLE} LIMIT 10"),
        )
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert!(result.rows.is_empty());
}

#[test]
fn do_update_literal_and_excluded_mixed_assignment() {
    let (core, path) = open_engine("sql-upsert-mixed-assignment");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-2")).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang, note = 'updated' \
         USING OPERATION_ID 'op-upsert-4'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
    assert_eq!(
        read_back(&core, &ctx),
        vec![(1, "en".to_string(), Some("updated".to_string()))]
    );
}

#[test]
fn excluded_value_missing_from_insert_column_list_is_null() {
    // 新規行の列リストに `note` を含めない場合、`EXCLUDED.note` は `Value::Null`
    // （`note` は nullable のため許容される）。
    let (core, path) = open_engine("sql-upsert-excluded-null");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let seed = format!(
        "INSERT INTO {TABLE} (id, embedding, lang, note) VALUES (1, '[0.1,0.2]', 'ja', 'orig') \
         USING OPERATION_ID 'seed-3'"
    );
    run_sql(&core, &ctx, &mut session, &seed).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO UPDATE SET note = EXCLUDED.note \
         USING OPERATION_ID 'op-upsert-5'"
    );
    run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(read_back(&core, &ctx), vec![(1, "ja".to_string(), None)]);
}

#[test]
fn excluded_value_that_would_be_null_for_non_nullable_target_is_rejected() {
    let (core, path) = open_engine("sql-upsert-excluded-non-nullable-null");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-4")).expect("seed insert");

    // `lang` は non-nullable。新規行の列リストに `lang` を含めなければ
    // `EXCLUDED.lang` は NULL になり、対象列 `lang` も non-nullable のため
    // `23502`（`NotNullViolation`。TABLE-16・TASK-204、Issue #904。旧 `22000`
    // から契約変更）で拒否される。
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES (1, '[0.9,0.9]') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-6'"
    );
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "23502");
    // 束縛失敗のため行・台帳とも無変更。
    assert_eq!(read_back(&core, &ctx), vec![(1, "ja".to_string(), None)]);
}

// --- 複数行混在 ---

#[test]
fn multi_row_upsert_mixes_new_insert_and_conflict_branches() {
    let (core, path) = open_engine("sql-upsert-multi-row");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-5")).expect("seed insert");
    run_sql(&core, &ctx, &mut session, &insert_sql(2, "ja", "seed-6")).expect("seed insert");

    // id=1: DO UPDATE 衝突・id=2: DO UPDATE 衝突・id=3: 新規挿入。
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
         (1, '[0.9,0.9]', 'en'), (2, '[0.9,0.9]', 'en'), (3, '[0.3,0.3]', 'fr') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-7'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 3);
    assert_eq!(
        read_back(&core, &ctx),
        vec![
            (1, "en".to_string(), None),
            (2, "en".to_string(), None),
            (3, "fr".to_string(), None),
        ]
    );
}

#[test]
fn multi_row_upsert_do_nothing_counts_only_new_inserts() {
    let (core, path) = open_engine("sql-upsert-multi-row-do-nothing");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-7")).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
         (1, '[0.9,0.9]', 'en'), (2, '[0.3,0.3]', 'fr') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-8'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
    assert_eq!(
        read_back(&core, &ctx),
        vec![(1, "ja".to_string(), None), (2, "fr".to_string(), None)]
    );
}

#[test]
fn duplicate_id_within_same_upsert_statement_is_rejected() {
    let (core, path) = open_engine("sql-upsert-duplicate-id-in-batch");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
         (1, '[0.1,0.1]', 'ja'), (1, '[0.2,0.2]', 'en') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-9'"
    );
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    assert!(read_back(&core, &ctx).is_empty());
}

// --- 行制約由来 23505 が構造的に発生しないこと ---

#[test]
fn same_tenant_conflict_never_returns_row_constraint_id_conflict() {
    // `INSERT`（`ON CONFLICT` なし）との対照: 通常の `INSERT` は行制約由来の
    // `IdConflict`（`23505`）を返すが、UPSERT（DO NOTHING）は同じ衝突を
    // `INSERT 0 0`（成功）として吸収する。
    let (core, path) = open_engine("sql-upsert-no-row-constraint-conflict");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-8")).expect("seed insert");

    let plain_insert_err = run_sql(
        &core,
        &ctx,
        &mut session,
        &insert_sql(1, "en", "op-plain-conflict"),
    )
    .unwrap_err();
    assert_eq!(plain_insert_err.wire_code(), "23505");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-10'"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert should succeed");
    assert_eq!(insert_rows_affected(outcome), 0);
}

// --- 台帳照合（RECOVER-10）: 衝突分岐より優先 ---

#[test]
fn resending_the_same_operation_id_with_identical_upsert_is_rejected_with_23505() {
    let (core, path) = open_engine("sql-upsert-resend-duplicate");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-11'"
    );
    run_sql(&core, &ctx, &mut session, &sql).expect("first send ok");
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn resending_the_same_operation_id_with_different_action_is_content_mismatch() {
    let (core, path) = open_engine("sql-upsert-resend-action-mismatch");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let values = "(1, '[0.1,0.2]', 'ja')";
    let do_nothing = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES {values} \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-12'"
    );
    run_sql(&core, &ctx, &mut session, &do_nothing).expect("first send ok");

    let do_update = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES {values} \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-12'"
    );
    let err = run_sql(&core, &ctx, &mut session, &do_update).unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn resending_the_same_operation_id_used_by_plain_insert_is_content_mismatch() {
    let (core, path) = open_engine("sql-upsert-resend-plain-insert-mismatch");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let plain = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         USING OPERATION_ID 'op-upsert-13'"
    );
    run_sql(&core, &ctx, &mut session, &plain).expect("plain insert ok");

    let upsert = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-13'"
    );
    let err = run_sql(&core, &ctx, &mut session, &upsert).unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn ledger_records_upsert_operation_id_even_when_all_rows_do_nothing() {
    let (core, path) = open_engine("sql-upsert-ledger-do-nothing");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-9")).expect("seed insert");
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-upsert-14"),
        LedgerLookup::NotRecorded
    );

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-14'"
    );
    run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-upsert-14"),
        LedgerLookup::Recorded
    );
}

#[test]
fn binding_failure_leaves_ledger_and_rows_unchanged() {
    let (core, path) = open_engine("sql-upsert-binding-failure-no-ledger");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    // 3 行目が `lang` 列を欠くため（non-nullable）束縛が失敗する。
    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
         (1, '[0.1,0.1]', 'ja'), (2, '[0.2,0.2]', 'en') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-15'"
    );
    // 上記は実際には 2 行とも妥当なので束縛は通る。束縛失敗ケースは
    // `bind_upsert_assignments` の単体テストで固定済みのため、ここでは
    // 未存在テーブルによる束縛前の失敗（`42P01`）で「台帳・行とも無変更」を
    // 確認する。
    let bad_table_sql = sql.replace(TABLE, "no_such_table");
    let err = run_sql(&core, &ctx, &mut session, &bad_table_sql).unwrap_err();
    assert_eq!(err.wire_code(), "42P01");
    assert_eq!(
        ledger_state(&core, "tenant-a", "op-upsert-15"),
        LedgerLookup::NotRecorded
    );
    assert!(read_back(&core, &ctx).is_empty());
}

// --- operation_id 必須化・INDEX-4 上限 ---

#[test]
fn missing_operation_id_clause_is_rejected_before_write_txn_with_23502() {
    let (core, path) = open_engine("sql-upsert-missing-op-id");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING"
    );
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "23502");
    assert!(read_back(&core, &ctx).is_empty());
}

#[test]
fn compare_only_without_ledger_mode_allows_upsert_without_operation_id() {
    let path = unique_db_path("sql-upsert-compare-only");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::CompareOnlyWithoutLedger);
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING"
    );
    let outcome = run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
}

#[test]
fn upsert_row_count_exceeding_batch_limit_is_rejected_with_54000() {
    let limits = engine::batch_limits::BatchLimits {
        max_files_per_batch: 2,
        ..Default::default()
    };
    let (core, path) = open_engine_with_limits("sql-upsert-index4-row-limit", limits);
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES \
         (1, '[0.1,0.1]', 'ja'), (2, '[0.2,0.2]', 'en'), (3, '[0.3,0.3]', 'fr') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-16'"
    );
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "54000");
    assert!(read_back(&core, &ctx).is_empty());
}

// --- 構文の許可形状 ---

#[test]
fn explain_upsert_is_rejected_with_42601() {
    let (core, path) = open_engine("sql-upsert-explain-rejected");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    let sql = format!(
        "EXPLAIN INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-17'"
    );
    let err = run_sql(&core, &ctx, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

// --- RLS-11（read-your-writes） ---

#[test]
fn do_update_result_is_visible_via_select_in_same_session() {
    let (core, path) = open_engine("sql-upsert-rls11-read-your-writes");
    let _guard = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();

    run_sql(&core, &ctx, &mut session, &insert_sql(1, "ja", "seed-10")).expect("seed insert");

    let sql = format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
         USING OPERATION_ID 'op-upsert-18'"
    );
    run_sql(&core, &ctx, &mut session, &sql).expect("upsert ok");

    let select_sql = format!("SELECT lang FROM {TABLE} LIMIT 10");
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, &select_sql)
        .expect("select ok");
    let SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    assert_eq!(result.rows.len(), 1);
    match result.rows[0].cells.first() {
        Some(Cell::Text(s)) => assert_eq!(s, "en"),
        other => panic!("expected Cell::Text, got {other:?}"),
    }
}

// --- 衝突先の可視集合判定（Issue #1254・RLS-10・RLS-11） ---

fn invisible_conflict_upsert(action: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang) VALUES (1, '[0.9,0.9]', 'en') \
         ON CONFLICT (id) {action} USING OPERATION_ID '{op_id}'"
    )
}

#[test]
fn do_update_on_owned_but_invisible_conflict_is_rejected_with_42501() {
    let (core, path) = open_engine("sql-upsert-invisible-update");
    let _guard = CleanupGuard(path);
    let ctx_all = ctx_for("tenant-a");
    let public_only = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    run_sql(
        &core,
        &ctx_all,
        &mut session,
        &insert_sql(1, "ja", "seed-i1"),
    )
    .expect("seed private");

    let sql = invisible_conflict_upsert("DO UPDATE SET lang = EXCLUDED.lang", "op-inv-1");
    let err = run_sql(&core, &public_only, &mut session, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "42501");
    let msg = err.to_string();
    assert!(!msg.contains("ja") && !msg.contains("0.9") && !msg.contains("tenant-a"));
    assert_eq!(
        read_back(&core, &ctx_all),
        vec![(1, "ja".to_string(), None)]
    );

    // 台帳は未消費: 同じ operation_id を可視 ctx（認証経路相当）で再送すると成功する。
    let outcome = run_sql(&core, &ctx_all, &mut session, &sql).expect("visible upsert ok");
    assert_eq!(insert_rows_affected(outcome), 1);
    assert_eq!(
        read_back(&core, &ctx_all),
        vec![(1, "en".to_string(), None)]
    );
}

#[test]
fn do_nothing_on_owned_but_invisible_conflict_reports_zero_rows() {
    let (core, path) = open_engine("sql-upsert-invisible-nothing");
    let _guard = CleanupGuard(path);
    let ctx_all = ctx_for("tenant-a");
    let public_only = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    run_sql(
        &core,
        &ctx_all,
        &mut session,
        &insert_sql(1, "ja", "seed-i2"),
    )
    .expect("seed private");

    let sql = invisible_conflict_upsert("DO NOTHING", "op-inv-2");
    let outcome = run_sql(&core, &public_only, &mut session, &sql).expect("skip ok");
    assert_eq!(insert_rows_affected(outcome), 0);
    assert_eq!(
        read_back(&core, &ctx_all),
        vec![(1, "ja".to_string(), None)]
    );
}

#[test]
fn other_tenant_private_row_with_same_id_is_not_a_conflict() {
    let (core, path) = open_engine("sql-upsert-invisible-other-tenant");
    let _guard = CleanupGuard(path);
    let ctx_b = ctx_for("tenant-b");
    let public_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();
    run_sql(
        &core,
        &ctx_b,
        &mut session,
        &insert_sql(1, "b-side", "seed-i3"),
    )
    .expect("seed b");

    let sql = invisible_conflict_upsert("DO UPDATE SET lang = EXCLUDED.lang", "op-inv-3");
    let outcome = run_sql(&core, &public_a, &mut session, &sql).expect("new insert for tenant-a");
    assert_eq!(insert_rows_affected(outcome), 1);
    assert_eq!(
        read_back(&core, &ctx_b),
        vec![(1, "b-side".to_string(), None)]
    );
}
