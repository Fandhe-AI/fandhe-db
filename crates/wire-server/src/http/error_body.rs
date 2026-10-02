//! `ErrorClass` → JSON エラー本文（NoSQL 表層。Issue #745。対象ビヘイビア
//! ERR-4・ERR-5。ポインタ: `docs/spec/05-tasks.md` TASK-180・
//! `docs/spec/04-behavior/error-format.md` ERR-4・ERR-5）。
//!
//! 責務境界: [`crate::error_response`]（SQL 表層。`ErrorClass` → wire
//! `ErrorResponse` バイト列）の HTTP 版に相当し、同じ「分類済みの `ErrorClass`
//! を受け取り応答表現へ整形するだけ」の純関数を提供する。ソケット I/O・
//! ステータス行・ヘッダ（`Content-Type`／`Content-Length`）の組み立ては
//! 呼び出し元（後続 Issue #746 の応答エンコーダ）の責務であり、本モジュールは
//! 応答本文の `String` を返すところまでに閉じる。
//!
//! `error_response.rs` の `encode`／`encode_with_detail` が `Result` を返すのは
//! wire プロトコルのフィールド終端が NUL バイトに依存し、`message`/`detail` への
//! NUL 混入がフレーム構造そのものを破壊しうるため（fail-closed に拒否する必要が
//! ある）。JSON はそもそも任意の Unicode スカラー値（NUL を含む）を
//! `"\u0000"` として表現できるため、本モジュールの [`encode`]／
//! [`encode_may_be_committed`] は **infallible**（`String` を直接返す）。
//!
//! `data` オブジェクトを応答本文へ含めるのは緊急応答（`RECOVER-5` (3)。commit
//! 後 panic 時に「commit は成功しているかもしれない」ことを伝える契約。ERR-5）
//! に該当する場合のみであり、その判定自体は呼び出し元（#747 の接続ハンドラ）の
//! 責務である——本モジュールは「該当する」と呼ばれたときに限り `data` を足す
//! 専用 API（[`encode_may_be_committed`]）を通常 API（[`encode`]）から構造的に
//! 分離することで、誤って通常応答へ `data` が混入する経路を作らない。
//!
//! `message` の契約: 呼び出し元は固定の英語文言、または
//! `engine::error_format::WireError` 由来（内部エラーは固定文言へ差し替え・
//! 長さ上限あり）の値のみを渡すこと。他テナントのデータ・存在情報・内部詳細を
//! `message` へ流し込まない（`.claude/rules/security.md` P0。`error_response.rs`
//! の同名引数と同じ制約）。

use std::fmt::Write as _;

use engine::error_format::ErrorClass;

/// 緊急応答（`RECOVER-5` (3)・ERR-5 ポインタ）の `data.state` に入れる固定値。
/// SQL 表層側の `crate::error_response::MAY_BE_COMMITTED_DETAIL`
/// （`"state=" + 本値`）と同一の状態語を共有する（両表層で状態語が乖離しない
/// ことをテストで固定する）。
pub const MAY_BE_COMMITTED_STATE: &str = "may_be_committed";

