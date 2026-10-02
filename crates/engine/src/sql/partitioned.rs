//! 分割実行 DML の SQL 表層（Issue #1129。ポインタ: ADR `docs/design/partitioned-dml.md`、
//! `docs/spec/04-behavior/sql.md` SQL-19、`docs/spec/04-behavior/recovery.md`
//! RECOVER-11・RECOVER-12、RLS-9・RLS-10、ERR-1・ERR-2・ERR-4）。
//!
//! 述語形 `UPDATE`／`DELETE ... USING OPERATION_ID '<id>' PARTITIONED [CHUNK <n>]` を、
//! チャンク単位で commit する非原子実行（[`crate::tenant::partitioned_dml`]）へ結線する
//! ための型と処理を集約する。
//!
//! - 構文木: [`PartitionedClause`]（`PARTITIONED [CHUNK n]`）、[`PartitionedControl`]
//!   （`SHOW`／`CANCEL PARTITIONED DML`・`EXPLAIN`）。`sql::allowlist` が構築し、
//!   `core.rs::EngineCore` が実行する（`ParsedSql::Partitioned`）。
//! - 失敗写像: [`map_partitioned_failure`]（実行器の停止原因 → `VD001`／`VD002`／原因コード）。
//! - 進捗照会・取り消し: [`execute_show`]・[`execute_cancel`]。**ユーザーテーブルのカタログを
//!   一切引かず**、ジョブ表とプロセス内登録簿だけを `(tenant, table, operation_id)` で引く。
//!   「ジョブなし」「他テナント」「テーブルなし」「見えないテーブル」は同じ応答（同じ 2 列・
//!   0 行）にそろえ、存在オラクルにしない（RLS-9）。
//! - `EXPLAIN`: [`explain_result`]。実行せず、サーバー設定値と文の形状だけを返す。
//!
//! # 呼び出し元
//!
//! `core.rs::EngineCore::{parse_tokens, execute_parsed_in_session, execute_in_active_txn}` と
//! `sql::exec` の分割実行入口。メッセージには件数（自テナントで commit 済みの数）・原因コード・
//! クライアント自身の `operation_id` 以外を載せない（内側のエラー文言・テナント名・テーブル名・
//! 行内容は載せない。security.md P0）。

use std::num::NonZeroUsize;

use redb::ReadableDatabase;

use crate::error_format::ClassifiedError;
use crate::policy::PolicyContext;
use crate::recovery::partitioned_job::{self, JobKey, JobRecord, JobStatus};
use crate::sql::allowlist::{SqlSurfaceError, ValidatedPredicateDelete, ValidatedPredicateUpdate};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::parser::PartitionedDmlLimits;
use crate::sql::using_operation_id::OperationId;
use crate::storage::Storage;
use crate::tenant::partitioned_dml::{PartitionedDmlFailure, PartitionedStopCause};

/// 同時実行数の上限（テナント単位・全体）と同じ `operation_id` の実行中を同じ文言で返す
/// （どちらの上限に当たったかを応答に表さない。ADR §15.4 の残余リスクは許容済み）。
const MSG_JOB_BUSY: &str = "a partitioned DML job with the same operation_id is already running";
const MSG_CONCURRENCY: &str = "too many concurrent partitioned DML jobs";
const MSG_INTERRUPTED_LIMIT: &str = "too many interrupted partitioned DML jobs for this table";

/// 述語形 `UPDATE`／`DELETE` の `PARTITIONED [CHUNK n]` 修飾（SQL-19）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionedClause {
    /// `CHUNK n` の指定値（正の整数。省略時は `None`＝サーバー設定のチャンク幅）。
    /// サーバー設定より小さい方向にしか変えられない（[`apply_chunk_override`]）。
    pub(crate) chunk_rows: Option<NonZeroUsize>,
}

/// `SHOW`／`CANCEL PARTITIONED DML '<id>' ON <table>` が指すジョブ。テーブル名は
/// 識別子規則の構文検査のみを通した値（カタログ未照会）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionedJobRef {
    pub(crate) table: String,
    pub(crate) operation_id: OperationId,
}

