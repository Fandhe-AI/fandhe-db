//! `EXPLAIN (FORMAT JSON)` の描画（Issue #2059・SQL-76 (1)(6) ポインタ）。
//!
//! 責務境界: 各 `EXPLAIN` 経路（`core.rs`・`sql::explain_tree` ほか）は従来どおり字下げ付きの
//! 計画テキストを作る。構文入口が `FORMAT JSON` を見つけたときだけ、その結果（`QUERY PLAN` 単一列の
//! テキスト行）を本モジュールが PostgreSQL 互換の JSON 1 セルへ変換する。計画テキストの字下げ規則
//! （`sql::explain_tree` の安定契約）を逆に読むため、JSON と TEXT は同一の情報を持つ（新しい情報源を
//! 足さず、行数・可視件数などテナント存在情報も増やさない。SQL-75 (5)）。
//!
//! JSON の形（出したら変えない安定契約）:
//! - 最上位は `[{"Plan": <節>}]`。2 スペース字下げの整形出力。
//! - 節は `"Node Type"`（見出し。見出しなしの節は固定の `result`）→ 属性行の `key: value`
//!   （最初の `": "` で分割。値は文字列のまま。行の順を保つ）→ 子があれば `"Plans"` 配列。
//! - 契約違反（`": "` を持たない属性行・同一節の重複キー・`Node Type`／`Plans` との衝突・
//!   字下げの不整合・複数のルート）は `Internal` で fail-closed にする。
//!
//! 解析も描画も再帰を使わない（行数の上限は木の構築側が保証している）。

use crate::catalog::ColumnType;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::explain::QUERY_PLAN_COLUMN;

/// 見出しなしの節に与える `Node Type`。
const DEFAULT_NODE_TYPE: &str = "result";
/// JSON 化する行数の上限（木の構築側の上限と同じ。独立した多層防御）。
const MAX_LINES: usize = 8192;

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

struct Node {
    node_type: Option<String>,
    attrs: Vec<(String, String)>,
    children: Vec<usize>,
}

/// 計画テキストの行を JSON 文字列へ変換する。
pub(crate) fn lines_to_json(lines: &[String]) -> Result<String, SqlSurfaceError> {
    if lines.is_empty() || lines.len() > MAX_LINES {
        return Err(internal("EXPLAIN plan is not convertible to JSON"));
    }
    let bad = || internal("EXPLAIN plan line cannot be converted to JSON");
    let mut nodes: Vec<Node> = Vec::new();
    // (深さ, nodes の添字)。根からの経路。
    let mut path: Vec<(usize, usize)> = Vec::new();

    for line in lines {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = line.get(indent..).ok_or_else(bad)?;
        if let Some(stage) = body.strip_prefix("-> ") {
            // 見出し行。深さ d >= 1 のとき字下げは 2(d-1)。
            if indent % 2 != 0 {
                return Err(bad());
            }
            let depth = indent / 2 + 1;
            while path.last().is_some_and(|(d, _)| *d >= depth) {
                path.pop();
            }
            let Some(&(pd, parent)) = path.last() else {
                return Err(bad());
            };
            if pd + 1 != depth {
                return Err(bad());
            }
            let idx = nodes.len();
            nodes.push(Node {
                node_type: Some(stage.to_string()),
                attrs: Vec::new(),
                children: Vec::new(),
            });
            nodes.get_mut(parent).ok_or_else(bad)?.children.push(idx);
            path.push((depth, idx));
            continue;
        }
        let has_pair = split_attr(body).is_some();
        if indent == 0 && path.is_empty() && !has_pair {
            // 深さ 0 の見出し（根）。
            nodes.push(Node {
                node_type: Some(body.to_string()),
                attrs: Vec::new(),
                children: Vec::new(),
            });
            path.push((0, 0));
            continue;
        }
        let (key, value) = split_attr(body).ok_or_else(bad)?;
        // 属性行の深さ。深さ 0 は字下げなし、d >= 1 は 2d+1。
        let depth = if indent == 0 {
            0
        } else if indent % 2 == 1 && indent >= 3 {
            (indent - 1) / 2
        } else {
            return Err(bad());
        };
        while path.last().is_some_and(|(d, _)| *d > depth) {
            path.pop();
        }
        let target = match path.last().copied() {
            Some((d, idx)) if d == depth => idx,
            Some((d, parent)) if d + 1 == depth => {
                // 見出しなしの子節。
                let idx = nodes.len();
                nodes.push(Node {
                    node_type: None,
                    attrs: Vec::new(),
                    children: Vec::new(),
                });
                nodes.get_mut(parent).ok_or_else(bad)?.children.push(idx);
                path.push((depth, idx));
                idx
            }
            None if depth == 0 => {
                // 見出しなしの根。
                nodes.push(Node {
                    node_type: None,
                    attrs: Vec::new(),
                    children: Vec::new(),
                });
                path.push((0, 0));
                0
            }
            _ => return Err(bad()),
        };
        let node = nodes.get_mut(target).ok_or_else(bad)?;
        if key == "Node Type" || key == "Plans" || node.attrs.iter().any(|(k, _)| k == key) {
            return Err(internal("EXPLAIN plan has a conflicting attribute key"));
        }
        node.attrs.push((key.to_string(), value.to_string()));
    }
    render(&nodes)
}

