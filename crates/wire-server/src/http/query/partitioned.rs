//! `POST /v1/query` の分割実行 DML 語彙（Issue #1130・TASK-186 系・対象ビヘイビア
//! NOSQL-12・ERR-1／2／4・RLS-9／10・RECOVER-11。ポインタ: ADR
//! `docs/design/partitioned-dml.md` 10 節）。
//!
//! 責務境界（wire 層は JSON の形と値の検証・engine 呼び出し・応答写像だけを担い、
//! 実行器・内容照合ハッシュ・上限・失敗写像は engine 側の共有本体に一任する）:
//!
//! - [`parse_modifier`]: `update`／`delete` op の分割実行修飾（`mode`・`chunk`）の解析。
//!   [`super::update`]・[`super::delete`] の `execute` が `bind_target_form` の後で呼ぶ。
//!   `mode` は `"partitioned"` だけを受理し（それ以外は黙って原子実行へ切り替えず `42601`）、
//!   `chunk` は `mode` 指定時だけ有効（SQL で `PARTITIONED` 無しの `CHUNK` が構文エラーに
//!   なるのと同じ）。`chunk` の値は SQL 字句解析とのパリティ（`0`・小数・`usize` 超過は
//!   `22000`、負数は `42601`）。サーバー幅超過の `22000` は engine の
//!   `EngineCore::execute_bound_partitioned_*_in_session` が判定する。
//! - [`check_target_form`]: 修飾は述語形（`filter`）のときだけ受理する（`where` との組合せは
//!   `42601`）。
//! - `show_partitioned_dml`／`cancel_partitioned_dml` op の [`execute_show`]／
//!   [`execute_cancel`]／[`handle_show`]／[`handle_cancel`]: SQL 表層の
//!   `SHOW`／`CANCEL PARTITIONED DML` と同じ engine 実行本体へ委譲する。応答は engine が返す
//!   `QueryResult` をそのまま [`super::response::encode`] へ渡す（該当なしは 0 行。
//!   ジョブなし・他テナント・テーブルなし・見えないテーブルは応答バイト同一。RLS-9）。
//! - [`encode_engine_error`]: `VD001`（部分完了）・`VD002`（取り消し）を HTTP 409 と
//!   `data.committed`・`data.operation_id` 付きの本文へ写像する。それ以外の分類は通常の
//!   エラー応答のまま。
//!
//! テナントは `principal`（唯一の入口）からのみ導出する（`security.md` P0）。JSON・ヘッダの
//! `tenant_id` 自己申告は gate のスキーマ検証が未知キーとして `42601` にする。

use std::num::NonZeroUsize;
use std::time::SystemTime;

use engine::core::EngineCore;
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::JsonNumber;
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;

use crate::http::response as http_response;
use crate::http::session::middleware::SessionPrincipal;

use super::dml_target::TargetForm;
use super::ident::{self, InvalidIdentifier};
use super::schema::{SchemaError, Validated};

/// `mode` が受理する唯一の値。
pub const MODE_PARTITIONED: &str = "partitioned";

/// 分割実行修飾（`mode: "partitioned"`・任意の `chunk`）の解析結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionedModifier {
    /// `chunk` の指定値（省略時は `None`＝サーバー設定のチャンク幅）。
    pub chunk_rows: Option<NonZeroUsize>,
}

/// 修飾の解析・検査の失敗。いずれも [`ClassifiedError`] を実装する。
#[derive(Debug, Clone)]
pub enum PartitionedRequestError {
    /// [`Validated`] アクセサの型・キー不整合。
    Shape(SchemaError),
    /// `mode` が `"partitioned"` 以外（`42601`）。
    InvalidMode,
    /// `mode` なしの `chunk`（`42601`）。
    ChunkWithoutMode,
    /// `chunk` が `0`・小数・`usize` に収まらない値（`22000`）。
    InvalidChunk,
    /// `chunk` が負数（`42601`。SQL の `CHUNK -1` と同じ）。
    NegativeChunk,
    /// `mode` と `where`（単一行形）の組合せ（`42601`）。
    WhereNotAllowed,
}

impl From<SchemaError> for PartitionedRequestError {
    fn from(err: SchemaError) -> Self {
        PartitionedRequestError::Shape(err)
    }
}

impl ClassifiedError for PartitionedRequestError {
    fn error_class(&self) -> ErrorClass {
        match self {
            PartitionedRequestError::Shape(err) => err.error_class(),
            PartitionedRequestError::InvalidMode
            | PartitionedRequestError::ChunkWithoutMode
            | PartitionedRequestError::NegativeChunk
            | PartitionedRequestError::WhereNotAllowed => ErrorClass::UnsupportedSqlSyntax,
            PartitionedRequestError::InvalidChunk => ErrorClass::InvalidInput,
        }
    }