/// 分割実行に関する制御文・説明文（`ParsedSql::Partitioned`）。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PartitionedControl {
    /// `SHOW PARTITIONED DML '<id>' ON <table>`（進捗照会。読み取りのみ）。
    Show(PartitionedJobRef),
    /// `CANCEL PARTITIONED DML '<id>' ON <table>`（取り消し。autocommit 専用）。
    Cancel(PartitionedJobRef),
    /// `EXPLAIN DELETE ... PARTITIONED`（実行しない）。
    ExplainDelete(ValidatedPredicateDelete),
    /// `EXPLAIN UPDATE ... PARTITIONED`（実行しない）。
    ExplainUpdate(ValidatedPredicateUpdate),
}

impl PartitionedControl {
    /// 書き込み（ジョブ記録の更新）を伴うか。明示・暗黙トランザクション内では拒否する。
    pub(crate) fn writes(&self) -> bool {
        matches!(self, PartitionedControl::Cancel(_))
    }
}

/// トークン列に分割実行修飾（`USING OPERATION_ID <文字列|NULL> PARTITIONED`）が含まれるか。
/// 複数文メッセージの拒否（`sql::statement_splitter`）と `EXPLAIN` の経路選択
/// （`core.rs`）が共有する字句レベルの判定で、`partitioned` という名前の列・テーブルを
/// 誤検出しないよう「`USING OPERATION_ID` 句の直後」の並びに限って判定する。
/// untrusted 入力のため `get()` のみで走査する（添字アクセスを使わない）。
pub(crate) fn tokens_have_partitioned_clause(tokens: &[crate::sql::lexer::Token]) -> bool {
    use crate::sql::lexer::Token;
    let word = |t: Option<&Token>, w: &str| matches!(t, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(w));
    (0..tokens.len()).any(|i| {
        word(tokens.get(i), "USING")
            && word(tokens.get(i.saturating_add(1)), "OPERATION_ID")
            && (matches!(
                tokens.get(i.saturating_add(2)),
                Some(Token::StringLiteral(_))
            ) || word(tokens.get(i.saturating_add(2)), "NULL"))
            && word(tokens.get(i.saturating_add(3)), "PARTITIONED")
    })
}

/// 先頭が `CANCEL PARTITIONED`（取り消し文）か。複数文メッセージの拒否が使う。
pub(crate) fn tokens_are_partitioned_cancel(tokens: &[crate::sql::lexer::Token]) -> bool {
    use crate::sql::lexer::Token;
    matches!(tokens.first(), Some(Token::Ident(a)) if a.eq_ignore_ascii_case("CANCEL"))
        && matches!(tokens.get(1), Some(Token::Ident(b)) if b.eq_ignore_ascii_case("PARTITIONED"))
}

/// `CHUNK n` をサーバー設定のチャンク幅に適用した実効設定を返す。サーバー設定を超える
/// 指定は `22000`（小さくする方向にしか変えられない。走査予算・保持時間・同時実行数は
/// 文からは変えられない）。パース時と実行時の 2 回呼ぶ多層防御で、`ParsedSql` を外部で
/// 組み立てた経路も塞ぐ。
pub(crate) fn apply_chunk_override(
    clause: &PartitionedClause,
    server: &PartitionedDmlLimits,
) -> Result<PartitionedDmlLimits, SqlSurfaceError> {
    let mut limits = *server;
    if let Some(n) = clause.chunk_rows {
        if n > server.chunk_rows {
            return Err(SqlSurfaceError::invalid_input(
                "CHUNK exceeds the server-configured chunk width",
            ));
        }
        limits.chunk_rows = n;
    }
    Ok(limits)
}

