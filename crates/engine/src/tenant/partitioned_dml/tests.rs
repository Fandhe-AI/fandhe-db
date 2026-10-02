//! 分割実行器（[`super`]）の単体テスト（Issue #1128）。入口の `pub(crate)` は結合テスト
//! から届かないため、モジュール内に置く。時間に依存する判定は疑似時計で決定的にし、
//! 実時間の閾値だけに頼るアサーションは書かない（#1213 の間欠失敗の再発防止）。
//!
//! 対象ビヘイビア（ポインタ）: RECOVER-11・RECOVER-12・TABLE-3・RLS-9・RLS-10・INDEX-4・
//! ERR-1/2/4。ADR `docs/design/partitioned-dml.md`。

use super::*;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::recovery::partitioned_job::JobRecord;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::storage::{RowInput, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use std::cell::Cell;
use std::convert::Infallible;

const TABLE: &str = "notes";

fn schema() -> TableSchema {
    TableSchema::new(TABLE, vec![ColumnDef::new("tag", ColumnType::Text, false)])
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(name: &str) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (storage, guard)
}

fn seed(storage: &Storage, tenant: &str, rows: &[(u64, &str)]) {
    let schema = schema();
    let txn = storage.begin_write_txn().expect("begin seed txn");
    {
        let mut t = txn
            .open_table(user_rows_table_def(&user_rows_table_name(TABLE)))
            .expect("open rows");
        for (id, tag) in rows {
            let metadata =
                crate::row_codec::encode_scalar_columns(&schema, &[Value::Text(tag.to_string())])
                    .expect("encode");
            let encoded = encode_row(&RowInput {
                tenant_id: tenant,
                visibility: Visibility::Private,
                embedding: &[],
                metadata: &metadata,
            })
            .expect("encode row");
            t.insert((tenant, *id), encoded.as_slice()).expect("insert");
        }
    }
    crate::catalog::bump_table_generation_in_txn(&txn, TABLE).expect("bump");
    crate::recovery::commit_boundary::commit(txn).expect("commit seed");
}

/// テナントの全行を `(id, tag)` の昇順で返す。
fn rows_of(storage: &Storage, tenant: &str) -> Vec<(u64, String)> {
    use redb::ReadableDatabase;
    let schema = schema();
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
            Some(Value::Text(s)) => s.clone(),
            other => panic!("unexpected value {other:?}"),
        };
        out.push((id, tag));
    }
    out
}

fn tag_of(c: &DmlCandidate<'_>) -> String {
    let schema = schema();
    let cols = crate::row_codec::scan_scalar_columns(&schema, c.metadata).expect("scan");
    cols.first()
        .copied()
        .flatten()
        .and_then(|v| v.as_text())
        .unwrap_or_default()
        .to_string()
}

fn limits(chunk: usize, budget: usize, hold_ms: u64) -> PartitionedDmlLimits {
    PartitionedDmlLimits {
        chunk_rows: NonZeroUsize::new(chunk).expect("non-zero"),
        scan_budget_rows: NonZeroUsize::new(budget).expect("non-zero"),
        max_writer_hold: Duration::from_millis(hold_ms),
        interrupted_record_limit: std::num::NonZeroU64::new(1_000).expect("non-zero"),
    }
}

fn hash(tag: &[u8]) -> content_hash::ContentHash {
    content_hash::ContentHash::for_test(tag)
}

fn job_record(storage: &Storage, tenant: &str, op: &OperationId) -> Option<JobRecord> {
    let txn = storage.begin_write_txn().expect("begin");
    let c = ctx(tenant);
    let key = JobKey::for_context(&c, TABLE, op);
    partitioned_job::lookup_in_write_txn(&txn, &key).expect("lookup")
}

fn ledger_has(storage: &Storage, tenant: &str, op: &OperationId) -> bool {
    let txn = storage.begin_write_txn().expect("begin");
    ledger::lookup_in_write_txn(&txn, tenant, TABLE, op).expect("lookup")
}

fn generation(storage: &Storage) -> u64 {
    storage.table_generation(TABLE).expect("generation")
}

/// 実時間に依存しない疑似時計（呼び出しごとに 1 ms 進む）。
struct TickClock {
    base: Instant,
    ticks: Cell<u64>,
}

impl TickClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            ticks: Cell::new(0),
        }
    }
    fn now(&self) -> Instant {
        let t = self.ticks.get();
        self.ticks.set(t + 1);
        self.base + Duration::from_millis(t)
    }
}

