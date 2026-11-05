//! `EXPLAIN (VERBOSE)` が根の段へ足す追加行の整形と挿入
//! （SQL-76 (4)(5) のポインタ。Issue #2060。設計記録は `docs/design/explain-search-engine-exposure.md`）。
//!
//! 役割分担:
//!
//! - 本モジュールは追加行の語彙（`output:`／`distance_metric:`）の整形と、描画済みの計画（`QUERY PLAN`
//!   1 列）の根の段の末尾への挿入だけを担う純粋関数の集まり。計画の生成・列の導出は
//!   `core/explain_verbose_exec.rs`（呼び出し元）が行う。
//! - 挿入位置は `sql::explain_analyze::root_end` と同じ規則（最初の子の段の直前）を共有する。
//!
//! 語彙（安定契約）: `output: <列>[, <列>...]`（結果の列名。素の識別子以外は `"` で囲み内部の `"` を二重化。
//! 制御文字を含む名前は `0A000`）、`distance_metric: <cosine|l2|ip|l1>`（距離順位付けの検索の段）。
//! 使った索引の名前は既定の出力の `scalar_plan:`／`ann_plan:` に既に併記されるため重ねて出さない。
//!
//! テナント境界・漏えい防止: 追加行は文とカタログから決まる値（列名・距離演算子の種別）だけで、表名・別名・
//! 行数・可視件数・推定コストを含めない（SQL-75 (5)・SQL-76 (3)(4)）。エラー文は固定の英語文で入力を
//! エコーしない。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, QueryResult, ResultRow};
use crate::sql::explain_tree::MAX_EXPLAIN_TREE_LINES;
use crate::sql::lexer::DistanceKind;

/// 根の段へ足す行の最大数（`output:`・`distance_metric:`）。上限判定の余裕に使う。
const MAX_VERBOSE_LINES: usize = 2;

/// 距離演算子の種別を語彙の値（`cosine|l2|ip|l1`）へ写す。
pub(crate) fn metric_name(kind: DistanceKind) -> &'static str {
    match kind {
        DistanceKind::Cosine => "cosine",
        DistanceKind::L2 => "l2",
        DistanceKind::NegativeInnerProduct => "ip",
        DistanceKind::L1 => "l1",
    }
}

/// 素の識別子（`[a-z_][a-z0-9_$]*`）か。
fn is_plain_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$')
}

/// 列名を計画テキストに出せる形へ整える。素の識別子以外は `"` で囲み、内部の `"` を二重にする
/// （区切りの `, ` を壊せない形にする）。制御文字（改行を含む）を含む名前は `0A000`（fail-closed）。
pub(crate) fn display_column_name(name: &str) -> Result<String, SqlSurfaceError> {
    if name.chars().any(char::is_control) {
        return Err(SqlSurfaceError::feature_not_supported(
            "EXPLAIN VERBOSE cannot display a column name",
        ));
    }
    if is_plain_identifier(name) {
        return Ok(name.to_string());
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

/// `output:` 行。列が 0 個なら `None`（行を出さない）。
pub(crate) fn output_line<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>, SqlSurfaceError> {
    let shown = names
        .into_iter()
        .map(display_column_name)
        .collect::<Result<Vec<_>, _>>()?;
    if shown.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("output: {}", shown.join(", "))))
}

/// `distance_metric:` 行。
pub(crate) fn metric_line(kind: DistanceKind) -> String {
    format!("distance_metric: {}", metric_name(kind))
}