/// 実行器の停止（[`PartitionedDmlFailure`]）をエラーコードへ写す。
///
/// - 1 件も commit していない（`committed_total == 0`）失敗は原因のコードのまま返す。
/// - 1 チャンク以上 commit した後の失敗は `VD001`（`PARTIAL_COMPLETION`。原因はコードだけ）。
/// - 取り消しは `VD002`（件数入り）。
/// - 実行中の重複・同時実行数上限は `55P03`、中断記録数上限は `54000`、台帳なし構成は `0A000`。
///
/// `op` は [`crate::sql::exec::map_write_error`] の操作名（`delete`／`update`）、
/// `map_predicate` は述語クロージャのエラー（`SqlSurfaceError`）の写像。
pub(crate) fn map_partitioned_failure<E>(
    failure: PartitionedDmlFailure<E>,
    operation_id: &str,
    op: &'static str,
    map_predicate: impl FnOnce(E) -> SqlSurfaceError,
) -> SqlSurfaceError {
    let committed = failure.committed_total;
    // 取り消しは件数入りの `VD002`（commit 件数の有無を問わない）。
    match failure.cause {
        PartitionedStopCause::CancelRequested => {
            return SqlSurfaceError::PartitionedDmlCancelled { committed };
        }
        PartitionedStopCause::ResendCancelled { committed } => {
            return SqlSurfaceError::PartitionedDmlCancelled { committed };
        }
        // 実行中の重複・同時実行数上限は、実行器が副作用を持つ前に判定する。
        PartitionedStopCause::AlreadyRunning => {
            return SqlSurfaceError::partitioned_job_busy(MSG_JOB_BUSY);
        }
        PartitionedStopCause::ConcurrencyLimitReached => {
            return SqlSurfaceError::partitioned_job_busy(MSG_CONCURRENCY);
        }
        PartitionedStopCause::LedgerRequired => {
            return SqlSurfaceError::FeatureNotSupported {
                detail: "partitioned DML requires the operation ledger".to_string(),
            };
        }
        _ => {}
    }
    let cause_error = match failure.cause {
        PartitionedStopCause::Write(e) => crate::sql::exec::map_write_error(e, op),
        PartitionedStopCause::Predicate(e) => map_predicate(e),
        PartitionedStopCause::AffectedRowsLimitExceeded { limit } => {
            SqlSurfaceError::payload_too_large(format!(
                "DML affected row count exceeds limit {limit}"
            ))
        }
        PartitionedStopCause::InterruptedRecordLimitReached => {
            SqlSurfaceError::payload_too_large(MSG_INTERRUPTED_LIMIT)
        }
        // 上で返却済み（網羅性のための到達不能腕。fail-closed に内部エラー）。
        _ => SqlSurfaceError::Internal {
            detail: format!("{op} failed"),
        },
    };
    if committed == 0 {
        return cause_error;
    }
    SqlSurfaceError::partial_completion(committed, cause_error.error_class(), operation_id)
}

/// 該当なし（ジョブなし・他テナント・テーブルなし・見えないテーブル）の共通応答。
/// 同じ 2 列・0 行で、どの原因でも同一になる（RLS-9）。
fn empty_status_result() -> QueryResult {
    QueryResult {
        columns: status_columns(),
        rows: Vec::new(),
    }
}

fn status_columns() -> Vec<ColumnMeta> {
    vec![
        ColumnMeta::Computed {
            name: "status".to_string(),
            ty: Some(crate::catalog::ColumnType::Text),
        },
        ColumnMeta::Computed {
            name: "rows".to_string(),
            ty: Some(crate::catalog::ColumnType::BigInt),
        },
    ]
}

/// 1 行の状態応答（`status`・`rows`）。件数は `i64` に収まらなければ飽和させる。
pub(crate) fn status_result(status: &str, rows: u64) -> QueryResult {
    QueryResult {
        columns: status_columns(),
        rows: vec![ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![
                Cell::Text(status.to_string()),
                Cell::SignedInteger(i64::try_from(rows).unwrap_or(i64::MAX)),
            ],
        }],
    }
}

/// ジョブ記録層のエラーを内部エラーへ写す（記録の破損は固定文言の `XX000`）。
fn job_record_error(_e: partitioned_job::PartitionedJobError) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "partitioned job record is unreadable".to_string(),
    }
}

fn read_error(_e: impl std::fmt::Debug) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "failed to read partitioned job state".to_string(),
    }
}