    fn client_message(&self) -> String {
        match self {
            PartitionedRequestError::Shape(err) => err.client_message(),
            PartitionedRequestError::InvalidMode => {
                "mode must be \"partitioned\" when specified".to_string()
            }
            PartitionedRequestError::ChunkWithoutMode => {
                "chunk requires mode \"partitioned\"".to_string()
            }
            PartitionedRequestError::InvalidChunk => "chunk must be a positive integer".to_string(),
            PartitionedRequestError::NegativeChunk => {
                "chunk must be a positive integer".to_string()
            }
            PartitionedRequestError::WhereNotAllowed => {
                "mode partitioned requires filter, not where".to_string()
            }
        }
    }
}

/// `update`／`delete` の `mode`・`chunk` を解析する。両方欠落は `Ok(None)`（原子実行）。
pub fn parse_modifier(
    validated: &Validated<'_>,
) -> Result<Option<PartitionedModifier>, PartitionedRequestError> {
    let mode = validated.optional_str("mode")?;
    let chunk = validated.optional_json_number("chunk")?;
    match (mode, chunk) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(PartitionedRequestError::ChunkWithoutMode),
        (Some(m), _) if m != MODE_PARTITIONED => Err(PartitionedRequestError::InvalidMode),
        (Some(_), None) => Ok(Some(PartitionedModifier { chunk_rows: None })),
        (Some(_), Some(n)) => Ok(Some(PartitionedModifier {
            chunk_rows: Some(chunk_to_nonzero(n)?),
        })),
    }
}

/// `chunk` の JSON 数値を SQL 字句解析とのパリティで `NonZeroUsize` へ変換する。
fn chunk_to_nonzero(n: &JsonNumber) -> Result<NonZeroUsize, PartitionedRequestError> {
    match n {
        JsonNumber::NegInt(_) => Err(PartitionedRequestError::NegativeChunk),
        JsonNumber::Float { .. } => Err(PartitionedRequestError::InvalidChunk),
        JsonNumber::PosInt(v) => usize::try_from(*v)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or(PartitionedRequestError::InvalidChunk),
    }
}

/// 修飾付きの要求は述語形だけを受理する（`where` との組合せは `42601`）。
pub fn check_target_form(
    modifier: Option<PartitionedModifier>,
    target: &TargetForm<'_>,
) -> Result<(), PartitionedRequestError> {
    match (modifier, target) {
        (Some(_), TargetForm::RowId(_)) => Err(PartitionedRequestError::WhereNotAllowed),
        _ => Ok(()),
    }
}

