//! `Idempotency-Key` ヘッダの取り出しと検証（Issue #2063。親 #2062・TASK-253・
//! ポインタ: `docs/spec/04-behavior/http-transport.md` HTTP-14 (2)）。
//!
//! 呼び出し文脈: `POST /v1/query` の書き込み op から、本文の `operation_id` と
//! 並ぶ `operation_id` の供給元として `gate::handle`（手順 5）から呼ばれる
//! （結線と本文との不一致拒否は Issue #2064。SQL／HTTP 間の相互再送は Issue #2065）。本モジュールは取り出しと
//! 検証だけを担い、第 2 の検証器を作らず `engine` の `OperationId::parse`
//! （SQL の `USING OPERATION_ID`・本文の `operation_id` と同一）へ委譲する。
//!
//! ## エラー契約
//!
//! `OperationId::parse` の判定条件（空・256 バイト超・制御文字）だけを再利用し、
//! エラーコードは再利用しない。`Err` は種別によらず一律 `42601`
//! （[`ErrorClass::UnsupportedSqlSyntax`]）、重複ヘッダは `08P01`。SQL 表層向けの
//! エラー文言や入力値はクライアントへ返さない。本文の空 `operation_id` は
//! 「省略」扱いだが、ヘッダの空値は `42601` で拒否する（意図した非対称）。
//!
//! ## 08P01 との層の分担
//!
//! ヘッダ値は構文解析（`crate::http::headers`）の段階で可視 ASCII／HTAB 以外が
//! `08P01` で拒否済みのため、本層まで届く制御文字は値内側の HTAB だけである
//! （前後の HTAB／SP は OWS として除去される）。それ以外の制御バイトは従来どおり
//! `08P01` であり、構文解析の検証は緩めない。
//!
//! 受信データ経路のため `unwrap`／`expect`／添字アクセスを用いない。

use crate::http::headers::Headers;
use engine::error_format::ErrorClass;
use engine::recovery::required_op_id::OperationId;

/// [`extract_idempotency_key`] の拒否理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyKeyError {
    /// `Idempotency-Key` ヘッダが 2 件以上あった（`08P01`）。
    Duplicate,
    /// 値が UTF-8 に復号できない（構文解析後は到達しないが fail-closed で `42601`）。
    Encoding,
    /// `OperationId::parse` が拒否した（空・長さ超過・制御文字。`42601`）。
    Invalid,
}

impl IdempotencyKeyError {
    /// この失敗の分類。`Duplicate` は `ProtocolViolation`、他は `UnsupportedSqlSyntax`。
    pub const fn error_class(&self) -> ErrorClass {
        match self {
            Self::Duplicate => ErrorClass::ProtocolViolation,
            Self::Encoding | Self::Invalid => ErrorClass::UnsupportedSqlSyntax,
        }
    }

    /// `error_class()` に対応する SQLSTATE 風 `wire_code`。
    pub fn sqlstate(&self) -> &'static str {
        self.error_class().wire_code()
    }

    /// クライアントへ返す固定文言（ヘッダ値・内部エラー文言を含めない）。
    pub const fn client_message(&self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate idempotency-key header",
            Self::Encoding | Self::Invalid => "invalid idempotency-key header",
        }
    }
}