type Pred = fn(&DmlCandidate<'_>) -> Result<bool, Infallible>;

fn match_x(c: &DmlCandidate<'_>) -> Result<bool, Infallible> {
    Ok(tag_of(c).starts_with('x'))
}

#[allow(clippy::too_many_arguments)]
fn run_delete(
    storage: &Storage,
    tenant: &str,
    op: &OperationId,
    h: &content_hash::ContentHash,
    lim: &PartitionedDmlLimits,
    max_total: Option<NonZeroUsize>,
    pred: impl FnMut(&DmlCandidate<'_>) -> Result<bool, Infallible>,
    hooks: &mut Hooks<'_>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<Infallible>> {
    run_partitioned(
        storage,
        TABLE,
        &ctx(tenant),
        LedgerWrite::Record(op),
        h,
        None,
        false,
        lim,
        max_total,
        Kind::Delete,
        pred,
        hooks,
    )
}

fn real_hooks<'a>(after: Option<&'a mut dyn FnMut(usize)>) -> Hooks<'a> {
    Hooks {
        clock: &Instant::now,
        after_chunk: after,
    }
}

fn op(s: &str) -> OperationId {
    OperationId::parse(s).expect("valid op id")
}

#[test]
fn delete_completes_over_multiple_chunks_and_bumps_generation_per_changed_chunk() {
    let (storage, _g) = setup("pdml-delete-complete");
    let rows: Vec<(u64, &str)> = (1..=10).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let gen_before = generation(&storage);
    let o = op("op-del-1");
    let outcome = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h1"),
        &limits(3, 1000, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect("completes");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 10 });
    assert!(rows_of(&storage, "t-a").is_empty());
    assert!(ledger_has(&storage, "t-a", &o));
    assert!(matches!(
        job_record(&storage, "t-a", &o),
        Some(JobRecord::Completed { total: 10, .. })
    ));
    // 10 行 / 幅 3 = 4 チャンク（3+3+3+1）。影響のあったチャンクごとに世代が進む。
    assert_eq!(generation(&storage), gen_before + 4);
}

#[test]
fn update_completes_and_leaves_non_matching_rows() {
    let (storage, _g) = setup("pdml-update-complete");
    seed(
        &storage,
        "t-a",
        &[(1, "x"), (2, "z"), (3, "x"), (4, "x"), (5, "z"), (6, "x")],
    );
    let o = op("op-upd-1");
    let assignments = [(0usize, Value::Text("y".to_string()))];
    let outcome = run_partitioned(
        &storage,
        TABLE,
        &ctx("t-a"),
        LedgerWrite::Record(&o),
        &hash(b"hu"),
        None,
        false,
        &limits(2, 1000, 1000),
        None,
        Kind::Update {
            assignments: &assignments,
        },
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect("completes");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 4 });
    let got = rows_of(&storage, "t-a");
    let tags: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(tags, vec!["y", "z", "y", "y", "z", "y"]);
}

#[test]
fn zero_match_completes_with_ledger_entry_and_completed_record() {
    let (storage, _g) = setup("pdml-zero-match");
    seed(&storage, "t-a", &[(1, "z"), (2, "z")]);
    let o = op("op-zero");
    let outcome = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hz"),
        &limits(10, 1000, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect("completes");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 0 });
    assert!(ledger_has(&storage, "t-a", &o));
    assert!(matches!(
        job_record(&storage, "t-a", &o),
        Some(JobRecord::Completed { total: 0, .. })
    ));
    assert_eq!(rows_of(&storage, "t-a").len(), 2);
}

#[test]
fn scan_budget_closes_chunks_without_too_many_rows_scanned() {
    let (storage, _g) = setup("pdml-scan-budget");
    let rows: Vec<(u64, &str)> = (1..=35).map(|i| (i, "z")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-budget");
    let mut chunks = 0usize;
    let mut hook = |_: usize| chunks += 1;
    let outcome = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hb"),
        &limits(1000, 10, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect("completes without TooManyRowsScanned");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 0 });
    // 走査予算 10 で 35 行: 10・10・10 でそれぞれ締め（コミット後フックは 3 回）、最後の 5 行で完了。
    assert_eq!(chunks, 3);
}

