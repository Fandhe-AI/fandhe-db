//! commit 記録（SQL-38・TASK-333・Issue #2404。設計は `docs/design/explicit-transaction.md` 1a-5・1a-7）の
//! クレート内テスト。
//!
//! 明示トランザクションの `BEGIN`（登録）を生かしたまま、別セッションの各経路の commit を流し、
//! `Storage::commit_log()` の記録の内容・並び・保持範囲・上限を検査する。`40001` の判定そのものは
//! #2405 の担当で、ここでは判定が使う記録だけを検査する。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::commit_log::{CommitRecord, Limits, Unverifiable};
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::BigInt, false),
        ],
    )
}

fn new_core_with(tag: &str, limits: Option<Limits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open storage");
    for name in ["ca", "cb"] {
        storage.create_table(&schema(name)).expect("create table");
    }
    if let Some(limits) = limits {
        storage
            .replace_commit_log_for_test(limits)
            .expect("replace log");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    new_core_with(tag, None)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn insert(table: &str, id: u64, op: &str) -> String {
    format!(
        "INSERT INTO {table} (id, embedding, n) VALUES ({id}, '[1.0, 0.0]', 1) \
         USING OPERATION_ID '{op}'"
    )
}

fn gen(core: &EngineCore) -> u64 {
    core.storage.current_generation().expect("generation")
}

fn stats(core: &EngineCore) -> (usize, usize, u64) {
    core.storage.commit_log().expect("log").snapshot_stats()
}

fn records(core: &EngineCore, g_s: u64) -> Result<Vec<CommitRecord>, Unverifiable> {
    core.storage
        .commit_log()
        .expect("log")
        .records_between(g_s, gen(core))
}

/// `BEGIN` した（登録済みの）明示トランザクションと、その `G_s`。
fn begin<'e>(
    core: &'e EngineCore,
    tenant: &str,
) -> (crate::sql::transaction::SessionTransaction<'e>, u64) {
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(tenant), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let g_s = txn.snapshot_generation().expect("active");
    (txn, g_s)
}

fn rollback<'e>(
    core: &'e EngineCore,
    tenant: &str,
    txn: &mut crate::sql::transaction::SessionTransaction<'e>,
) {
    let mut session = SessionState::default();
    core.execute_sql_in_txn(&ctx(tenant), &mut session, txn, "ROLLBACK")
        .expect("rollback");
}

fn footprint<'a>(
    rec: &'a CommitRecord,
    table: &str,
) -> &'a crate::storage::commit_log::TableFootprint {
    &rec.tables
        .iter()
        .find(|(t, _)| &**t == table)
        .unwrap_or_else(|| panic!("{table} must be recorded: {rec:?}"))
        .1
}

// --- 内容と並び -----------------------------------------------------------------------------

