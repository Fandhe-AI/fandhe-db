//! 分割実行器（[`super`]）のシナリオテスト（Issue #1131。ADR `docs/design/partitioned-dml.md`
//! 9.3 節の検証基準・並行書き込みとの整合・RLS 境界）。`tests.rs`（実行器の単体）と役割を分け、
//! 「チャンク間フック（`after_chunk`）から本物の SQL 表層（`EngineCore`）を呼ぶ」決定的な
//! シナリオだけを置く。ジョブは `run_partitioned` を直接駆動する（登録簿への登録は実行器の
//! 内側で行われるため、フック内の SQL は production と同じ「実行中ジョブ」を観測する）。
//!
//! 時間に依存する判定はしない（締め条件は適用行数だけ。`max_writer_hold` は範囲上限・
//! `scan_budget_rows` は十分大きくする。#1213 の間欠失敗の再発防止）。
//!
//! 対象ビヘイビア（ポインタ）: RECOVER-11・RECOVER-12・RLS-9・RLS-10・TABLE-3・PERSIST-1。
//! 「テーブルはあるが要求元から見えない」（ADR 8.1）は SHOW／CANCEL がカタログを引かない
//! 実装のため対象外（N/A）。他テナントの存在で応答が変わらないことは下の
//! `other_tenant_responses_are_identical_across_job_states` で固定する。

use std::cell::RefCell;
use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::time::Duration;

use super::*;
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::recovery::ledger::OP_LEDGER_TABLE;
use crate::recovery::partitioned_job::JobRecord;
use crate::recovery::required_op_id::OperationId;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::Visibility;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const JOB: &str = "job-1";
const CHUNK: usize = 2;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn limits(per_tenant: usize, total: usize) -> PartitionedDmlLimits {
    let nz = |v: usize| NonZeroUsize::new(v).expect("non-zero");
    PartitionedDmlLimits {
        chunk_rows: nz(CHUNK),
        scan_budget_rows: nz(100_000),
        max_writer_hold: Duration::from_millis(5000),
        max_jobs_per_tenant: nz(per_tenant),
        max_jobs_total: nz(total),
        ..PartitionedDmlLimits::default()
    }
}

fn new_core(label: &str, lim: PartitionedDmlLimits) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_partitioned_dml_limits(lim);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("sys"),
        &mut session,
        "CREATE TABLE docs (tag TEXT, n BIGINT)",
    )
    .expect("create table");
    (core, guard)
}