#[test]
fn zero_change_chunk_does_not_create_record_until_a_row_changes() {
    let (storage, _g) = setup("pdml-zero-change-no-record");
    let mut rows: Vec<(u64, &str)> = (1..=8).map(|i| (i, "z")).collect();
    rows.push((9, "x"));
    seed(&storage, "t-a", &rows);
    let o = op("op-nr");
    let seen: std::cell::RefCell<Vec<Option<JobRecord>>> = Default::default();
    let mut hook = |_: usize| seen.borrow_mut().push(job_record(&storage, "t-a", &o));
    run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hn"),
        &limits(5, 3, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect("completes");
    let seen = seen.into_inner();
    // 走査予算 3: id 1..3・4..6 は変更 0 件で記録を作らない。id 9 を含む第 3 チャンクで
    // 初めて記録（カーソル 9・件数 1）ができ、最後に空の範囲で完了する。
    assert_eq!(seen.len(), 3);
    assert!(seen.first().is_some_and(Option::is_none));
    assert!(seen.get(1).is_some_and(Option::is_none));
    assert!(matches!(
        seen.get(2),
        Some(Some(JobRecord::Running {
            cursor: 9,
            processed: 1,
            ..
        }))
    ));
    assert_eq!(rows_of(&storage, "t-a").len(), 8);
}

#[test]
fn zero_change_chunk_with_record_updates_cursor_only() {
    let (storage, _g) = setup("pdml-zero-change-with-record");
    let mut rows: Vec<(u64, &str)> = vec![(1, "x")];
    rows.extend((2..=8).map(|i| (i, "z")));
    rows.push((9, "x"));
    seed(&storage, "t-a", &rows);
    let o = op("op-wr");
    let seen: std::cell::RefCell<Vec<Option<JobRecord>>> = Default::default();
    let mut hook = |_: usize| seen.borrow_mut().push(job_record(&storage, "t-a", &o));
    run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hw"),
        &limits(5, 3, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect("completes");
    let seen = seen.into_inner();
    assert!(matches!(
        seen.first(),
        Some(Some(JobRecord::Running {
            cursor: 3,
            processed: 1,
            ..
        }))
    ));
    // 第 2 チャンク（id 4..6）は変更 0 件。記録のカーソルだけが進み、件数は据え置き。
    assert!(matches!(
        seen.get(1),
        Some(Some(JobRecord::Running {
            cursor: 6,
            processed: 1,
            ..
        }))
    ));
}

#[test]
fn hold_limit_closes_scan_with_fake_clock() {
    let (storage, _g) = setup("pdml-hold-scan");
    let rows: Vec<(u64, &str)> = (1..=30).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-hold");
    let clock = TickClock::new();
    let now = || clock.now();
    let mut chunks = 0usize;
    let mut hook = |_: usize| chunks += 1;
    let mut hooks = Hooks {
        clock: &now,
        after_chunk: Some(&mut hook),
    };
    // 1 呼び出し 1 ms 進む疑似時計と保持時間 5 ms で、幅（1000）に達する前に締まる。
    let outcome = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hh"),
        &limits(1000, 1000, 5),
        None,
        match_x as Pred,
        &mut hooks,
    )
    .expect("completes");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 30 });
    assert!(chunks >= 2, "hold limit must split the job: {chunks}");
    assert!(rows_of(&storage, "t-a").is_empty());
}

#[test]
fn hold_limit_cuts_apply_stage_and_cursor_is_last_applied_id() {
    let (storage, _g) = setup("pdml-hold-apply");
    let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-hold-apply");
    let jumped = Cell::new(false);
    let base = Instant::now();
    let now = || {
        if jumped.get() {
            base + Duration::from_secs(3600)
        } else {
            base
        }
    };
    // 走査の最後の行を評価した時点で時計を進める（以降の適用段で保持時間超過になる）。
    let pred = |c: &DmlCandidate<'_>| -> Result<bool, Infallible> {
        if c.id == 6 {
            jumped.set(true);
        }
        Ok(true)
    };
    let seen: std::cell::RefCell<Vec<Option<JobRecord>>> = Default::default();
    let mut hook = |_: usize| seen.borrow_mut().push(job_record(&storage, "t-a", &o));
    let mut hooks = Hooks {
        clock: &now,
        after_chunk: Some(&mut hook),
    };
    run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"hha"),
        &limits(1000, 1000, 100),
        None,
        pred,
        &mut hooks,
    )
    .expect("completes");
    let seen = seen.into_inner();
    // 第 1 チャンクは 1 件だけ適用し、カーソルは最後に適用した id（1）。
    assert!(matches!(
        seen.first(),
        Some(Some(JobRecord::Running {
            cursor: 1,
            processed: 1,
            ..
        }))
    ));
    assert!(rows_of(&storage, "t-a").is_empty());
}