#[test]
fn other_sessions_commits_are_recorded_in_generation_order_with_tenant_footprints() {
    let (core, _g) = new_core("clog-content");
    let (mut txn, g_s) = begin(&core, "tenant-a");

    run(&core, "tenant-b", &insert("ca", 1, "b-1"));
    run(
        &core,
        "tenant-b",
        "UPDATE ca SET n = 5 WHERE id = 1 USING OPERATION_ID 'b-2'",
    );
    run(&core, "tenant-c", &insert("cb", 7, "c-1"));
    run(
        &core,
        "tenant-b",
        "DELETE FROM ca WHERE id = 1 USING OPERATION_ID 'b-3'",
    );

    let recs = records(&core, g_s).expect("verifiable");
    assert_eq!(recs.len(), 4, "{recs:?}");
    // 世代の順に連続している。
    for pair in recs.windows(2) {
        assert_eq!(pair[1].generation, pair[0].generation + 1);
    }
    assert_eq!(recs[0].generation, g_s + 1);
    assert_eq!(recs.last().expect("last").generation, gen(&core));

    // INSERT / UPDATE / DELETE は書き込んだテナント自身の行だけを足跡に持つ。他テナントの組は無い。
    for (rec, table, tenant) in [
        (&recs[0], "ca", "tenant-b"),
        (&recs[1], "ca", "tenant-b"),
        (&recs[2], "cb", "tenant-c"),
        (&recs[3], "ca", "tenant-b"),
    ] {
        assert_eq!(rec.tables.len(), 1, "{rec:?}");
        let fp = footprint(rec, table);
        assert!(!fp.ddl);
        assert!(
            fp.private_tenants.iter().all(|t| &**t == tenant),
            "{fp:?} must only contain {tenant}"
        );
        assert!(
            fp.public || fp.private_tenants.contains(tenant),
            "{fp:?} must contain a footprint"
        );
        assert!(!fp.private_tenants.contains("tenant-a"));
    }
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn truncate_and_ddl_are_recorded() {
    let (core, _g) = new_core("clog-truncate-ddl");
    run(&core, "tenant-b", &insert("ca", 1, "seed"));
    let (mut txn, g_s) = begin(&core, "tenant-a");

    run(
        &core,
        "tenant-b",
        "TRUNCATE TABLE ca USING OPERATION_ID 'tr-1'",
    );
    core.storage
        .create_table(&schema("cc"))
        .expect("ddl while registered");

    let recs = records(&core, g_s).expect("verifiable");
    assert_eq!(recs.len(), 2, "{recs:?}");
    let fp = footprint(&recs[0], "ca");
    assert!(!fp.ddl);
    assert!(
        fp.public || fp.private_tenants.contains("tenant-b"),
        "{fp:?}"
    );
    assert!(footprint(&recs[1], "cc").ddl);
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn zero_row_update_does_not_get_rejected_and_records_no_footprint() {
    let (core, _g) = new_core("clog-zero-row");
    let (mut txn, g_s) = begin(&core, "tenant-a");
    run(
        &core,
        "tenant-b",
        "UPDATE ca SET n = 1 WHERE id = 99 USING OPERATION_ID 'z-1'",
    );
    // 0 行更新は commit するが足跡は無い（記録は空なので保持しない）。
    assert!(records(&core, g_s).expect("verifiable").is_empty());
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn records_written_before_begin_are_excluded() {
    let (core, _g) = new_core("clog-before-begin");
    run(&core, "tenant-b", &insert("ca", 1, "pre"));
    let (mut txn, g_s) = begin(&core, "tenant-a");
    assert!(records(&core, g_s).expect("verifiable").is_empty());
    run(&core, "tenant-b", &insert("ca", 2, "post"));
    assert_eq!(records(&core, g_s).expect("verifiable").len(), 1);
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn explicit_transaction_commit_records_the_written_tables() {
    let (core, _g) = new_core("clog-explicit-commit");
    let (mut reader, g_s) = begin(&core, "tenant-a");

    let mut session = SessionState::default();
    let writer_ctx = ctx("tenant-b");
    let mut writer = core.new_session_transaction();
    for sql in [
        "BEGIN".to_string(),
        insert("ca", 1, "e-1"),
        insert("cb", 1, "e-2"),
        "COMMIT".to_string(),
    ] {
        core.execute_sql_in_txn(&writer_ctx, &mut session, &mut writer, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let recs = records(&core, g_s).expect("verifiable");
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].tables.len(), 2);
    footprint(&recs[0], "ca");
    footprint(&recs[0], "cb");
    rollback(&core, "tenant-a", &mut reader);
}

#[test]
fn write_without_a_footprint_is_rejected_before_commit() {
    let (core, _g) = new_core("clog-unrecordable");
    let (mut txn, g_s) = begin(&core, "tenant-a");
    let before = gen(&core);
    // 書き込み点が足跡を積まずに表世代だけを進める（結線漏れの模擬）。
    let write_txn = core.storage.begin_write_txn().expect("write txn");
    crate::catalog::bump_table_generation_in_txn(&write_txn, "ca").expect("bump");
    let err = crate::recovery::commit_boundary::commit(write_txn).expect_err("must reject");
    assert!(matches!(
        err,
        crate::storage::StorageError::CommitFootprintUnrecordable
    ));
    assert_eq!(format!("{err}"), "commit footprint limit exceeded");
    // commit 前の拒否のため、世代も記録も進まない。
    assert_eq!(gen(&core), before);
    assert!(records(&core, g_s).expect("verifiable").is_empty());
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn per_commit_footprint_cap_rejects_instead_of_collapsing_private_rows() {
    let limits = Limits {
        max_per_commit: 3,
        ..Limits::default()
    };
    let (core, _g) = new_core_with("clog-per-commit", Some(limits));
    let (mut txn, g_s) = begin(&core, "tenant-a");
    let before = gen(&core);
    let write_txn = core.storage.begin_write_txn().expect("write txn");
    for i in 0..10 {
        crate::storage::commit_log::note_row(
            &write_txn,
            "ca",
            &format!("tenant-{i}"),
            Visibility::Private,
        );
    }
    crate::catalog::bump_table_generation_in_txn(&write_txn, "ca").expect("bump");
    let err = crate::recovery::commit_boundary::commit(write_txn).expect_err("must reject");
    assert!(matches!(
        err,
        crate::storage::StorageError::CommitFootprintUnrecordable
    ));
    assert_eq!(gen(&core), before);
    assert!(records(&core, g_s).expect("verifiable").is_empty());
    rollback(&core, "tenant-a", &mut txn);
}

// --- 保持範囲と上限（受け入れ条件 B） ---------------------------------------------------------

#[test]
fn nothing_is_retained_without_a_live_registration() {
    let (core, _g) = new_core("clog-no-registration");
    for i in 0..20 {
        run(&core, "tenant-b", &insert("ca", i, &format!("n-{i}")));
    }
    assert_eq!(stats(&core).0, 0);
}

#[test]
fn retained_records_are_dropped_when_the_registration_ends_in_every_way() {
    let (core, _g) = new_core("clog-lifecycle");

    // ROLLBACK
    let (mut txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 1, "l-1"));
    assert_eq!(stats(&core).0, 1);
    rollback(&core, "tenant-a", &mut txn);
    assert_eq!(stats(&core).0, 0);

    // COMMIT（読み取りだけの Tx）
    let (mut txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 2, "l-2"));
    assert_eq!(stats(&core).0, 1);
    let mut session = SessionState::default();
    core.execute_sql_in_txn(&ctx("tenant-a"), &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(stats(&core).0, 0);

    // writer の取得
    let (mut txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 3, "l-3"));
    assert_eq!(stats(&core).0, 1);
    txn.ensure_writer().expect("writer");
    assert_eq!(stats(&core).0, 0, "registration ends at writer acquisition");
    drop(txn);

    // fail
    let (mut txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 4, "l-4"));
    assert_eq!(stats(&core).0, 1);
    txn.fail();
    assert_eq!(stats(&core).0, 0);

    // drop
    let (txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 5, "l-5"));
    assert_eq!(stats(&core).0, 1);
    drop(txn);
    assert_eq!(stats(&core).0, 0);
}

#[test]
fn expired_transaction_releases_its_registration() {
    let path = unique_db_path("clog-expired");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage.create_table(&schema("ca")).expect("table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_transaction_limits(crate::sql::transaction::TransactionLimits {
            max_duration: std::time::Duration::from_millis(1),
            max_statements: 100,
        });
    let (mut txn, _) = begin(&core, "tenant-a");
    run(&core, "tenant-b", &insert("ca", 1, "x-1"));
    assert_eq!(stats(&core).0, 1);
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert!(txn.release_if_expired());
    assert_eq!(stats(&core).0, 0);
}