fn exec(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn exec_ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    exec(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

/// 応答を比較可能な文字列へ直す（成功は Debug、失敗は wire_code と client 向け文言）。
fn normalize(r: Result<SqlOutcome, SqlSurfaceError>) -> String {
    match r {
        Ok(o) => format!("ok:{o:?}"),
        Err(e) => format!("err:{}:{}", e.wire_code(), e.client_message()),
    }
}

fn seed(core: &EngineCore, tenant: &str, rows: &[(u64, &str)]) {
    let first = rows.first().map(|(id, _)| *id).unwrap_or(0);
    let values: Vec<String> = rows
        .iter()
        .map(|(id, tag)| format!("({id}, '{tag}', {id})"))
        .collect();
    exec_ok(
        core,
        tenant,
        &format!(
            "INSERT INTO docs (id, tag, n) VALUES {} USING OPERATION_ID 'seed-{tenant}-{first}'",
            values.join(", ")
        ),
    );
}

/// 行テーブルを直接読んだ `(id, tag)` の昇順（キャッシュを通さない正解データ）。
fn rows_direct(storage: &Storage, tenant: &str) -> Vec<(u64, String)> {
    use redb::ReadableDatabase;
    let schema = storage.get_table_schema(TABLE).expect("schema");
    let read = storage.db().begin_read().expect("begin read");
    let t = read
        .open_table(user_rows_table_def(&user_rows_table_name(TABLE)))
        .expect("open rows");
    let mut out = Vec::new();
    for entry in t
        .range::<(&str, u64)>((
            std::ops::Bound::Included((tenant, 0u64)),
            std::ops::Bound::Included((tenant, u64::MAX)),
        ))
        .expect("range")
    {
        let (k, v) = entry.expect("entry");
        let (_, id) = k.value();
        let row = crate::storage::decode_row(id, v.value()).expect("decode");
        let values = crate::row_codec::decode_scalar_columns(&schema, &row.metadata).expect("cols");
        let tag = match values.first() {
            Some(crate::row_codec::Value::Text(s)) => s.clone(),
            other => panic!("unexpected value {other:?}"),
        };
        out.push((id, tag));
    }
    out
}

fn ids_matching(rows: &[(u64, String)]) -> Vec<u64> {
    rows.iter()
        .filter(|(_, t)| t == "x")
        .map(|(id, _)| *id)
        .collect()
}

fn job_record(storage: &Storage, tenant: &str, op: &str) -> Option<JobRecord> {
    let txn = storage.begin_write_txn().expect("begin");
    let c = ctx(tenant);
    let operation = OperationId::parse(op).expect("valid op");
    let key = JobKey::for_context(&c, TABLE, &operation);
    partitioned_job::lookup_in_write_txn(&txn, &key).expect("lookup")
}

fn cursor_of(storage: &Storage, tenant: &str, op: &str) -> u64 {
    match job_record(storage, tenant, op) {
        Some(JobRecord::Running { cursor, .. }) => cursor,
        other => panic!("expected Running record, got {other:?}"),
    }
}

/// `(tenant, table)` の台帳エントリの operation_id 一覧。
fn ledger_entries(storage: &Storage, tenant: &str) -> Vec<String> {
    use redb::{ReadableDatabase, ReadableTable};
    let read = storage.db().begin_read().expect("begin read");
    let t = match read.open_table(OP_LEDGER_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Vec::new(),
        Err(e) => panic!("open ledger: {e}"),
    };
    let mut out = Vec::new();
    for entry in t.iter().expect("iter") {
        let (k, _) = entry.expect("entry");
        let (t_id, table, op) = k.value();
        if t_id == tenant && table == TABLE {
            out.push(op.to_string());
        }
    }
    out
}

fn content_hash_for(tag: &[u8]) -> content_hash::ContentHash {
    content_hash::ContentHash::for_test(tag)
}

/// `tag = 'x'` に一致する行を対象にした DELETE を `tenant` で駆動する。
fn drive_delete(
    core: &EngineCore,
    tenant: &str,
    op: &str,
    hook: &mut dyn FnMut(usize),
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<Infallible>> {
    let storage = core.storage_for_test();
    let schema = storage.get_table_schema(TABLE).expect("schema");
    let operation = OperationId::parse(op).expect("valid op");
    let lim = core_limits(core);
    run_partitioned(
        storage,
        TABLE,
        &ctx(tenant),
        LedgerWrite::Record(&operation),
        &content_hash_for(op.as_bytes()),
        None,
        false,
        &lim,
        None,
        Kind::Delete,
        |c: &DmlCandidate<'_>| {
            let cols = crate::row_codec::scan_scalar_columns(&schema, c.metadata).expect("scan");
            Ok::<bool, Infallible>(
                cols.first()
                    .copied()
                    .flatten()
                    .and_then(|v| v.as_text())
                    .map(|t| t == "x")
                    .unwrap_or(false),
            )
        },
        &mut Hooks {
            clock: &Instant::now,
            after_chunk: Some(hook),
        },
    )
}

fn core_limits(core: &EngineCore) -> PartitionedDmlLimits {
    core.partitioned_dml_limits()
}

fn show_rows(core: &EngineCore, tenant: &str, op: &str) -> Vec<(String, i64)> {
    match exec_ok(
        core,
        tenant,
        &format!("SHOW PARTITIONED DML '{op}' ON docs"),
    ) {
        SqlOutcome::Query(q) => q
            .rows
            .iter()
            .map(|r| match (&r.cells[0], &r.cells[1]) {
                (Cell::Text(s), Cell::SignedInteger(n)) => (s.clone(), *n),
                other => panic!("unexpected cells {other:?}"),
            })
            .collect(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn job_sql(op: &str) -> String {
    format!("DELETE FROM docs WHERE tag = 'x' USING OPERATION_ID '{op}' PARTITIONED CHUNK 2")
}

#[test]
fn concurrent_writes_match_recorded_per_chunk_target_sets() {
    let (core, _g) = new_core("pdml-scn-concurrent", limits(1, 4));
    let storage = core.storage_for_test();
    let initial: Vec<(u64, &str)> = (1..=20).map(|i| (i, "x")).chain([(50, "y")]).collect();
    seed(&core, "ta", &initial);

    // 各フックで「注入前後のスナップショット」とカーソル・次チャンクの期待対象を記録する。
    let prev_after_injection: RefCell<Vec<(u64, String)>> =
        RefCell::new(rows_direct(storage, "ta"));
    let expected_next: RefCell<Vec<u64>> = RefCell::new(Vec::new());
    let observed_sets: RefCell<Vec<Vec<u64>>> = RefCell::new(Vec::new());
    let mut hook = |k: usize| {
        // 直前の注入後スナップショットとの差分が、記録した期待対象と一致する（A2・A5）。
        let now = rows_direct(storage, "ta");
        let prev = prev_after_injection.borrow().clone();
        let removed: Vec<u64> = prev
            .iter()
            .filter(|(id, _)| !now.iter().any(|(i2, _)| i2 == id))
            .map(|(id, _)| *id)
            .collect();
        let added = now
            .iter()
            .any(|(id, _)| !prev.iter().any(|(i2, _)| i2 == id));
        assert!(!added, "chunk {k} must not add rows");
        if k > 1 {
            assert_eq!(
                removed,
                *expected_next.borrow(),
                "chunk {k} target set differs from the recorded expectation"
            );
        }
        observed_sets.borrow_mut().push(removed);

        let c = cursor_of(storage, "ta", JOB);
        // 通常 DML の注入（別 operation_id）。
        match k {
            1 => {
                // カーソルより前（残る）／後（消える）への一致行 INSERT。
                exec_ok(
                    &core,
                    "ta",
                    "INSERT INTO docs (id, tag, n) VALUES (0, 'x', 0) USING OPERATION_ID 'inj-a'",
                );
                exec_ok(&core, "ta", "INSERT INTO docs (id, tag, n) VALUES (100, 'x', 100) USING OPERATION_ID 'inj-b'");
            }
            2 => {
                // 未走査の一致行を非一致へ（残る）。
                exec_ok(
                    &core,
                    "ta",
                    "UPDATE docs SET tag = 'y' WHERE id = 15 USING OPERATION_ID 'inj-c'",
                );
            }
            3 => {
                // 未走査の非一致行を一致へ（消える）。
                exec_ok(
                    &core,
                    "ta",
                    "UPDATE docs SET tag = 'x' WHERE id = 50 USING OPERATION_ID 'inj-d'",
                );
            }
            4 => {
                // 未走査の一致行を通常 DELETE（件数に入らない）。
                exec_ok(
                    &core,
                    "ta",
                    "DELETE FROM docs WHERE id = 17 USING OPERATION_ID 'inj-e'",
                );
            }
            _ => {}
        }
        let after = rows_direct(storage, "ta");
        let next: Vec<u64> = ids_matching(&after)
            .into_iter()
            .filter(|id| *id > c)
            .take(CHUNK)
            .collect();
        *expected_next.borrow_mut() = next;
        *prev_after_injection.borrow_mut() = after;
    };
    let out = drive_delete(&core, "ta", JOB, &mut hook).expect("job completes");
    let PartitionedDmlOutcome::Completed { total_rows } = out;

    // 処理済み件数 = フックで観測した各チャンクの削除件数の和 + 最後のフック以降の分。
    let observed: usize = observed_sets.borrow().iter().map(Vec::len).sum();
    let final_rows = rows_direct(storage, "ta");
    let tail = prev_after_injection.borrow().len() - final_rows.len();
    assert_eq!(total_rows as usize, observed + tail);
    // 残る一致行は「カーソルより前に INSERT した id 0」だけ。
    assert_eq!(ids_matching(&final_rows), vec![0]);
    let remaining: Vec<u64> = final_rows.iter().map(|(id, _)| *id).collect();
    assert!(
        remaining.contains(&0),
        "row inserted before the cursor must remain"
    );
    assert!(
        remaining.contains(&15),
        "row turned non-matching must remain"
    );
    assert!(
        !remaining.contains(&100),
        "row inserted after the cursor must be deleted"
    );
    assert!(
        !remaining.contains(&50),
        "row turned matching must be deleted"
    );
    assert!(
        !remaining.contains(&17),
        "row deleted by a plain DML stays deleted"
    );
}

#[test]
fn resend_while_running_is_55p03_and_show_reports_running() {
    let (core, _g) = new_core("pdml-scn-resend", limits(1, 4));
    let storage = core.storage_for_test();
    let rows: Vec<(u64, &str)> = (1..=10).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows);
    let seen = RefCell::new(Vec::new());
    let mut hook = |k: usize| {
        if k == 1 {
            let e = exec(&core, "ta", &job_sql(JOB)).expect_err("resend while running");
            seen.borrow_mut()
                .push((e.wire_code().to_string(), e.client_message()));
            let shown = show_rows(&core, "ta", JOB);
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].0, "running");
            assert!(shown[0].1 > 0);
        }
    };
    drive_delete(&core, "ta", JOB, &mut hook).expect("job completes");
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "55P03");
    assert!(rows_direct(storage, "ta").is_empty());
    assert_eq!(
        show_rows(&core, "ta", JOB),
        vec![("completed".to_string(), 10)]
    );
}

#[test]
fn cancel_via_sql_while_running_stops_at_next_boundary() {
    let (core, _g) = new_core("pdml-scn-cancel", limits(1, 4));
    let storage = core.storage_for_test();
    let rows: Vec<(u64, &str)> = (1..=10).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows);
    let mut hook = |k: usize| {
        if k == 1 {
            match exec_ok(
                &core,
                "ta",
                &format!("CANCEL PARTITIONED DML '{JOB}' ON docs"),
            ) {
                SqlOutcome::Query(q) => match q.rows.first().and_then(|r| r.cells.first()) {
                    Some(Cell::Text(s)) => assert_eq!(s, "cancelling"),
                    other => panic!("unexpected cells {other:?}"),
                },
                other => panic!("expected Query, got {other:?}"),
            }
        }
    };
    let err = drive_delete(&core, "ta", JOB, &mut hook).expect_err("cancelled");
    assert!(matches!(err.cause, PartitionedStopCause::CancelRequested));
    // commit 済みのチャンク（1 チャンク分）は戻らない。
    assert_eq!(err.committed_total, CHUNK as u64);
    assert_eq!(rows_direct(storage, "ta").len(), 10 - CHUNK);
    assert_eq!(
        show_rows(&core, "ta", JOB),
        vec![("cancelled".to_string(), CHUNK as i64)]
    );
    // 取り消し済みの ledger エントリは作られない（完了していない）。
    assert!(!ledger_entries(storage, "ta").contains(&JOB.to_string()));
}