#[test]
fn cancel_at_chunk_boundary_then_resume_skips_rows_before_cursor() {
    let (storage, _g) = setup("pdml-cancel-resume");
    let rows: Vec<(u64, &str)> = (1..=9).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-resume");
    let h = hash(b"hr");
    let c = ctx("t-a");
    let registry = std::sync::Arc::clone(storage.partitioned_job_registry());
    let mut hook = |n: usize| {
        if n == 1 {
            let key = JobKey::for_context(&c, TABLE, &o);
            assert!(registry.request_cancel(&key));
        }
    };
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(3, 1000, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect_err("cancelled at boundary");
    assert!(matches!(err.cause, PartitionedStopCause::CancelRequested));
    assert_eq!(err.committed_total, 3);
    assert_eq!(rows_of(&storage, "t-a").len(), 6);

    // 再送で再開する。カーソル（3）より前の行は走査しない。
    let min_seen = Cell::new(u64::MAX);
    let pred = |c: &DmlCandidate<'_>| -> Result<bool, Infallible> {
        min_seen.set(min_seen.get().min(c.id));
        Ok(true)
    };
    let outcome = run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(3, 1000, 1000),
        None,
        pred,
        &mut real_hooks(None),
    )
    .expect("resume completes");
    assert_eq!(outcome, PartitionedDmlOutcome::Completed { total_rows: 9 });
    assert_eq!(min_seen.get(), 4);
    assert!(rows_of(&storage, "t-a").is_empty());
}

