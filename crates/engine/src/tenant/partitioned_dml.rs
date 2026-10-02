//! 述語形 `UPDATE`／`DELETE ... WHERE` の分割実行器（Issue #1128。ポインタ:
//! ADR `docs/design/partitioned-dml.md` §4・§5・§6・§7・§8・§15.2、
//! `docs/spec/04-behavior/recovery.md` RECOVER-11・RECOVER-12、RLS-9・RLS-10、
//! TABLE-3、INDEX-4、ERR-1・ERR-2・ERR-4）。
//!
//! 既存の 1 文 1 トランザクション実行（[`super::delete_rows_where_unchecked`]・
//! [`super::update_rows_where_unchecked`]）とは別に、キー順のカーソルで対象範囲を
//! 区切り、**チャンクごとに** writer gate を取り直して commit する非原子の実行器を
//! 提供する。各チャンクは 1 トランザクションで次をすべて行う。
//!
//! 1. 台帳照合（走査の前。同じ `operation_id` の台帳エントリがあれば `22023` で停止）
//! 2. テナント所有かつ可視の閉区間での走査と、その時点の行内容での述語再評価
//! 3. 適用・UNIQUE 索引・FK 即時検査（原子経路と共有する適用段。`apply_predicate_*_in_txn`）
//! 4. 影響 1 行以上ならテーブル世代の bump
//! 5. ジョブ記録（カーソル・累計件数。[`crate::recovery::partitioned_job`]）の保存と commit
//!
//! # 呼び出し元・呼び出し先の文脈
//!
//! - 呼び出し元: #1129 の `sql::exec`（SQL 構文 `PARTITIONED`・`CHUNK n` の結線時）。
//!   現状は未結線のため入口に `#[allow(dead_code)]` を付けている（結線時に外す）。
//!   入口は autocommit 専用の `&Storage` を取り、[`super::WriteTarget`] は取らない
//!   （明示トランザクション内での分割実行を型で不可能にする。#1129 が `25001` で拒否）。
//! - 呼び出し先: [`crate::storage::Storage::begin_partitioned_chunk_txn`]（通常の書き込みを
//!   優先する writer gate 取得）、`recovery::partitioned_job`（登録簿・ジョブ記録）、
//!   `recovery::ledger::lookup_in_write_txn`（台帳照合）。
//!
//! # 不変条件
//!
//! - 走査はテナントの閉区間 `(tenant, 0)..=(tenant, u64::MAX)` に限り、キーのテナントが
//!   変わったら打ち切る（他テナント領域のキー・値は読まない。RLS-9・RLS-10）。
//! - 走査行数は自テナントの行だけで数える（他テナントの行数が応答に表れない）。
//! - 1 チャンクの走査予算は `MAX_SCANNED_ROWS` 以下で、ジョブ全体には課さない。
//! - どの段で失敗しても write txn は commit せず drop（abort）する。停止原因と一緒に
//!   返す `committed_total` は commit 済みのチャンクの累計だけを表す。
//! - 失敗は fail-closed。ジョブ記録とメモリ上の状態の不一致は `XX000` 相当で止める。
//! - メッセージにテナント・テーブル・`operation_id`・行内容を含めない。

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use super::*;
use crate::recovery::partitioned_job::{
    self, chunk_range, ChunkCommitOutcome, JobKey, JobRecord, PartitionedJobError, ResendDecision,
};
use crate::sql::parser::PartitionedDmlLimits;

/// 分割実行の正常終了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartitionedDmlOutcome {
    /// 範囲を走査し尽くして完了した（`total_rows` は再開前の処理済み件数を含む累計）。
    Completed { total_rows: u64 },
}

/// 分割実行の停止。`committed_total` は commit 済みチャンクの累計件数（再開前の処理済み
/// 件数を含む）。#1129 は `committed_total > 0` なら部分完了（`VD001`）へ写像する。
#[derive(Debug)]
#[allow(dead_code)] // #1129 が停止原因を写像するまで読み手がない
pub(crate) struct PartitionedDmlFailure<E> {
    pub cause: PartitionedStopCause<E>,
    pub committed_total: u64,
}

