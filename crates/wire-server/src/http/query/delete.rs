//! `POST /v1/query` の `delete` op を SQL 表層の
//! `DELETE FROM ... WHERE id = <n> USING OPERATION_ID`（SQL-18）・述語形
//! `DELETE FROM ... WHERE <述語> USING OPERATION_ID`（SQL-19・TASK-192）と
//! **同一の実行器**（`engine::sql::exec::execute_delete`・
//! `execute_predicate_delete`）へ、SQL テキストを組み立てずに束縛済み計画で
//! 到達させるモジュール（Issue #876・#1062・TASK-186・対象ビヘイビア
//! NOSQL-6・NOSQL-12。ポインタ: `docs/spec/05-tasks.md` TASK-178・TASK-186・
//! TASK-192・`docs/spec/04-behavior/nosql-surface.md` NOSQL-6・NOSQL-12・
//! `docs/spec/04-behavior/sql-surface.md` SQL-18・SQL-19）。
//!
//! 責務境界: [`super::schema::DELETE_SCHEMA`] が形を検証済みの JSON
//! オブジェクトから `table`／`where`（または `filter`）／`operation_id` を
//! 取り出し、[`super::dml_target::bind_target_form`]（`where`／`filter` の
//! 排他判定。[`super::update`] と共有）で対象行を確定したうえで、
//! [`execute`] が単一行形は
//! `engine::core::EngineCore::execute_bound_delete_in_session`
//! （[`super::insert`]・[`super::update`] と同型のセッション対応エントリ）、
//! 述語形は `execute_bound_predicate_delete_in_session`（Issue #1062）へ
//! 委譲する。単一行形は `id` 疑似列以外の列を参照しないため
//! `engine::sql::parser::BoundDelete` を直接構築するが、述語形は
//! [`super::filter::bind_filter_where_predicates`]（`WHERE` 述語列を SQL 表層
//! と同一の `WherePredicate` へ写像する。`content_hash` 一致のため必須）を
//! 経由する。いずれも第 2 の実行器は作らない。
//!
//! 分割実行修飾（`mode: "partitioned"`・`chunk`。Issue #1130・NOSQL-12・RECOVER-11）は
//! `bind_target_form` の後で [`super::partitioned::parse_modifier`] が解析し、述語形
//! （`filter`）のときだけ `EngineCore::execute_bound_partitioned_delete_in_session` へ委譲する
//! （SQL 表層の `... PARTITIONED` と同一の共通本体。`where` との併用は `42601`）。
//! `VD001`／`VD002` は [`handle`] が 409 と `data` 付き本文へ写像する。
//!
//! テナントは `principal`（唯一の入口）からのみ導出する（`security.md` P0）。
//!
//! `operation_id` の欠落・`null`・空文字はいずれも `23502`。同一
//! `operation_id` への再送は台帳照合（TASK-101・RECOVER-10）により内容一致
//! なら `23505`・不一致なら `22023` へ収束する（SQL 表層と同じ台帳キー空間
//! `(tenant, table, operation_id)` を共有する。`insert.rs`／`update.rs` と
//! 同じ契約）。
//!
//! [`execute`] は成功時 [`DeleteSuccess`] を返し、[`handle`] が
//! [`encode_success_body`]（`{"deleted":<n>,"operation_id":"<escaped>"}`。
//! キー順固定・空白なし）を経由して `gate.rs` の `Op::Delete` アームへ
//! ディスパッチ可能な応答バイト列へ写像する。他テナント所有 id・未存在 id
//! はいずれも `deleted:0`・`200`（RLS-9。`execute_delete` のドキュメント
//! 参照）。
//!
//! 対象外: `RETURNING`（Issue #873。NoSQL は公開しない）。`filter` は `search` と同じ語彙
//! （`or`・範囲比較・`in`・数値列を含む。Issue #1356）を `filter.rs` の
//! `bind_filter_where_predicates` が SQL の述語形 `DELETE` と同一の構文形へ写像する
//! （`ARRAY`／`JSON`／`JSONB` 列への `eq`／`ne` のみ `0A000`）。

use std::fmt::Write as _;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::DeleteOutcome;
use engine::sql::parser::BoundDelete;

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::dml_target::{bind_target_form, DmlTargetError, TargetForm};
use super::filter;
use super::ident::{self, InvalidIdentifier};
use super::partitioned::{self, PartitionedRequestError};
use super::schema::{SchemaError, Validated};

/// [`execute`] の失敗を表す。いずれも [`ClassifiedError`] を実装する
/// （`insert.rs::InsertError`・`update.rs::UpdateError` と同じ設計）。
#[derive(Debug, Clone)]
pub enum DeleteError {
    /// [`Validated`] アクセサの型・キー不整合（多層防御）。
    Shape(SchemaError),
    /// `table` が識別子として意味を持ちうる形状を満たさない。
    InvalidIdentifier,
    /// `where`／`filter` の排他判定・述語形未対応（[`super::dml_target`]）。
    Target(DmlTargetError),
    /// `EngineCore::execute_bound_delete_in_session` のエラー
    /// （`operation_id` 必須化・台帳照合・テーブル不存在等）をそのまま透過する。
    Engine(SqlSurfaceError),
    /// 分割実行修飾（`mode`／`chunk`）の解析・検査の失敗（Issue #1130）。
    Partitioned(PartitionedRequestError),
}

