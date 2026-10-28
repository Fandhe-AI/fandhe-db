//! `crates/wire-server/docs/nosql-api.md`（Issue #781・TASK-184。対象
//! ビヘイビア ERR-4・ERR-5）の「エラー応答」節が、`http::status::http_status`・
//! `http::error_body::{encode, encode_may_be_committed}`・`http::response::
//! reason_phrase` を単一情報源とする射影・本文仕様から乖離していないことを
//! 固定する非 vacuous ガード。
//!
//! `nosql_api_doc_examples.rs`（op スキーマ節の json フェンス検証）・
//! `tls_scram_design_doc.rs`（WIRE-9・HTTP-10）と同じ「ドキュメントを読み込み、
//! 該当章を切り出してコードと突き合わせる」設計に倣う。表の 5 列目
//! （NoSQL 表層での主な発生源）は自由記述であり機械検証の対象にしない
//! （`docs/design/` に倣い、将来ハンドラが増えた際は手動更新する前提）。

use std::fs;
use std::path::PathBuf;

use engine::error_format::ErrorClass;
use engine::json::parse_json;
use wire_server::http::error_body::{encode, encode_may_be_committed, MAY_BE_COMMITTED_STATE};
use wire_server::http::response::reason_phrase;
use wire_server::http::status::http_status;

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

