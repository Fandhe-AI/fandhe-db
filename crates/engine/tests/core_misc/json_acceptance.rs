//! 共有 JSON パーサ（`engine::json`）の受理規則の境界テスト（Issue #733。
//! 対象ビヘイビア・タスクポインタ: TASK-172・NOSQL-8。`docs/spec/05-tasks.md`
//! TASK-172）。
//!
//! `engine::json` の単体テスト（`crates/engine/src/json.rs` 内 `mod tests`）は
//! 深さ超過・末尾ゴミ・重複キー・RFC 8259 数値の代表例のみを固定しているのに
//! 対し、本ファイルは **上限ちょうど／超過を分ける境界**（深さ・コンテナ要素数・
//! 文字列長・数値テキスト長・生制御文字・トークン間空白・重複キー・末尾/先頭
//! ゴミ）を公開 API（[`engine::json::parse_json`]・[`engine::json::JsonError`]・
//! [`engine::error_format::ClassifiedError`]）のみで固定する結合テストである。
//!
//! 深さの定義は「コンテナ（配列・オブジェクト）のネスト数」であり、空コンテナ
//! （`[]`・`{}`）を最内に置いた入力でもスカラーを最内に置いた入力と同じ境界
//! （[`engine::json::MAX_JSON_DEPTH`] 個まで）で揃うことを固定する（Issue #733
//! の事前調査で見つかった off-by-one を `json.rs` 側で修正済み）。
//!
//! UTF-8 不正バイト列について: `parse_json` は `&str` を引数に取るため、生の
//! 不正バイト列は型システムにより構造的にパーサへ到達できない。本ファイルは
//! (a) production 経路が実際に検査する「`\u` エスケープ経由の不正スカラー
//! （孤立サロゲート等）」の拒否と、(b) バイト列→`&str` 変換の失敗を同じ
//! `JsonError`（`42601`）へ束ねる契約形を、テストローカルなゲート関数
//! （`std::str::from_utf8` の失敗を `JsonError` へ変換するだけの薄いラッパ）で
//! 固定する。(b) は production の実施点そのものではない
//! （TASK-173／175 の NoSQL 転送路実装でバイト入口 API が追加された際に、
//! そちらが同じ契約を担う想定。詳細は PR 本文の「対象外」節を参照）。

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::{
    parse_json, JsonError, JsonValue, MAX_JSON_CONTAINER_ITEMS, MAX_JSON_DEPTH,
    MAX_JSON_STRING_CHARS,
};

/// `parse_json` が `Err(JsonError)` を返し、かつ ERR-2（`ClassifiedError`）の
/// 契約どおり `wire_code`＝`42601`・`client_message`＝`"invalid JSON"`（バイト
/// 位置等の内部詳細を含まない）であることを一括で確認する。
fn assert_rejected(name: &str, input: &str) {
    match parse_json(input) {
        Ok(_) => panic!("{name}: expected rejection, but input was accepted: {input:?}"),
        Err(err) => {
            assert_eq!(
                err, JsonError,
                "{name}: JsonError は常に単一の不透明なマーカー型"
            );
            assert_eq!(
                err.wire_code(),
                "42601",
                "{name}: wire_code は常に UnsupportedSqlSyntax(42601)"
            );
            assert_eq!(
                err.error_class(),
                ErrorClass::UnsupportedSqlSyntax,
                "{name}: error_class は常に UnsupportedSqlSyntax"
            );
            assert_eq!(
                err.client_message(),
                "invalid JSON",
                "{name}: client_message はバイト位置等を含まない固定文言"
            );
        }
    }
}

/// `parse_json` が受理し `Ok` を返すことだけを確認する（値の中身は呼び出し元が
/// 必要な範囲で個別に検証する）。
fn assert_accepted(name: &str, input: &str) -> JsonValue {
    parse_json(input).unwrap_or_else(|_| panic!("{name}: expected acceptance for {input:?}"))
}

/// UTF-8 バイト列→`&str` 変換の失敗を `parse_json` と同じ `JsonError`
/// （`42601`）へ束ねる、テストローカルの薄いゲート（`utf8_rejection` テスト専用。
/// production の実施点ではない。§冒頭コメント参照）。
fn parse_json_bytes_via_utf8_gate(bytes: &[u8]) -> Result<JsonValue, JsonError> {
    let s = std::str::from_utf8(bytes).map_err(|_| JsonError)?;
    parse_json(s)
}

fn nested_arrays(n: usize, inner: &str) -> String {
    let mut s = String::new();
    for _ in 0..n {
        s.push('[');
    }
    s.push_str(inner);
    for _ in 0..n {
        s.push(']');
    }
    s
}