/// 分割実行の停止原因。公開 enum は増やさず `pub(crate)` で閉じる。コードへの写像は
/// #1129 の担当（各 variant の想定コードを併記する）。
#[derive(Debug)]
#[allow(dead_code)] // #1129 が停止原因を写像するまで読み手がない
pub(crate) enum PartitionedStopCause<E> {
    /// 既存の書き込みエラー（`55P03` の gate 待機超過・`22023`・`23505`・制約違反・
    /// `XX000` 等。[`TenantWriteError`] の variant をそのまま再利用する）。
    Write(TenantWriteError),
    /// 呼び出し元が注入した述語クロージャのエラー。
    Predicate(E),
    /// `--max-dml-affected-rows` の明示指定をジョブ累計が超えるチャンクを適用せずに止めた。
    /// `committed_total == 0` なら `54000`（副作用ゼロ）。
    AffectedRowsLimitExceeded { limit: NonZeroUsize },
    /// 中断記録数の上限に達した（`54000`）。
    InterruptedRecordLimitReached,
    /// 同じキーのジョブがこのプロセスで実行中（`55P03`）。
    AlreadyRunning,
    /// 取り消し済みジョブへの再送（`VD002`）。
    ResendCancelled,
    /// チャンク境界で取り消し要求を検出した（`VD002`。取り消し済み記録の書き込みは #1129）。
    CancelRequested,
    /// 台帳を持たない構成（`LedgerWrite::Disabled`）での実行を拒否した（`0A000` を推奨）。
    LedgerRequired,
}

/// 述語形 `DELETE` の分割実行入口（#1129 が結線する）。
#[allow(clippy::too_many_arguments, dead_code)] // #1129 の SQL 表層が結線するまで未使用
pub(crate) fn delete_rows_where_partitioned_unchecked<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    content_hash_value: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    needs_embedding: bool,
    limits: &PartitionedDmlLimits,
    max_affected_total: Option<NonZeroUsize>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    run_partitioned(
        storage,
        table,
        ctx,
        ledger_write,
        content_hash_value,
        expected_schema,
        needs_embedding,
        limits,
        max_affected_total,
        Kind::Delete,
        predicate,
        &mut Hooks::production(),
    )
}

/// 述語形 `UPDATE` の分割実行入口（#1129 が結線する）。UPDATE は分割版のハッシュ
/// （新しいドメイン）だけを使い、旧ハッシュ互換（`legacy_hashes`）は持たない。
#[allow(clippy::too_many_arguments, dead_code)] // #1129 の SQL 表層が結線するまで未使用
pub(crate) fn update_rows_where_partitioned_unchecked<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    content_hash_value: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    assignments: &[(usize, crate::row_codec::Value)],
    needs_embedding: bool,
    limits: &PartitionedDmlLimits,
    max_affected_total: Option<NonZeroUsize>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    run_partitioned(
        storage,
        table,
        ctx,
        ledger_write,
        content_hash_value,
        expected_schema,
        needs_embedding,
        limits,
        max_affected_total,
        Kind::Update { assignments },
        predicate,
        &mut Hooks::production(),
    )
}

/// 実行種別。
#[derive(Clone, Copy)]
enum Kind<'a> {
    Delete,
    Update {
        assignments: &'a [(usize, crate::row_codec::Value)],
    },
}

/// 時計とチャンク間フックの注入点。本番は実時間・no-op、テストは疑似時計と
/// 「チャンク commit 後（txn の外）」の割り込みを差し込む。
struct Hooks<'a> {
    clock: &'a dyn Fn() -> Instant,
    after_chunk: Option<&'a mut dyn FnMut(usize)>,
}

impl Hooks<'static> {
    fn production() -> Self {
        Hooks {
            clock: &Instant::now,
            after_chunk: None,
        }
    }
}

/// チャンク間で持ち回るメモリ上の状態。commit が成功した後にだけ更新する。
struct JobState {
    /// 最後に走査した id（下端排他）。`None` は先頭から。
    cursor: Option<u64>,
    /// commit 済みの累計処理件数。
    processed: u64,
    /// ジョブ表に `Running` 記録があるか。
    has_record: bool,
    /// 最初のチャンクか（再送判定を行うのは最初のチャンクだけ）。
    first_chunk: bool,
}

fn w<E>(e: impl Into<TenantWriteError>) -> PartitionedStopCause<E> {
    PartitionedStopCause::Write(e.into())
}

fn from_predicate_err<E>(e: PredicateDmlError<E>) -> PartitionedStopCause<E> {
    match e {
        PredicateDmlError::Write(e) => PartitionedStopCause::Write(e),
        PredicateDmlError::Predicate(e) => PartitionedStopCause::Predicate(e),
    }
}