/// `update`／`delete` の engine エラーを応答へ写像する。`VD001`・`VD002` は 409 と
/// `data`（自テナントの commit 済み件数・クライアント自身の `operation_id`）付きの本文、
/// それ以外は通常のエラー応答。`validated` の `operation_id` は engine 到達時点で検証済み。
pub fn encode_engine_error(
    err: &SqlSurfaceError,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    let operation_id = validated
        .optional_str("operation_id")
        .ok()
        .flatten()
        .unwrap_or("");
    match err {
        SqlSurfaceError::PartialCompletion { committed, .. }
        | SqlSurfaceError::PartitionedDmlCancelled { committed } => {
            http_response::encode_error_partitioned(
                err.error_class(),
                &err.client_message(),
                *committed,
                operation_id,
                now_wall,
            )
        }
        _ => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

/// `show_partitioned_dml`／`cancel_partitioned_dml` の失敗。
#[derive(Debug, Clone)]
pub enum JobOpError {
    Shape(SchemaError),
    InvalidIdentifier,
    Engine(SqlSurfaceError),
}

impl From<SchemaError> for JobOpError {
    fn from(err: SchemaError) -> Self {
        JobOpError::Shape(err)
    }
}

impl From<InvalidIdentifier> for JobOpError {
    fn from(_err: InvalidIdentifier) -> Self {
        JobOpError::InvalidIdentifier
    }
}

impl From<SqlSurfaceError> for JobOpError {
    fn from(err: SqlSurfaceError) -> Self {
        JobOpError::Engine(err)
    }
}

impl ClassifiedError for JobOpError {
    fn error_class(&self) -> ErrorClass {
        match self {
            JobOpError::Shape(err) => err.error_class(),
            JobOpError::InvalidIdentifier => ErrorClass::UnsupportedSqlSyntax,
            JobOpError::Engine(err) => err.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            JobOpError::Shape(err) => err.client_message(),
            JobOpError::InvalidIdentifier => "invalid identifier".to_string(),
            JobOpError::Engine(err) => err.client_message(),
        }
    }
}

/// `table`・`operation_id` を取り出して検証する（`update` op と同じ規則。
/// `operation_id` の欠落・`null`・空文字は `23502`）。
fn parse_job_ref<'a>(validated: &Validated<'a>) -> Result<(&'a str, OperationId), JobOpError> {
    let table = validated.required_str("table")?;
    ident::check_identifier(table)?;
    let raw = validated.optional_str("operation_id")?.unwrap_or("");
    let operation_id = OperationId::parse(raw)?;
    Ok((table, operation_id))
}

/// `show_partitioned_dml` を実行する。
pub fn execute_show(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<QueryResult, JobOpError> {
    let (table, operation_id) = parse_job_ref(validated)?;
    Ok(core.show_partitioned_dml_in_session(principal.policy_context(), table, &operation_id)?)
}

/// `cancel_partitioned_dml` を実行する。
pub fn execute_cancel(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
) -> Result<QueryResult, JobOpError> {
    let (table, operation_id) = parse_job_ref(validated)?;
    Ok(core.cancel_partitioned_dml_in_session(principal.policy_context(), table, &operation_id)?)
}

/// 結果セットを応答バイト列へ写像する（show・cancel 共通）。
fn encode_result(result: Result<QueryResult, JobOpError>, now_wall: SystemTime) -> Vec<u8> {
    match result {
        Ok(rows) => match super::response::encode(&rows) {
            Ok(body) => http_response::encode_ok(&body, now_wall),
            Err(err) => {
                http_response::encode_error(err.error_class(), &err.client_message(), now_wall)
            }
        },
        Err(err) => http_response::encode_error(err.error_class(), &err.client_message(), now_wall),
    }
}

/// `show_partitioned_dml` op を処理して応答バイト列を返す（`gate.rs` から呼ばれる）。
pub fn handle_show(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    encode_result(execute_show(core, principal, validated), now_wall)
}

/// `cancel_partitioned_dml` op を処理して応答バイト列を返す（`gate.rs` から呼ばれる）。
pub fn handle_cancel(
    core: &EngineCore,
    principal: &SessionPrincipal,
    validated: &Validated<'_>,
    now_wall: SystemTime,
) -> Vec<u8> {
    encode_result(execute_cancel(core, principal, validated), now_wall)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::json::{parse_json, JsonValue};

    fn validated_update(extra: &str) -> Validated<'static> {
        let json = format!(
            r#"{{"op":"update","table":"docs","set":{{"lang":"en"}},"filter":[{{"op":"not_null","field":"lang"}}]{extra}}}"#
        );
        let value: &'static JsonValue = Box::leak(Box::new(parse_json(&json).expect("json")));
        super::super::schema::UPDATE_SCHEMA
            .validate(value)
            .expect("schema")
    }

    #[test]
    fn absent_modifier_is_atomic() {
        assert_eq!(parse_modifier(&validated_update("")).unwrap(), None);
    }

    #[test]
    fn mode_partitioned_with_and_without_chunk() {
        let v = validated_update(r#","mode":"partitioned""#);
        assert_eq!(
            parse_modifier(&v).unwrap(),
            Some(PartitionedModifier { chunk_rows: None })
        );
        let v = validated_update(r#","mode":"partitioned","chunk":5"#);
        assert_eq!(
            parse_modifier(&v).unwrap(),
            Some(PartitionedModifier {
                chunk_rows: NonZeroUsize::new(5)
            })
        );
    }

    #[test]
    fn invalid_modes_are_rejected_with_42601() {
        for mode in ["atomic", "PARTITIONED", "", " partitioned"] {
            let v = validated_update(&format!(r#","mode":"{mode}""#));
            let err = parse_modifier(&v).unwrap_err();
            assert_eq!(err.wire_code(), "42601", "mode={mode:?}");
        }
    }

    #[test]
    fn chunk_without_mode_is_42601() {
        let err = parse_modifier(&validated_update(r#","chunk":2"#)).unwrap_err();
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn chunk_values_follow_sql_lexer_parity() {
        for (chunk, code) in [
            ("0", "22000"),
            ("1.5", "22000"),
            ("1e2", "22000"),
            ("18446744073709551616", "22000"),
            ("-1", "42601"),
            ("-0", "42601"),
        ] {
            let v = validated_update(&format!(r#","mode":"partitioned","chunk":{chunk}"#));
            let err = parse_modifier(&v).unwrap_err();
            assert_eq!(err.wire_code(), code, "chunk={chunk}");
        }
    }

    #[test]
    fn modifier_with_row_id_target_is_rejected() {
        let m = Some(PartitionedModifier { chunk_rows: None });
        assert!(check_target_form(m, &TargetForm::RowId(1)).is_err());
        assert!(check_target_form(None, &TargetForm::RowId(1)).is_ok());
        assert!(check_target_form(m, &TargetForm::Predicate(&[])).is_ok());
    }
}