/// 永続記録を読む（ジョブ表が無ければ `None`）。
fn lookup_record(
    storage: &Storage,
    key: &JobKey<'_>,
) -> Result<Option<JobRecord>, SqlSurfaceError> {
    let read_txn = storage.db().begin_read().map_err(read_error)?;
    partitioned_job::lookup_in_read_txn(&read_txn, key).map_err(job_record_error)
}

fn status_to_result(status: JobStatus) -> QueryResult {
    match status {
        JobStatus::Running { processed } => status_result("running", processed),
        JobStatus::Interrupted { processed, .. } => status_result("interrupted", processed),
        JobStatus::Cancelled { committed } => status_result("cancelled", committed),
        JobStatus::Completed { total } => status_result("completed", total),
    }
}

/// `SHOW PARTITIONED DML`: 自テナントのジョブの状態を返す。writer を使わず、ユーザーテーブルの
/// カタログも引かない。該当なしは [`empty_status_result`]。
pub(crate) fn execute_show(
    storage: &Storage,
    ctx: &PolicyContext,
    job: &PartitionedJobRef,
) -> Result<QueryResult, SqlSurfaceError> {
    let key = JobKey::for_context(ctx, &job.table, &job.operation_id);
    let record = lookup_record(storage, &key)?;
    let registered = storage.partitioned_job_registry().is_registered(&key);
    Ok(
        match partitioned_job::derive_status(record.as_ref(), registered) {
            Some(status) => status_to_result(status),
            None => empty_status_result(),
        },
    )
}

/// `CANCEL PARTITIONED DML`（autocommit 専用）: 実行中なら次のチャンク境界で止める要求を出し、
/// 中断中なら取り消し済みへ縮める。commit 済みのチャンクは戻さない。
///
/// 1. 登録簿に載っていれば取り消しを要求する（`cancelling`）。
/// 2. 載っていなければ記録を読む。なし・完了・取り消し済みは書き込まずに状態を返す。
/// 3. 中断（`Running` 記録のみ）なら writer を取り、**write txn の中で**登録を再確認する
///    （登録されていれば `cancelling`。実行器のチャンクが途中で記録の書き換えを見て
///    `XX000` にならないよう、書き込みは writer の直列化の内側で行う）。
///
/// `try_register` は使わない（同時実行数の枠を一時的に占有しないため）。
pub(crate) fn execute_cancel(
    storage: &Storage,
    ctx: &PolicyContext,
    job: &PartitionedJobRef,
) -> Result<QueryResult, SqlSurfaceError> {
    let key = JobKey::for_context(ctx, &job.table, &job.operation_id);
    let registry = storage.partitioned_job_registry();

    if registry.request_cancel(&key) {
        return cancelling_result(storage, &key);
    }
    match lookup_record(storage, &key)? {
        None => return Ok(empty_status_result()),
        Some(JobRecord::Completed { total, .. }) => return Ok(status_result("completed", total)),
        Some(JobRecord::Cancelled { committed, .. }) => {
            return Ok(status_result("cancelled", committed))
        }
        Some(JobRecord::Running { .. }) => {}
    }

    let txn = storage.begin_write_txn().map_err(|e| match e {
        crate::storage::StorageError::WriteLockTimeout
        | crate::storage::StorageError::WriteTxnHeldByCurrentSession => {
            SqlSurfaceError::LockNotAvailable
        }
        _ => SqlSurfaceError::Internal {
            detail: "cancel failed".to_string(),
        },
    })?;
    if registry.is_registered(&key) {
        // 直前に再開された。実行器へ要求を渡す（writer を手放してから）。
        drop(txn);
        registry.request_cancel(&key);
        return cancelling_result(storage, &key);
    }
    match partitioned_job::cancel_in_txn(&txn, &key).map_err(job_record_error)? {
        Some(committed) => {
            crate::recovery::commit_boundary::commit(txn).map_err(|_| {
                SqlSurfaceError::Internal {
                    detail: "cancel failed".to_string(),
                }
            })?;
            Ok(status_result("cancelled", committed))
        }
        None => {
            // 読み取りと書き込みの間で状態が変わった（完了・取り消し済み）。書き込まずに返す。
            let record =
                partitioned_job::lookup_in_write_txn(&txn, &key).map_err(job_record_error)?;
            drop(txn);
            Ok(match record {
                Some(JobRecord::Completed { total, .. }) => status_result("completed", total),
                Some(JobRecord::Cancelled { committed, .. }) => {
                    status_result("cancelled", committed)
                }
                _ => empty_status_result(),
            })
        }
    }
}