/// ジョブ記録層のエラーを停止原因へ写す。記録の破損・状態の不一致は台帳破損と同じ
/// `XX000` 固定の variant（クライアント入力の不正と取り違えない。fail-closed）。
fn map_job_err<E>(e: PartitionedJobError) -> PartitionedStopCause<E> {
    match e {
        PartitionedJobError::Corrupted(se) => {
            PartitionedStopCause::Write(TenantWriteError::LedgerCorrupted(se))
        }
        PartitionedJobError::StateConflict => {
            PartitionedStopCause::Write(TenantWriteError::LedgerCorrupted(StorageError::Codec(
                "partitioned job state conflict".to_string(),
            )))
        }
        PartitionedJobError::LedgerConflict(le) => PartitionedStopCause::Write(le.into()),
        PartitionedJobError::InterruptedRecordLimitReached => {
            PartitionedStopCause::InterruptedRecordLimitReached
        }
        PartitionedJobError::AlreadyRunning => PartitionedStopCause::AlreadyRunning,
    }
}

/// 実行器本体。`committed_total` を失敗に添えるため、ループ本体
/// （[`chunk_loop`]）の結果へ状態の累計を付けて返す。
#[allow(clippy::too_many_arguments)]
fn run_partitioned<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    hash: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    needs_embedding: bool,
    limits: &PartitionedDmlLimits,
    max_affected_total: Option<NonZeroUsize>,
    kind: Kind<'_>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
    hooks: &mut Hooks<'_>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    let mut state = JobState {
        cursor: None,
        processed: 0,
        has_record: false,
        first_chunk: true,
    };
    let result = chunk_loop(
        storage,
        table,
        ctx,
        ledger_write,
        hash,
        expected_schema,
        needs_embedding,
        limits,
        max_affected_total,
        kind,
        predicate,
        hooks,
        &mut state,
    );
    result.map_err(|cause| PartitionedDmlFailure {
        cause,
        committed_total: state.processed,
    })
}

