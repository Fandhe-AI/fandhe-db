//! SEQUENCE 関数 `setval`／`lastval` の実行本体（TABLE-24・ERR-6・Issue #1691。親: #1686。
//! 設計記録: `docs/design/sequence-namespace-and-ledger.md` §10）。
//!
//! 役割: `core.rs` の `ParsedSql::SequenceFunction` 腕（autocommit・明示／暗黙トランザクション・
//! Describe）から呼ばれ、検証済みの [`ValidatedSequenceFunction`] を実行して 1 行 1 列の
//! `bigint` 結果（[`QueryResult`]）を返す。永続状態の読み書きは `catalog::sequence`
//! （`Storage::set_sequence_value`・`setval_in_txn`・`sequence_id_exists_in`）へ委譲し、
//! セッションの `currval`／`lastval` 状態は [`crate::sql::sequence_scope`] が持つ。
//!
//! テナント境界（P0）: `setval` が書くのは呼び出し元テナントの状態行だけで、他テナントの採番へ
//! 影響しない（TABLE-24 (2)）。`lastval`・`currval` はテナント込みのキーで引く。エラー文言に
//! 値・テナント・他テナントの存在情報を含めない。`setval` は DDL 権限ゲートをかけない
//! （PostgreSQL でも UPDATE 権限相当で、作用範囲が自テナントに閉じるため。判断は設計記録 §10）。
//!
//! 永続化の契約: autocommit は自前の書き込み txn で状態行を書き、commit 成功後にメモリ上の現在値を
//! 反映する。トランザクション内は共有 write txn に書いて即時にメモリへ反映する（ROLLBACK しても
//! メモリは戻らず、PostgreSQL の `setval` の非トランザクション性と同じ）。

use crate::catalog::sequence::{SetvalArgs, SetvalError};
use crate::catalog::{CatalogError, ColumnType};
use crate::sql::allowlist::{SequenceFnKind, SqlSurfaceError, ValidatedSequenceFunction};
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// 結果列のメタデータ（1 列・`bigint`）。実行と Describe が共有する唯一の写像。
pub(crate) fn columns(f: &ValidatedSequenceFunction) -> Vec<ColumnMeta> {
    vec![ColumnMeta::Computed {
        name: f.column_name(),
        ty: Some(ColumnType::BigInt),
    }]
}

fn result(f: &ValidatedSequenceFunction, value: i64) -> QueryResult {
    QueryResult {
        columns: columns(f),
        rows: vec![ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::SignedInteger(value)],
        }],
    }
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// `setval` の失敗を SQL 表層のエラーへ写像する（範囲外 `22003`・件数上限 `54000`・
/// カタログ系は SEQUENCE DDL と同じ写像）。文言は固定で値・テナントを含まない。
fn map_setval_error(e: SetvalError, poisoned: bool) -> SqlSurfaceError {
    use crate::catalog::sequence::AllocError;
    match e {
        SetvalError::OutOfRange => {
            SqlSurfaceError::numeric_out_of_range("setval: value is out of bounds for sequence")
        }
        SetvalError::SessionFull if poisoned => internal("sequence session state unavailable"),
        SetvalError::SessionFull => SqlSurfaceError::payload_too_large("too many sequences"),
        SetvalError::Alloc(AllocError::TooManyCursors) => {
            SqlSurfaceError::payload_too_large("too many sequences")
        }
        SetvalError::Alloc(AllocError::Catalog(c)) | SetvalError::Catalog(c) => {
            crate::sql::ddl::map_sequence_error(c, "setval failed")
        }
        SetvalError::Alloc(_) => internal("sequence allocator unavailable"),
    }
}

/// `setval(..., true)` の値をセッションの `currval` へ記録する（`lastval` の対象は変えない）。
fn record_session(
    session: &SessionState,
    tenant_id: &str,
    sequence_id: u64,
    value: i64,
) -> Result<(), SqlSurfaceError> {
    let handle = session.sequences_handle();
    let mut state = handle
        .lock()
        .map_err(|_| internal("sequence session state unavailable"))?;
    if state.record_setval(tenant_id, sequence_id, value) {
        Ok(())
    } else {
        Err(SqlSurfaceError::payload_too_large("too many sequences"))
    }
}

