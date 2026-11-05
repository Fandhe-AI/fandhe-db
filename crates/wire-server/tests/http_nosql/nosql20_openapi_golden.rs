//! `docs/openapi.json`（`OP_SCHEMAS` から機械生成した OpenAPI 3.1 文書のゴールデン）が
//! 生成器 `wire_server::http::query::openapi` の出力と一致することを固定する検査
//! （Issue #2105。親 #2103。NOSQL-20 ポインタ: `docs/spec/04-behavior/nosql-surface.md`）。
//!
//! 役割: 静的表 `OP_SCHEMAS`（またはクレート版 `info.version`）を変えたのに文書を
//! 更新し忘れると本テストが失敗し、CI（rust-ci の `cargo test --workspace`）で検出される。
//! 再生成は環境変数による自動上書きを持たず、次の明示コマンドで行う:
//! `cargo run -q -p fandhe-db-wire-server --example openapi_dump > crates/wire-server/docs/openapi.json`

use std::fs;
use std::path::PathBuf;

use engine::json::parse_json;
use wire_server::http::query::openapi::{document, render_pretty};

const REGEN: &str = "cargo run -q -p fandhe-db-wire-server --example openapi_dump > \
                     crates/wire-server/docs/openapi.json";

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/openapi.json")
}

fn read_golden() -> String {
    let path = golden_path();
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read golden {}: {e}; regenerate with: {REGEN}",
            path.display()
        )
    })
}

#[test]
fn nosql20_openapi_golden_matches_generator() {
    let generated = render_pretty().expect("OpenAPI generation must succeed");
    let golden = read_golden();
    if generated == golden {
        return;
    }
    let g: Vec<&str> = generated.lines().collect();
    let f: Vec<&str> = golden.lines().collect();
    let idx = (0..g.len().max(f.len()))
        .find(|&i| g.get(i) != f.get(i))
        .unwrap_or(0);
    let ctx = |v: &[&str]| -> String {
        let lo = idx.saturating_sub(2).min(v.len());
        let hi = (idx + 3).min(v.len());
        v.get(lo..hi).map(|s| s.join("\n")).unwrap_or_default()
    };
    panic!(
        "docs/openapi.json is out of date (first difference at line {}).\n\
         --- generated ---\n{}\n--- golden ---\n{}\n\
         Regenerate after changing OP_SCHEMAS or the crate version (info.version): {REGEN}",
        idx + 1,
        ctx(&g),
        ctx(&f)
    );
}

/// `GET /v1/openapi.json`（Issue #2244）が返す埋め込み文書がゴールデン・生成器と一致する。
#[test]
fn nosql20_openapi_embedded_document_matches_golden_and_generator() {
    use wire_server::http::openapi_endpoint::OPENAPI_DOCUMENT;
    assert_eq!(
        OPENAPI_DOCUMENT,
        read_golden(),
        "embedded document differs from docs/openapi.json"
    );
    assert_eq!(
        OPENAPI_DOCUMENT,
        render_pretty().expect("OpenAPI generation must succeed"),
        "embedded document differs from the generator; regenerate: {REGEN}"
    );
}

#[test]
fn nosql20_openapi_golden_parses_to_document() {
    let golden = read_golden();
    let parsed = parse_json(&golden).expect("golden must be valid JSON");
    let expected = document().expect("OpenAPI generation must succeed");
    assert_eq!(
        parsed, expected,
        "golden differs semantically; regenerate: {REGEN}"
    );
}

#[test]
fn nosql20_nosql_api_doc_references_openapi() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/nosql-api.md");
    let md = fs::read_to_string(&path).expect("nosql-api.md must be readable");
    assert!(
        md.contains("docs/openapi.json"),
        "nosql-api.md must reference docs/openapi.json"
    );
}