#[allow(clippy::too_many_arguments)]
fn chunk_loop<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    hash: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    needs_embedding: bool,
    limits: &PartitionedDmlLimits,
    max_affected_total: Option<NonZeroUsize>,
    kind: Kind<'_>,
    mut predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
    hooks: &mut Hooks<'_>,
    state: &mut JobState,
) -> Result<PartitionedDmlOutcome, PartitionedStopCause<E>> {
    validate_identifier(table).map_err(w)?;
    // 台帳を持たない構成は、テーブルにもジョブ表にも触れる前に拒否する（fail-closed）。
    let LedgerWrite::Record(op_id) = ledger_write else {
        return Err(PartitionedStopCause::LedgerRequired);
    };
    let tenant = ctx.tenant_id();
    let key = JobKey::for_context(ctx, table, op_id);
    // 登録は関数の終わりまで保持する（drop で外れる。二重実行の拒否と取り消し要求の受け口）。
    let guard = storage
        .partitioned_job_registry()
        .try_register(&key)
        .map_err(map_job_err)?;
    let clock = hooks.clock;
    let mut chunk_index: usize = 0;

    loop {
        if guard.cancel_requested() {
            return Err(PartitionedStopCause::CancelRequested);
        }
        // 通常の書き込みが待機している間は取得されない（ADR §8.2）。失敗は `55P03`。
        let write_txn = storage
            .begin_partitioned_chunk_txn()
            .map_err(|e| w(convert_write_txn_err(e)))?;
        // 保持時間は permit を得てから数える（待機時間を保持時間に含めない）。
        let start = clock();

        let schema = require_table_schema_write(&write_txn, table).map_err(w)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(w(CatalogError::Invalid(
                    "table schema changed after the statement was bound".to_string(),
                )));
            }
        }
        if let Kind::Update { assignments } = kind {
            validate_set_assignments(&schema, assignments).map_err(w)?;
        }

        // ジョブ記録の照合。最初のチャンクは再送判定、以降はメモリ上の状態との一致確認。
        let record = partitioned_job::lookup_in_write_txn(&write_txn, &key).map_err(map_job_err)?;
        if state.first_chunk {
            match partitioned_job::classify_resend(record.as_ref(), hash, false) {
                ResendDecision::Fresh => {}
                ResendDecision::Resume { cursor, processed } => {
                    state.cursor = Some(cursor);
                    state.processed = processed;
                    state.has_record = true;
                }
                ResendDecision::Duplicate => return Err(w(TenantWriteError::DuplicateOperationId)),
                ResendDecision::ContentMismatch => {
                    return Err(w(TenantWriteError::OperationIdContentMismatch))
                }
                ResendDecision::Cancelled => return Err(PartitionedStopCause::ResendCancelled),
                ResendDecision::AlreadyRunning => return Err(PartitionedStopCause::AlreadyRunning),
            }
            state.first_chunk = false;
        } else {
            let consistent = match (&record, state.has_record) {
                (None, false) => true,
                (
                    Some(JobRecord::Running {
                        hash: stored,
                        cursor,
                        processed,
                    }),
                    true,
                ) => {
                    hash.matches(stored)
                        && Some(*cursor) == state.cursor
                        && *processed == state.processed
                }
                _ => false,
            };
            if !consistent {
                // 想定外の記録の変化は止める（fail-closed）。
                return Err(map_job_err(PartitionedJobError::StateConflict));
            }
        }

        // 台帳照合（走査の前）。通常の DML が同じ `operation_id` を確定していたら止める。
        if ledger::lookup_in_write_txn(&write_txn, tenant, table, op_id).map_err(w)? {
            return Err(w(TenantWriteError::OperationIdContentMismatch));
        }

        // 1 チャンクの適用行数の幅。累計上限があれば「超過を判定できる最小の 1 件超過分」
        // まで（`remaining + 1`）に絞る。
        let mut width = limits.chunk_rows.get();
        if let Some(limit) = max_affected_total {
            let processed = usize::try_from(state.processed).unwrap_or(usize::MAX);
            let remaining = limit.get().saturating_sub(processed);
            width = width.min(remaining.saturating_add(1));
        }

        let scan = scan_chunk(
            &write_txn,
            table,
            ctx,
            state.cursor,
            needs_embedding,
            width,
            limits.scan_budget_rows.get(),
            limits.max_writer_hold,
            start,
            clock,
            &mut predicate,
        )?;
        let ScanResult {
            candidates,
            last_scanned,
            mut exhausted,
        } = scan;

        // ジョブ累計の影響行数上限（明示指定時のみ）。超過するチャンクは適用しない。
        if let Some(limit) = max_affected_total {
            let total = state
                .processed
                .saturating_add(u64::try_from(candidates.len()).unwrap_or(u64::MAX));
            if total > u64::try_from(limit.get()).unwrap_or(u64::MAX) {
                return Err(PartitionedStopCause::AffectedRowsLimitExceeded { limit });
            }
        }

        // 適用（原子経路と共有する適用段）。保持時間を超えたら残りを次チャンクへ回す。
        let mut stop_by_hold =
            || clock().saturating_duration_since(start) >= limits.max_writer_hold;
        let fk_mode = crate::constraint::FkCheckMode::All;
        let applied = match kind {
            Kind::Delete => apply_predicate_delete_in_txn::<E>(
                &write_txn,
                table,
                ctx,
                &schema,
                &candidates,
                fk_mode,
                None,
                Some(&mut stop_by_hold),
            ),
            Kind::Update { assignments } => apply_predicate_update_in_txn::<E>(
                &write_txn,
                table,
                ctx,
                &schema,
                &candidates,
                assignments,
                fk_mode,
                None,
                Some(&mut stop_by_hold),
            ),
        }
        .map_err(from_predicate_err)?;

        // 打ち切った場合のカーソルは「最後に適用した候補の id」（それ以降の行は次の
        // チャンクで再走査・再評価される）。
        let new_cursor = if applied < candidates.len() {
            exhausted = false;
            applied
                .checked_sub(1)
                .and_then(|i| candidates.get(i))
                .copied()
        } else {
            last_scanned
        };
        let rows_changed = u64::try_from(applied).unwrap_or(u64::MAX);

        if applied > 0 {
            crate::catalog::bump_table_generation_in_txn(&write_txn, table).map_err(w)?;
        }

        let completed_total = if exhausted {
            Some(
                partitioned_job::complete_in_txn(&write_txn, &key, hash, rows_changed)
                    .map_err(map_job_err)?,
            )
        } else {
            // `!exhausted` なら 1 行以上走査済みでカーソルが存在する。
            let Some(cursor) = new_cursor else {
                return Err(map_job_err(PartitionedJobError::StateConflict));
            };
            match partitioned_job::record_chunk_progress_in_txn(
                &write_txn,
                &key,
                hash,
                rows_changed,
                cursor,
                limits.interrupted_record_limit.get(),
            )
            .map_err(map_job_err)?
            {
                ChunkCommitOutcome::NoRecord => {
                    // 記録がなく変更も 0 件: 何も書かずカーソルだけメモリ上で進める。
                    drop(write_txn);
                    state.cursor = Some(cursor);
                    chunk_index = chunk_index.saturating_add(1);
                    if let Some(f) = hooks.after_chunk.as_mut() {
                        f(chunk_index);
                    }
                    continue;
                }
                ChunkCommitOutcome::Created => {
                    state.has_record = true;
                    None
                }
                ChunkCommitOutcome::Updated => None,
            }
        };

        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(|e| w(TenantWriteError::from(e)))?;

        // commit 後にだけメモリ上の状態を更新する。
        state.processed = state.processed.saturating_add(rows_changed);
        if let Some(total) = completed_total {
            state.processed = total;
            return Ok(PartitionedDmlOutcome::Completed { total_rows: total });
        }
        state.cursor = new_cursor;
        chunk_index = chunk_index.saturating_add(1);
        if let Some(f) = hooks.after_chunk.as_mut() {
            f(chunk_index);
        }
    }
}