/// `cancelling` 応答（要求を出した時点の commit 済み件数を添える）。
fn cancelling_result(storage: &Storage, key: &JobKey<'_>) -> Result<QueryResult, SqlSurfaceError> {
    let processed = match lookup_record(storage, key)? {
        Some(JobRecord::Running { processed, .. }) => processed,
        _ => 0,
    };
    Ok(status_result("cancelling", processed))
}

/// `EXPLAIN` の応答列名（`QUERY PLAN`。他の `EXPLAIN` と同じ安定契約）。
/// 出すのはサーバー設定値と文の形状だけで、行数などテナントの存在情報は含めない。
pub(crate) fn explain_result(
    statement: &str,
    limits: &PartitionedDmlLimits,
    effective_chunk_rows: usize,
) -> QueryResult {
    let hold_ms = u64::try_from(limits.max_writer_hold.as_millis()).unwrap_or(u64::MAX);
    let lines = vec![
        "execution: partitioned (non-atomic)".to_string(),
        format!("statement: {statement}"),
        format!("chunk_rows: {effective_chunk_rows}"),
        format!("scan_budget_rows: {}", limits.scan_budget_rows),
        format!("max_writer_hold_ms: {hold_ms}"),
        "intermediate_state_visible: true".to_string(),
        "resume: resend the same statement with the same operation_id".to_string(),
    ];
    crate::sql::explain::lines_to_query_result(lines)
}

