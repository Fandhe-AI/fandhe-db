//! `POST /v1/query` の `table`／`aggregates[].column`（他 op でも共通に使う
//! 識別子）の**形状**検証（Issue #768。TASK-175・NOSQL-4。ポインタ:
//! `docs/spec/05-tasks.md` TASK-175・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-4）。
//!
//! SQL 表層の字句解析（`engine::sql::lexer::tokenize`）は識別子（`Token::Ident`）
//! を「先頭が ASCII 英字または `_`、以降が ASCII 英数字または `_`」の語として
//! 読む（それ以外の文字は字句解析自体が拒否する形状）。本モジュールは
//! NoSQL 表層の JSON 文字列に対して同じ形状検査を行い、SQL 表層なら字句解析
//! 段階で `42601` になる入力を NoSQL 表層でも同じ分類（`42601`）に落とす。
//!
//! テーブル・列の実在確認（未知列 `22000`・テーブル不存在 `42P01`）は本
//! モジュールの対象外——それらは engine 側（`resolve_aggregate_input`・
//! `catalog::TableLookup`）が既存の SQL 経路と同一の判定を行う。ここでの
//! 検査は「識別子として意味を持ちうる文字列か」だけを見る、形状の
//! 事前フィルタである。
//!
//! `MAX_IDENTIFIER_LEN` は `engine::catalog` の同名の実装上限（`pub(crate)`
//! のためクレート外から参照できない）と同じ値を採用する（この長さを超える
//! 文字列は engine 側の `validate_identifier` を経由する DDL 上のテーブル・
//! 列としては構造的に存在し得ないため、`Vec` 確保・schema 走査より前に
//! `42601` で弾いてよい。JSON 文字列自体は `engine::json::MAX_JSON_STRING_CHARS`
//! 〔1 MiB〕まで受理されうるため、この上限は untrusted な長大文字列を
//! schema 突き合わせより前に打ち切る DoS 対策も兼ねる）。

/// `engine::catalog` の識別子長上限（`pub(crate)` のため値だけをここへ複製
/// する。値がずれた場合の実害は「本来 engine 側で `22000`／`42P01` になる
/// はずの入力が、NoSQL 表層でだけ `42601` になる」程度に留まり、テナント
/// 境界・fail-closed 契約には影響しない）。
const MAX_IDENTIFIER_LEN: usize = 63;

/// [`check_identifier`] の失敗を表す。untrusted な識別子文字列は一切保持
/// しない（`super::op::UnsupportedOp`・`super::schema::SchemaError::
/// UnknownKey` と同じ判断。JSON 文字列はエスケープ経由で制御文字を含み
/// 得るため、untrusted 値をエラー文言へ埋め込むと `error_response::encode`
/// の制御文字拒否で `XX000` へ縮退しうる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidIdentifier;

impl engine::error_format::ClassifiedError for InvalidIdentifier {
    fn error_class(&self) -> engine::error_format::ErrorClass {
        engine::error_format::ErrorClass::UnsupportedSqlSyntax
    }

    fn client_message(&self) -> String {
        "invalid identifier".to_string()
    }
}

/// `raw` が識別子として意味を持ちうる形状（非空・[`MAX_IDENTIFIER_LEN`] 以下・
/// 先頭 ASCII 英字または `_`・以降 ASCII 英数字または `_`）かを検証する。
/// `engine::sql::lexer` が `Token::Ident` として読む語と同じ文字集合。
pub fn check_identifier(raw: &str) -> Result<(), InvalidIdentifier> {
    if raw.is_empty() || raw.len() > MAX_IDENTIFIER_LEN {
        return Err(InvalidIdentifier);
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return Err(InvalidIdentifier);
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(InvalidIdentifier);
    }
    if chars.any(|c| !(c.is_ascii_alphanumeric() || c == '_')) {
        return Err(InvalidIdentifier);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::error_format::ClassifiedError;

    #[test]
    fn accepts_valid_forms() {
        for raw in ["a", "_foo", "foo_bar123", "A1", "docs", "embedding"] {
            assert!(check_identifier(raw).is_ok(), "raw={raw:?}");
        }
    }

    /// TABLE-27・Issue #1717: NoSQL 表層の `table` は裸の識別子だけを受理する。SCHEMA 配下の
    /// relation の内部キー（`#<id>.<name>`）を JSON から直接指定できない（テナント境界。ADR D2）。
    #[test]
    fn rejects_schema_internal_relation_keys() {
        for raw in ["#1.t", "#12.docs", "#1", "1.t", "s.t", "\"#1.t\""] {
            assert!(check_identifier(raw).is_err(), "raw={raw:?}");
        }
    }

    #[test]
    fn accepts_at_length_limit() {
        let raw = "a".repeat(MAX_IDENTIFIER_LEN);
        assert!(check_identifier(&raw).is_ok());
    }

    #[test]
    fn rejects_over_length_limit() {
        let raw = "a".repeat(MAX_IDENTIFIER_LEN + 1);
        let err = check_identifier(&raw).expect_err("must reject");
        assert_eq!(err.wire_code(), "42601");
    }

    #[test]
    fn rejects_invalid_forms() {
        let control_byte = char::from_u32(0).expect("valid char");
        let with_nul = format!("a{control_byte}b");
        let newline_byte = char::from_u32(10).expect("valid char");
        let with_newline = format!("a{newline_byte}b");
        let cases: Vec<String> = vec![
            "".to_string(),
            "1abc".to_string(),
            "-abc".to_string(),
            "a b".to_string(),
            "a:b".to_string(),
            with_newline,
            "h\u{e9}llo".to_string(),
            "do cs".to_string(),
            "a.b".to_string(),
            "a;b".to_string(),
            with_nul,
        ];
        for raw in cases {
            let err = check_identifier(&raw).expect_err("must reject");
            assert_eq!(err.wire_code(), "42601", "raw={raw:?}");
        }
    }

    #[test]
    fn does_not_leak_untrusted_string_in_debug_or_message() {
        let err = check_identifier("").expect_err("must reject");
        assert_eq!(err.client_message(), "invalid identifier");
        assert_eq!(format!("{err:?}"), "InvalidIdentifier");
    }
}
