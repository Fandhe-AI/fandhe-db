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
//! `operation_id` の欠落・`null`・空文字はいずれも「省略」として扱い、wire 層では拒否しない。
//! 省略の可否は engine の `LedgerMode`（単一の真実源）が判定し、既定（任意）では台帳に
//! 記録しない通常の書き込みとして成功し（応答本文に `operation_id` キーを含めない）、
//! `--require-operation-id`（必須）では `23502` になる（Issue #2203・RECOVER-1・NOSQL-6
//! ポインタ）。 同一
//! `operation_id` への再送は台帳照合（TASK-101・RECOVER-10）により内容一致
//! なら `23505`・不一致なら `22023` へ収束する（SQL 表層と同じ台帳キー空間
//! `(tenant, table, operation_id)` を共有する。`insert.rs`／`update.rs` と
//! 同じ契約）。
//!
//! [`execute`] は成功時 [`DeleteSuccess`] を返し、[`handle`] が
//! [`encode_success_body`]（`{"deleted":<n>[,"operation_id":"<escaped>"]}`（`operation_id` は指定時のみ）。
//! キー順固定・空白なし）を経由して `gate.rs` の `Op::Delete` アームへ
//! ディスパッチ可能な応答バイト列へ写像する。他テナント所有 id・未存在 id
//! はいずれも `deleted:0`・`200`（RLS-9。`execute_delete` のドキュメント
//! 参照）。
//!
//! `returning` キー（Issue #2090・#2091・NOSQL-22）は `gate.rs` 手順 5b が検査した投影を
//! [`handle`] が受け取り、各形の engine の RETURNING 付き入口（SQL 表層 `DELETE ... RETURNING`
//! と同一の実行器）へ委譲する。成功本文の形は `encode_success_body_with_returning`。`filter` は `search` と同じ語彙
//! （`or`・範囲比較・`in`・数値列を含む。Issue #1356）を `filter.rs` の
//! `bind_filter_where_predicates` が SQL の述語形 `DELETE` と同一の構文形へ写像する
//! （`ARRAY`／`JSON`／`JSONB` 列の `eq`／`ne`／`in` も受理する。Issue #1410）。

use std::fmt::Write as _;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::{Projection, SqlSurfaceError};
use engine::sql::exec::QueryResult;
use engine::sql::parser::BoundDelete;

use crate::http::error_body::escape_json_string_into;
use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::dml_target::{self, bind_target_form, DmlTargetError, TargetForm};
use super::filter;
use super::ident::{self, InvalidIdentifier};
use super::operation_id_source::{resolve_operation_id, OperationIdSourceError};
use super::partitioned::{self, PartitionedRequestError};
use super::response::{write_columns_and_rows_into, ResponseEncodeError};
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
    /// 省略時は `None`（応答本文に `operation_id` キーを含めない。Issue #2203）。
    pub operation_id: Option<OperationId>,
}

/// [`DeleteSuccess`] を成功応答本文（JSON）へ写像する（infallible）。
/// 出力形 `{"deleted":<n>[,"operation_id":"<escaped>"]}`（`operation_id` は指定時のみ）。
pub fn encode_success_body(success: &DeleteSuccess) -> String {
    let mut out = String::with_capacity(
        success
            .operation_id
            .as_ref()
            .map_or(0, |id| id.as_str().len())
            + 64,
    );
    out.push('{');
    write_success_fields(&mut out, success);
    out.push('}');
    out
}

/// `"deleted":<n>[,"operation_id":"<escaped>"]` を書く（波括弧は含めない）。
/// [`encode_success_body`] と [`encode_success_body_with_returning`] が共有する。
fn write_success_fields(out: &mut String, success: &DeleteSuccess) {
    out.push_str("\"deleted\":");
    let _ = write!(out, "{}", success.deleted);
    if let Some(id) = &success.operation_id {
        out.push_str(",\"operation_id\":\"");
        escape_json_string_into(out, id.as_str());
        out.push('"');
    }
}

/// `returning` 付きの成功応答本文（Issue #2090・NOSQL-22）。
/// 出力形 `{"deleted":<n>[,"operation_id":"..."],"columns":[...],"rows":[...]}`
/// （`insert::encode_success_body_with_returning` と同型。`row_count` は付けない）。
/// 呼び出し元は [`handle`]（Issue #2091）。
pub fn encode_success_body_with_returning(
    success: &DeleteSuccess,
    result: &QueryResult,
) -> Result<String, ResponseEncodeError> {
    let mut out = String::new();
    out.push('{');
    write_success_fields(&mut out, success);
    out.push(',');
    write_columns_and_rows_into(&mut out, result)?;
    out.push('}');
    Ok(out)
}

