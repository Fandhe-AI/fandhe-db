//! 書き込み op（`insert`／`update`／`delete`）の `operation_id` 供給元の解決
//! （Issue #2064。親 #2062・TASK-253・ポインタ: `docs/spec/04-behavior/http-transport.md`
//! HTTP-14 (1)(3)(4)）。
//!
//! 呼び出し文脈: `gate::handle` が `Idempotency-Key` ヘッダを検証済みの
//! [`OperationId`] として取り出し（`crate::http::idempotency_key`）、各 op の
//! `execute` が本文の `operation_id` と合わせて本関数へ渡す。3 op に同一の取り出し
//! ロジックが重複していたため、ここへ単一情報源として集約する。
//!
//! ## 規則
//!
//! - 本文の `operation_id` の欠落・`null`・空文字は「省略」（従来どおり。ヘッダの
//!   空値が `42601` になるのとは意図的に非対称）。値があれば `OperationId::parse`
//!   （エラーは従来どおり `23502`／`22000`）。
//! - 本文のみ → 本文値、ヘッダのみ → ヘッダ値、どちらも無し → `None`
//!   （要否判定は engine の `LedgerMode` が担う）。
//! - 両方ある場合はバイト単位の完全一致（trim・大小文字の正規化なし）を必須とし、
//!   不一致は [`OperationIdSourceError::Mismatch`]（`42601`・固定文言）。
//! - 判定順: 本文のスキーマ／parse エラーが不一致判定より先に出る。
//!
//! エラー文言にはヘッダ値・本文値を含めない。受信データ経路のため
//! `unwrap`／`expect`／添字アクセスを用いない。

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::recovery::required_op_id::OperationId;
use engine::sql::allowlist::SqlSurfaceError;

use super::schema::{SchemaError, Validated};

/// 不一致時にクライアントへ返す固定文言（どちらの値も含めない）。
pub const MISMATCH_MESSAGE: &str = "idempotency-key header does not match operation_id";

/// [`resolve_operation_id`] の失敗。
#[derive(Debug, Clone)]
pub enum OperationIdSourceError {
    /// `Validated` アクセサの型・キー不整合（多層防御）。
    Shape(SchemaError),
    /// 本文の `operation_id` が `OperationId::parse` に拒否された（`23502`／`22000`）。
    Body(SqlSurfaceError),
    /// 本文とヘッダの値が一致しない（`42601`）。
    Mismatch,
}

impl OperationIdSourceError {
    /// `Body`／`Mismatch` を engine 側エラー型へ写す（`Shape` は呼び出し側の `Shape` で扱う）。
    pub fn into_sql_surface_error(self) -> SqlSurfaceError {
        match self {
            Self::Body(err) => err,
            Self::Mismatch => SqlSurfaceError::UnsupportedSyntax {
                detail: MISMATCH_MESSAGE.to_string(),
            },
            Self::Shape(_) => SqlSurfaceError::Internal {
                detail: "unexpected shape error during operation_id resolution".to_string(),
            },
        }
    }
}

impl ClassifiedError for OperationIdSourceError {
    fn error_class(&self) -> ErrorClass {
        match self {
            Self::Shape(err) => err.error_class(),
            Self::Body(err) => err.error_class(),
            Self::Mismatch => ErrorClass::UnsupportedSqlSyntax,
        }
    }

    fn client_message(&self) -> String {
        match self {
            Self::Shape(err) => err.client_message(),
            Self::Body(err) => err.client_message(),
            Self::Mismatch => self.clone().into_sql_surface_error().client_message(),
        }
    }
}

/// 本文の `operation_id` とヘッダ由来の `header` から、実際に使う `operation_id` を決める。
pub fn resolve_operation_id(
    validated: &Validated<'_>,
    header: Option<&OperationId>,
) -> Result<Option<OperationId>, OperationIdSourceError> {
    let body = match validated
        .optional_str("operation_id")
        .map_err(OperationIdSourceError::Shape)?
        .filter(|raw| !raw.is_empty())
    {
        None => None,
        Some(raw) => Some(OperationId::parse(raw).map_err(OperationIdSourceError::Body)?),
    };
    match (body, header) {
        (None, None) => Ok(None),
        (Some(b), None) => Ok(Some(b)),
        (None, Some(h)) => Ok(Some(h.clone())),
        (Some(b), Some(h)) if b.as_str() == h.as_str() => Ok(Some(b)),
        (Some(_), Some(_)) => Err(OperationIdSourceError::Mismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::query::op::Op;
    use engine::json::parse_json;

    fn resolve(body: &str, header: Option<&str>) -> Result<Option<String>, OperationIdSourceError> {
        let value = parse_json(body).expect("valid json");
        let validated = Op::Insert.schema().validate(&value).expect("schema ok");
        let h = header.map(|h| OperationId::parse(h).expect("valid header id"));
        resolve_operation_id(&validated, h.as_ref()).map(|o| o.map(|id| id.as_str().to_string()))
    }

    fn body_with(extra: &str) -> String {
        let base = r#""op":"insert","table":"t","rows":[]"#;
        if extra.is_empty() {
            format!("{{{base}}}")
        } else {
            format!("{{{base},{extra}}}")
        }
    }

    #[test]
    fn neither_is_none() {
        assert_eq!(resolve(&body_with(""), None).expect("ok"), None);
    }

    #[test]
    fn body_only() {
        let b = body_with(r#""operation_id":"b1""#);
        assert_eq!(resolve(&b, None).expect("ok").as_deref(), Some("b1"));
    }

    #[test]
    fn header_only() {
        assert_eq!(
            resolve(&body_with(""), Some("h1")).expect("ok").as_deref(),
            Some("h1")
        );
    }

    #[test]
    fn empty_or_null_body_with_header_uses_header() {
        for extra in [r#""operation_id":"""#, r#""operation_id":null"#] {
            let b = body_with(extra);
            assert_eq!(resolve(&b, Some("h1")).expect("ok").as_deref(), Some("h1"));
        }
    }

    #[test]
    fn both_equal_is_accepted() {
        let b = body_with(r#""operation_id":"same""#);
        assert_eq!(
            resolve(&b, Some("same")).expect("ok").as_deref(),
            Some("same")
        );
    }

    #[test]
    fn both_different_is_mismatch_42601_without_values() {
        let b = body_with(r#""operation_id":"body-secret""#);
        let err = resolve(&b, Some("header-secret")).expect_err("mismatch");
        assert!(matches!(err, OperationIdSourceError::Mismatch));
        assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        assert_eq!(err.error_class().wire_code(), "42601");
        let msg = err.client_message();
        assert!(msg.contains(MISMATCH_MESSAGE), "{msg}");
        assert!(!msg.contains("secret"));
        let sql = err.into_sql_surface_error();
        assert_eq!(sql.error_class(), ErrorClass::UnsupportedSqlSyntax);
        assert_eq!(sql.client_message(), msg);
    }

    #[test]
    fn comparison_is_exact_bytes() {
        let b = body_with(r#""operation_id":"Op""#);
        assert!(matches!(
            resolve(&b, Some("op")),
            Err(OperationIdSourceError::Mismatch)
        ));
    }

    #[test]
    fn invalid_body_error_precedes_mismatch() {
        let b = body_with(r#""operation_id":"a\u0001b""#);
        let err = resolve(&b, Some("other")).expect_err("body error");
        assert!(matches!(err, OperationIdSourceError::Body(_)));
    }
}