impl From<SchemaError> for DeleteError {
    fn from(err: SchemaError) -> Self {
        DeleteError::Shape(err)
    }
}

impl From<InvalidIdentifier> for DeleteError {
    fn from(_err: InvalidIdentifier) -> Self {
        DeleteError::InvalidIdentifier
    }
}

impl From<DmlTargetError> for DeleteError {
    fn from(err: DmlTargetError) -> Self {
        DeleteError::Target(err)
    }
}

impl From<PartitionedRequestError> for DeleteError {
    fn from(err: PartitionedRequestError) -> Self {
        DeleteError::Partitioned(err)
    }
}

impl From<SqlSurfaceError> for DeleteError {
    fn from(err: SqlSurfaceError) -> Self {
        DeleteError::Engine(err)
    }
}

impl ClassifiedError for DeleteError {
    fn error_class(&self) -> ErrorClass {
        match self {
            DeleteError::Shape(err) => err.error_class(),
            DeleteError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            DeleteError::Target(err) => err.error_class(),
            DeleteError::Engine(err) => err.error_class(),
            DeleteError::Partitioned(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            DeleteError::Shape(err) => err.client_message(),
            DeleteError::InvalidIdentifier => "invalid identifier".to_string(),
            DeleteError::Target(err) => err.client_message(),
            DeleteError::Engine(err) => err.client_message(),
            DeleteError::Partitioned(err) => err.client_message(),
        }
    }
}

/// `delete` op 成功時の応答材料（[`encode_success_body`] の唯一の情報源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteSuccess {
    pub deleted: u64,
    pub operation_id: OperationId,
}

/// [`DeleteSuccess`] を成功応答本文（JSON）へ写像する（infallible）。
/// 出力形 `{"deleted":<n>,"operation_id":"<escaped>"}`。
pub fn encode_success_body(success: &DeleteSuccess) -> String {
    let mut out = String::with_capacity(success.operation_id.as_str().len() + 64);
    out.push_str("{\"deleted\":");
    let _ = write!(out, "{}", success.deleted);
    out.push_str(",\"operation_id\":\"");
    escape_json_string_into(&mut out, success.operation_id.as_str());
    out.push_str("\"}");
    out
}

/// `POST /v1/query` の `delete` op を実行する。`validated` は
/// [`super::schema::DELETE_SCHEMA::validate`] を通過済みの JSON オブジェクト。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<DeleteSuccess, DeleteError> {
    let table = validated
        .required_str("table")
        .map_err(DeleteError::Shape)?;
    ident::check_identifier(table)?;

    let target = bind_target_form(validated)?;
    // 分割実行修飾（Issue #1130）。述語形（`filter`）だけが受理する。
    let modifier = partitioned::parse_modifier(validated)?;
    partitioned::check_target_form(modifier, &target)?;

    let operation_id_raw = validated
        .optional_str("operation_id")
        .map_err(DeleteError::Shape)?
        .unwrap_or("");
    let operation_id = OperationId::parse(operation_id_raw)?;

    let outcome: DeleteOutcome = match target {
        TargetForm::RowId(id) => {
            let bound = BoundDelete {
                table: table.to_string(),
                id,
                operation_id: Some(operation_id.clone()),
            };
            core.execute_bound_delete_in_session(principal.policy_context(), &bound)?
        }
        // 述語形（`filter`。TASK-186・NOSQL-12、Issue #1062）: `WHERE` 述語列は
        // `filter::bind_filter_where_predicates`（SQL 表層の述語形 `DELETE` が
        // 使うのと同一の `WherePredicate` を生成する）で得る。実行本体
        // （台帳照合・影響行数上限・RLS 適用）は `EngineCore::
        // execute_bound_predicate_delete_in_session` に一任し、第 2 の実行器は
        // 作らない。
        TargetForm::Predicate(items) => {
            let bind = |schema: &engine::catalog::TableSchema| {
                filter::bind_filter_where_predicates(items, schema)
                    .map_err(filter::FilterError::into_sql_surface_error)
            };
            match modifier {
                // 分割実行（Issue #1130）: SQL 表層 `DELETE ... PARTITIONED` と同一の
                // 共通本体へ到達する。
                Some(m) => core.execute_bound_partitioned_delete_in_session(
                    principal.policy_context(),
                    table,
                    Some(&operation_id),
                    m.chunk_rows,
                    bind,
                )?,
                None => core.execute_bound_predicate_delete_in_session(
                    principal.policy_context(),
                    table,
                    Some(&operation_id),
                    bind,
                )?,
            }
        }
    };

    Ok(DeleteSuccess {
        deleted: outcome.rows_affected,
        operation_id,
    })
}

/// `POST /v1/query` の `delete` op を処理し応答バイト列を返す（`gate.rs` から
/// `engine` 接続済み時のみ呼ばれる。`insert::handle`／`update::handle` と
/// 同一シグネチャ形）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute(core, principal, validated) {
        Ok(success) => http_response::encode_ok(&encode_success_body(&success), now_wall),
        // `VD001`／`VD002` は 409 と `data` 付きの本文（Issue #1130）。
        Err(DeleteError::Engine(err)) => {
            partitioned::encode_engine_error(&err, validated, now_wall)
        }
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_error_wire_codes_match_expected_classes() {
        assert_eq!(DeleteError::InvalidIdentifier.wire_code(), "42601");
    }
}