/// `headers` から `Idempotency-Key` を取り出し [`OperationId`] へ検証変換する。
///
/// ヘッダなしは `Ok(None)`。手順（fail-closed）: 取り出し（重複は `Duplicate`）
/// → UTF-8 復号（`Encoding`）→ `OperationId::parse`（`Invalid`）。
pub fn extract_idempotency_key(
    headers: &Headers<'_>,
) -> Result<Option<OperationId>, IdempotencyKeyError> {
    let Some(raw) = headers
        .idempotency_key()
        .map_err(|_| IdempotencyKeyError::Duplicate)?
    else {
        return Ok(None);
    };
    let text = std::str::from_utf8(raw).map_err(|_| IdempotencyKeyError::Encoding)?;
    OperationId::parse(text)
        .map(Some)
        .map_err(|_| IdempotencyKeyError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};
    use engine::sql::using_operation_id::MAX_OPERATION_ID_LEN;

    // テスト専用: 借用元を延命する意図的なリーク（本番コードには存在しない）。
    fn extract(raw: &[u8]) -> Result<Option<OperationId>, IdempotencyKeyError> {
        let mut input = raw.to_vec();
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let leaked: &'static [u8] = Box::leak(input.into_boxed_slice());
        match parse_headers(leaked).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => extract_idempotency_key(&headers),
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn with_value(value: &[u8]) -> Vec<u8> {
        let mut raw = b"Idempotency-Key: ".to_vec();
        raw.extend_from_slice(value);
        raw.extend_from_slice(b"\r\n");
        raw
    }

    fn assert_invalid(r: Result<Option<OperationId>, IdempotencyKeyError>) {
        let err = r.unwrap_err();
        assert_eq!(err, IdempotencyKeyError::Invalid);
        assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        assert_eq!(err.sqlstate(), "42601");
    }

    #[test]
    fn limit_constant_is_256() {
        assert_eq!(MAX_OPERATION_ID_LEN, 256);
    }

    #[test]
    fn absent_header_is_none() {
        assert!(extract(b"").expect("ok").is_none());
    }

    #[test]
    fn empty_value_is_invalid() {
        assert_invalid(extract(b"Idempotency-Key:\r\n"));
    }

    #[test]
    fn space_only_value_is_invalid() {
        assert_invalid(extract(b"Idempotency-Key:    \r\n"));
    }

    #[test]
    fn one_byte_is_accepted() {
        let id = extract(&with_value(b"a")).expect("ok").expect("some");
        assert_eq!(id.as_str(), "a");
    }

    #[test]
    fn max_len_is_accepted_and_over_is_invalid() {
        // ヘッダ部の上限 8 KiB に収まる長さであること。
        let ok = vec![b'x'; MAX_OPERATION_ID_LEN];
        let id = extract(&with_value(&ok)).expect("ok").expect("some");
        assert_eq!(id.as_str().len(), MAX_OPERATION_ID_LEN);
        let over = vec![b'x'; MAX_OPERATION_ID_LEN + 1];
        assert_invalid(extract(&with_value(&over)));
    }

    #[test]
    fn inner_htab_is_invalid() {
        assert_invalid(extract(&with_value(b"a\tb")));
    }

    #[test]
    fn surrounding_ows_is_trimmed() {
        let id = extract(b"Idempotency-Key:  op-1\t\r\n")
            .expect("ok")
            .expect("some");
        assert_eq!(id.as_str(), "op-1");
    }

    #[test]
    fn duplicate_is_protocol_violation() {
        let err = extract(b"Idempotency-Key: a\r\nIdempotency-Key: a\r\n").unwrap_err();
        assert_eq!(err, IdempotencyKeyError::Duplicate);
        assert_eq!(err.error_class(), ErrorClass::ProtocolViolation);
        assert_eq!(err.sqlstate(), "08P01");
    }

    #[test]
    fn other_control_bytes_fail_at_header_parse_with_08p01() {
        for bad in [&b"a\x01b"[..], &b"a\x7fb"[..]] {
            let mut input = with_value(bad);
            input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
            let err = parse_headers(&input).unwrap_err();
            assert_eq!(err.sqlstate(), Some("08P01"));
        }
    }

    #[test]
    fn client_message_is_fixed_and_hides_value() {
        let err = extract(&with_value(b"secret\top")).unwrap_err();
        assert!(!err.client_message().contains("secret"));
        assert_eq!(err.client_message(), "invalid idempotency-key header");
        assert_eq!(
            IdempotencyKeyError::Duplicate.client_message(),
            "duplicate idempotency-key header"
        );
    }

    #[test]
    fn encoding_maps_to_42601() {
        let err = IdempotencyKeyError::Encoding;
        assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        assert_eq!(err.sqlstate(), "42601");
    }
}
