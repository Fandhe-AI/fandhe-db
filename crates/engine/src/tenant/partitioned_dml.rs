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
//! - 呼び出し元: `EngineCore`（`core.rs`）の述語形 `UPDATE`／`DELETE ... PARTITIONED`
//!   実行 form（Issue #1129）。入口は autocommit 専用の `&Storage` を取り、
//!   [`super::WriteTarget`] は取らない（明示トランザクション内での分割実行を型で不可能に
//!   する。トランザクション内・複数文メッセージ内の指定は SQL 表層が `25001` で拒否）。
//!   停止原因のエラーコード写像は `sql::partitioned::map_partitioned_failure`。
//! - 呼び出し先: [`crate::storage::Storage::begin_partitioned_chunk_txn`]（通常の書き込みを
//!   優先する writer gate 取得）、`recovery::partitioned_job`（登録簿・ジョブ記録）、
//!   `recovery::ledger::lookup_in_write_txn`（台帳照合）。
//!
//! # 不変条件
//!
//! - 走査はテナントの閉区間 `(tenant, RowId::MIN)..=(tenant, RowId::MAX)`（物理 rowid の閉区間）に限り、キーのテナントが
//!   変わったら打ち切る（他テナント領域のキー・値は読まない。RLS-9・RLS-10）。
//! - 走査行数は自テナントの行だけで数える（他テナントの行数が応答に表れない）。
//! - 1 チャンクの走査予算はハード上限以下で、ジョブ全体には課さない（`--max-scanned-rows` と非連動）。
//! - writer 保持時間の上限（`max_writer_hold`）は**行境界での打ち切り判定**であり、
//!   走査の各行の後と適用の各行の前にだけ時計を見る。上限を超えて保持し得るのは、
//!   最後に判定を通過した 1 行の適用（UNIQUE 索引・FK 即時検査を含む）と、世代 bump・
//!   ジョブ記録の保存・commit（いずれも行数に比例しない固定段）だけで、時計では中断
//!   できない（redb の書き込みは途中で打ち切ると不整合になるため、段の途中では切らない）。
//!   通常の書き込みの待ちは「上限 × 2 ＋ この非中断段の所要」で見積もる（ADR §15.2）。
//! - チャンク間隔（`chunk_interval`。SQL-77、Issue #2071）の待機は、チャンクの commit（または
//!   空振りチャンクの txn 破棄）の後・次チャンク開始前にだけ置く。最初のチャンクの前と最後の
//!   チャンクの後では待たない。待機中は write txn と writer permit を保持せず（通常の書き込みが
//!   取れる）、`max_writer_hold` の計測にも含まれない。一方で登録簿の登録（`JobGuard`）は保持し、
//!   `SHOW` は `running` のまま・同時実行数の枠も占有する。待機は `CHUNK_INTERVAL_POLL_SLICE`
//!   刻みで取り消しと文の期限を確認し（反映遅れは高々 1 刻み）、待機時間は文の期限の経過に含める。
//!   新しい停止原因は設けず、既存の `CancelRequested`・`DeadlineExceeded` を再利用する。
//! - 実行時間の上限（`max_run_time`。SQL-77 (3)、Issue #2072）は 1 回の実行にかかり（再送で 0 から
//!   数え直す）、0（既定）なら時計も読まない。判定はチャンクの write txn 内の記録段の前で、
//!   `経過 + chunk_interval >= 上限` なら**そのチャンクを commit したうえで**止める（実行中の
//!   チャンクは打ち切らず、最初のチャンクは必ず実行する）。走査し尽くしたチャンクは上限より
//!   完了を優先する。記録がなく変更 0 件でも再開用の `Running` を作る（唯一の例外）。止める際は
//!   `chunk_interval` を待たない。取り消し・文の期限の確認は上限停止より先に評価される。
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
use crate::sql::privilege_gate::write::RelationPrivilegeRecheck;
use crate::storage::rowid::{split_row_key, RowId};

/// 分割実行の正常終了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartitionedDmlOutcome {
    /// 範囲を走査し尽くして完了した（`total_rows` は再開前の処理済み件数を含む累計）。
    Completed { total_rows: u64 },
}

/// 分割実行の停止。`committed_total` は commit 済みチャンクの累計件数（再開前の処理済み
/// 件数を含む。実際の影響行数のまま改変しない）。`side_effects_committed` は件数 0 でもトリガの
/// 別表書き込み等の副作用を commit 済みかを表す。SQL 表層は `committed_total > 0` または
/// `side_effects_committed` なら部分完了（`VD001`）へ写像する。
#[derive(Debug)]
pub(crate) struct PartitionedDmlFailure<E> {
    pub cause: PartitionedStopCause<E>,
    pub committed_total: u64,
    pub side_effects_committed: bool,
}