/// JSON 文字列リテラルの中身（囲みの `"` を含まない）へ 1 文字ずつ変換して
/// `out` へ追記する。`"`・`\`・U+0000〜U+001F（一般制御文字）のみをエスケープし、
/// それ以外（非 ASCII・補助面文字・U+007F を含む）は UTF-8 のまま透過する
/// （応答本文は `charset=utf-8` で送出される前提のため ASCII 化は不要。
/// `crate::http::error_body` 外部への公開はしない——本文組み立てを閉じた
/// 語彙に保つための `pub(crate)`。#746 以降の他本文生成での再利用を想定）。
///
/// 出力バイト列は 0x20 未満のバイトを含まない（本文が常に改行なしの 1 行に
/// 収まる不変条件。後続の応答エンコーダがフレーミングで改行を気にしなくて
/// よいようにする）。
pub(crate) fn escape_json_string_into(out: &mut String, input: &str) {
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000A}' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\u{000D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                // `write!` への `String` 追記は infallible（`fmt::Write for
                // String` はアロケーション失敗以外で失敗しない）ため戻り値は
                // 捨ててよい。`unwrap`/`expect` を使わず `let _ =` で明示する。
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// `encode`／`encode_may_be_committed` の共有実体。`with_data` が真のときのみ
/// 末尾に `"data":{"state":"<MAY_BE_COMMITTED_STATE>"}` を追加する。
///
/// キー順序は `wire_code` → `code` → `message` → (`data`) の固定・空白なしの
/// コンパクト形（golden 文字列テストで固定。`Content-Length`〔#746〕算出対象を
/// 安定させるため）。
fn encode_inner(class: ErrorClass, message: &str, with_data: bool) -> String {
    // message 長 + 各フィールドの固定オーバーヘッド分だけ事前確保する
    // （呼び出し元契約により message は固定文言または長さ上限つきの
    // `WireError` 由来のみのため、確保サイズは有界）。
    let mut out = String::with_capacity(message.len() + 96);
    out.push_str("{\"error\":{\"wire_code\":\"");
    escape_json_string_into(&mut out, class.wire_code());
    out.push_str("\",\"code\":\"");
    escape_json_string_into(&mut out, class.label());
    out.push_str("\",\"message\":\"");
    escape_json_string_into(&mut out, message);
    out.push('"');
    if with_data {
        out.push_str(",\"data\":{\"state\":\"");
        out.push_str(MAY_BE_COMMITTED_STATE);
        out.push_str("\"}");
    }
    out.push_str("}}");
    out
}

/// 通常エラー応答の本文。
/// `{"error":{"wire_code":"<wire_code>","code":"<label>","message":"<message>"}}`。
/// `data` キーは決して含めない（緊急応答専用の [`encode_may_be_committed`] と
/// 構造的に分離している）。
pub fn encode(class: ErrorClass, message: &str) -> String {
    encode_inner(class, message, false)
}

/// 緊急応答（`RECOVER-5` (3) 該当時に限り呼び出す契約。該当判定は呼び出し元
/// ＝接続ハンドラ〔#747〕の責務）の本文。[`encode`] と同じ 3 フィールドの
/// 直後に `"data":{"state":"<MAY_BE_COMMITTED_STATE>"}` を追加する。
pub fn encode_may_be_committed(class: ErrorClass, message: &str) -> String {
    encode_inner(class, message, true)
}

