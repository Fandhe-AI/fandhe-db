//! `json` 型（入力テキストを原文のまま保存する型。TABLE-25 ポインタ・Issue #2180）の
//! 部分テキスト取り出し。
//!
//! 役割: `json` 列の格納テキスト（`crate::json::validate_json_column_text` 検証済み。
//! 空白・キー順・重複キーを保持している）から、`->`／`->>`／`#>`／`#>>` と `json_*`
//! 関数が返すべき「原文のままの部分テキスト」を切り出す。`jsonb` 向けの
//! `crate::json::jsonb_access` は値を正規化して返すため、原文を返す本モジュールとは
//! 意味が異なる（呼び分けは `sql::udf_call` の束縛段が型で確定する）。
//!
//! 呼び出し元: `sql::udf_call` の式評価器・`sql::json_fn`。パーサは `crate::json` の
//! `JsonParser` を共有する（第 2 のパーサは作らない。#889 の方針）。重複キーは
//! [`DuplicateKeys::LastWins`] で読み、取り出しは最後に現れた値を採る。
//!
//! 契約: 入力は格納時に検証済みのテキストで、走査に失敗した場合は格納契約違反として
//! [`JsonError`] を返す（呼び出し元が固定文言の内部エラーへ写像する。fail-closed）。
//! 返す範囲は値トークンの先頭から末尾まで（外側の空白は除き、内側の空白は保持する）。
//! 受信データ経路のため添字アクセスは使わず `get()` を使う。

use crate::json::{DuplicateKeys, JsonError, JsonParser, MAX_JSON_CONTAINER_ITEMS};

/// 走査対象の 1 値（`text` の先頭の値トークン列）のコンテナ種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Array,
    Scalar,
}

fn kind_of(text: &str) -> Kind {
    match text
        .bytes()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    {
        Some(b'{') => Kind::Object,
        Some(b'[') => Kind::Array,
        _ => Kind::Scalar,
    }
}

/// 値の外側の空白を除いた範囲（`parse_value` が先頭の空白を読み飛ばすため、末尾は
/// `trim_end` 相当で落とす）。
fn trim_json_ws(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

/// オブジェクト `text` の全メンバー（重複を含む出現順）を `(復号済みキー, 値の原文)` で返す。
/// オブジェクトでなければ `Ok(None)`。
fn object_entries(text: &str) -> Result<Option<Vec<(String, &str)>>, JsonError> {
    if kind_of(text) != Kind::Object {
        return Ok(None);
    }
    let mut p = JsonParser::new(text).with_dup(DuplicateKeys::LastWins);
    p.skip_ws();
    p.expect_byte(b'{')?;
    let mut out: Vec<(String, &str)> = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b'}') {
        return Ok(Some(out));
    }
    loop {
        if out.len() >= MAX_JSON_CONTAINER_ITEMS {
            return Err(JsonError);
        }
        p.skip_ws();
        let key = p.parse_string()?;
        p.skip_ws();
        p.expect_byte(b':')?;
        p.skip_ws();
        let start = p.pos;
        p.parse_value(1)?;
        let span = text.get(start..p.pos).ok_or(JsonError)?;
        out.push((key, span));
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err(JsonError),
        }
    }
    Ok(Some(out))
}

/// 配列 `text` の全要素の原文を返す。配列でなければ `Ok(None)`。
fn array_items(text: &str) -> Result<Option<Vec<&str>>, JsonError> {
    if kind_of(text) != Kind::Array {
        return Ok(None);
    }
    let mut p = JsonParser::new(text).with_dup(DuplicateKeys::LastWins);
    p.skip_ws();
    p.expect_byte(b'[')?;
    let mut out: Vec<&str> = Vec::new();
    p.skip_ws();
    if p.peek() == Some(b']') {
        return Ok(Some(out));
    }
    loop {
        if out.len() >= MAX_JSON_CONTAINER_ITEMS {
            return Err(JsonError);
        }
        p.skip_ws();
        let start = p.pos;
        p.parse_value(1)?;
        out.push(text.get(start..p.pos).ok_or(JsonError)?);
        p.skip_ws();
        match p.bump() {
            Some(b',') => continue,
            Some(b']') => break,
            _ => return Err(JsonError),
        }
    }
    Ok(Some(out))
}

/// 配列長 `len` と添字 `idx`（負値は末尾から数える）から位置を求める（範囲外は `None`）。
fn resolve_index(len: usize, idx: i64) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let pos = if idx < 0 { len.checked_add(idx)? } else { idx };
    if pos < 0 || pos >= len {
        return None;
    }
    usize::try_from(pos).ok()
}