/// 分割実行の停止原因。公開 enum は増やさず `pub(crate)` で閉じる。コードへの写像は
/// `sql::partitioned::map_partitioned_failure`（各 variant の想定コードを併記する）。
#[derive(Debug)]
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
    /// 同時実行数の上限（テナント単位・全体）に達した（`55P03`。どちらの上限でも同じ）。
    ConcurrencyLimitReached,
    /// 取り消し済みジョブへの再送（`VD002`。`committed` は取り消し時点の件数）。
    ResendCancelled { committed: u64 },
    /// チャンク境界で取り消し要求を検出した（`VD002`。取り消し済み記録は
    /// [`finalize_cancel`] が書いてから返す）。
    CancelRequested,
    /// 台帳を持たない構成（`LedgerWrite::Disabled`）での実行を拒否した（`0A000` を推奨）。
    LedgerRequired,
    /// チャンクの write txn 内の権限再判定で、対象表の必要権限が無かった（`42501`。Issue #1968）。
    /// 第 1 チャンクではジョブ記録・台帳より前（commit 前）に止まり副作用はゼロ。途中で `REVOKE` された
    /// 場合は commit 済みチャンクを残して止まる（`VD001`。同じ `operation_id` の再送で再開できる）。
    InsufficientPrivilege,
    /// 文の期限（Issue #1953。SQL-71 のポインタ）を超過した。実行中のチャンクは commit せず破棄して
    /// 止める。`committed_total == 0` なら `57014`（副作用ゼロ）、commit 済みチャンクがあれば `VD001`
    /// （原因 `57014`。同じ `operation_id` の再送で再開できる）。`suppressed` 区間では共有の確認点が
    /// 評価されないため、期限は注入時計（`Hooks::clock`）で明示評価する。
    DeadlineExceeded,
    /// 1 回の実行の時間上限（`max_run_time`。SQL-77 (3)、Issue #2072）に達し、チャンク境界で
    /// 止めた。直前のチャンクは commit 済みで、再開用の `Running` 記録も残してある。commit 済み
    /// 件数が 0 でも `VD001`（原因クラス `54000`・補足文言つき）へ写像する。
    RunTimeLimitReached,
}

/// 述語形 `DELETE` の分割実行入口（`core.rs` の分割実行 form から呼ばれる）。
#[allow(clippy::too_many_arguments)]
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
    fixed_time: Option<i64>,
    privilege_recheck: Option<RelationPrivilegeRecheck>,
    // 行トリガの通過証（Issue #3283）。SQL 表層だけが `Some`、NoSQL 表層は `None`（該当トリガがあれば `0A000`）。
    triggers: Option<&crate::trigger::TriggerRuntime>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    // Issue #1798: チャンク commit 後に 57014 を返すと SQL-32／SQL-71 (4) の契約（commit 済みなら
    // `VD001`）とずれるため、ジョブ全体を協調的中断（切断）の対象外にする。文の期限は Issue #1953 で
    // `chunk_loop` が注入時計により明示評価する（`PartitionedStopCause::DeadlineExceeded`）。
    crate::interrupt::suppressed(|| {
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
            fixed_time,
            Kind::Delete,
            triggers,
            predicate,
            &mut Hooks::production(privilege_recheck),
        )
    })
}

/// 述語形 `UPDATE` の分割実行入口（`core.rs` の分割実行 form から呼ばれる）。UPDATE は
/// 分割版のハッシュ（新しいドメイン）だけを使い、旧ハッシュ互換（`legacy_hashes`）は
/// 持たない。
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_rows_where_partitioned_unchecked<E>(
    storage: &Storage,
    table: &str,
    ctx: &PolicyContext,
    ledger_write: LedgerWrite<'_>,
    content_hash_value: &content_hash::ContentHash,
    expected_schema: Option<&crate::catalog::TableSchema>,
    assignments: &[(usize, crate::row_codec::Value)],
    // SET 右辺が式の項（Issue #1544・SQL-45 のポインタ）。適用段（原子経路と共有）が
    // 行ごとに更新前の値で評価するため、チャンク境界をまたいでも各行に 1 回だけ適用される。
    set_exprs: &[(usize, crate::sql::set_expr::BoundSetExpr)],
    needs_embedding: bool,
    limits: &PartitionedDmlLimits,
    max_affected_total: Option<NonZeroUsize>,
    fixed_time: Option<i64>,
    privilege_recheck: Option<RelationPrivilegeRecheck>,
    // 行トリガの通過証（Issue #3283）。SQL 表層だけが `Some`、NoSQL 表層は `None`（該当トリガがあれば `0A000`）。
    triggers: Option<&crate::trigger::TriggerRuntime>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    // Issue #1798: チャンク commit 後に 57014 を返すと SQL-32／SQL-71 (4) の契約（commit 済みなら
    // `VD001`）とずれるため、ジョブ全体を協調的中断（切断）の対象外にする。文の期限は Issue #1953 で
    // `chunk_loop` が注入時計により明示評価する（`PartitionedStopCause::DeadlineExceeded`）。
    crate::interrupt::suppressed(|| {
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
            fixed_time,
            Kind::Update {
                assignments,
                set_exprs,
            },
            triggers,
            predicate,
            &mut Hooks::production(privilege_recheck),
        )
    })
}

/// 実行種別。
#[derive(Clone, Copy)]
enum Kind<'a> {
    Delete,
    Update {
        assignments: &'a [(usize, crate::row_codec::Value)],
        set_exprs: &'a [(usize, crate::sql::set_expr::BoundSetExpr)],
    },
}

/// チャンク間隔の待機を小分けにする刻み。取り消し要求の反映遅れの上限になる（Issue #2071）。
const CHUNK_INTERVAL_POLL_SLICE: Duration = Duration::from_millis(10);

/// 時計・sleep とチャンク間フックの注入点。本番は実時間・no-op、テストは疑似時計と
/// 「チャンク commit 後（txn の外）」の割り込みを差し込む。
struct Hooks<'a> {
    clock: &'a dyn Fn() -> Instant,
    /// チャンク間隔の待機（1 刻み分）。テストは疑似時計を進めるクロージャを差し込む。
    sleep: &'a dyn Fn(Duration),
    after_chunk: Option<&'a mut dyn FnMut(usize)>,
    /// チャンクごとの権限再判定（Issue #1968）。現在ロール束縛済みで運用者権限なしの主体だけ `Some`。
    privilege: Option<RelationPrivilegeRecheck>,
}

