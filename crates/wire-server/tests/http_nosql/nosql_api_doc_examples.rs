//! `crates/wire-server/docs/nosql-api.md`（Issue #780・TASK-184）に埋め込んだ
//! ```json フェンスが、実際の op スキーマ（`wire_server::http::query::schema`）
//! に対して構文的に妥当であることを固定する非 vacuous ガード。
//!
//! `tls_scram_design_doc.rs`（WIRE-9・HTTP-10）と同じ「ドキュメントを読み込み、
//! フェンスを抜き出して検証する」設計に倣う。文書の本文（説明文・表）は検証
//! 対象にせず、`json` フェンスのみを見る。

use std::fs;
use std::path::PathBuf;

use engine::json::{parse_json, JsonValue};
use wire_server::http::query::op::Op;
use wire_server::http::query::schema::schema_for;

/// テスト対象ドキュメントの絶対パス。`CARGO_MANIFEST_DIR` は
/// `crates/wire-server` を指すため、そこからの相対パスで解決する。
fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/nosql-api.md")
}

fn read_doc() -> String {
    let path = doc_path();
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "NoSQL API ドキュメントの読み込みに失敗: {}: {}",
            path.display(),
            e
        )
    })
}

/// ` ```json ` フェンスで囲まれた本文を宣言順に抜き出す。フェンスの直前行が
/// `<!-- expect: reject -->` であれば、そのブロックは「拒否例」として扱う
/// （現時点の文書は成功例のみを埋め込むため、この経路は将来の拡張用の
/// フックとして用意するのみで、本テストでは常に空になる）。
struct Fence {
    body: String,
    expect_reject: bool,
}

fn extract_json_fences(markdown: &str) -> Vec<Fence> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut fences = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "```json" {
            let expect_reject = i > 0 && lines[i - 1].trim() == "<!-- expect: reject -->";
            let mut body = String::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "```" {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            fences.push(Fence {
                body,
                expect_reject,
            });
        }
        i += 1;
    }
    fences
}

/// フェンス本文からトップレベルの `op` フィールド（存在する場合）を取り出す。
fn op_of(value: &JsonValue) -> Option<&str> {
    let JsonValue::Object(map) = value else {
        return None;
    };
    match map.get("op") {
        Some(JsonValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

#[test]
fn doc_exists() {
    assert!(
        doc_path().is_file(),
        "NoSQL API ドキュメントが見つからない: {}",
        doc_path().display()
    );
}

/// すべての `json` フェンスが構文的に妥当な JSON であることを検証する
/// （拒否例として明示マークされたブロックであっても、構文自体は妥当である
/// 前提——本文書は構文不正の例を載せない）。
#[test]
fn all_json_fences_are_syntactically_valid() {
    let markdown = read_doc();
    let fences = extract_json_fences(&markdown);
    assert!(
        !fences.is_empty(),
        "ドキュメントに json フェンスが 1 件も無い"
    );
    for fence in &fences {
        parse_json(&fence.body)
            .unwrap_or_else(|_| panic!("json フェンスの構文解析に失敗:\n{}", fence.body));
    }
}

/// `op` キーを持つブロックは、対応する op スキーマの `validate` を通過する
/// （拒否例マーカーが付いていない限り）。schema_for が `None` を返す
/// （語彙外 `op`）ケースは、拒否例マーカー付きのみ許容する。
#[test]
fn op_tagged_fences_validate_against_their_schema() {
    let markdown = read_doc();
    let fences = extract_json_fences(&markdown);
    for fence in &fences {
        let value = match parse_json(&fence.body) {
            Ok(v) => v,
            Err(_) => continue, // 構文検証は別テストの責務
        };
        let Some(op) = op_of(&value) else {
            continue; // op を持たないブロック（session 要求例・応答例等）は対象外
        };
        match schema_for(op) {
            Some(schema) => {
                let result = schema.validate(&value);
                if fence.expect_reject {
                    assert!(
                        result.is_err(),
                        "拒否例マーカー付きだが validate が成功した: op={op}\n{}",
                        fence.body
                    );
                } else {
                    assert!(
                        result.is_ok(),
                        "op={op} のフェンスが schema.validate を通過しなかった: {:?}\n{}",
                        result.err(),
                        fence.body
                    );
                }
            }
            None => {
                assert!(
                    fence.expect_reject,
                    "op={op:?} は語彙外のはずだが拒否例マーカーが無い\n{}",
                    fence.body
                );
            }
        }
    }
}

/// 非 vacuous 条件: [`Op::ALL`]（6 op）それぞれについて、要求例として
/// 最低 1 件の `json` フェンスが存在する（`op` キーを持ち、対応する
/// フィールドがスキーマ検証を通過するブロック）。`Op::ALL` 駆動にすることで
/// 語彙が増えた際（Issue #875 の `update`／`delete` 追加を含む）にこの
/// テストが黙って旧語彙のままにならない。
#[test]
fn all_ops_have_at_least_one_example() {
    let markdown = read_doc();
    let fences = extract_json_fences(&markdown);
    for expected_op in Op::ALL.iter().map(|op| op.name()) {
        let found = fences.iter().any(|fence| {
            !fence.expect_reject
                && parse_json(&fence.body)
                    .ok()
                    .and_then(|v| op_of(&v).map(str::to_string))
                    .as_deref()
                    == Some(expected_op)
        });
        assert!(found, "op={expected_op} の要求例が見つからない");
    }
}

/// `/v1/session` の要求例（`user`／`password` を持ち `op` キーを持たない
/// オブジェクト）が最低 1 件存在する。
#[test]
fn session_request_example_exists() {
    let markdown = read_doc();
    let fences = extract_json_fences(&markdown);
    let found = fences.iter().any(|fence| {
        let Ok(JsonValue::Object(map)) = parse_json(&fence.body) else {
            return false;
        };
        matches!(map.get("user"), Some(JsonValue::String(_)))
            && matches!(map.get("password"), Some(JsonValue::String(_)))
    });
    assert!(found, "/v1/session の要求例が見つからない");
}