#[test]
fn other_tenant_responses_are_identical_across_job_states() {
    let (core, _g) = new_core("pdml-scn-tenant-id", limits(1, 4));
    let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows);
    seed(&core, "tc", &[(1, "y")]);

    let show_sql = format!("SHOW PARTITIONED DML '{JOB}' ON docs");
    let cancel_sql = format!("CANCEL PARTITIONED DML '{JOB}' ON docs");
    // 基準: どのテナントにもジョブが無い状態での tc の応答。
    let base_show = normalize(exec(&core, "tc", &show_sql));
    let base_cancel = normalize(exec(&core, "tc", &cancel_sql));

    // running 中: tc の応答は変わらず、tc の CANCEL で ta のジョブは止まらない。
    let mut hook = |_k: usize| {
        assert_eq!(normalize(exec(&core, "tc", &show_sql)), base_show);
        assert_eq!(normalize(exec(&core, "tc", &cancel_sql)), base_cancel);
    };
    drive_delete(&core, "ta", JOB, &mut hook).expect("ta job completes despite tc cancel");

    // completed 後。
    assert_eq!(normalize(exec(&core, "tc", &show_sql)), base_show);
    assert_eq!(normalize(exec(&core, "tc", &cancel_sql)), base_cancel);

    // cancelled 後（別 operation_id で取り消しまで進める）。
    let rows2: Vec<(u64, &str)> = (101..=108).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows2);
    let cancel2 = "CANCEL PARTITIONED DML 'job-2' ON docs";
    let show2 = "SHOW PARTITIONED DML 'job-2' ON docs";
    let base_show2 = normalize(exec(&core, "tc", show2));
    let base_cancel2 = normalize(exec(&core, "tc", cancel2));
    let mut hook2 = |k: usize| {
        if k == 1 {
            exec_ok(&core, "ta", cancel2);
        }
    };
    let err = drive_delete(&core, "ta", "job-2", &mut hook2).expect_err("cancelled");
    assert!(matches!(err.cause, PartitionedStopCause::CancelRequested));
    assert_eq!(normalize(exec(&core, "tc", show2)), base_show2);
    assert_eq!(normalize(exec(&core, "tc", cancel2)), base_cancel2);
}