/// `->`（キー）: オブジェクトの `key` の値の原文。キーが重複する場合は最後の値。
/// オブジェクト以外・キー不在は `Ok(None)`（SQL の NULL）。
pub(crate) fn json_text_get_field<'a>(
    text: &'a str,
    key: &str,
) -> Result<Option<&'a str>, JsonError> {
    Ok(object_entries(text)?.and_then(|entries| {
        entries
            .into_iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| trim_json_ws(v))
    }))
}

/// `->`（添字）: 配列の `idx` 番目（負値は末尾から）の原文。配列以外・範囲外は `Ok(None)`。
pub(crate) fn json_text_get_index(text: &str, idx: i32) -> Result<Option<&str>, JsonError> {
    Ok(array_items(text)?.and_then(|items| {
        resolve_index(items.len(), i64::from(idx))
            .and_then(|p| items.get(p).copied())
            .map(trim_json_ws)
    }))
}

/// `#>`／`json_extract_path`: `path` を辿った先の原文。各要素はオブジェクトならキー、
/// 配列なら整数（解釈できなければ NULL）。要素に NULL があれば結果は NULL。空パスは値全体。
/// 途中段に重複キーがある場合は、辿り切れた出現のうち最後のものを採る
/// （PostgreSQL 17 の `json` と同じ結果。実測は層 A のテストで固定）。
pub(crate) fn json_text_get_path<'a>(
    text: &'a str,
    path: &[Option<String>],
) -> Result<Option<&'a str>, JsonError> {
    if path.iter().any(Option::is_none) {
        return Ok(None);
    }
    walk(trim_json_ws(text), path)
}

fn walk<'a>(span: &'a str, path: &[Option<String>]) -> Result<Option<&'a str>, JsonError> {
    let Some((head, rest)) = path.split_first() else {
        return Ok(Some(span));
    };
    let Some(elem) = head.as_deref() else {
        return Ok(None);
    };
    match kind_of(span) {
        Kind::Object => {
            let mut best = None;
            for (k, v) in object_entries(span)?.unwrap_or_default() {
                if k == elem {
                    if let Some(found) = walk(trim_json_ws(v), rest)? {
                        best = Some(found);
                    }
                }
            }
            Ok(best)
        }
        Kind::Array => {
            let Ok(i) = elem.parse::<i32>() else {
                return Ok(None);
            };
            let items = array_items(span)?.unwrap_or_default();
            match resolve_index(items.len(), i64::from(i)).and_then(|p| items.get(p).copied()) {
                Some(item) => walk(trim_json_ws(item), rest),
                None => Ok(None),
            }
        }
        Kind::Scalar => Ok(None),
    }
}

/// `->>`／`#>>`: 値の原文（[`json_text_get_field`] 等の戻り値）を text へ変換する。
/// JSON 文字列はエスケープを復号した生の文字列、JSON の `null` は SQL の NULL、
/// それ以外は原文のまま（`1.0`・`1e2`・`{"a" : 1}` の表記を保つ）。
pub(crate) fn json_text_as_text(span: &str) -> Result<Option<String>, JsonError> {
    let span = trim_json_ws(span);
    match span.as_bytes().first() {
        Some(b'"') => {
            let mut p = JsonParser::new(span);
            Ok(Some(p.parse_string()?))
        }
        Some(b'n') => Ok(None),
        Some(_) => Ok(Some(span.to_string())),
        None => Err(JsonError),
    }
}

/// `json_typeof` のラベル（`object`／`array`／`string`／`number`／`boolean`／`null`）。
pub(crate) fn json_text_typeof(text: &str) -> Result<&'static str, JsonError> {
    let first = text
        .bytes()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'));
    match first {
        Some(b'{') => Ok("object"),
        Some(b'[') => Ok("array"),
        Some(b'"') => Ok("string"),
        Some(b't') | Some(b'f') => Ok("boolean"),
        Some(b'n') => Ok("null"),
        Some(b'-') | Some(b'0'..=b'9') => Ok("number"),
        _ => Err(JsonError),
    }
}