/// `key: value`（最初の `": "` で分割。値が空の `key:` も許す）。キーが空なら `None`。
fn split_attr(body: &str) -> Option<(&str, &str)> {
    let (k, v) = match body.split_once(": ") {
        Some(kv) => kv,
        None => (body.strip_suffix(':')?, ""),
    };
    (!k.is_empty()).then_some((k, v))
}

enum Task {
    Node {
        idx: usize,
        level: usize,
        inline: bool,
    },
    Raw(String),
}

fn pad(level: usize) -> String {
    "  ".repeat(level)
}

fn render(nodes: &[Node]) -> Result<String, SqlSurfaceError> {
    let bad = || internal("EXPLAIN plan line cannot be converted to JSON");
    let mut out = String::new();
    out.push_str("[\n  {\n    \"Plan\": ");
    let mut stack = vec![
        Task::Raw("\n  }\n]".to_string()),
        Task::Node {
            idx: 0,
            level: 2,
            inline: true,
        },
    ];
    while let Some(task) = stack.pop() {
        let (idx, level, inline) = match task {
            Task::Raw(s) => {
                out.push_str(&s);
                continue;
            }
            Task::Node { idx, level, inline } => (idx, level, inline),
        };
        let node = nodes.get(idx).ok_or_else(bad)?;
        if !inline {
            out.push_str(&pad(level));
        }
        out.push_str("{\n");
        let inner = pad(level + 1);
        let has_children = !node.children.is_empty();
        out.push_str(&inner);
        out.push_str("\"Node Type\": ");
        crate::json::write_canonical_string(
            node.node_type.as_deref().unwrap_or(DEFAULT_NODE_TYPE),
            &mut out,
        );
        if !node.attrs.is_empty() || has_children {
            out.push(',');
        }
        out.push('\n');
        for (i, (k, v)) in node.attrs.iter().enumerate() {
            out.push_str(&inner);
            crate::json::write_canonical_string(k, &mut out);
            out.push_str(": ");
            crate::json::write_canonical_string(v, &mut out);
            if i + 1 < node.attrs.len() || has_children {
                out.push(',');
            }
            out.push('\n');
        }
        // 閉じ（積む順は逆）。
        stack.push(Task::Raw(format!("{}}}", pad(level))));
        if has_children {
            out.push_str(&inner);
            out.push_str("\"Plans\": [\n");
            stack.push(Task::Raw(format!("{inner}]\n")));
            let last = node.children.len() - 1;
            for (i, child) in node.children.iter().enumerate().rev() {
                stack.push(Task::Raw(if i == last { "\n" } else { ",\n" }.to_string()));
                stack.push(Task::Node {
                    idx: *child,
                    level: level + 2,
                    inline: false,
                });
            }
        }
    }
    Ok(out)
}