/// 件数上限の事前検査（commit の前に呼び、commit 後の記録失敗を防ぐ）。ロックの毒化は
/// `poisoned` を立てて拒否側に倒す。
fn admit_fn<'a>(
    session: &'a SessionState,
    tenant_id: &'a str,
    is_called: bool,
    poisoned: &'a std::cell::Cell<bool>,
) -> impl FnOnce(u64) -> bool + 'a {
    move |id| {
        if !is_called {
            return true;
        }
        match session.sequences_handle().lock() {
            Ok(state) => state.can_record(tenant_id, id),
            Err(_) => {
                poisoned.set(true);
                false
            }
        }
    }
}

/// autocommit での実行（`setval` は自前の書き込み txn、`lastval` はスナップショット読み取り）。
pub(crate) fn execute_autocommit(
    storage: &Storage,
    session: &SessionState,
    tenant_id: &str,
    f: &ValidatedSequenceFunction,
) -> Result<QueryResult, SqlSurfaceError> {
    match &f.kind {
        SequenceFnKind::Setval {
            name,
            value,
            is_called,
        } => {
            let poisoned = std::cell::Cell::new(false);
            let args = SetvalArgs {
                name,
                value: *value,
                is_called: *is_called,
            };
            let id = storage
                .set_sequence_value(
                    tenant_id,
                    &args,
                    admit_fn(session, tenant_id, *is_called, &poisoned),
                )
                .map_err(|e| map_setval_error(e, poisoned.get()))?;
            // commit 済みの後段。件数上限は `admit_fn` が commit 前に検査済みで、ここで失敗しうるのは
            // セッション状態のロックの毒化だけ（接続単位の値で、発生は想定しない）。
            if *is_called {
                record_session(session, tenant_id, id, *value)?;
            }
            Ok(result(f, *value))
        }
        SequenceFnKind::Lastval => {
            lastval(session, tenant_id, f, |id| storage.sequence_id_exists(id))
        }
    }
}

/// 明示／暗黙トランザクション内での実行。`write_txn` はセッションの共有書き込み txn で、
/// `setval` はこれへ状態行を書き（commit は呼び出し元の COMMIT）、メモリ上の現在値を即時に
/// 反映する。戻り値の真偽は「永続状態を書いたか」（呼び出し元が `has_writes` を立てる根拠）。
pub(crate) fn execute_in_txn(
    storage: &Storage,
    write_txn: &redb::WriteTransaction,
    session: &SessionState,
    tenant_id: &str,
    f: &ValidatedSequenceFunction,
) -> Result<(QueryResult, bool), SqlSurfaceError> {
    match &f.kind {
        SequenceFnKind::Setval {
            name,
            value,
            is_called,
        } => {
            let poisoned = std::cell::Cell::new(false);
            let args = SetvalArgs {
                name,
                value: *value,
                is_called: *is_called,
            };
            let id = crate::catalog::sequence::setval_in_txn(
                storage.sequence_allocator(),
                write_txn,
                tenant_id,
                &args,
                admit_fn(session, tenant_id, *is_called, &poisoned),
                true,
            )
            .map_err(|e| map_setval_error(e, poisoned.get()))?;
            if *is_called {
                record_session(session, tenant_id, id, *value)?;
            }
            Ok((result(f, *value), true))
        }
        SequenceFnKind::Lastval => {
            let r = lastval(session, tenant_id, f, |id| {
                crate::catalog::sequence::sequence_id_exists_in(write_txn, id)
            })?;
            Ok((r, false))
        }
    }
}

/// `lastval()` の本体。対象はこのセッション・呼び出し元テナントで最後に `nextval` が成功した
/// SEQUENCE で、内部 ID で存在確認する（RENAME は ID を保つため生き、DROP 後は同名で作り直しても
/// 別 ID なので無効）。無効なら該当エントリを捨てて `55000`（fail-closed。設計記録 D6）。
fn lastval(
    session: &SessionState,
    tenant_id: &str,
    f: &ValidatedSequenceFunction,
    exists: impl FnOnce(u64) -> Result<bool, CatalogError>,
) -> Result<QueryResult, SqlSurfaceError> {
    let handle = session.sequences_handle();
    let target = handle
        .lock()
        .map_err(|_| internal("sequence session state unavailable"))?
        .lastval(tenant_id);
    let Some((id, value)) = target else {
        return Err(SqlSurfaceError::LastvalNotYetDefined);
    };
    let alive = exists(id).map_err(|e| crate::sql::ddl::map_sequence_error(e, "lastval failed"))?;
    if !alive {
        handle
            .lock()
            .map_err(|_| internal("sequence session state unavailable"))?
            .forget(tenant_id, id);
        return Err(SqlSurfaceError::LastvalNotYetDefined);
    }
    Ok(result(f, value))
}