#[test]
fn resend_after_completion_is_duplicate_or_content_mismatch() {
    let (storage, _g) = setup("pdml-resend");
    seed(&storage, "t-a", &[(1, "x")]);
    let o = op("op-resend");
    let h = hash(b"h-resend");
    run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(5, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect("first run");
    let dup = run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(5, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("duplicate");
    assert!(matches!(
        dup.cause,
        PartitionedStopCause::Write(TenantWriteError::DuplicateOperationId)
    ));
    let mismatch = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"other"),
        &limits(5, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("mismatch");
    assert!(matches!(
        mismatch.cause,
        PartitionedStopCause::Write(TenantWriteError::OperationIdContentMismatch)
    ));
}

#[test]
fn resend_of_cancelled_job_is_rejected() {
    let (storage, _g) = setup("pdml-resend-cancelled");
    let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-cancelled");
    let h = hash(b"h-cancelled");
    let c = ctx("t-a");
    let registry = std::sync::Arc::clone(storage.partitioned_job_registry());
    let mut hook = |n: usize| {
        if n == 1 {
            registry.request_cancel(&JobKey::for_context(&c, TABLE, &o));
        }
    };
    run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(2, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect_err("cancelled");
    let txn = storage.begin_write_txn().expect("begin");
    let key = JobKey::for_context(&c, TABLE, &o);
    assert_eq!(
        partitioned_job::cancel_in_txn(&txn, &key).expect("cancel"),
        Some(2)
    );
    crate::catalog::bump_table_generation_in_txn(&txn, TABLE).expect("bump");
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &h,
        &limits(2, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("cancelled resend");
    assert!(matches!(err.cause, PartitionedStopCause::ResendCancelled));
}

/// 通常の DML が同じ `operation_id` を確定した状況を、別ハッシュの台帳エントリで再現する。
fn inject_normal_ledger_entry(storage: &Storage, tenant: &str, o: &OperationId) {
    let txn = storage.begin_write_txn().expect("begin");
    ledger::record_in_txn(
        &txn,
        tenant,
        TABLE,
        LedgerWrite::Record(o),
        &hash(b"normal"),
    )
    .expect("record");
    crate::catalog::bump_table_generation_in_txn(&txn, TABLE).expect("bump");
    crate::recovery::commit_boundary::commit(txn).expect("commit");
}

#[test]
fn ledger_entry_between_chunks_stops_with_22023_when_no_job_record_exists() {
    // 記録あり（`Running` 記録がある間）は、通常の DML 側が台帳照合でジョブ表を引いて
    // 同じ `operation_id` を拒否するため、台帳エントリはそもそも作れない（#1127 の契約）。
    let (storage, _g) = setup("pdml-ledger-between-with");
    let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-led-1");
    let mut hook = |n: usize| {
        if n == 1 {
            let txn = storage.begin_write_txn().expect("begin");
            let res = ledger::record_in_txn(
                &txn,
                "t-a",
                TABLE,
                LedgerWrite::Record(&o),
                &hash(b"normal"),
            );
            assert!(res.is_err(), "normal DML must be refused while Running");
        }
    };
    run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-led"),
        &limits(2, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect("job still completes");

    // 記録なし（第 1 チャンクが変更 0 件）。
    let (storage, _g2) = setup("pdml-ledger-between-without");
    let mut rows: Vec<(u64, &str)> = (1..=8).map(|i| (i, "z")).collect();
    rows.push((9, "x"));
    seed(&storage, "t-a", &rows);
    let o = op("op-led-2");
    let mut hook = |n: usize| {
        if n == 1 {
            inject_normal_ledger_entry(&storage, "t-a", &o);
        }
    };
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-led2"),
        &limits(5, 3, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect_err("stopped");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::Write(TenantWriteError::OperationIdContentMismatch)
    ));
    assert_eq!(err.committed_total, 0);
    assert_eq!(rows_of(&storage, "t-a").len(), 9);
}

#[test]
fn pre_existing_ledger_entry_stops_first_chunk_with_zero_side_effects() {
    let (storage, _g) = setup("pdml-ledger-first");
    seed(&storage, "t-a", &[(1, "x"), (2, "x")]);
    let o = op("op-led-first");
    inject_normal_ledger_entry(&storage, "t-a", &o);
    let gen_before = generation(&storage);
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-first"),
        &limits(5, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("stopped");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::Write(TenantWriteError::OperationIdContentMismatch)
    ));
    assert_eq!(err.committed_total, 0);
    assert_eq!(rows_of(&storage, "t-a").len(), 2);
    assert_eq!(generation(&storage), gen_before);
    assert!(job_record(&storage, "t-a", &o).is_none());
}

#[test]
fn cumulative_affected_rows_limit_in_first_chunk_has_zero_side_effects() {
    let (storage, _g) = setup("pdml-limit-first");
    let rows: Vec<(u64, &str)> = (1..=10).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-lim-first");
    let gen_before = generation(&storage);
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-lf"),
        &limits(10, 100, 1000),
        NonZeroUsize::new(2),
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("limit");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::AffectedRowsLimitExceeded { .. }
    ));
    assert_eq!(err.committed_total, 0);
    assert_eq!(rows_of(&storage, "t-a").len(), 10);
    assert_eq!(generation(&storage), gen_before);
    assert!(!ledger_has(&storage, "t-a", &o));
    assert!(job_record(&storage, "t-a", &o).is_none());
}

#[test]
fn cumulative_affected_rows_limit_mid_job_leaves_committed_chunks_only() {
    let (storage, _g) = setup("pdml-limit-mid");
    let rows: Vec<(u64, &str)> = (1..=10).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let o = op("op-lim-mid");
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-lm"),
        &limits(3, 100, 1000),
        NonZeroUsize::new(5),
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("limit");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::AffectedRowsLimitExceeded { .. }
    ));
    // 第 1 チャンク（3 件）だけ commit 済み。超過する第 2 チャンクは適用しない。
    assert_eq!(err.committed_total, 3);
    assert_eq!(rows_of(&storage, "t-a").len(), 7);
}

#[test]
fn stale_expected_schema_is_rejected_before_any_change() {
    let (storage, _g) = setup("pdml-schema");
    seed(&storage, "t-a", &[(1, "x")]);
    let stale = TableSchema::new(
        TABLE,
        vec![ColumnDef::new("other", ColumnType::Text, false)],
    );
    let o = op("op-schema");
    let err = run_partitioned(
        &storage,
        TABLE,
        &ctx("t-a"),
        LedgerWrite::Record(&o),
        &hash(b"h-s"),
        Some(&stale),
        false,
        &limits(5, 100, 1000),
        None,
        Kind::Delete,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("schema changed");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::Write(TenantWriteError::Catalog(_))
    ));
    assert_eq!(rows_of(&storage, "t-a").len(), 1);
}