impl Hooks<'static> {
    fn production(privilege: Option<RelationPrivilegeRecheck>) -> Self {
        Hooks {
            clock: &Instant::now,
            sleep: &std::thread::sleep,
            after_chunk: None,
            privilege,
        }
    }
}

/// チャンク間で持ち回るメモリ上の状態。commit が成功した後にだけ更新する。
struct JobState {
    /// 最後に走査した物理 rowid（下端排他）。`None` は先頭から（TABLE-23 (5)）。
    cursor: Option<RowId>,
    /// commit 済みの累計処理件数。
    processed: u64,
    /// 影響行数 0 でもトリガの別表書き込み等の副作用を commit 済みか（再開時は記録の存在から復元）。
    side_effects_committed: bool,
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
        PartitionedJobError::ConcurrencyLimitReached => {
            PartitionedStopCause::ConcurrencyLimitReached
        }
    }
}

/// 取り消し検出時に、中断（`Running`）記録があれば `Cancelled` へ縮めて commit する。
/// 実行中の write txn は呼び出し前に drop 済みであること。記録がなければ何も書かない
/// （最初の変更チャンクより前の取り消しは記録を残さず、`operation_id` を再利用できる）。
/// 判定はメモリ上の `has_record` ではなく永続記録で行う（再開ジョブがループ先頭で
/// 取り消された場合、`has_record` はまだ false のため）。失敗は呼び出し元へ返し、
/// 記録は中断のまま残る（再度の `CANCEL` で取り消せる）。
fn finalize_cancel(storage: &Storage, key: &JobKey<'_>) -> Result<(), TenantWriteError> {
    let has_running = {
        let read_txn = storage
            .db()
            .begin_read()
            .map_err(|e| TenantWriteError::from(StorageError::from(e)))?;
        matches!(
            partitioned_job::lookup_in_read_txn(&read_txn, key)
                .map_err(|e| TenantWriteError::LedgerCorrupted(e.into_storage_error()))?,
            Some(JobRecord::Running { .. })
        )
    };
    if !has_running {
        return Ok(());
    }
    let txn = storage.begin_write_txn().map_err(convert_write_txn_err)?;
    partitioned_job::cancel_in_txn(&txn, key)
        .map_err(|e| TenantWriteError::LedgerCorrupted(e.into_storage_error()))?;
    crate::recovery::commit_boundary::commit(txn).map_err(TenantWriteError::from)
}

/// チャンク間隔の待機（SQL-77、Issue #2071）。`interval` が 0 なら時計も読まず即座に戻る
/// （既定設定で既存の挙動を変えない）。writer gate は保持しない状態で呼ぶこと。刻みごとに
/// 取り消し・文の期限を確認し、期限が先に来るなら最後の刻みを残り時間へ縮める。
fn wait_chunk_interval<E>(
    interval: Duration,
    hooks: &Hooks<'_>,
    deadline: crate::interrupt::StatementDeadline,
    cancel_requested: &dyn Fn() -> bool,
    on_cancel: &dyn Fn() -> PartitionedStopCause<E>,
) -> Result<(), PartitionedStopCause<E>> {
    let mut remaining = interval;
    while !remaining.is_zero() {
        if cancel_requested() {
            return Err(on_cancel());
        }
        let now = (hooks.clock)();
        if deadline.is_expired_at(now) {
            return Err(PartitionedStopCause::DeadlineExceeded);
        }
        let mut slice = remaining.min(CHUNK_INTERVAL_POLL_SLICE);
        if let Some(left) = deadline.remaining_at(now) {
            slice = slice.min(left);
        }
        if slice.is_zero() {
            // 期限ちょうど: 空回りさせず期限超過として止める。
            return Err(PartitionedStopCause::DeadlineExceeded);
        }
        (hooks.sleep)(slice);
        remaining = remaining.saturating_sub(slice);
    }
    Ok(())
}

/// 実行時間の上限に達したか（SQL-77 (3)、Issue #2072）。次のチャンクの前に `chunk_interval` を
/// 待つ分を先読みして加え、待った直後の境界で上限を超えているなら待たずに止める。overflow は
/// `saturating_*` で潰す。`max_run` が 0（無効）なら常に `false`。
fn run_time_limit_reached(
    max_run: Duration,
    interval: Duration,
    run_start: Instant,
    now: Instant,
) -> bool {
    if max_run.is_zero() {
        return false;
    }
    now.saturating_duration_since(run_start)
        .saturating_add(interval)
        >= max_run
}