/// `QUERY PLAN` 単一列のテキスト結果を、JSON 1 セル（列型 `json`）の結果へ変換する。
pub(crate) fn query_result_to_json(result: QueryResult) -> Result<QueryResult, SqlSurfaceError> {
    let mut lines = Vec::with_capacity(result.rows.len().min(MAX_LINES));
    for row in result.rows {
        match row.cells.as_slice() {
            [Cell::Text(t)] if lines.len() < MAX_LINES => lines.push(t.clone()),
            _ => return Err(internal("EXPLAIN result is not convertible to JSON")),
        }
    }
    let json = lines_to_json(&lines)?;
    Ok(QueryResult {
        columns: vec![ColumnMeta::Computed {
            name: QUERY_PLAN_COLUMN.to_string(),
            ty: Some(ColumnType::Json),
        }],
        rows: vec![ResultRow {
            id: 0,
            score: 0.0,
            cells: vec![Cell::Json(json)],
        }],
    })
}

/// 実行結果（`Explain`／分割実行の `Query`）を JSON へ変換する。EXPLAIN 以外の結果は契約違反。
pub(crate) fn outcome_to_json(
    outcome: crate::sql::SqlOutcome,
) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
    match outcome {
        crate::sql::SqlOutcome::Explain(r) => {
            Ok(crate::sql::SqlOutcome::Explain(query_result_to_json(r)?))
        }
        crate::sql::SqlOutcome::Query(r) => {
            Ok(crate::sql::SqlOutcome::Query(query_result_to_json(r)?))
        }
        _ => Err(internal("EXPLAIN result is not convertible to JSON")),
    }
}

/// Describe 用: `QUERY PLAN` 列の型を `json` にする。
pub(crate) fn columns_to_json(columns: Vec<ColumnMeta>) -> Vec<ColumnMeta> {
    columns
        .into_iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } => ColumnMeta::Computed {
                name,
                ty: Some(ColumnType::Json),
            },
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn headingless_leaf() {
        // SQL-76 (6)
        let j = lines_to_json(&l(&["a: 1", "b: x y"])).unwrap();
        crate::json::parse_json(&j).unwrap();
        assert!(j.contains("\"Node Type\": \"result\""));
        assert!(j.contains("\"b\": \"x y\""));
    }

    #[test]
    fn nested_tree_matches_text() {
        let lines = l(&[
            "limit",
            "k: v",
            "  -> sort",
            "     sort_keys: id",
            "     -> scan",
            "        t: a",
            "  -> scan",
            "     t: \"q\\x\"",
        ]);
        // 実際の字下げ規則に合わせた入力を、木から作って確かめる。
        let _ = lines;
        use crate::sql::explain_tree::ExplainNode;
        let mut root = ExplainNode::try_stage("limit".into(), l(&["k: v"])).unwrap();
        let mut sort = ExplainNode::try_stage("sort".into(), l(&["sort_keys: id"])).unwrap();
        sort.push_child(ExplainNode::try_stage("scan".into(), l(&["t: a"])).unwrap())
            .unwrap();
        root.push_child(sort).unwrap();
        root.push_child(ExplainNode::try_stage("scan".into(), l(&["t: \"q\"\\x"])).unwrap())
            .unwrap();
        let text = root.into_lines();
        let j = lines_to_json(&text).unwrap();
        let v = crate::json::parse_json(&j).unwrap();
        let s = format!("{v:?}");
        assert!(s.contains("limit") && s.contains("sort") && s.contains("Plans"));
        assert!(j.starts_with("[\n  {\n    \"Plan\": {\n      \"Node Type\": \"limit\","));
    }

    #[test]
    fn rejects_contract_violations() {
        assert!(lines_to_json(&l(&["x: 1", "x: 2"])).is_err());
        assert!(lines_to_json(&l(&["Plans: 1"])).is_err());
        assert!(lines_to_json(&l(&["Node Type: 1"])).is_err());
        assert!(lines_to_json(&l(&["root", "no pair"])).is_err());
        assert!(lines_to_json(&l(&["a: 1", "root"])).is_err());
        assert!(lines_to_json(&l(&[])).is_err());
        assert!(lines_to_json(&l(&["a: 1", "    -> deep"])).is_err());
    }

    #[test]
    fn escapes_values() {
        let j = lines_to_json(&l(&["a: \"quoted\" \\ end"])).unwrap();
        crate::json::parse_json(&j).unwrap();
    }
}