/// `POST /v1/query` の `delete` op を実行する。`validated` は
/// [`super::schema::DELETE_SCHEMA::validate`] を通過済みの JSON オブジェクト。
pub fn execute(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    header_operation_id: Option<&OperationId>,
) -> Result<DeleteSuccess, DeleteError> {
    execute_with(core, principal, validated, header_operation_id, None).map(|(success, _)| success)
}

/// [`execute`] の本体。`returning` が `Some` のときは各形に対応する engine の RETURNING 付き
/// 入口（SQL 表層 `DELETE ... RETURNING` と同一の実行器。NOSQL-22・Issue #2091）へ委譲して
/// 削除前の `QueryResult` も返し、`None` のときは従来の件数だけの入口を呼ぶ。分割実行修飾との
/// 併用は `gate.rs` 手順 5b が `42601` で止めるが、ここでも多層防御として同じ `42601` で拒否する
/// （分割実行器は `RETURNING` を持たない）。
fn execute_with(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    header_operation_id: Option<&OperationId>,
    returning: Option<&Projection>,
) -> Result<(DeleteSuccess, Option<QueryResult>), DeleteError> {
    let table = validated
        .required_str("table")
        .map_err(DeleteError::Shape)?;
    ident::check_identifier(table)?;

    let target = bind_target_form(validated)?;
    // 分割実行修飾（Issue #1130）。述語形（`filter`）だけが受理する。
    let modifier = partitioned::parse_modifier(validated)?;
    partitioned::check_target_form(modifier, &target)?;
    if returning.is_some() && modifier.is_some() {
        return Err(DeleteError::Engine(SqlSurfaceError::UnsupportedSyntax {
            detail: "returning cannot be combined with mode partitioned".to_string(),
        }));
    }

    // 本文／ヘッダの供給元解決（Issue #2064・HTTP-14）。省略の可否は engine の
    // `LedgerMode`（単一の真実源）が担うため、wire 層では拒否しない（Issue #2203）。
    let operation_id =
        resolve_operation_id(validated, header_operation_id).map_err(|e| match e {
            OperationIdSourceError::Shape(err) => DeleteError::Shape(err),
            other => DeleteError::Engine(other.into_sql_surface_error()),
        })?;

    let (deleted, result): (u64, Option<QueryResult>) = match target {
        // 単一行アドレス（`where`。Issue #2201）: 疑似列 `id` を持つ表で `{"id"}` のみなら従来の
        // 単一行形、それ以外（採番方式の表・PRIMARY KEY 形）は PK の等価述語で述語形へ流す。
        // 経路は事前判定で選び、束縛時スナップショットと食い違えば（並行 DDL）束縛 closure が
        // fail-closed で拒否する。
        TargetForm::RowAddress(address)
            if dml_target::is_id_address(address)
                && !core.table_uses_allocated_row_identity(table) =>
        {
            let id = dml_target::row_id_from_address(address)?;
            let bound = BoundDelete {
                table: table.to_string(),
                id,
                operation_id: operation_id.clone(),
            };
            match returning {
                None => (
                    core.execute_bound_delete_checked_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        &bound,
                        super::gate::reject_allocated_schema,
                    )?
                    .rows_affected,
                    None,
                ),
                Some(projection) => {
                    let o = core.execute_bound_delete_checked_returning_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        &bound,
                        projection,
                        super::gate::reject_allocated_schema,
                    )?;
                    (o.rows_affected, Some(o.result))
                }
            }
        }
        TargetForm::RowAddress(address) => {
            let bind = |schema: &engine::catalog::TableSchema| {
                dml_target::bind_primary_key_predicates(address, schema)
            };
            match returning {
                None => (
                    core.execute_bound_predicate_delete_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        table,
                        operation_id.as_ref(),
                        bind,
                    )?
                    .rows_affected,
                    None,
                ),
                Some(projection) => {
                    let o = core.execute_bound_predicate_delete_returning_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        table,
                        operation_id.as_ref(),
                        projection,
                        bind,
                    )?;
                    (o.rows_affected, Some(o.result))
                }
            }
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
            match (modifier, returning) {
                // 分割実行（Issue #1130）: SQL 表層 `DELETE ... PARTITIONED` と同一の
                // 共通本体へ到達する。
                (Some(m), _) => (
                    core.execute_bound_partitioned_delete_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        table,
                        operation_id.as_ref(),
                        m.chunk_rows,
                        bind,
                    )?
                    .rows_affected,
                    None,
                ),
                (None, None) => (
                    core.execute_bound_predicate_delete_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        table,
                        operation_id.as_ref(),
                        bind,
                    )?
                    .rows_affected,
                    None,
                ),
                (None, Some(projection)) => {
                    let o = core.execute_bound_predicate_delete_returning_in_session(
                        principal.policy_context(),
                        &principal.session_state(),
                        table,
                        operation_id.as_ref(),
                        projection,
                        bind,
                    )?;
                    (o.rows_affected, Some(o.result))
                }
            }
        }
    };

    Ok((
        DeleteSuccess {
            deleted,
            operation_id,
        },
        result,
    ))
}