fn nested_objects(n: usize, inner: &str) -> String {
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(r#"{"k":"#);
    }
    s.push_str(inner);
    for _ in 0..n {
        s.push('}');
    }
    s
}

/// 相異なる値を持つ配列（`[0,1,2,...,n-1]`）を生成する。
fn array_with_items(n: usize) -> String {
    let items: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!("[{}]", items.join(","))
}

/// 相異なるキー（`"k0","k1",...`）を持つオブジェクトを生成する。
fn object_with_distinct_keys(n: usize) -> String {
    let items: Vec<String> = (0..n).map(|i| format!(r#""k{i}":{i}"#)).collect();
    format!("{{{}}}", items.join(","))
}

/// `ch` を `n` 回繰り返した文字列を JSON 文字列リテラルとして生成する
/// （`ch` は `"`・`\` を含まない前提）。
fn string_of_chars(n: usize, ch: char) -> String {
    let mut s = String::with_capacity(n + 2);
    s.push('"');
    for _ in 0..n {
        s.push(ch);
    }
    s.push('"');
    s
}

#[test]
fn depth_boundary() {
    assert_eq!(
        MAX_JSON_DEPTH, 16,
        "本テストは MAX_JSON_DEPTH=16 を前提に境界値を生成する"
    );

    // 配列・スカラー最内。
    assert_accepted("array/scalar at limit", &nested_arrays(MAX_JSON_DEPTH, "1"));
    assert_rejected(
        "array/scalar over limit",
        &nested_arrays(MAX_JSON_DEPTH + 1, "1"),
    );

    // 配列・空配列最内（off-by-one の直接的な回帰テスト）。
    assert_accepted("array/empty at limit", &nested_arrays(MAX_JSON_DEPTH, ""));
    assert_rejected(
        "array/empty over limit",
        &nested_arrays(MAX_JSON_DEPTH + 1, ""),
    );

    // オブジェクト・スカラー最内。
    assert_accepted(
        "object/scalar at limit",
        &nested_objects(MAX_JSON_DEPTH, "1"),
    );
    assert_rejected(
        "object/scalar over limit",
        &nested_objects(MAX_JSON_DEPTH + 1, "1"),
    );

    // オブジェクト・空オブジェクト最内。
    // `nested_objects(m, "{}")` は `m` 個のラップ用オブジェクト＋内側の空オブジェクト
    // 1 個＝合計 `m+1` 個のコンテナを生成するため、合計を `MAX_JSON_DEPTH` に
    // 揃えるには `m = MAX_JSON_DEPTH - 1` を渡す（配列版 `nested_arrays(n, "")` が
    // `n` 個の開き括弧そのままで合計 `n` 個のコンテナになる形と揃えるための補正）。
    assert_accepted(
        "object/empty at limit",
        &nested_objects(MAX_JSON_DEPTH - 1, "{}"),
    );
    assert_rejected(
        "object/empty over limit",
        &nested_objects(MAX_JSON_DEPTH, "{}"),
    );
}

#[test]
fn container_item_count_boundary() {
    // 配列: ちょうど上限・上限+1。
    let at_limit = array_with_items(MAX_JSON_CONTAINER_ITEMS);
    match assert_accepted("array at item limit", &at_limit) {
        JsonValue::Array(items) => assert_eq!(items.len(), MAX_JSON_CONTAINER_ITEMS),
        other => panic!("expected array, got {other:?}"),
    }
    assert_rejected(
        "array over item limit",
        &array_with_items(MAX_JSON_CONTAINER_ITEMS + 1),
    );

    // オブジェクト: 相異なるキーちょうど上限・上限+1。
    let obj_at_limit = object_with_distinct_keys(MAX_JSON_CONTAINER_ITEMS);
    match assert_accepted("object at item limit", &obj_at_limit) {
        JsonValue::Object(map) => assert_eq!(map.len(), MAX_JSON_CONTAINER_ITEMS),
        other => panic!("expected object, got {other:?}"),
    }
    assert_rejected(
        "object over item limit",
        &object_with_distinct_keys(MAX_JSON_CONTAINER_ITEMS + 1),
    );

    // ネストした配列内でも同じ上限が効くこと。
    let nested = format!("[{}]", array_with_items(MAX_JSON_CONTAINER_ITEMS + 1));
    assert_rejected("nested array over item limit", &nested);
}

#[test]
fn string_length_boundary() {
    // ASCII: ちょうど上限・上限+1（chars 単位）。
    let at_limit = string_of_chars(MAX_JSON_STRING_CHARS, 'a');
    match assert_accepted("ascii string at char limit", &at_limit) {
        JsonValue::String(s) => assert_eq!(s.chars().count(), MAX_JSON_STRING_CHARS),
        other => panic!("expected string, got {other:?}"),
    }
    assert_rejected(
        "ascii string over char limit",
        &string_of_chars(MAX_JSON_STRING_CHARS + 1, 'a'),
    );

    // マルチバイト文字（chars 単位であり byte 単位でないことの証拠）。
    let multibyte_at_limit = string_of_chars(MAX_JSON_STRING_CHARS, 'あ');
    match assert_accepted("multibyte string at char limit", &multibyte_at_limit) {
        JsonValue::String(s) => assert_eq!(s.chars().count(), MAX_JSON_STRING_CHARS),
        other => panic!("expected string, got {other:?}"),
    }

    // オブジェクトのキーにも同じ上限が効くこと。
    let key_at_limit = string_of_chars(MAX_JSON_STRING_CHARS, 'k');
    let key_over_limit = string_of_chars(MAX_JSON_STRING_CHARS + 1, 'k');
    assert_accepted("object key at char limit", &format!("{{{key_at_limit}:1}}"));
    assert_rejected(
        "object key over char limit",
        &format!("{{{key_over_limit}:1}}"),
    );
}

#[test]
fn number_text_length_boundary() {
    // `self.pos - start > 64` が判定基準（数値テキスト全体の桁数、符号含む）。
    let at_limit = format!("1{}", "0".repeat(63)); // 64 桁
    assert_eq!(at_limit.len(), 64);
    assert_accepted("number text at 64 digits", &at_limit);

    let over_limit = format!("1{}", "0".repeat(64)); // 65 桁
    assert_eq!(over_limit.len(), 65);
    assert_rejected("number text at 65 digits", &over_limit);
}

#[test]
fn raw_control_characters() {
    // 0x00-0x1F の生制御文字は文字列内で全て拒否（エスケープ必須）。
    for b in 0x00u8..=0x1f {
        let input = format!("\"{}\"", b as char);
        assert_rejected(&format!("raw control char 0x{b:02x} in string"), &input);
    }
    // キー内でも同様。
    for b in [0x00u8, 0x09, 0x1f] {
        let input = format!("{{\"{}\":1}}", b as char);
        assert_rejected(&format!("raw control char 0x{b:02x} in key"), &input);
    }

    // DEL（0x7F）は制御文字扱いされず受理される。
    assert_accepted("DEL (0x7f) in string", "\"\u{7f}\"");

    // 標準エスケープ・`\u0000` は受理される。
    assert_accepted("escaped null in string", "\"\\u0000\"");
    assert_accepted("escaped newline in string", "\"\\n\"");

    // トークン間の空白は space/tab/LF/CR の 4 種のみ。VT・FF・NBSP・先頭 BOM は拒否。
    assert_rejected("vertical tab between tokens", "\u{0b}{}");
    assert_rejected("form feed between tokens", "\u{0c}{}");
    assert_rejected("nbsp between tokens", "\u{a0}{}");
    assert_rejected("leading BOM", "\u{feff}{}");

    // 先頭・末尾の space/tab/LF/CR は受理される。
    assert_accepted("leading/trailing standard whitespace", " \t{}\n\r");
}

#[test]
fn utf8_rejection() {
    // (a) production 経路: `\u` エスケープ経由の不正スカラー。
    assert_rejected("isolated high surrogate", "\"\\ud800\"");
    assert_rejected("isolated low surrogate", "\"\\udc00\"");
    assert_rejected(
        "high surrogate not followed by low surrogate",
        "\"\\ud800A\"",
    );
    assert_rejected("short hex escape", "\"\\u12\"");
    assert_rejected("unknown escape", "\"\\x41\"");

    // (b) バイト境界ゲート（テストローカル。§冒頭コメント参照）。
    let reject_bytes = |name: &str, bytes: &[u8]| match parse_json_bytes_via_utf8_gate(bytes) {
        Ok(_) => panic!("{name}: expected rejection for invalid UTF-8 bytes"),
        Err(err) => assert_eq!(err.wire_code(), "42601", "{name}: wire_code は 42601"),
    };
    reject_bytes("lone continuation byte", &[0x80]);
    reject_bytes("overlong encoding", &[0xC0, 0x80]);
    reject_bytes("truncated multibyte sequence", &[0xE3, 0x81]);
    reject_bytes(
        "surrogate-encoded bytes (CESU-8 style)",
        &[0xED, 0xA0, 0x80],
    );
    reject_bytes("invalid leading byte 0xF5", &[0xF5]);
    reject_bytes("invalid leading byte 0xFF", &[0xFF]);
    // JSON 本文中の値として不正バイト列を埋め込んだ形。
    let mut body = br#"{"a":""#.to_vec();
    body.extend_from_slice(&[0xC0, 0x80]);
    body.extend_from_slice(br#""}"#);
    reject_bytes("invalid utf-8 embedded in object value", &body);

    // 有効な UTF-8（マルチバイト文字・絵文字）はゲート経由でも受理される。
    match parse_json_bytes_via_utf8_gate("\"あ\"".as_bytes()) {
        Ok(JsonValue::String(s)) => assert_eq!(s, "あ"),
        other => panic!("expected accepted string, got {other:?}"),
    }
    match parse_json_bytes_via_utf8_gate("\"\u{1f600}\"".as_bytes()) {
        Ok(JsonValue::String(s)) => assert_eq!(s, "\u{1f600}"),
        other => panic!("expected accepted string, got {other:?}"),
    }
}

#[test]
fn duplicate_keys() {
    assert_rejected("duplicate key at top level", r#"{"a":1,"a":2}"#);
    assert_rejected("duplicate key in nested object", r#"{"x":{"a":1,"a":2}}"#);
    assert_rejected(
        "duplicate key in array of objects",
        r#"[{"a":1},{"a":2,"a":3}]"#,
    );
    assert_rejected("duplicate empty key", r#"{"":1,"":2}"#);
    // `\u0061` 単独のキーが復号後 `"a"` として受理されることを先に固定し、
    // 直後の重複拒否がエスケープそのものの拒否ではなく復号後キー比較による
    // ものだと切り分けられるようにする。
    match assert_accepted("escaped key alone decodes to a", r#"{"\u0061":1}"#) {
        JsonValue::Object(map) => assert!(map.contains_key("a"), "復号後キーは \"a\""),
        other => panic!("expected object, got {other:?}"),
    }
    // `\u0061` エスケープが復号後 `"a"` と等しいことを利用し、比較が復号後の
    // キー文字列で行われている（生のリテラル一致ではない）ことを固定する。
    assert_rejected(
        "duplicate key literal vs escaped a",
        r#"{"a":1,"\u0061":2}"#,
    );

    // NFC/NFD で異なるバイト列を持つキーは正規化されず別キーとして受理される
    // （バイト列相違＝別キー。復号後のキーで比較していることの証拠でもある）。
    // "é" (NFC, U+00E9) と "e" + U+0301 (NFD 分解形) は異なるバイト列。
    let nfc = "\u{e9}";
    let nfd = "e\u{301}";
    let input = format!(r#"{{"{nfc}":1,"{nfd}":2}}"#);
    match assert_accepted("NFC/NFD distinct keys accepted", &input) {
        JsonValue::Object(map) => assert_eq!(map.len(), 2, "正規化されず 2 件のキーとして扱われる"),
        other => panic!("expected object, got {other:?}"),
    }

    // 非重複の 2 キーは通常どおり受理される。
    assert_accepted("non-duplicate keys", r#"{"a":1,"b":2}"#);
}

#[test]
fn trailing_and_leading_garbage() {
    assert_rejected("trailing garbage after object", "{}garbage");
    assert_rejected("two top-level values (objects)", "{} {}");
    assert_rejected("trailing bracket after array", "[]]");
    assert_rejected("two top-level values (numbers)", "1 2");
    assert_rejected("trailing NUL byte", "{}\u{0}");
    assert_rejected("empty input", "");
    assert_rejected("whitespace-only input", "   ");
    assert_rejected("trailing comma in array", "[1,]");
    assert_rejected("trailing comma in object", "{\"a\":1,}");
    assert_rejected("unquoted object key", "{a:1}");
    assert_rejected("single-quoted string", "'a'");

    // 末尾の標準空白（LF/CR/tab）は受理される。
    assert_accepted("trailing standard whitespace", "{} \n\t\r");
}

/// DoS 対策としての「上限超過を確保前に拒否する」実装方針の軽量な機械的裏付け
/// （§冒頭「何を検証するか」の補足）: `json.rs` のパーサ実装が任意の未検証長で
/// `Vec::with_capacity` / `String::with_capacity` を呼ばないこと（成長は
/// `Vec::new()` / `String::new()` からの通常の push によるものであり、入力長を
/// 超える先行確保が無いこと）をソーステキストの grep で固定する
/// （`tests/ann_future_work_doc.rs` の `include_str!` 前例に倣う軽量ガード）。
#[test]
fn allocation_guard_no_with_capacity_in_parser() {
    let source = include_str!("../../src/json.rs");
    assert!(
        !source.contains("with_capacity"),
        "json.rs は with_capacity を使わない実装であること \
         （件数検査は次要素のパース前・確保は Vec::new()/String::new() の成長のみ）"
    );
}