#[test]
fn same_operation_id_runs_concurrently_in_two_tenants() {
    let (core, _g) = new_core("pdml-scn-two-tenants", limits(1, 4));
    let storage = core.storage_for_test();
    let rows: Vec<(u64, &str)> = (1..=8).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows);
    seed(&core, "tb", &rows);
    let mut hook = |k: usize| {
        if k == 1 {
            // ta の実行中に、tb が同じ operation_id・同じテーブルで最後まで実行できる。
            exec_ok(&core, "tb", &job_sql(JOB));
            assert_eq!(rows_direct(storage, "tb").len(), 0);
            assert_eq!(show_rows(&core, "ta", JOB)[0].0, "running");
            assert_eq!(
                show_rows(&core, "tb", JOB),
                vec![("completed".to_string(), 8)]
            );
            assert_eq!(rows_direct(storage, "ta").len(), 8 - CHUNK);
        }
    };
    drive_delete(&core, "ta", JOB, &mut hook).expect("ta completes");
    assert!(rows_direct(storage, "ta").is_empty());
    assert_eq!(
        show_rows(&core, "ta", JOB),
        vec![("completed".to_string(), 8)]
    );
    assert_eq!(
        ledger_entries(storage, "ta")
            .iter()
            .filter(|o| *o == JOB)
            .count(),
        1
    );
    assert_eq!(
        ledger_entries(storage, "tb")
            .iter()
            .filter(|o| *o == JOB)
            .count(),
        1
    );
}