/// `SHOW`／`CANCEL`／`EXPLAIN` の結果列（Describe 用。実行せずに列だけを返す）。
pub(crate) fn describe_columns(control: &PartitionedControl) -> Vec<ColumnMeta> {
    match control {
        PartitionedControl::Show(_) | PartitionedControl::Cancel(_) => status_columns(),
        PartitionedControl::ExplainDelete(_) | PartitionedControl::ExplainUpdate(_) => {
            vec![ColumnMeta::Computed {
                name: crate::sql::explain::QUERY_PLAN_COLUMN.to_string(),
                ty: Some(crate::catalog::ColumnType::Text),
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::StorageError;
    use crate::tenant::TenantWriteError;

    fn fail<E>(cause: PartitionedStopCause<E>, committed: u64) -> PartitionedDmlFailure<E> {
        PartitionedDmlFailure {
            cause,
            committed_total: committed,
        }
    }

    fn map(cause: PartitionedStopCause<SqlSurfaceError>, committed: u64) -> SqlSurfaceError {
        map_partitioned_failure(fail(cause, committed), "op-1", "delete", |e| e)
    }

    #[test]
    fn zero_committed_failures_keep_the_cause_code() {
        let e = map(
            PartitionedStopCause::Write(TenantWriteError::UniqueViolation),
            0,
        );
        assert_eq!(e.wire_code(), "23505");
        let e = map(
            PartitionedStopCause::AffectedRowsLimitExceeded {
                limit: NonZeroUsize::MIN,
            },
            0,
        );
        assert_eq!(e.wire_code(), "54000");
        let e = map(PartitionedStopCause::InterruptedRecordLimitReached, 0);
        assert_eq!(e.wire_code(), "54000");
        let e = map(
            PartitionedStopCause::Predicate(SqlSurfaceError::invalid_input("x")),
            0,
        );
        assert_eq!(e.wire_code(), "22000");
        let e = map(
            PartitionedStopCause::Write(TenantWriteError::OperationIdContentMismatch),
            0,
        );
        assert_eq!(e.wire_code(), "22023");
        let e = map(
            PartitionedStopCause::Write(TenantWriteError::DuplicateOperationId),
            0,
        );
        assert_eq!(e.wire_code(), "23505");
    }

    #[test]
    fn committed_failures_become_partial_completion_with_cause_code_only() {
        let e = map(
            PartitionedStopCause::Write(TenantWriteError::UniqueViolation),
            3,
        );
        assert_eq!(e.wire_code(), "VD001");
        let msg = e.client_message();
        assert!(msg.contains("committed 3 rows"), "{msg}");
        assert!(msg.contains("cause 23505"), "{msg}");
        assert!(msg.contains("operation_id 'op-1'"), "{msg}");
        let e = map(
            PartitionedStopCause::AffectedRowsLimitExceeded {
                limit: NonZeroUsize::MIN,
            },
            2,
        );
        assert_eq!(e.wire_code(), "VD001");
        assert!(e.client_message().contains("cause 54000"));
        // 防御的に、記録上限でも件数があれば VD001。
        let e = map(PartitionedStopCause::InterruptedRecordLimitReached, 1);
        assert_eq!(e.wire_code(), "VD001");
        // 内部エラーは原因コード XX000 のまま件数付きで返し、内側の文言は載せない。
        let e = map(
            PartitionedStopCause::Write(TenantWriteError::Storage(StorageError::Codec(
                "secret detail".to_string(),
            ))),
            4,
        );
        assert_eq!(e.wire_code(), "VD001");
        assert!(!e.client_message().contains("secret"));
    }

    #[test]
    fn cancel_busy_and_ledger_causes_map_regardless_of_committed() {
        for committed in [0u64, 5] {
            let e = map(PartitionedStopCause::CancelRequested, committed);
            assert_eq!(e.wire_code(), "VD002");
            assert!(e
                .client_message()
                .contains(&format!("committed {committed} rows")));
            let e = map(PartitionedStopCause::AlreadyRunning, committed);
            assert_eq!(e.wire_code(), "55P03");
            let e = map(PartitionedStopCause::ConcurrencyLimitReached, committed);
            assert_eq!(e.wire_code(), "55P03");
        }
        let e = map(PartitionedStopCause::ResendCancelled { committed: 7 }, 0);
        assert_eq!(e.wire_code(), "VD002");
        assert!(e.client_message().contains("committed 7 rows"));
        let e = map(PartitionedStopCause::LedgerRequired, 0);
        assert_eq!(e.wire_code(), "0A000");
    }

    #[test]
    fn busy_messages_are_identical_for_tenant_and_total_limits() {
        // 上限の種別を応答から識別できない（同時実行数の上限は同じ文言）。
        let a = map(PartitionedStopCause::ConcurrencyLimitReached, 0).client_message();
        let b = map(PartitionedStopCause::ConcurrencyLimitReached, 0).client_message();
        assert_eq!(a, b);
        assert_ne!(
            a,
            map(PartitionedStopCause::AlreadyRunning, 0).client_message()
        );
    }

    #[test]
    fn chunk_override_may_only_shrink() {
        let server = PartitionedDmlLimits::default();
        let ok = PartitionedClause {
            chunk_rows: NonZeroUsize::new(10),
        };
        assert_eq!(
            apply_chunk_override(&ok, &server)
                .expect("shrink")
                .chunk_rows
                .get(),
            10
        );
        let same = PartitionedClause {
            chunk_rows: Some(server.chunk_rows),
        };
        assert!(apply_chunk_override(&same, &server).is_ok());
        let over = PartitionedClause {
            chunk_rows: NonZeroUsize::new(server.chunk_rows.get() + 1),
        };
        assert_eq!(
            apply_chunk_override(&over, &server)
                .expect_err("over")
                .wire_code(),
            "22000"
        );
        let none = PartitionedClause { chunk_rows: None };
        assert_eq!(
            apply_chunk_override(&none, &server)
                .expect("default")
                .chunk_rows,
            server.chunk_rows
        );
    }

    #[test]
    fn explain_result_exposes_only_server_settings() {
        let limits = PartitionedDmlLimits::default();
        let r = explain_result("delete", &limits, 25);
        let lines: Vec<String> = r
            .rows
            .iter()
            .filter_map(|row| match row.cells.first() {
                Some(Cell::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines.first().map(String::as_str),
            Some("execution: partitioned (non-atomic)")
        );
        assert!(lines.contains(&"statement: delete".to_string()));
        assert!(lines.contains(&"chunk_rows: 25".to_string()));
        assert!(lines.contains(&"intermediate_state_visible: true".to_string()));
    }

    // --- 同時実行数の上限（登録簿を先に埋めて EngineCore 経由で検証する）-----------

    fn core_with(
        label: &str,
        limits: PartitionedDmlLimits,
    ) -> (
        crate::core::EngineCore,
        crate::test_util::temp_db::CleanupGuard,
    ) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let core = crate::core::EngineCore::from_storage(
            storage,
            Box::new(crate::kernel::CpuScalarProvider),
        )
        .with_partitioned_dml_limits(limits);
        let mut session = crate::sql::mode::SessionState::default();
        session.allow_ddl();
        let sys = PolicyContext::new("sys").expect("tenant");
        core.execute_sql_in_session(&sys, &mut session, "CREATE TABLE docs (n BIGINT)")
            .expect("create table");
        (core, guard)
    }

    fn run_delete(
        core: &crate::core::EngineCore,
        tenant: &str,
        op: &str,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let ctx = PolicyContext::new(tenant).expect("tenant");
        let mut session = crate::sql::mode::SessionState::default();
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("DELETE FROM docs WHERE n > 0 USING OPERATION_ID '{op}' PARTITIONED"),
        )
    }

    #[test]
    fn tenant_and_total_concurrency_limits_reject_with_55p03_and_the_same_message() {
        let nz = |n: usize| NonZeroUsize::new(n).expect("non-zero");
        // テナント単位 1・全体 4（既定）: alice が実行中のところへ alice の別ジョブ。
        let (core, _g) = core_with("pdml-conc-tenant", PartitionedDmlLimits::default());
        let alice = PolicyContext::new("alice").expect("tenant");
        let busy_op = OperationId::parse("busy").expect("op");
        let _held = core
            .storage_for_test()
            .partitioned_job_registry()
            .try_register(&JobKey::for_context(&alice, "docs", &busy_op))
            .expect("register");
        let tenant_err = run_delete(&core, "alice", "new-a").expect_err("tenant limit");
        assert_eq!(tenant_err.wire_code(), "55P03");

        // 全体 1: 別テナント（bob）のジョブで枠が埋まっている。
        let (core2, _g2) = core_with(
            "pdml-conc-total",
            PartitionedDmlLimits {
                max_jobs_per_tenant: nz(1),
                max_jobs_total: nz(1),
                ..PartitionedDmlLimits::default()
            },
        );
        let bob = PolicyContext::new("bob").expect("tenant");
        let _held2 = core2
            .storage_for_test()
            .partitioned_job_registry()
            .try_register(&JobKey::for_context(&bob, "docs", &busy_op))
            .expect("register");
        let total_err = run_delete(&core2, "alice", "new-a").expect_err("total limit");
        assert_eq!(total_err.wire_code(), "55P03");
        // どちらの上限でも同じコード・同じ文言（上限の種別を識別できない）。
        assert_eq!(tenant_err.client_message(), total_err.client_message());

        // 同じキーの実行中は 55P03 だが文言が異なる。
        let same = run_delete(&core, "alice", "busy").expect_err("same key");
        assert_eq!(same.wire_code(), "55P03");
        assert_ne!(same.client_message(), tenant_err.client_message());
    }

    #[test]
    fn rejected_start_has_no_side_effects_and_succeeds_after_release() {
        let (core, _g) = core_with("pdml-conc-release", PartitionedDmlLimits::default());
        let alice = PolicyContext::new("alice").expect("tenant");
        let busy_op = OperationId::parse("busy").expect("op");
        let held = core
            .storage_for_test()
            .partitioned_job_registry()
            .try_register(&JobKey::for_context(&alice, "docs", &busy_op))
            .expect("register");
        assert!(run_delete(&core, "alice", "later").is_err());
        drop(held);
        // 解放後は同じ operation_id で開始できる（拒否時に記録を作っていない）。
        assert!(run_delete(&core, "alice", "later").is_ok());
    }
}