/// 1 チャンクの走査結果。
struct ScanResult {
    /// 述語に一致した id（昇順。長さは高々チャンク幅）。
    candidates: Vec<u64>,
    /// このチャンクで最後に走査した id（1 行も走査しなければ `None`）。
    last_scanned: Option<u64>,
    /// 範囲を走査し尽くしたか。
    exhausted: bool,
}

/// カーソルの次からテナント所有かつ可視の範囲を走査し、述語に一致した id を集める。
/// 適用行数がチャンク幅・走査行数が走査予算・writer 保持時間が上限に達したら締める
/// （判定は 1 行以上走査した後）。メモリは候補 id（チャンク幅以下）と 1 行分の作業領域
/// だけで、対象行数に比例しない。
#[allow(clippy::too_many_arguments)]
fn scan_chunk<E>(
    write_txn: &redb::WriteTransaction,
    table: &str,
    ctx: &PolicyContext,
    cursor: Option<u64>,
    needs_embedding: bool,
    width: usize,
    scan_budget: usize,
    max_hold: Duration,
    start: Instant,
    clock: &dyn Fn() -> Instant,
    predicate: &mut impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<ScanResult, PartitionedStopCause<E>> {
    let tenant = ctx.tenant_id();
    let Some(range) = chunk_range(tenant, cursor) else {
        // `u64::MAX` まで走査済み（記録としては取り得ないが、取った場合も走査し尽くし）。
        return Ok(ScanResult {
            candidates: Vec::new(),
            last_scanned: None,
            exhausted: true,
        });
    };
    let row_table_name = user_rows_table_name(table);
    let row_table = write_txn
        .open_table(user_rows_table_def(&row_table_name))
        .map_err(|e| w(map_row_table_error(e)))?;
    let mut candidates: Vec<u64> = Vec::with_capacity(width.min(4096));
    let mut embedding_scratch: Vec<f32> = Vec::new();
    let mut scanned: usize = 0;
    let mut last_scanned: Option<u64> = None;
    let mut exhausted = true;

    for entry in row_table
        .range::<(&str, u64)>(range)
        .map_err(|e| w(CatalogError::from(e)))?
    {
        let (k, v) = entry.map_err(|e| w(CatalogError::from(e)))?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant {
            // 閉区間により理論上到達しない多層防御（他テナント領域は読まない）。
            break;
        }
        scanned = scanned.saturating_add(1);
        last_scanned = Some(id);

        let hit: bool = 'row: {
            let buf = v.value();
            let (row_tenant, visibility, offset) =
                crate::storage::decode_row_header(buf).map_err(w)?;
            crate::storage::verify_row_key_tenant(key_tenant, row_tenant).map_err(w)?;
            // 可視性で数え方を変えない（走査数は加算済み）。不可視行の本体は読まない。
            if !ctx.is_owner(row_tenant) || !ctx.is_visible(row_tenant, visibility) {
                break 'row false;
            }
            let (dim, metadata): (u32, &[u8]) = if needs_embedding {
                crate::storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                    .map_err(w)?
            } else {
                crate::storage::decode_row_dim_and_metadata_borrowed(buf).map_err(w)?
            };
            let embedding: &[f32] = if needs_embedding {
                embedding_scratch.as_slice()
            } else {
                &[]
            };
            predicate(&DmlCandidate {
                id,
                dim,
                embedding,
                metadata,
            })
            .map_err(PartitionedStopCause::Predicate)?
        };
        if hit {
            candidates.push(id);
        }

        let close = candidates.len() >= width
            || scanned >= scan_budget
            || clock().saturating_duration_since(start) >= max_hold;
        if close {
            // 最後の id まで走査した場合のみ、その先に行は存在しない。
            exhausted = id == u64::MAX;
            break;
        }
    }
    Ok(ScanResult {
        candidates,
        last_scanned,
        exhausted,
    })
}

#[cfg(test)]
mod tests;