/// `json_array_length` の要素数。配列でなければ `Ok(None)`（呼び出し元が `22023`）。
pub(crate) fn json_text_array_length(text: &str) -> Result<Option<usize>, JsonError> {
    Ok(array_items(text)?.map(|items| items.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn get_field_returns_last_value_and_keeps_inner_whitespace() {
        let doc = r#" {"a":1, "a" : [ 1 , 2 ] , "b" : "x" } "#;
        assert_eq!(json_text_get_field(doc, "a").unwrap(), Some("[ 1 , 2 ]"));
        assert_eq!(json_text_get_field(doc, "b").unwrap(), Some("\"x\""));
        assert_eq!(json_text_get_field(doc, "zz").unwrap(), None);
    }

    #[test]
    fn get_field_matches_decoded_key() {
        assert_eq!(json_text_get_field(r#"{"a":7}"#, "a").unwrap(), Some("7"));
    }

    #[test]
    fn get_field_on_non_object_is_none() {
        assert_eq!(json_text_get_field("[1]", "a").unwrap(), None);
        assert_eq!(json_text_get_field("1", "a").unwrap(), None);
    }

    #[test]
    fn get_index_supports_negative_and_out_of_range() {
        let doc = "[1, 2 ,3]";
        assert_eq!(json_text_get_index(doc, 0).unwrap(), Some("1"));
        assert_eq!(json_text_get_index(doc, -1).unwrap(), Some("3"));
        assert_eq!(json_text_get_index(doc, 3).unwrap(), None);
        assert_eq!(json_text_get_index(doc, -4).unwrap(), None);
        assert_eq!(json_text_get_index(r#"{"a":1}"#, 0).unwrap(), None);
    }

    #[test]
    fn get_path_walks_objects_and_arrays() {
        let doc = r#"{"a":{"b":[10, 20 ,{"c":  true}]}}"#;
        let path = [s("a"), s("b"), s("2"), s("c")];
        assert_eq!(json_text_get_path(doc, &path).unwrap(), Some("true"));
        let path = [s("a"), s("b"), s("-1")];
        assert_eq!(
            json_text_get_path(doc, &path).unwrap(),
            Some(r#"{"c":  true}"#)
        );
        assert_eq!(json_text_get_path(doc, &[s("a"), s("x")]).unwrap(), None);
        assert_eq!(json_text_get_path(doc, &[s("a"), None]).unwrap(), None);
        assert_eq!(json_text_get_path(doc, &[]).unwrap(), Some(doc));
    }

    #[test]
    fn get_path_with_duplicate_keys_takes_last_resolvable_occurrence() {
        let doc = r#"{"a":{"b":1},"a":{"c":2}}"#;
        assert_eq!(
            json_text_get_path(doc, &[s("a"), s("b")]).unwrap(),
            Some("1")
        );
        assert_eq!(
            json_text_get_path(doc, &[s("a"), s("c")]).unwrap(),
            Some("2")
        );
        let doc = r#"{"a":{"b":1},"a":{"b":2}}"#;
        assert_eq!(
            json_text_get_path(doc, &[s("a"), s("b")]).unwrap(),
            Some("2")
        );
    }

    #[test]
    fn as_text_decodes_strings_and_keeps_number_literals() {
        assert_eq!(
            json_text_as_text(r#""xAy""#).unwrap(),
            Some("xAy".to_string())
        );
        assert_eq!(
            json_text_as_text("1.0e2").unwrap(),
            Some("1.0e2".to_string())
        );
        assert_eq!(json_text_as_text("null").unwrap(), None);
        assert_eq!(
            json_text_as_text(r#"{"x" : 1.0}"#).unwrap(),
            Some(r#"{"x" : 1.0}"#.to_string())
        );
    }

    #[test]
    fn typeof_labels() {
        assert_eq!(json_text_typeof(r#" {"a":1}"#).unwrap(), "object");
        assert_eq!(json_text_typeof("[1]").unwrap(), "array");
        assert_eq!(json_text_typeof("\"x\"").unwrap(), "string");
        assert_eq!(json_text_typeof("-1").unwrap(), "number");
        assert_eq!(json_text_typeof("true").unwrap(), "boolean");
        assert_eq!(json_text_typeof("false").unwrap(), "boolean");
        assert_eq!(json_text_typeof("null").unwrap(), "null");
    }

    #[test]
    fn array_length_counts_top_level_items_only() {
        assert_eq!(json_text_array_length("[1,[2,3],null]").unwrap(), Some(3));
        assert_eq!(json_text_array_length("[]").unwrap(), Some(0));
        assert_eq!(json_text_array_length("{}").unwrap(), None);
        assert_eq!(json_text_array_length("1").unwrap(), None);
    }

    #[test]
    fn malformed_stored_text_is_an_error_not_a_panic() {
        assert!(json_text_get_field("{\"a\":", "a").is_err());
        assert!(json_text_get_index("[1,", 0).is_err());
    }
}