/// 描画済みの計画の根の段の末尾（最初の子の段の直前）へ `extra` を挿入する。挿入後の行数が
/// `MAX_EXPLAIN_TREE_LINES` を超える場合は `54001`（固定文）で fail-closed にする。
pub(crate) fn insert_at_root(
    plan: QueryResult,
    extra: Vec<String>,
) -> Result<QueryResult, SqlSurfaceError> {
    let too_many = || SqlSurfaceError::StatementTooComplex {
        detail: "EXPLAIN plan has too many lines",
    };
    let QueryResult { columns, mut rows } = plan;
    let limit = MAX_EXPLAIN_TREE_LINES
        .checked_add(MAX_VERBOSE_LINES)
        .ok_or_else(too_many)?;
    if rows.len() > limit || extra.len() > MAX_VERBOSE_LINES {
        return Err(too_many());
    }
    let at = crate::sql::explain_analyze::root_end(&rows);
    let tail = rows.split_off(at);
    rows.extend(extra.into_iter().map(|text| ResultRow {
        id: 0,
        score: 0.0,
        cells: vec![Cell::Text(text)],
    }));
    rows.extend(tail);
    if rows.len() > limit {
        return Err(too_many());
    }
    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(lines: &[&str]) -> QueryResult {
        QueryResult {
            columns: Vec::new(),
            rows: lines
                .iter()
                .map(|t| ResultRow {
                    id: 0,
                    score: 0.0,
                    cells: vec![Cell::Text((*t).to_string())],
                })
                .collect(),
        }
    }

    fn texts(q: &QueryResult) -> Vec<String> {
        q.rows
            .iter()
            .map(|r| match r.cells.first() {
                Some(Cell::Text(t)) => t.clone(),
                _ => String::new(),
            })
            .collect()
    }

    /// SQL-76 (5): 素の識別子はそのまま、それ以外は `"` で囲んで二重化する。
    #[test]
    fn column_names_are_quoted_when_not_plain() {
        let show = |n: &str| display_column_name(n).expect(n);
        assert_eq!(show("id"), "id");
        assert_eq!(show("a_1$"), "a_1$");
        assert_eq!(show("Kind"), "\"Kind\"");
        assert_eq!(show("a, b"), "\"a, b\"");
        assert_eq!(show("a\"b"), "\"a\"\"b\"");
        assert_eq!(show("1x"), "\"1x\"");
        assert_eq!(show(""), "\"\"");
    }

    /// SQL-76 (5): 制御文字を含む名前は 0A000（行区切りを壊させない）。
    #[test]
    fn control_characters_are_rejected() {
        for name in ["a\nb", "a\rb", "a\tb", "a\0b"] {
            let err = display_column_name(name).expect_err(name);
            assert_eq!(err.wire_code(), "0A000");
        }
    }

    #[test]
    fn output_line_joins_and_skips_empty() {
        assert_eq!(
            output_line(["id", "Kind"]).expect("ok").as_deref(),
            Some("output: id, \"Kind\"")
        );
        assert_eq!(output_line([]).expect("ok"), None);
    }

    #[test]
    fn metric_names() {
        assert_eq!(metric_line(DistanceKind::Cosine), "distance_metric: cosine");
        assert_eq!(metric_name(DistanceKind::L2), "l2");
        assert_eq!(metric_name(DistanceKind::NegativeInnerProduct), "ip");
        assert_eq!(metric_name(DistanceKind::L1), "l1");
    }

    /// 根の段の末尾（最初の子の段の直前）へ入り、既存の行の順序は変わらない。
    #[test]
    fn insert_goes_before_first_child() {
        let p = plan(&["mode: recall", "scalar_plan: x", "-> scan", "  detail: y"]);
        let out = insert_at_root(p, vec!["output: id".to_string()]).expect("ok");
        assert_eq!(
            texts(&out),
            vec![
                "mode: recall",
                "scalar_plan: x",
                "output: id",
                "-> scan",
                "  detail: y"
            ]
        );
    }

    /// 上限を超える木は 54001。
    #[test]
    fn insert_rejects_over_limit() {
        let lines: Vec<String> = (0..MAX_EXPLAIN_TREE_LINES + 2)
            .map(|i| format!("k{i}: v"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let err = insert_at_root(plan(&refs), vec!["output: id".to_string()]).expect_err("limit");
        assert_eq!(err.wire_code(), "54001");
    }
}