/// 取り消し検出時の共通終了処理。記録の書き込みに失敗した場合はその原因を返す
/// （committed_total > 0 なら呼び出し側が部分完了として報告する）。
fn cancel_stop<E>(storage: &Storage, key: &JobKey<'_>) -> PartitionedStopCause<E> {
    match finalize_cancel(storage, key) {
        Ok(()) => PartitionedStopCause::CancelRequested,
        Err(e) => PartitionedStopCause::Write(e),
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
    fixed_time: Option<i64>,
    kind: Kind<'_>,
    triggers: Option<&crate::trigger::TriggerRuntime>,
    predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
    hooks: &mut Hooks<'_>,
) -> Result<PartitionedDmlOutcome, PartitionedDmlFailure<E>> {
    let mut state = JobState {
        cursor: None,
        processed: 0,
        side_effects_committed: false,
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
        fixed_time,
        kind,
        triggers,
        predicate,
        hooks,
        &mut state,
    );
    result.map_err(|cause| PartitionedDmlFailure {
        cause,
        // 件数は実値のまま渡し、影響行数 0 でも副作用を commit 済みかは別フラグで渡す
        // （ジョブ記録・SHOW・エラー応答の件数を一致させる）。
        committed_total: state.processed,
        side_effects_committed: state.side_effects_committed,
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
    fixed_time: Option<i64>,
    kind: Kind<'_>,
    triggers: Option<&crate::trigger::TriggerRuntime>,
    mut predicate: impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
    hooks: &mut Hooks<'_>,
    state: &mut JobState,
) -> Result<PartitionedDmlOutcome, PartitionedStopCause<E>> {
    validate_relation_key(table).map_err(w)?;
    // 台帳を持たない構成は、テーブルにもジョブ表にも触れる前に拒否する（fail-closed）。
    let Some(op_id) = ledger_write.op_id() else {
        return Err(PartitionedStopCause::LedgerRequired);
    };
    let tenant = ctx.tenant_id();
    let key = JobKey::for_context(ctx, table, op_id);
    // 登録は関数の終わりまで保持する（drop で外れる。二重実行の拒否と取り消し要求の受け口）。
    // 同時実行数の判定と登録は 1 つのロックの中で原子的に行う（並行起動でも上限を超えない）。
    let guard = storage
        .partitioned_job_registry()
        .try_register_with_limits(
            &key,
            limits.max_jobs_per_tenant.get(),
            limits.max_jobs_total.get(),
        )
        .map_err(map_job_err)?;
    let clock = hooks.clock;
    // 文の期限（Issue #1953）。`suppressed` 区間では `interrupt::check` が期限を評価しないため、
    // ジョブ開始時に値（`Copy`）を読み、注入時計で各確認点を明示評価する。
    let deadline = crate::interrupt::current_deadline();
    let mut chunk_index: usize = 0;
    // 1 回の実行の開始時刻（再送は `chunk_loop` を新たに呼ぶので 0 から数え直す）。上限が無効なら
    // 時計を余分に読まない。
    let run_start = (!limits.max_run_time.is_zero()).then(clock);

    // 再開時の commit 済み累計を、取り消し・gate 待機・スキーマ検査など最初のチャンクの
    // どの早期失敗よりも前にメモリ状態へ復元する（失敗に添える `committed_total` が
    // 0 になり「副作用ゼロ」と誤認されるのを防ぐ。#1129 は `> 0` を部分完了へ写像する）。
    // 判定の正は最初のチャンクの write txn 内の照合（そこで同じ値が再設定される）。
    // 同じキーのジョブは登録簿により本プロセスで 1 つだけなので、ここと照合の間に
    // 記録が変わることはない。ハッシュ不一致・完了済み・取り消し済みは復元しない
    // （それらは書き込みを行わずに止まるため 0 が正しい）。
    {
        let read_txn = storage
            .db()
            .begin_read()
            .map_err(|e| w(StorageError::from(e)))?;
        if let Some(JobRecord::Running {
            hash: stored,
            processed,
            trigger_effects,
            ..
        }) = partitioned_job::lookup_in_read_txn(&read_txn, &key).map_err(map_job_err)?
        {
            if hash.matches(&stored) {
                state.processed = processed;
                // 件数が 0 でも、記録にトリガの別表書き込みの印があれば副作用は commit 済み。
                // 印のない 0 件の記録（実行時間の上限による停止点）は副作用ゼロのまま（Issue #3283）。
                state.side_effects_committed = processed > 0 || trigger_effects;
            }
        }
    }

    loop {
        if guard.cancel_requested() {
            return Err(cancel_stop(storage, &key));
        }
        // 期限の確認点 (a): チャンク開始前（write txn を取る前。副作用なし）。
        if deadline.is_expired_at(clock()) {
            return Err(PartitionedStopCause::DeadlineExceeded);
        }
        // 通常の書き込みが待機している間は取得されない（ADR §8.2）。失敗は `55P03`。待機は文の期限で
        // 短縮され（`Storage` 側）、期限が先に来たなら `55P03` ではなく期限超過として止める。
        let write_txn = match storage.begin_partitioned_chunk_txn() {
            Ok(t) => t,
            Err(e) => {
                if matches!(
                    e,
                    StorageError::WriteLockTimeout | StorageError::WriteTxnHeldByCurrentSession
                ) && deadline.is_expired_at(clock())
                {
                    return Err(PartitionedStopCause::DeadlineExceeded);
                }
                return Err(w(convert_write_txn_err(e)));
            }
        };
        // SQL-40・Issue #1968: 再開時とチャンクの途中で権限を剥奪された後に続行させない。ジョブ記録・台帳
        // 照合・走査・適用より前に、この txn から ACL を読んで判定する（拒否は commit 前なので副作用なし）。
        if let Some(recheck) = &hooks.privilege {
            recheck
                .check_in_txn(&*write_txn, &[table.to_string()])
                .map_err(|()| PartitionedStopCause::InsufficientPrivilege)?;
        }
        // 保持時間は permit を得てから数える（待機時間を保持時間に含めない）。
        let start = clock();

        let schema = require_table_schema_write_any_identity(&write_txn, table).map_err(w)?;
        if let Some(expected) = expected_schema {
            if expected != &schema {
                return Err(w(CatalogError::Invalid(
                    "table schema changed after the statement was bound".to_string(),
                )));
            }
        }
        if let Kind::Update {
            assignments,
            set_exprs,
        } = kind
        {
            validate_set_assignments(&schema, assignments).map_err(w)?;
            // 式項は束縛時スキーマの列 index に結び付く（`expected_schema` 必須）。
            require_expected_schema_for_set_exprs(set_exprs, expected_schema).map_err(w)?;
        }

        // トリガの分類（Issue #3283・実行モデルの D8）。行トリガ（BEFORE／AFTER ROW）は各チャンクの
        // write txn の中で通常経路と同じ段取りで発火する。文トリガ・参照アクションが及ぶ子表のトリガ・
        // 通過証なし（NoSQL 表層）は固定文言の `0A000` で拒否し、黙って飛ばさない（文トリガの「文につき
        // 1 回・全行の前後」は、チャンクごとに commit し再開もある非原子実行では保証できない）。
        // ジョブ記録・台帳照合より前に判定するので、第 1 チャンクでは commit 前に止まり副作用はゼロ
        // （登録簿の登録は drop で外れる）。途中のチャンクで DDL によりトリガが追加された場合は、次の
        // チャンクで同じ判定が働き、既存の部分完了の写像（`VD001`）に従う。
        // 子表トリガは供給せず（分割実行では結線しない）、該当すれば先にここで `0A000` にする。
        crate::trigger::plan::reject_referential_child_triggers(
            &write_txn,
            tenant,
            table,
            match kind {
                Kind::Delete => crate::catalog::trigger::TriggerEvents::DELETE,
                Kind::Update { .. } => crate::catalog::trigger::TriggerEvents::UPDATE,
            },
        )
        .map_err(w)?;
        let trigger_plan = match kind {
            Kind::Delete => crate::trigger::plan::plan_row_triggers(
                &write_txn,
                tenant,
                table,
                crate::catalog::trigger::TriggerEvents::DELETE,
                None,
                triggers,
            ),
            Kind::Update {
                assignments,
                set_exprs,
            } => plan_update_triggers(
                &write_txn,
                ctx,
                table,
                &schema,
                assignments,
                set_exprs,
                &[],
                triggers,
            ),
        }
        .map_err(w)?;
        if let Some(plan) = &trigger_plan {
            crate::trigger::plan::reject_statement_triggers(plan).map_err(w)?;
        }

        // ジョブ記録の照合。最初のチャンクは再送判定、以降はメモリ上の状態との一致確認。
        let record = partitioned_job::lookup_in_write_txn(&write_txn, &key).map_err(map_job_err)?;
        if state.first_chunk {
            match partitioned_job::classify_resend(
                record.as_ref(),
                hash,
                false,
                fixed_time.is_some(),
            ) {
                ResendDecision::Fresh => {
                    // 新規ジョブの開始時に中断記録数の上限を判定する（commit 前なので副作用
                    // なし）。`Resume` は新しい記録を作らないため対象外。作成 txn 内での再計数は
                    // `record_chunk_progress_in_txn` が行う。
                    let limit = limits.interrupted_record_limit.get();
                    let count =
                        partitioned_job::count_interrupted_in_txn(&write_txn, tenant, table, limit)
                            .map_err(map_job_err)?;
                    if count >= limit {
                        return Err(PartitionedStopCause::InterruptedRecordLimitReached);
                    }
                }
                ResendDecision::Resume {
                    cursor,
                    processed,
                    fixed_time: recorded_time,
                    trigger_effects,
                } => {
                    // 呼び出し元が束縛に使った固定時刻は、ジョブ開始時に永続化した値と一致する
                    // こと（事前読み込みから本 txn までの間の記録の変化を検出する。設計 D6）。
                    if recorded_time != fixed_time {
                        return Err(map_job_err(PartitionedJobError::StateConflict));
                    }
                    state.cursor = Some(cursor);
                    state.processed = processed;
                    state.side_effects_committed = processed > 0 || trigger_effects;
                    state.has_record = true;
                }
                ResendDecision::Duplicate => return Err(w(TenantWriteError::DuplicateOperationId)),
                ResendDecision::ContentMismatch => {
                    return Err(w(TenantWriteError::OperationIdContentMismatch))
                }
                ResendDecision::Cancelled { committed } => {
                    return Err(PartitionedStopCause::ResendCancelled { committed })
                }
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
                        fixed_time: stored_time,
                        ..
                    }),
                    true,
                ) => {
                    hash.matches(stored)
                        && Some(*cursor) == state.cursor
                        && *processed == state.processed
                        && *stored_time == fixed_time
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

        // RLS-13（Issue #2276）: 利用者ポリシーをチャンクの write txn・同じスキーマから解決する
        // （チャンクごとに最新の状態を読む。ポリシーは行を狭めるだけで、対象は常に所有・可視の内側）。
        let policy = resolve_dml_row_policy(
            &write_txn,
            ctx,
            &schema,
            table,
            match kind {
                Kind::Delete => crate::sql::row_policy::PolicyCmd::Delete {
                    reads_existing: true,
                },
                Kind::Update { .. } => crate::sql::row_policy::PolicyCmd::Update {
                    reads_existing: true,
                },
            },
        )
        .map_err(w)?;
        let scan = scan_chunk(
            &write_txn,
            table,
            ctx,
            &schema,
            policy.as_ref(),
            state.cursor,
            needs_embedding,
            width,
            limits.scan_budget_rows.get(),
            limits.max_writer_hold,
            start,
            clock,
            deadline,
            &mut predicate,
        )?;
        let ScanResult {
            candidates,
            last_scanned,
            mut exhausted,
        } = scan;

        // ジョブ累計の影響行数上限（明示指定時のみ）。超過するチャンクは適用しない。
        if let Some(limit) = max_affected_total {
            // 行トリガのある表は BEFORE の `NULL` 返却で候補がスキップされ得るため、上限判定は
            // 適用後の実適用件数で行う（下の適用段の後）。トリガ無しは従来どおり適用前に判定する。
            if trigger_plan.is_none() {
                let total = state
                    .processed
                    .saturating_add(u64::try_from(candidates.len()).unwrap_or(u64::MAX));
                if total > u64::try_from(limit.get()).unwrap_or(u64::MAX) {
                    return Err(PartitionedStopCause::AffectedRowsLimitExceeded { limit });
                }
            }
        }

        // 適用段（原子経路と共有）は物理キー要素の `u64` 列を受ける。従来方式の表では rowid の生値＝
        // `id` 列値、採番方式の表（TABLE-23・Issue #2199）では払い出した rowid の生値で、どちらも
        // `legacy_id()` が行ストアの物理キー要素を返す（利用者へは出さない）。
        // 長さはチャンク幅以下で有界（走査段が候補をチャンク幅で締める）。
        let candidate_ids: Vec<u64> = candidates.iter().map(|r| r.legacy_id()).collect();
        // 適用（原子経路と共有する適用段）。保持時間を超えたら残りを次チャンクへ回す。
        // 期限の確認点 (c): 適用段の打ち切り判定（保持時間と同じ呼び出し点）。期限による停止は
        // 部分 commit にせず、適用後にチャンクごと破棄する。
        let deadline_hit = std::cell::Cell::new(false);
        let mut stop_by_hold = || {
            let now = clock();
            if deadline.is_expired_at(now) {
                deadline_hit.set(true);
                return true;
            }
            now.saturating_duration_since(start) >= limits.max_writer_hold
        };
        let fk_mode = crate::constraint::FkCheckMode::All;
        // 行トリガのある表は、行ストアのハンドルを開かない別の適用段（通常経路と共有）へ回す。分割実行は
        // autocommit 専用なので `Autocommit` を渡す（トリガが書いた表の遅延 FK を各チャンクの末尾で確定する。
        // `InTxn` を渡すと COMMIT まで先送りされ fail-open になる）。`consumed` は BEFORE の `NULL` 返却で
        // スキップした行を含む消費件数で、カーソルの根拠になる（`applied` は書いた行数）。
        let (applied, consumed, trigger_wrote) = if let Some(plan) = &trigger_plan {
            // 行トリガ本体の時刻値関数（`now()` 等）がチャンク内の全行で同一の時刻を返すよう、
            // 通常 DML と同じくトランザクション固定の時計スコープを張る。再送で時刻が記録済みなら
            // その値（`fixed_time`）を使い、無ければこのチャンクの開始時刻を使う。
            let clock_micros = match fixed_time {
                Some(t) => t,
                None => crate::sql::time_fn::now_micros().map_err(|_| {
                    w(TenantWriteError::ReturningProjectionFailed(
                        "system clock unavailable".to_string(),
                    ))
                })?,
            };
            let _trigger_clock_scope =
                crate::sql::time_fn::ClockScope::enter(crate::sql::time_fn::StatementClock {
                    txn_start_micros: clock_micros,
                    stmt_start_micros: clock_micros,
                });
            let writes =
                crate::trigger::write_handle::TriggerWriteHandle::new(&write_txn, ctx, triggers);
            let target = WriteTarget::Autocommit(storage);
            let done = match kind {
                Kind::Delete => trigger_dml::delete_rows_with_triggers(
                    &write_txn,
                    &target,
                    table,
                    ctx,
                    &schema,
                    &candidate_ids,
                    plan,
                    &writes,
                    Some(&mut stop_by_hold),
                ),
                Kind::Update {
                    assignments,
                    set_exprs,
                } => trigger_dml::update_rows_with_triggers(
                    &write_txn,
                    &target,
                    table,
                    ctx,
                    &schema,
                    &candidate_ids,
                    &trigger_dml::UpdateSpec {
                        assignments,
                        set_exprs,
                        row_overrides: None,
                    },
                    plan,
                    &writes,
                    Some(&mut stop_by_hold),
                ),
            }
            .map_err(w)?;
            (done.applied, done.consumed, done.trigger_wrote)
        } else {
            let applied = match kind {
                Kind::Delete => apply_predicate_delete_in_txn::<E>(
                    &write_txn,
                    table,
                    ctx,
                    &schema,
                    &candidate_ids,
                    fk_mode,
                    None,
                    Some(&mut stop_by_hold),
                ),
                Kind::Update {
                    assignments,
                    set_exprs,
                } => apply_predicate_update_in_txn::<E>(
                    &write_txn,
                    table,
                    ctx,
                    &schema,
                    &candidate_ids,
                    assignments,
                    set_exprs,
                    None,
                    fk_mode,
                    None,
                    Some(&mut stop_by_hold),
                ),
            }
            .map_err(from_predicate_err)?;
            (applied, applied, false)
        };
        if deadline_hit.get() {
            drop(write_txn);
            return Err(PartitionedStopCause::DeadlineExceeded);
        }
        // トリガ経路の累計上限判定（Skip を除く実適用件数。超過するチャンクは commit せず破棄する）。
        if let (Some(limit), Some(_)) = (max_affected_total, &trigger_plan) {
            let total = state
                .processed
                .saturating_add(u64::try_from(applied).unwrap_or(u64::MAX));
            if total > u64::try_from(limit.get()).unwrap_or(u64::MAX) {
                drop(write_txn);
                return Err(PartitionedStopCause::AffectedRowsLimitExceeded { limit });
            }
        }

        // 打ち切った場合のカーソルは「最後に消費した候補の rowid」（それ以降の行は次の
        // チャンクで再走査・再評価される）。
        let new_cursor = if consumed < candidates.len() {
            exhausted = false;
            consumed
                .checked_sub(1)
                .and_then(|i| candidates.get(i))
                .copied()
        } else {
            last_scanned
        };
        let rows_changed = u64::try_from(applied).unwrap_or(u64::MAX);

        // 実行時間の上限（SQL-77 (3)）。走査し尽くしたチャンクは完了を優先する。
        let stop_by_run_time = !exhausted
            && run_start.is_some_and(|started| {
                run_time_limit_reached(limits.max_run_time, limits.chunk_interval, started, clock())
            });

        let completed_total = if exhausted {
            Some(
                partitioned_job::complete_with_fixed_time_in_txn(
                    &write_txn,
                    &key,
                    hash,
                    rows_changed,
                    fixed_time,
                )
                .map_err(map_job_err)?,
            )
        } else {
            // `!exhausted` なら 1 行以上走査済みでカーソルが存在する。
            let Some(cursor) = new_cursor else {
                return Err(map_job_err(PartitionedJobError::StateConflict));
            };
            // 上限停止の境界だけは、変更 0 件でも再開用の記録を作る（SQL-77 (3)）。
            // BEFORE トリガが別表へ書いて行をスキップしたチャンク（`applied == 0` かつ `trigger_wrote`）は、
            // 書き込みを破棄しないよう再開用の記録を作って commit する（Issue #3283）。
            let persist_without_changes = stop_by_run_time || (trigger_wrote && applied == 0);
            let record_progress = if persist_without_changes {
                partitioned_job::record_stop_point_in_txn
            } else {
                partitioned_job::record_chunk_progress_in_txn
            };
            match record_progress(
                &write_txn,
                &key,
                hash,
                rows_changed,
                cursor,
                limits.interrupted_record_limit.get(),
                fixed_time,
                trigger_wrote,
            )
            .map_err(map_job_err)?
            {
                ChunkCommitOutcome::NoRecord => {
                    // 停止点用は `NoRecord` を返さない。万一返れば再開用の記録がないまま
                    // 止まることになるため、fail-closed で止める。
                    if persist_without_changes {
                        return Err(map_job_err(PartitionedJobError::StateConflict));
                    }
                    // 記録がなく変更も 0 件: 何も書かずカーソルだけメモリ上で進める。
                    drop(write_txn);
                    state.cursor = Some(cursor);
                    chunk_index = chunk_index.saturating_add(1);
                    if let Some(f) = hooks.after_chunk.as_mut() {
                        f(chunk_index);
                    }
                    wait_chunk_interval(
                        limits.chunk_interval,
                        hooks,
                        deadline,
                        &|| guard.cancel_requested(),
                        &|| cancel_stop(storage, &key),
                    )?;
                    continue;
                }
                ChunkCommitOutcome::Created => {
                    state.has_record = true;
                    None
                }
                ChunkCommitOutcome::Updated => None,
            }
        };

        // commit 直前の取り消し確認。走査・適用中に届いた要求は、このチャンクを破棄
        // （write txn を drop）して記録を残さず止める。最終チャンクでも完了として返さない。
        if guard.cancel_requested() {
            drop(write_txn);
            return Err(cancel_stop(storage, &key));
        }
        // 期限の確認点 (d): commit 直前。commit の後（記録・台帳・recovery）には置かない。
        if deadline.is_expired_at(clock()) {
            drop(write_txn);
            return Err(PartitionedStopCause::DeadlineExceeded);
        }

        // テーブル世代のバンプは commit 直前に置く（取り消し・期限の確認で txn を破棄する
        // 経路ではバンプも破棄される。`table_generation_bump_coverage` の近傍検査にも対応）。
        if applied > 0 || trigger_wrote {
            // commit 記録（SQL-38）: 0 行でも世代を進める経路（トリガの別表書き込みのみ）は表を記録する。
            if applied == 0 {
                crate::storage::commit_log::note_table(&write_txn, table);
            }
            crate::catalog::bump_table_generation_in_txn(&write_txn, table).map_err(w)?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(|e| w(TenantWriteError::from(e)))?;

        // commit 後にだけメモリ上の状態を更新する。
        state.processed = state.processed.saturating_add(rows_changed);
        if trigger_wrote {
            state.side_effects_committed = true;
        }
        if let Some(total) = completed_total {
            state.processed = total;
            return Ok(PartitionedDmlOutcome::Completed { total_rows: total });
        }
        state.cursor = new_cursor;
        chunk_index = chunk_index.saturating_add(1);
        if let Some(f) = hooks.after_chunk.as_mut() {
            f(chunk_index);
        }
        // 上限停止はチャンク間隔を待たずに返す（待っても次のチャンクは実行しないため）。
        if stop_by_run_time {
            return Err(PartitionedStopCause::RunTimeLimitReached);
        }
        wait_chunk_interval(
            limits.chunk_interval,
            hooks,
            deadline,
            &|| guard.cancel_requested(),
            &|| cancel_stop(storage, &key),
        )?;
    }
}

/// 1 チャンクの走査結果。
struct ScanResult {
    /// 述語に一致した行の物理 rowid（昇順。長さは高々チャンク幅）。
    candidates: Vec<RowId>,
    /// このチャンクで最後に走査した物理 rowid（1 行も走査しなければ `None`）。
    last_scanned: Option<RowId>,
    /// 範囲を走査し尽くしたか。
    exhausted: bool,
}

/// カーソルの次からテナント所有かつ可視の範囲を走査し、述語に一致した行の rowid を集める。
/// 適用行数がチャンク幅・走査行数が走査予算・writer 保持時間が上限に達したら締める
/// （判定は 1 行以上走査した後）。メモリは候補 rowid（チャンク幅以下）と 1 行分の作業領域
/// だけで、対象行数に比例しない。
#[allow(clippy::too_many_arguments)]
fn scan_chunk<E>(
    write_txn: &redb::WriteTransaction,
    table: &str,
    ctx: &PolicyContext,
    schema: &crate::catalog::TableSchema,
    policy: Option<&crate::sql::row_policy::BoundRowPolicy>,
    cursor: Option<RowId>,
    needs_embedding: bool,
    width: usize,
    scan_budget: usize,
    max_hold: Duration,
    start: Instant,
    clock: &dyn Fn() -> Instant,
    deadline: crate::interrupt::StatementDeadline,
    predicate: &mut impl FnMut(&DmlCandidate<'_>) -> Result<bool, E>,
) -> Result<ScanResult, PartitionedStopCause<E>> {
    let tenant = ctx.tenant_id();
    let Some(range) = chunk_range(tenant, cursor) else {
        // `RowId::MAX` まで走査済み（記録としては取り得ないが、取った場合も走査し尽くし）。
        return Ok(ScanResult {
            candidates: Vec::new(),
            last_scanned: None,
            exhausted: true,
        });
    };
    let row_table_name = user_rows_table_name(table);
    let row_table = write_txn
        .open_table_logged(user_rows_table_def(&row_table_name))
        .map_err(|e| w(map_row_table_error(e)))?;
    let mut candidates: Vec<RowId> = Vec::with_capacity(width.min(4096));
    let mut embedding_scratch: Vec<f32> = Vec::new();
    // ポリシー段の復号マスク・スタック（行ごとに確保しない。ポリシーが無ければ未使用）。
    let mut policy_mask: Vec<bool> = vec![false; schema.columns.len()];
    let policy_needs = policy
        .map(|p| p.mark_needs(&mut policy_mask))
        .unwrap_or_default();
    let mut policy_scratch: Vec<crate::sql::expr_program::StackValue> = Vec::new();
    let mut scanned: usize = 0;
    let mut last_scanned: Option<RowId> = None;
    let mut exhausted = true;

    for entry in row_table
        .range::<(&str, u64)>(range)
        .map_err(|e| w(CatalogError::from(e)))?
    {
        let (k, v) = entry.map_err(|e| w(CatalogError::from(e)))?;
        let (key_tenant, rowid) = split_row_key(k.value());
        if key_tenant != tenant {
            // 閉区間により理論上到達しない多層防御（他テナント領域は読まない）。
            break;
        }
        scanned = scanned.saturating_add(1);
        last_scanned = Some(rowid);

        let hit: bool = 'row: {
            let buf = v.value();
            let (row_tenant, visibility, offset) =
                crate::storage::decode_row_header(buf).map_err(w)?;
            crate::storage::verify_row_key_tenant(key_tenant, row_tenant).map_err(w)?;
            // 可視性で数え方を変えない（走査数は加算済み）。不可視行の本体は読まない。
            if !ctx.is_owner(row_tenant) || !ctx.is_visible(row_tenant, visibility) {
                break 'row false;
            }
            let (dim, metadata): (u32, &[u8]) = if needs_embedding || policy_needs.embedding {
                crate::storage::decode_row_body_into(buf, offset, &mut embedding_scratch)
                    .map_err(w)?
            } else {
                crate::storage::decode_row_dim_and_metadata_borrowed(buf).map_err(w)?
            };
            // POLICY 段（RLS-13・Issue #2276）: 所有・可視の内側・利用者の述語より前。通らない行は
            // 走査数には数え済みのまま候補から外す（存在しない行と同じ扱い）。
            if let Some(policy) = policy {
                let cols = crate::row_codec::scan_scalar_columns_masked(
                    schema,
                    metadata,
                    Some(&policy_mask),
                )
                .map_err(|e| w(TenantWriteError::TriggerFailed(SqlSurfaceError::from(e))))?;
                let policy_embedding: &[f32] = if policy_needs.embedding {
                    embedding_scratch.as_slice()
                } else {
                    &[]
                };
                let passed = policy
                    .passes(
                        &cols,
                        rowid.legacy_id(),
                        policy_embedding,
                        dim as usize,
                        &mut policy_scratch,
                    )
                    .map_err(|e| w(TenantWriteError::TriggerFailed(e)))?;
                if !passed {
                    break 'row false;
                }
            }
            let embedding: &[f32] = if needs_embedding {
                embedding_scratch.as_slice()
            } else {
                &[]
            };
            predicate(&DmlCandidate {
                // 従来方式の表の `id` 列値（採番方式の表はゲートで到達不能。#2197 で差し替え）。
                id: rowid.legacy_id(),
                dim,
                embedding,
                metadata,
            })
            .map_err(PartitionedStopCause::Predicate)?
        };
        if hit {
            candidates.push(rowid);
        }

        // 期限の確認点 (b): 行ごと（保持時間判定と同じ時計値を使う）。
        let now = clock();
        if deadline.is_expired_at(now) {
            return Err(PartitionedStopCause::DeadlineExceeded);
        }
        let close = candidates.len() >= width
            || scanned >= scan_budget
            || now.saturating_duration_since(start) >= max_hold;
        if close {
            // 最大 rowid まで走査した場合のみ、その先に行は存在しない。
            exhausted = rowid == RowId::MAX;
            break;
        }
    }
    Ok(ScanResult {
        candidates,
        last_scanned,
        exhausted,
    })
}

/// テスト用: チャンク間隔 0（既定）のとき sleep が一度も呼ばれないことを担保する注入先。
/// 子モジュール（`tests`・`scenario_tests`）が共有する。
#[cfg(test)]
fn no_sleep(_: Duration) {
    panic!("sleep must not be called when chunk_interval is zero");
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod scenario_tests;
