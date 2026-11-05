//! `GET /v1/openapi.json` の応答（NOSQL-20 (3)・Issue #2244。ポインタ:
//! `docs/spec/04-behavior/nosql-surface.md`・`docs/spec/05-tasks.md` TASK-253）。
//!
//! 役割: ビルド時に埋め込んだ OpenAPI 3.1 文書（`docs/openapi.json`。`OP_SCHEMAS` から
//! [`crate::http::query::openapi`] が機械生成したゴールデン）を、そのままの
//! バイト列で `200`／`application/json` として返す応答を組み立てる。
//! [`crate::http::router::Router`] が `middleware::authenticate`（`/v1/query` と
//! 同一の Bearer 検証）を通過した後にだけ [`response`] を呼ぶ。認証前に本モジュールの
//! 内容を返す経路は無い。
//!
//! テナント非依存: 文書は静的表 `OP_SCHEMAS` 由来の定数で、カタログ・セッション・
//! テナント文脈を一切参照しない。主体によらず本文はバイト同一である。実行時の生成や
//! ファイル I/O も行わない（`include_str!` による埋め込みのみ）。
//!
//! 一致検査: 埋め込み内容がゴールデン・生成器の出力と一致することは
//! `tests/http_nosql/nosql20_openapi_golden.rs` が CI（`cargo test --workspace`）で
//! 検証する。生成器の入力（`OP_SCHEMAS`）を変えたらゴールデンを再生成する
//! （手順は `examples/openapi_dump.rs`）。

use std::time::SystemTime;

use crate::http::response;

/// 埋め込み済みの OpenAPI 文書（`crates/wire-server/docs/openapi.json` と同一）。
pub const OPENAPI_DOCUMENT: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json"));

/// 認証付き応答を中間キャッシュ・ブラウザに保存させない固定値（CR/LF を含まない定数）。
const CACHE_CONTROL: &str = "no-store";

/// 認証済み要求へ返す `200` 応答バイト列。`Content-Length` は本文バイト長と一致する。
pub(crate) fn response(now: SystemTime) -> Vec<u8> {
    response::encode_ok_with_cache_control(OPENAPI_DOCUMENT, CACHE_CONTROL, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::body::MAX_BODY_LEN;
    use crate::http::query::openapi::render_pretty;
    use engine::json::{parse_json, JsonValue};

    #[test]
    fn embedded_document_equals_generator_output() {
        assert_eq!(
            OPENAPI_DOCUMENT,
            render_pretty().expect("generator succeeds")
        );
    }

    #[test]
    fn embedded_document_is_openapi_3_1_json() {
        let v = parse_json(OPENAPI_DOCUMENT).expect("valid JSON");
        let JsonValue::Object(members) = v else {
            panic!("root must be an object");
        };
        let get = |k: &str| {
            members
                .iter()
                .find(|(name, _)| name.as_str() == k)
                .map(|(_, v)| v)
        };
        match get("openapi") {
            Some(JsonValue::String(s)) => assert!(s.starts_with("3.1."), "got {s}"),
            other => panic!("openapi member missing: {other:?}"),
        }
        assert!(get("info").is_some());
        assert!(get("paths").is_some());
    }

    #[test]
    fn embedded_document_is_smaller_than_body_limit() {
        assert!(OPENAPI_DOCUMENT.len() < MAX_BODY_LEN);
    }

    #[test]
    fn response_has_matching_content_length_and_no_store() {
        let bytes = response(SystemTime::UNIX_EPOCH);
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json; charset=utf-8\r\n"));
        assert!(text.contains(&format!("Content-Length: {}\r\n", OPENAPI_DOCUMENT.len())));
        assert!(text.contains("Cache-Control: no-store\r\n"));
        assert!(text.ends_with(OPENAPI_DOCUMENT));
    }
}
