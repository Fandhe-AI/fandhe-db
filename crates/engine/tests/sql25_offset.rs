//! `OFFSET` とページング（Issue #916。ポインタ: SQL-25 (b)・TASK-209。
//! 詳細は `docs/design/sql-offset-paging.md` 参照）の結合テスト。
//!
//! `tests/sql_scan.rs`・`tests/rls_generalized.rs`（`scan_*`）・`tests/sql_cursor.rs`・
//! `tests/table18_view.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）で、
//! 広域取得・`GROUP BY` 集計双方の `OFFSET` を検証する。テナント境界（RLS-7/8）の
//! 一般化検証（多テーブル・多テナント直積）は `tests/rls_generalized.rs` の管轄の
//! ままとし、本ファイルは「`OFFSET` の計数対象は可視行に限る」という契約自体を
//! 単純な 2 テナント固定で固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
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
        ],
    )
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    visibility: Visibility,
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{id}"))
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn scan(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("scan should succeed: sql={sql:?} err={e:?}")),
    )
}

// ---------- ページング一貫性（可視行の和集合が LIMIT 全件と一致・重複/欠落なし） ----------

#[test]
fn offset_paging_union_matches_full_scan_with_no_duplicates_or_gaps() {
    let path = unique_db_path("offset-paging-union");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=17u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let full = scan(&core, "tenant-a", "SELECT id FROM docs LIMIT 10000");
    let mut full_ids = result_ids(&full);
    full_ids.sort_unstable();

    // ページサイズ 6 で全ページを OFFSET 0/6/12 と連結する（最後のページは端数）。
    const PAGE: u64 = 6;
    let mut paged_ids: Vec<u64> = Vec::new();
    let mut offset = 0u64;
    loop {
        let sql = format!("SELECT id FROM docs LIMIT {PAGE} OFFSET {offset}");
        let page = scan(&core, "tenant-a", &sql);
        if page.rows.is_empty() {
            break;
        }
        paged_ids.extend(result_ids(&page));
        offset += PAGE;
    }
    paged_ids.sort_unstable();

    assert_eq!(
        paged_ids, full_ids,
        "paging via OFFSET must reconstruct the same set as a single LIMIT-all scan"
    );
    // 重複なし: sort 後の長さが元の長さと一致（`sort_unstable` は重複を除去しない）。
    let mut dedup = paged_ids.clone();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        paged_ids.len(),
        "paged union must not contain duplicate ids"
    );
}

#[test]
fn offset_beyond_visible_count_returns_empty_but_keeps_columns() {
    let path = unique_db_path("offset-beyond-count");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=3u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let result = scan(&core, "tenant-a", "SELECT id FROM docs LIMIT 10 OFFSET 3");
    assert_eq!(result.rows.len(), 0);
    assert_eq!(result.columns.len(), 1);
}

#[test]
fn offset_zero_is_equivalent_to_omitting_offset() {
    let path = unique_db_path("offset-zero-equivalence");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=5u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let without = result_ids(&scan(&core, "tenant-a", "SELECT id FROM docs LIMIT 5"));
    let with_zero = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 5 OFFSET 0",
    ));
    assert_eq!(without, with_zero);
}

#[test]
fn offset_query_is_deterministic_across_repeated_calls() {
    let path = unique_db_path("offset-determinism");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=9u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let first = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 4 OFFSET 3",
    ));
    let second = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 4 OFFSET 3",
    ));
    assert_eq!(first, second);
}

// ---------- RLS: OFFSET は可視行のみを計数する（RLS-7/8） ----------

/// 物理走査順（`(tenant_id, id)` 辞書順）の先頭に他テナントの `Private` 行を
/// 大量に挟んでも、`OFFSET` の計数・結果のいずれにも現れないことを固定する
/// （不可視行の存在・件数を `OFFSET` の挙動から推測できない）。
#[test]
fn offset_does_not_count_or_leak_other_tenant_rows() {
    let path = unique_db_path("offset-rls-no-leak");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    // "tenant-0" は辞書順で "tenant-a" より前に来る。Private のため tenant-a
    // からは不可視。
    let other_ctx = ctx("tenant-0");
    for id in 1..=8u64 {
        insert_row(&storage, &other_ctx, id, "ja", Visibility::Private);
    }
    let tenant_ctx = ctx("tenant-a");
    for id in 101..=105u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    // 不可視行が挟まっていない場合と同じ挙動になるはず: 可視 5 件中 OFFSET 2 で
    // 先頭 2 件（101・102）を読み飛ばし、残り 3 件（103・104・105）を返す。
    let mut got = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 10 OFFSET 2",
    ));
    got.sort_unstable();
    assert_eq!(got, vec![103, 104, 105]);

    // 他テナントの Private 行は allow_private なしのコンテキストでも一切見えない
    // ことも併せて確認する（本テストの前提）。
    let leaked = scan(&core, "tenant-a", "SELECT id FROM docs LIMIT 10000");
    assert!(
        result_ids(&leaked).iter().all(|id| *id >= 101),
        "tenant-0's Private rows must never be visible to tenant-a"
    );
}