/// `## エラー応答` 見出し行から、次の `## ` 見出し行（`## curl 例`）の直前
/// までを切り出す。他章にも表（`op 別スキーマ`の `| キー | 必須 | ... |`
/// 等）があるため、章の範囲を先に確定させてから表を探す。
fn error_section(markdown: &str) -> String {
    let start_marker = "## エラー応答\n";
    let start = markdown
        .find(start_marker)
        .expect("`## エラー応答` 見出しが見つからない");
    let after_start = &markdown[start..];
    let rest = &after_start[start_marker.len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    format!("{start_marker}{}", &rest[..end])
}

struct Row {
    wire_code: String,
    code: String,
    status: u16,
    reason: String,
}

/// 章内の射影表（先頭セルが `` `wire_code` `` のヘッダ行を持つ表）を
/// パースする。区切り行（`| --- | ... |`）はヘッダの直後 1 行として読み飛ばす。
/// 5 列目（発生源）は自由記述のため保持しない。
fn projection_rows(section: &str) -> Vec<Row> {
    let lines: Vec<&str> = section.lines().collect();
    let header_idx = lines
        .iter()
        .position(|l| l.trim_start().starts_with("| `wire_code` |"))
        .expect("射影表のヘッダ行（`| `wire_code` | ...`）が見つからない");

    let mut rows = Vec::new();
    // ヘッダの次（区切り行）をスキップし、その次の行から連続する `|` 開始行を読む。
    let mut i = header_idx + 2;
    while i < lines.len() {
        let line = lines[i].trim();
        if !line.starts_with('|') {
            break;
        }
        let cells: Vec<String> = line
            .trim_start_matches('|')
            .trim_end_matches('|')
            .split('|')
            .map(|c| c.trim().trim_matches('`').to_string())
            .collect();
        assert!(cells.len() >= 4, "射影表の行の列数が不足している: {line:?}");
        let status: u16 = cells[2]
            .parse()
            .unwrap_or_else(|_| panic!("HTTP ステータス列が数値でない: {line:?}"));
        rows.push(Row {
            wire_code: cells[0].clone(),
            code: cells[1].clone(),
            status,
            reason: cells[3].clone(),
        });
        i += 1;
    }
    rows
}

/// 章内の ` ```json ` フェンス本文（trim 済み）を宣言順に抜き出す。
fn json_fences_in(section: &str) -> Vec<String> {
    let lines: Vec<&str> = section.lines().collect();
    let mut fences = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "```json" {
            let mut body = String::new();
            i += 1;
            while i < lines.len() && lines[i].trim() != "```" {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            fences.push(body.trim().to_string());
        }
        i += 1;
    }
    fences
}

#[test]
fn doc_has_error_section_with_projection_table() {
    let markdown = read_doc();
    let section = error_section(&markdown);
    let rows = projection_rows(&section);
    assert!(!rows.is_empty(), "射影表の行が 1 件も抽出できなかった");
}

/// 表の行数が `ErrorClass::ALL` と一致し、各 class の `code`（ラベル）が
/// ちょうど 1 回現れ、各行の `code` が既知の `ErrorClass` に解決できること
/// （stale な余剰行・欠落行のいずれも検出する）。ERR-6（TABLE-16・TASK-204、
/// Issue #904）が `wire_code`（`23502`）の分類間共有を認めたため、一意性の
/// キーは `wire_code` ではなく `code`（ラベル。全分類で一意なまま）を使う。
#[test]
fn projection_table_covers_every_error_class_exactly_once() {
    let markdown = read_doc();
    let section = error_section(&markdown);
    let rows = projection_rows(&section);

    assert_eq!(
        rows.len(),
        ErrorClass::ALL.len(),
        "表の行数が ErrorClass::ALL の件数と不一致"
    );

    for class in ErrorClass::ALL {
        let occurrences = rows.iter().filter(|r| r.code == class.label()).count();
        assert_eq!(
            occurrences,
            1,
            "code={} は表にちょうど 1 回現れるべき",
            class.label()
        );
    }

    for row in &rows {
        assert!(
            ErrorClass::ALL.iter().any(|c| c.label() == row.code),
            "表の code={} が既知の ErrorClass に解決できない（stale な行の疑い）",
            row.code
        );
    }
}

/// 各行で `http_status`／`label`／`reason_phrase` が実装の返値と一致する。
#[test]
fn projection_table_matches_http_status_label_and_reason_phrase() {
    let markdown = read_doc();
    let section = error_section(&markdown);
    let rows = projection_rows(&section);

    for row in &rows {
        // `code`（ラベル）で解決する: `wire_code`（`23502`）は ERR-6 で
        // 複数分類が共有し得るため、行を一意に特定できるキーは `code` のみ。
        let class = ErrorClass::ALL
            .into_iter()
            .find(|c| c.label() == row.code)
            .unwrap_or_else(|| panic!("code={} が解決できない", row.code));
        assert_eq!(
            class.wire_code(),
            row.wire_code,
            "code={} の wire_code が不一致",
            row.code
        );
        assert_eq!(
            http_status(class),
            row.status,
            "code={} の HTTP ステータスが不一致",
            row.code
        );
        let expected_reason = reason_phrase(row.status)
            .unwrap_or_else(|| panic!("status={} の reason_phrase が表外", row.status));
        assert_eq!(
            expected_reason, row.reason,
            "wire_code={} の理由句が不一致",
            row.wire_code
        );
    }
}

/// 行順が (HTTP ステータス, wire_code) の昇順で安定していることを固定する
/// （無言の並べ替えでレビューの見落としを防ぐ）。
#[test]
fn projection_table_row_order_is_status_then_wire_code() {
    let markdown = read_doc();
    let section = error_section(&markdown);
    let rows = projection_rows(&section);

    let mut sorted: Vec<(u16, String)> = rows
        .iter()
        .map(|r| (r.status, r.wire_code.clone()))
        .collect();
    sorted.sort();

    let actual: Vec<(u16, String)> = rows
        .iter()
        .map(|r| (r.status, r.wire_code.clone()))
        .collect();

    assert_eq!(
        actual, sorted,
        "射影表の行順が (HTTP ステータス, wire_code) の昇順になっていない"
    );
}

/// 章内の golden json フェンス（通常応答・緊急応答の 2 件）が
/// `error_body::encode`／`encode_may_be_committed` の実際の出力と
/// バイト単位で一致すること。
#[test]
fn error_body_examples_match_encoders_byte_exact() {
    let markdown = read_doc();
    let section = error_section(&markdown);
    let fences = json_fences_in(&section);

    let expected_normal = encode(ErrorClass::InternalError, "internal error");
    let expected_emergency = encode_may_be_committed(ErrorClass::InternalError, "internal error");

    assert!(
        fences.iter().any(|f| f == &expected_normal),
        "通常応答の golden json フェンスが encode() の出力と一致しない: {expected_normal}"
    );
    assert!(
        fences.iter().any(|f| f == &expected_emergency),
        "緊急応答の golden json フェンスが encode_may_be_committed() の出力と一致しない: {expected_emergency}"
    );

    // 章内のすべての json フェンスが構文的に妥当であることもあわせて確認する
    // （illustrative な 1 例目を含む。`nosql_api_doc_examples.rs` は op 別
    // スキーマ節のみを対象にするため、エラー応答節は本テストが担う）。
    for fence in &fences {
        parse_json(fence).unwrap_or_else(|_| panic!("json フェンスの構文解析に失敗: {fence}"));
    }
}

/// 章本文に、ヘッダ・状態語・仕様ポインタの必須記述が含まれること。
#[test]
fn error_section_mentions_headers_and_state_word() {
    let markdown = read_doc();
    let section = error_section(&markdown);

    for needle in [
        "application/json; charset=utf-8",
        MAY_BE_COMMITTED_STATE,
        "WWW-Authenticate: Bearer",
        "Connection: close",
    ] {
        assert!(
            section.contains(needle),
            "エラー応答節に必須記述 {needle:?} が含まれていない"
        );
    }
}

/// spec ビヘイビア ID のポインタ表記が含まれること（本文を転記せず ID
/// 参照のみに留める運用の固定。`.claude/rules/spec-confidentiality.md`）。
#[test]
fn error_section_keeps_spec_pointer_notation() {
    let markdown = read_doc();
    let section = error_section(&markdown);

    for needle in ["ERR-4", "ERR-5"] {
        assert!(
            section.contains(needle),
            "エラー応答節に spec ポインタ {needle:?} が含まれていない"
        );
    }
}