#[test]
fn alter_table_between_chunks_stops_the_job() {
    let (storage, _g) = setup("pdml-alter-between");
    let rows: Vec<(u64, &str)> = (1..=6).map(|i| (i, "x")).collect();
    seed(&storage, "t-a", &rows);
    let bound = storage.get_table_schema(TABLE).expect("schema");
    let o = op("op-alter");
    let mut hook = |n: usize| {
        if n == 1 {
            storage
                .alter_table_add_column(TABLE, ColumnDef::new("extra", ColumnType::Text, true))
                .expect("alter");
        }
    };
    let err = run_partitioned(
        &storage,
        TABLE,
        &ctx("t-a"),
        LedgerWrite::Record(&o),
        &hash(b"h-alt"),
        Some(&bound),
        false,
        &limits(2, 100, 1000),
        None,
        Kind::Delete,
        match_x as Pred,
        &mut real_hooks(Some(&mut hook)),
    )
    .expect_err("stopped by schema change");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::Write(TenantWriteError::Catalog(_))
    ));
    assert_eq!(err.committed_total, 2);
    assert_eq!(rows_of(&storage, "t-a").len(), 4);
}

#[test]
fn disabled_ledger_is_rejected_without_touching_anything() {
    let (storage, _g) = setup("pdml-no-ledger");
    seed(&storage, "t-a", &[(1, "x")]);
    let gen_before = generation(&storage);
    let err = run_partitioned(
        &storage,
        TABLE,
        &ctx("t-a"),
        LedgerWrite::Disabled,
        &hash(b"h-d"),
        None,
        false,
        &limits(5, 100, 1000),
        None,
        Kind::Delete,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("ledger required");
    assert!(matches!(err.cause, PartitionedStopCause::LedgerRequired));
    assert_eq!(err.committed_total, 0);
    assert_eq!(rows_of(&storage, "t-a").len(), 1);
    assert_eq!(generation(&storage), gen_before);
}

#[test]
fn already_registered_job_is_rejected() {
    let (storage, _g) = setup("pdml-already-running");
    seed(&storage, "t-a", &[(1, "x")]);
    let o = op("op-running");
    let c = ctx("t-a");
    let key = JobKey::for_context(&c, TABLE, &o);
    let _guard = storage
        .partitioned_job_registry()
        .try_register(&key)
        .expect("register");
    let err = run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-run"),
        &limits(5, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("already running");
    assert!(matches!(err.cause, PartitionedStopCause::AlreadyRunning));
    assert_eq!(rows_of(&storage, "t-a").len(), 1);
}

#[test]
fn other_tenant_rows_and_jobs_are_untouched() {
    let (storage, _g) = setup("pdml-tenant-isolation");
    seed(&storage, "t-a", &[(1, "x"), (2, "x"), (3, "x")]);
    seed(&storage, "t-b", &[(1, "x"), (2, "x"), (3, "x")]);
    let o = op("op-iso");
    run_delete(
        &storage,
        "t-a",
        &o,
        &hash(b"h-iso"),
        &limits(2, 100, 1000),
        None,
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect("completes");
    assert!(rows_of(&storage, "t-a").is_empty());
    assert_eq!(rows_of(&storage, "t-b").len(), 3);
    assert!(job_record(&storage, "t-b", &o).is_none());
    assert!(!ledger_has(&storage, "t-b", &o));
}

#[test]
fn unique_violation_rolls_back_only_the_failing_chunk() {
    let (storage, _g) = setup("pdml-unique");
    storage
        .alter_table_add_unique_constraint(TABLE, &["tag"])
        .expect("add unique");
    seed(&storage, "t-a", &[(1, "x1"), (2, "x2"), (3, "x3")]);
    let o = op("op-uniq");
    let assignments = [(0usize, Value::Text("k".to_string()))];
    let err = run_partitioned(
        &storage,
        TABLE,
        &ctx("t-a"),
        LedgerWrite::Record(&o),
        &hash(b"h-u"),
        None,
        false,
        &limits(1, 100, 1000),
        None,
        Kind::Update {
            assignments: &assignments,
        },
        match_x as Pred,
        &mut real_hooks(None),
    )
    .expect_err("unique violation");
    assert!(matches!(
        err.cause,
        PartitionedStopCause::Write(TenantWriteError::UniqueViolation)
    ));
    assert_eq!(err.committed_total, 1);
    let got = rows_of(&storage, "t-a");
    let tags: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(tags, vec!["k", "x2", "x3"]);
}