// ---------- 範囲検証 ----------

#[test]
fn offset_accepts_exactly_max_search_k_and_rejects_one_over() {
    let path = unique_db_path("offset-range-boundary");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    insert_row(&storage, &tenant_ctx, 1, "ja", Visibility::Public);
    let core = new_core(storage);

    core.execute_sql(&ctx("tenant-a"), "SELECT id FROM docs LIMIT 5 OFFSET 10000")
        .expect("OFFSET at MAX_SEARCH_K must be accepted");

    let err = core
        .execute_sql(&ctx("tenant-a"), "SELECT id FROM docs LIMIT 5 OFFSET 10001")
        .expect_err("OFFSET above MAX_SEARCH_K must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// ---------- 検索 SELECT・EXPLAIN は引き続き OFFSET を拒否する ----------

#[test]
fn search_select_with_offset_is_still_rejected_as_syntax_error() {
    let path = unique_db_path("offset-search-form-rejected");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 5 OFFSET 1",
        )
        .expect_err("ranked search SELECT must not accept OFFSET");
    assert_eq!(err.wire_code(), "42601");
}

// ---------- GROUP BY 集計への OFFSET ----------

#[test]
fn group_by_having_order_by_limit_offset_skips_sorted_groups() {
    let path = unique_db_path("offset-group-by");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    // 5 グループ（"a".."e"）を各 1 行ずつ。
    for (idx, lang) in ["a", "b", "c", "d", "e"].iter().enumerate() {
        insert_row(
            &storage,
            &tenant_ctx,
            (idx + 1) as u64,
            lang,
            Visibility::Public,
        );
    }
    let core = new_core(storage);

    let sql = "SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang ORDER BY lang LIMIT 2 OFFSET 2";
    let result = scan(&core, "tenant-a", sql);
    let keys: Vec<String> = result
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            engine::sql::exec::Cell::Text(s) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect();
    assert_eq!(keys, vec!["c".to_string(), "d".to_string()]);
}

// ---------- DECLARE CURSOR 内の OFFSET ----------

#[test]
fn declare_cursor_with_offset_matches_direct_execution() {
    let path = unique_db_path("offset-cursor");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=6u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);
    let caller = ctx("tenant-a");

    let direct = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 10 OFFSET 2",
    ));

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let declare_sql = "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 10 OFFSET 2";
    assert_eq!(
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, declare_sql)
            .expect("declare"),
        SqlOutcome::DeclareCursor
    );

    let mut fetched: Vec<u64> = Vec::new();
    loop {
        let outcome = core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 3 FROM c")
            .expect("fetch");
        let SqlOutcome::Fetch(result) = outcome else {
            panic!("expected Fetch outcome");
        };
        if result.rows.is_empty() {
            break;
        }
        fetched.extend(result_ids(&result));
    }
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "CLOSE c")
        .expect("close");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");

    let mut fetched_sorted = fetched.clone();
    fetched_sorted.sort_unstable();
    let mut direct_sorted = direct.clone();
    direct_sorted.sort_unstable();
    assert_eq!(fetched_sorted, direct_sorted);
}

// ---------- ビュー経由の OFFSET（TABLE-18・SQL-23） ----------

#[test]
fn view_scan_with_offset_matches_base_table_equivalent() {
    let path = unique_db_path("offset-view");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let tenant_ctx = ctx("tenant-a");
    for id in 1..=6u64 {
        insert_row(&storage, &tenant_ctx, id, "ja", Visibility::Public);
    }
    let core = new_core(storage);

    let mut ddl_session = SessionState::default();
    ddl_session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut ddl_session,
        "CREATE VIEW v AS SELECT id FROM docs",
    )
    .expect("create view should succeed");

    let via_view = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM v LIMIT 10 OFFSET 2",
    ));
    let via_base = result_ids(&scan(
        &core,
        "tenant-a",
        "SELECT id FROM docs LIMIT 10 OFFSET 2",
    ));
    let mut via_view_sorted = via_view.clone();
    via_view_sorted.sort_unstable();
    let mut via_base_sorted = via_base.clone();
    via_base_sorted.sort_unstable();
    assert_eq!(via_view_sorted, via_base_sorted);
}