/// `returning` 付きの成功本文を `200` 応答へ組み立てる。本文のエンコードは commit **後**に
/// 行われるため、失敗しても書き込みは確定済みである。成功を偽らず・失敗とも断定しない
/// may-be-committed 応答（`data.state`）へ写像する（RECOVER-5 (3)・ERR-5 ポインタ。
/// 固定文言のみで列名・値を含めない）。
fn render_returning_ok(
    success: &DeleteSuccess,
    result: &QueryResult,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match encode_success_body_with_returning(success, result) {
        Ok(body) => http_response::encode_ok(&body, now_wall),
        Err(ResponseEncodeError) => http_response::encode_error_may_be_committed(
            ErrorClass::InternalError,
            "internal error",
            now_wall,
        ),
    }
}

/// `POST /v1/query` の `delete` op を処理し応答バイト列を返す（`gate.rs` から
/// `engine` 接続済み時のみ呼ばれる。`insert::handle`／`update::handle` と
/// 同一シグネチャ形）。`returning`（`gate.rs` 手順 5b が検証済みの投影。NOSQL-22・
/// Issue #2091）が `Some` のときは本文に削除前の `columns`／`rows` を加える
/// （`None` は従来とバイト単位で同一）。
pub fn handle(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    header_operation_id: Option<&OperationId>,
    returning: Option<&Projection>,
    now_wall: std::time::SystemTime,
) -> Vec<u8> {
    match execute_with(core, principal, validated, header_operation_id, returning) {
        Ok((success, Some(result))) => render_returning_ok(&success, &result, now_wall),
        Ok((success, None)) => http_response::encode_ok(&encode_success_body(&success), now_wall),
        // `VD001`／`VD002` は 409 と `data` 付きの本文（Issue #1130）。
        // 409 の `data.operation_id` はヘッダのみ指定でも解決済みの値を返す（Issue #2064）。
        Err(DeleteError::Engine(err)) => {
            let resolved = resolve_operation_id(validated, header_operation_id)
                .ok()
                .flatten();
            partitioned::encode_engine_error(&err, resolved.as_ref(), now_wall)
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

    #[test]
    fn encode_success_body_omits_operation_id_key_when_absent() {
        let success = DeleteSuccess {
            deleted: 3,
            operation_id: None,
        };
        assert_eq!(encode_success_body(&success), r#"{"deleted":3}"#);
    }

    #[test]
    fn encode_success_body_keeps_operation_id_key_when_present() {
        let success = DeleteSuccess {
            deleted: 3,
            operation_id: Some(OperationId::parse("op-1").expect("valid operation_id")),
        };
        assert_eq!(
            encode_success_body(&success),
            r#"{"deleted":3,"operation_id":"op-1"}"#
        );
    }

    fn returning_result() -> engine::sql::exec::QueryResult {
        use engine::catalog::ColumnType;
        use engine::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
        QueryResult {
            columns: vec![
                ColumnMeta::Id,
                ColumnMeta::Scalar {
                    name: "body".to_string(),
                    ty: ColumnType::Text,
                },
            ],
            rows: vec![ResultRow {
                id: 7,
                score: 0.0,
                cells: vec![Cell::Integer(7), Cell::Text("a\"b\\c".to_string())],
            }],
        }
    }

    #[test]
    fn encode_success_body_with_returning_has_fixed_key_order() {
        let result = returning_result();
        let columns_rows = {
            let mut s = String::new();
            crate::http::query::response::write_columns_and_rows_into(&mut s, &result)
                .expect("encode");
            s
        };
        let plain = DeleteSuccess {
            deleted: 3,
            operation_id: None,
        };
        assert_eq!(
            encode_success_body_with_returning(&plain, &result).expect("encode"),
            format!(r#"{{"deleted":3,{columns_rows}}}"#)
        );
        let with_id = DeleteSuccess {
            deleted: 3,
            operation_id: Some(OperationId::parse("op-1").expect("valid operation_id")),
        };
        assert_eq!(
            encode_success_body_with_returning(&with_id, &result).expect("encode"),
            format!(r#"{{"deleted":3,"operation_id":"op-1",{columns_rows}}}"#)
        );
        assert!(!columns_rows.contains("row_count"));
        assert!(columns_rows.starts_with(r#""columns":["#));
    }

    #[test]
    fn encode_success_body_with_returning_empty_rows() {
        use engine::sql::exec::{ColumnMeta, QueryResult};
        let success = DeleteSuccess {
            deleted: 0,
            operation_id: None,
        };
        let empty = QueryResult {
            columns: vec![ColumnMeta::Id],
            rows: vec![],
        };
        let body = encode_success_body_with_returning(&success, &empty).expect("encode");
        assert!(body.ends_with(r#""rows":[]}"#), "{body}");
    }
}