#[test]
fn concurrency_limit_rejections_are_identical_per_tenant_and_total() {
    let reject = |per_tenant: usize, total: usize, other_tenant: &str, label: &str| {
        let (core, _g) = new_core(label, limits(per_tenant, total));
        let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
        seed(&core, "ta", &rows);
        if other_tenant != "ta" {
            seed(&core, other_tenant, &rows);
        }
        let got = RefCell::new(None);
        let mut hook = |k: usize| {
            if k == 1 {
                let e = exec(&core, other_tenant, &job_sql("job-other")).expect_err("limit");
                *got.borrow_mut() = Some((e.wire_code().to_string(), e.client_message()));
                // 拒否は副作用ゼロ（ジョブ記録なし）。
                assert!(show_rows(&core, other_tenant, "job-other").is_empty());
            }
        };
        drive_delete(&core, "ta", JOB, &mut hook).expect("job completes");
        let r = got.borrow().clone().expect("rejection observed");
        r
    };
    // テナント単位の上限（同じテナントの別 operation_id）と全体の上限（別テナント）。
    let per_tenant = reject(1, 4, "ta", "pdml-scn-limit-tenant");
    let total = reject(4, 1, "tb", "pdml-scn-limit-total");
    assert_eq!(per_tenant.0, "55P03");
    assert_eq!(
        per_tenant, total,
        "per-tenant and total rejections must be indistinguishable"
    );
}

#[test]
fn ledger_has_exactly_one_entry_after_completion() {
    let (core, _g) = new_core("pdml-scn-ledger", limits(1, 4));
    let storage = core.storage_for_test();
    let rows: Vec<(u64, &str)> = (1..=9).map(|i| (i, "x")).collect();
    seed(&core, "ta", &rows);
    let baseline = ledger_entries(storage, "ta");
    let mut hook = |_k: usize| {
        // 完了前は増えていない（9.3 節の基準 4）。
        assert_eq!(ledger_entries(storage, "ta"), baseline);
    };
    drive_delete(&core, "ta", JOB, &mut hook).expect("job completes");
    let after = ledger_entries(storage, "ta");
    assert_eq!(after.len(), baseline.len() + 1);
    assert_eq!(
        after.iter().filter(|o| *o == JOB).count(),
        1,
        "exactly one ledger entry keyed by the job operation_id"
    );
}

#[test]
fn search_and_caches_reflect_each_committed_chunk() {
    let (core, _g) = new_core("pdml-scn-search", limits(1, 4));
    let storage = core.storage_for_test();
    let rows: Vec<(u64, &str)> = (1..=12)
        .map(|i| (i, if i % 3 == 0 { "y" } else { "x" }))
        .collect();
    seed(&core, "ta", &rows);
    let select = "SELECT id FROM docs WHERE tag = 'x' LIMIT 1000";
    let ids = |core: &EngineCore| -> Vec<u64> {
        match exec_ok(core, "ta", select) {
            SqlOutcome::Query(q) => {
                let mut v: Vec<u64> = q.rows.iter().map(|r| r.id).collect();
                v.sort_unstable();
                v
            }
            other => panic!("expected Query, got {other:?}"),
        }
    };
    // キャッシュを温める。
    assert_eq!(ids(&core), ids_matching(&rows_direct(storage, "ta")));
    let last_gen = RefCell::new(storage.table_generation(TABLE).expect("generation"));
    let mut hook = |_k: usize| {
        // 各 commit 済みチャンクの直後に、検索結果が直接読みの正解と一致する（G1）。
        assert_eq!(ids(&core), ids_matching(&rows_direct(storage, "ta")));
        let g = storage.table_generation(TABLE).expect("generation");
        assert!(
            g > *last_gen.borrow(),
            "generation must advance per changed chunk"
        );
        *last_gen.borrow_mut() = g;
    };
    drive_delete(&core, "ta", JOB, &mut hook).expect("job completes");
    assert!(ids(&core).is_empty());
}