#[test]
fn record_count_stays_bounded_while_a_transaction_is_kept_open() {
    let limits = Limits {
        max_records: 4,
        ..Limits::default()
    };
    let (core, _g) = new_core_with("clog-count-cap", Some(limits));
    let (mut txn, g_s) = begin(&core, "tenant-a");
    let mut last_floor = 0;
    for i in 0..30 {
        run(&core, "tenant-b", &insert("ca", i, &format!("c-{i}")));
        let (n, _, floor) = stats(&core);
        assert!(n <= 4, "retained {n} records");
        assert!(floor >= last_floor, "the floor must not decrease");
        last_floor = floor;
    }
    assert_eq!(stats(&core).0, 4);
    assert!(last_floor > g_s);
    // 床より古い `G_s` は判定できない（#2405 が衝突として扱う）。
    assert_eq!(records(&core, g_s), Err(Unverifiable));
    // 範囲内の新しい `G_s` なら判定できる。
    assert_eq!(records(&core, last_floor).expect("verifiable").len(), 4);
    rollback(&core, "tenant-a", &mut txn);
    assert_eq!(stats(&core).0, 0);
}

#[test]
fn footprint_element_count_stays_bounded() {
    let limits = Limits {
        max_elements: 6,
        ..Limits::default()
    };
    let (core, _g) = new_core_with("clog-element-cap", Some(limits));
    let (mut txn, _) = begin(&core, "tenant-a");
    for i in 0..30 {
        run(&core, "tenant-b", &insert("ca", i, &format!("e-{i}")));
        assert!(stats(&core).1 <= 6, "elements {}", stats(&core).1);
    }
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn a_commit_that_skipped_the_gate_raises_the_floor() {
    let (core, _g) = new_core("clog-gap");
    let (mut txn, g_s) = begin(&core, "tenant-a");
    // 記録を経ない commit（ゲート・frame を持たない素の write txn）を模擬する。
    use redb::ReadableDatabase as _;
    let _ = core.storage.db().begin_read().expect("read");
    let raw = core.storage.db().begin_write().expect("raw write");
    crate::storage::prepare_generation_bump(&raw).expect("bump generation");
    raw.commit().expect("raw commit");
    run(&core, "tenant-b", &insert("ca", 1, "gap-1"));
    // 世代の欠けのため、`G_s` 時点からの範囲は判定できない。
    assert_eq!(records(&core, g_s), Err(Unverifiable));
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn upsert_update_side_is_recorded() {
    let (core, _g) = new_core("clog-upsert");
    run(&core, "tenant-b", &insert("ca", 1, "u-seed"));
    let (mut txn, g_s) = begin(&core, "tenant-a");

    // 既存行への `ON CONFLICT DO UPDATE`（更新側）。足跡を積み忘れると commit 前に拒否される。
    run(
        &core,
        "tenant-b",
        "INSERT INTO ca (id, embedding, n) VALUES (1, '[1.0, 0.0]', 2) \
         ON CONFLICT (id) DO UPDATE SET n = 9 USING OPERATION_ID 'u-up'",
    );
    let recs = records(&core, g_s).expect("verifiable");
    assert_eq!(recs.len(), 1, "{recs:?}");
    let fp = footprint(&recs[0], "ca");
    assert!(
        fp.public || fp.private_tenants.contains("tenant-b"),
        "{fp:?}"
    );
    assert!(!fp.private_tenants.contains("tenant-a"));
    rollback(&core, "tenant-a", &mut txn);
}

// --- ビュー DDL・足跡登録の寿命（PR #2835 レビュー指摘） ---------------------------------------

#[test]
fn view_definition_changes_are_recorded_as_ddl() {
    let (core, _g) = new_core("clog-view-ddl");
    let (mut txn, g_s) = begin(&core, "tenant-a");

    core.storage
        .create_view("vw", "ca", "SELECT n FROM ca")
        .expect("create view");
    let old = core.storage.view_definition("vw").expect("get view");
    core.storage
        .create_or_replace_view_checked(
            "vw",
            "ca",
            "SELECT embedding FROM ca",
            &[],
            old.as_ref(),
            None,
            true,
            crate::catalog::ViewCheckOption::None,
        )
        .expect("replace view");
    core.storage.drop_view("vw").expect("drop view");

    let recs = records(&core, g_s).expect("verifiable");
    assert_eq!(recs.len(), 3, "{recs:?}");
    for rec in &recs {
        assert!(footprint(rec, "vw").ddl, "{rec:?}");
    }
    rollback(&core, "tenant-a", &mut txn);
}

#[test]
fn repeated_write_txns_are_never_rejected_by_a_reused_frame_address() {
    let (core, _g) = new_core("clog-handle-reuse");
    for _ in 0..16 {
        let w = core.storage.begin_write_txn().expect("write txn");
        crate::storage::commit_log::note_row(&w, "ca", "tenant-x", Visibility::Private);
        crate::catalog::bump_table_generation_in_txn(&w, "ca").expect("bump");
        crate::recovery::commit_boundary::commit(w).expect("a normal commit must not be rejected");
    }
}