/// 分割実行 DML の部分完了（`VD001`）・取り消し（`VD002`）の応答本文（Issue #1130・
/// NOSQL-12・ERR-4・RECOVER-11）。[`encode`] と同じ 3 フィールドの直後に
/// `"data":{"committed":<n>,"operation_id":"<escaped>"}` を追加する。
///
/// `data` に載せるのは自テナントで commit 済みの件数とクライアント自身が指定した
/// `operation_id` だけ（呼び出し元の契約。他テナント情報・テーブル名・内側のメッセージは
/// 載せない）。通常 API・緊急応答 API とは構造的に分離し、誤って他の経路へ `data` が
/// 混入しないようにする（`wire-server::http::query::partitioned` の 2 variant のみが呼ぶ）。
pub fn encode_partitioned(
    class: ErrorClass,
    message: &str,
    committed: u64,
    operation_id: &str,
) -> String {
    let mut out = String::with_capacity(message.len() + operation_id.len() + 128);
    out.push_str("{\"error\":{\"wire_code\":\"");
    escape_json_string_into(&mut out, class.wire_code());
    out.push_str("\",\"code\":\"");
    escape_json_string_into(&mut out, class.label());
    out.push_str("\",\"message\":\"");
    escape_json_string_into(&mut out, message);
    out.push_str("\",\"data\":{\"committed\":");
    let _ = write!(out, "{committed}");
    out.push_str(",\"operation_id\":\"");
    escape_json_string_into(&mut out, operation_id);
    out.push_str("\"}}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::json::{parse_json, JsonValue};
    use std::collections::BTreeSet;

    /// 本文をトップレベル `{"error": Object}` として構造的に解析するテスト
    /// 専用ヘルパー。`unwrap`/`expect` はテストコードでは許容する
    /// （`.claude/rules/coding-rust.md` の添字アクセス禁止は受信入力経路が
    /// 対象。`error_response.rs` の tests と同じ方針）。
    fn error_object(body: &str) -> std::collections::BTreeMap<String, JsonValue> {
        let parsed = parse_json(body).expect("valid JSON");
        let JsonValue::Object(mut top) = parsed else {
            panic!("top level must be an object");
        };
        let error_value = top.remove("error").expect("error key present");
        assert!(top.is_empty(), "top level must contain only the error key");
        let JsonValue::Object(error_obj) = error_value else {
            panic!("error value must be an object");
        };
        error_obj
    }

    fn as_str(value: &JsonValue) -> &str {
        match value {
            JsonValue::String(s) => s.as_str(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    /// R2: `encode` は全 `ErrorClass` で `{wire_code, code, message}` の
    /// 厳密 3 キーのみを持ち `data` を含まない。値は `wire_code()`／`label()`
    /// と一致する。
    #[test]
    fn normal_body_has_exactly_three_keys_for_all_classes() {
        for class in ErrorClass::ALL {
            let body = encode(class, "boom");
            let obj = error_object(&body);
            let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                BTreeSet::from(["wire_code", "code", "message"]),
                "class={class:?}"
            );
            assert_eq!(
                as_str(&obj["wire_code"]),
                class.wire_code(),
                "class={class:?}"
            );
            assert_eq!(as_str(&obj["code"]), class.label(), "class={class:?}");
            assert_eq!(as_str(&obj["message"]), "boom", "class={class:?}");
        }
    }

    /// R3: `encode_may_be_committed` は全 `ErrorClass` で `data` を含む
    /// 厳密 4 キーを持ち、`data == {"state": MAY_BE_COMMITTED_STATE}`。
    /// 先頭 3 キーの値は `encode` と一致する。
    #[test]
    fn may_be_committed_body_adds_data_state_for_all_classes() {
        for class in ErrorClass::ALL {
            let normal = error_object(&encode(class, "boom"));
            let emergency_body = encode_may_be_committed(class, "boom");
            let emergency = error_object(&emergency_body);

            let keys: BTreeSet<&str> = emergency.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                BTreeSet::from(["wire_code", "code", "message", "data"]),
                "class={class:?}"
            );
            assert_eq!(
                emergency["wire_code"], normal["wire_code"],
                "class={class:?}"
            );
            assert_eq!(emergency["code"], normal["code"], "class={class:?}");
            assert_eq!(emergency["message"], normal["message"], "class={class:?}");

            let JsonValue::Object(data_obj) = &emergency["data"] else {
                panic!("data must be an object (class={class:?})");
            };
            let data_keys: BTreeSet<&str> = data_obj.keys().map(String::as_str).collect();
            assert_eq!(data_keys, BTreeSet::from(["state"]), "class={class:?}");
            assert_eq!(
                as_str(&data_obj["state"]),
                MAY_BE_COMMITTED_STATE,
                "class={class:?}"
            );
        }
    }

    /// SQL 表層（`error_response::MAY_BE_COMMITTED_DETAIL`）と HTTP 表層
    /// （本モジュールの `MAY_BE_COMMITTED_STATE`）の状態語が乖離しないことを
    /// 固定する。`error_response.rs` の golden bytes・定数値そのものは無変更。
    #[test]
    fn state_constant_matches_sql_surface_detail() {
        assert_eq!(
            crate::error_response::MAY_BE_COMMITTED_DETAIL,
            format!("state={MAY_BE_COMMITTED_STATE}")
        );
    }

    /// R4: U+0000〜U+001F 全走査＋`"`／`\` を含む message を `encode` した
    /// 本文が 0x20 未満のバイトを一切含まず、`parse_json` 往復で原文へ一致する
    /// ことを確認する。パーサは生の制御文字を拒否するため、エスケープ漏れが
    /// あればここで往復失敗として検出される。
    #[test]
    fn escapes_quote_backslash_and_all_control_chars() {
        let mut message = String::new();
        for cp in 0u32..=0x1F {
            let ch = char::from_u32(cp).expect("valid control char codepoint");
            message.push(ch);
        }
        message.push('"');
        message.push('\\');

        let body = encode(ErrorClass::InvalidInput, &message);
        assert!(
            body.bytes().all(|b| b >= 0x20),
            "body must not contain raw bytes below 0x20: {body:?}"
        );

        let obj = error_object(&body);
        assert_eq!(as_str(&obj["message"]), message);
    }

    /// よく使う制御文字は短縮形（`\n`/`\r`/`\t`/`\b`/`\f`）、それ以外の
    /// 一般制御文字は小文字 `\u00xx` になることを文字列一致で固定する。
    #[test]
    fn short_escape_forms_are_used_for_common_controls() {
        let mut out = String::new();
        escape_json_string_into(&mut out, "\u{0008}\u{0009}\u{000A}\u{000C}\u{000D}");
        assert_eq!(out, "\\b\\t\\n\\f\\r");

        let mut out = String::new();
        escape_json_string_into(&mut out, "\u{0001}");
        assert_eq!(out, "\\u0001");

        let mut out = String::new();
        escape_json_string_into(&mut out, "\u{001F}");
        assert_eq!(out, "\\u001f");
    }

    /// 非 ASCII（日本語・補助面の絵文字）・U+007F（DEL。RFC 8259 上エスケープ
    /// 不要）はエスケープされず透過し、往復一致する。
    #[test]
    fn non_ascii_and_supplementary_plane_pass_through() {
        let message = "日本語\u{1F600}\u{007F}";
        let body = encode(ErrorClass::InvalidInput, message);
        let obj = error_object(&body);
        assert_eq!(as_str(&obj["message"]), message);
    }

    /// 出力文字列そのものをキー順・空白なしのコンパクト形で固定する
    /// （`Content-Length`〔#746〕算出対象の安定性のため）。
    #[test]
    fn golden_string_is_fixed() {
        assert_eq!(
            encode(ErrorClass::InternalError, "internal error"),
            "{\"error\":{\"wire_code\":\"XX000\",\"code\":\"INTERNAL_ERROR\",\"message\":\"internal error\"}}"
        );
        assert_eq!(
            encode_may_be_committed(ErrorClass::InternalError, "internal error"),
            "{\"error\":{\"wire_code\":\"XX000\",\"code\":\"INTERNAL_ERROR\",\"message\":\"internal error\",\"data\":{\"state\":\"may_be_committed\"}}}"
        );
    }

    /// 同一入力からは常に同一出力（外部状態・時刻・乱数への非依存の確認）。
    #[test]
    fn encode_is_deterministic() {
        for class in ErrorClass::ALL {
            assert_eq!(encode(class, "x"), encode(class, "x"), "class={class:?}");
            assert_eq!(
                encode_may_be_committed(class, "x"),
                encode_may_be_committed(class, "x"),
                "class={class:?}"
            );
        }
    }

    /// `wire_code()`／`label()` は固定 ASCII のためエスケープの前後で不変
    /// であることを機械的に確認する（一律エスケーパを通す設計の裏付け）。
    #[test]
    fn wire_code_and_label_never_require_escaping() {
        for class in ErrorClass::ALL {
            let mut wire_code_escaped = String::new();
            escape_json_string_into(&mut wire_code_escaped, class.wire_code());
            assert_eq!(wire_code_escaped, class.wire_code(), "class={class:?}");

            let mut label_escaped = String::new();
            escape_json_string_into(&mut label_escaped, class.label());
            assert_eq!(label_escaped, class.label(), "class={class:?}");
        }
    }

    /// message が空文字でも妥当な JSON になる。
    #[test]
    fn empty_message_is_valid_json() {
        let body = encode(ErrorClass::InvalidInput, "");
        let obj = error_object(&body);
        assert_eq!(as_str(&obj["message"]), "");
    }

    /// Issue #1130: 分割実行本文の golden 文字列（キー順固定）とエスケープ。
    #[test]
    fn partitioned_body_golden_and_escaping() {
        let body = encode_partitioned(ErrorClass::PartialCompletion, "m", 3, "op-1");
        assert_eq!(
            body,
            "{\"error\":{\"wire_code\":\"VD001\",\"code\":\"PARTIAL_COMPLETION\",\"message\":\"m\",\"data\":{\"committed\":3,\"operation_id\":\"op-1\"}}}"
        );
        let body = encode_partitioned(ErrorClass::PartitionedDmlCancelled, "m", 0, "a\"b\\c\u{1}");
        let obj = error_object(&body);
        let JsonValue::Object(data) = &obj["data"] else {
            panic!("data must be an object");
        };
        assert_eq!(as_str(&data["operation_id"]), "a\"b\\c\u{1}");
        assert_eq!(as_str(&obj["wire_code"]), "VD002");
    }
}
