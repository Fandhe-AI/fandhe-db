//! CTE・派生表と WHERE の部分クエリの `EXPLAIN` 用の段の木の組み立て
//! （Issue #2048・SQL-75 (2) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `Statement::Explain` アーム（`run_cte_explain` と、単一表の広域取得・集計に
//! 部分クエリが付く場合の `run_subquery_explain`）。`sql::subquery` が内側を実行せずに静的検証して集めた
//! 記述（[`SubqueryStageSpec`]）と、`core.rs` が作る走査分類の行を受け取り、`sql::explain_tree` の
//! [`ExplainNode`] を返す純粋関数だけを置く。行・ロック・台帳・世代・キャッシュには触れない。
//!
//! 出力語彙は `sql::join::explain`・`sql::set_op_explain` と同じ閉じた集合に
//! `cte`・`cte_source`・`cte_mode`・`subquery`・`subquery_kind`・`correlated` を足したもの。
//! CTE 名・別名・表名・リテラル・`LIMIT` 値・行数は出さない（SQL-75 (5)。テナント存在情報と
//! 利用者リテラルの露出を避ける）。

use crate::sql::allowlist::CteExplainSource;
use crate::sql::explain_tree::{present, ExplainNode, ExplainTreeError};
use crate::sql::set_op::AggregateShape;
use crate::sql::subquery::explain_sink::CorrelationShape;

/// 部分クエリの種別。出力の `subquery_kind` 行の閉じた語彙に写る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubqueryKind {
    In,
    NotIn,
    Exists,
    NotExists,
    Scalar,
    Any,
    All,
}

impl SubqueryKind {
    fn token(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::NotIn => "not_in",
            Self::Exists => "exists",
            Self::NotExists => "not_exists",
            Self::Scalar => "scalar",
            Self::Any => "any",
            Self::All => "all",
        }
    }
}

/// 部分クエリの内側の形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InnerStageSpec {
    /// 内側が広域取得。`lines` は単一表の `EXPLAIN` と共有の走査分類の行。
    Scan { lines: Vec<String> },
    /// 内側が集計形スカラー。走査はキャッシュ・索引なしの固定分類（`lines`）。
    Aggregate {
        shape: AggregateShape,
        lines: Vec<String>,
    },
}

/// 部分クエリ 1 件分の段の記述。`children` は内側の WHERE にある入れ子の部分クエリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubqueryStageSpec {
    pub(crate) kind: SubqueryKind,
    pub(crate) inner: InnerStageSpec,
    /// 相関部分クエリのときだけ `Some`（Issue #2977。キーの数と残余の有無だけを持つ）。
    pub(crate) correlated: Option<CorrelationShape>,
    pub(crate) children: Vec<SubqueryStageSpec>,
}

/// `subquery` 段（`subquery_kind`・`correlated: false`）→ 内側の `scan`（集計形は
/// `aggregate` → `scan`）の節を作る。入れ子の部分クエリは内側の `scan` の子に並べる。
/// 再帰の深さは構文段の部分クエリ深さ上限（4）で抑えられ、木の上限は [`ExplainNode`] が守る。
fn subquery_node(spec: SubqueryStageSpec) -> Result<ExplainNode, ExplainTreeError> {
    let SubqueryStageSpec {
        kind,
        inner,
        correlated,
        children,
    } = spec;
    // 相関のときだけ、相関キーの数と残余述語の有無（ハッシュ半結合か行ごとの再評価かの静的判定）を足す。
    // 列名・値は出さない（SQL-75 (5)）。非相関の出力は従来とバイト単位で同一。
    let mut head = vec![format!("subquery_kind: {}", kind.token())];
    match correlated {
        None => head.push("correlated: false".to_string()),
        Some(shape) => {
            head.push("correlated: true".to_string());
            head.push(format!("correlation_keys: {}", shape.keys));
            head.push(format!("correlated_residual: {}", present(shape.residual)));
        }
    }
    let mut node = ExplainNode::try_stage("subquery".to_string(), head)?;
    let (aggregate, lines) = match inner {
        InnerStageSpec::Scan { lines } => (None, lines),
        InnerStageSpec::Aggregate { shape, lines } => (Some(shape), lines),
    };
    let mut scan = ExplainNode::try_stage("scan".to_string(), lines)?;
    for child in children {
        scan.push_child(subquery_node(child)?)?;
    }
    match aggregate {
        Some(shape) => {
            let mut agg = aggregate_node(&shape)?;
            agg.push_child(scan)?;
            node.push_child(agg)?;
        }
        None => node.push_child(scan)?,
    }
    Ok(node)
}

fn aggregate_node(shape: &AggregateShape) -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage(
        "aggregate".to_string(),
        vec![
            format!("group_keys: {}", shape.group_keys),
            format!("aggregates: {}", shape.aggregates),
            format!("having: {}", present(shape.having)),
        ],
    )
}

/// 外側の単一表 `EXPLAIN`（見出しなしの葉）に、出現順の `subquery` 段を子として足す。
/// 部分クエリが無い（`specs` が空）ときは葉をそのまま返し、出力は従来とバイト単位で同一。
pub(crate) fn with_subqueries(
    outer_lines: Vec<String>,
    specs: Vec<SubqueryStageSpec>,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut root = ExplainNode::leaf(outer_lines);
    for spec in specs {
        root.push_child(subquery_node(spec)?)?;
    }
    Ok(root)
}

/// インライン展開された CTE・派生表の主クエリの形（件数・有無のみ。値は持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CteMainShape {
    pub(crate) has_limit: bool,
    pub(crate) has_offset: bool,
    pub(crate) sort_keys: usize,
    pub(crate) aggregate: Option<AggregateShape>,
}

/// インライン展開の `cte` 節。`cte`（`cte_source`・`cte_mode: inline`）の下に、外側から
/// `limit`（あるときだけ）→ `sort`（キーがあるときだけ）→ `aggregate`（集計のときだけ）→ `scan` を並べる。
/// 畳み込み後の文が実行される形をそのまま示し、CTE の定義ごとの段は作らない。
pub(crate) fn cte_inline_tree(
    source: CteExplainSource,
    shape: &CteMainShape,
    scan_lines: Vec<String>,
    subqueries: Vec<SubqueryStageSpec>,
) -> Result<ExplainNode, ExplainTreeError> {
    let source_token = match source {
        CteExplainSource::With => "with",
        CteExplainSource::DerivedTable => "derived_table",
    };
    let mut current = ExplainNode::try_stage("scan".to_string(), scan_lines)?;
    // 内側の WHERE の部分クエリ（Issue #2977）は畳み込み後の `scan` の子に出現順で並べる。
    for spec in subqueries {
        current.push_child(subquery_node(spec)?)?;
    }
    if let Some(agg) = &shape.aggregate {
        let mut node = aggregate_node(agg)?;
        node.push_child(current)?;
        current = node;
    }
    if shape.sort_keys > 0 {
        let mut node = ExplainNode::try_stage(
            "sort".to_string(),
            vec![format!("sort_keys: {}", shape.sort_keys)],
        )?;
        node.push_child(current)?;
        current = node;
    }
    if shape.has_limit || shape.has_offset {
        let mut node = ExplainNode::try_stage(
            "limit".to_string(),
            vec![
                format!("limit: {}", present(shape.has_limit)),
                format!("offset: {}", present(shape.has_offset)),
            ],
        )?;
        node.push_child(current)?;
        current = node;
    }
    let mut root = ExplainNode::try_stage(
        "cte".to_string(),
        vec![
            format!("cte_source: {source_token}"),
            "cte_mode: inline".to_string(),
        ],
    )?;
    root.push_child(current)?;
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_subqueries_renders_the_bare_leaf() {
        let node = with_subqueries(lines(&["scalar_plan: plain_scan"]), Vec::new()).unwrap();
        assert_eq!(node.into_lines(), lines(&["scalar_plan: plain_scan"]));
    }

    #[test]
    fn nested_subquery_hangs_under_the_inner_scan() {
        let inner = SubqueryStageSpec {
            kind: SubqueryKind::Exists,
            inner: InnerStageSpec::Scan {
                lines: lines(&["a: 1"]),
            },
            correlated: None,
            children: Vec::new(),
        };
        let outer = SubqueryStageSpec {
            kind: SubqueryKind::In,
            inner: InnerStageSpec::Scan {
                lines: lines(&["b: 2"]),
            },
            correlated: None,
            children: vec![inner],
        };
        let out = with_subqueries(lines(&["root: 0"]), vec![outer])
            .unwrap()
            .into_lines();
        assert_eq!(
            out,
            lines(&[
                "root: 0",
                "-> subquery",
                "   subquery_kind: in",
                "   correlated: false",
                "  -> scan",
                "     b: 2",
                "    -> subquery",
                "       subquery_kind: exists",
                "       correlated: false",
                "      -> scan",
                "         a: 1",
            ])
        );
    }

    #[test]
    fn correlated_subquery_reports_key_count_and_residual() {
        let spec = SubqueryStageSpec {
            kind: SubqueryKind::Exists,
            inner: InnerStageSpec::Scan {
                lines: lines(&["a: 1"]),
            },
            correlated: Some(CorrelationShape {
                keys: 2,
                residual: true,
            }),
            children: Vec::new(),
        };
        let out = with_subqueries(lines(&["root: 0"]), vec![spec])
            .unwrap()
            .into_lines();
        assert_eq!(
            out,
            lines(&[
                "root: 0",
                "-> subquery",
                "   subquery_kind: exists",
                "   correlated: true",
                "   correlation_keys: 2",
                "   correlated_residual: present",
                "  -> scan",
                "     a: 1",
            ])
        );
    }

    #[test]
    fn inline_cte_stacks_limit_sort_scan() {
        let shape = CteMainShape {
            has_limit: true,
            has_offset: false,
            sort_keys: 2,
            aggregate: None,
        };
        let out = cte_inline_tree(CteExplainSource::With, &shape, lines(&["x: 1"]), Vec::new())
            .unwrap()
            .into_lines();
        assert_eq!(
            out,
            lines(&[
                "cte",
                "cte_source: with",
                "cte_mode: inline",
                "-> limit",
                "   limit: present",
                "   offset: none",
                "  -> sort",
                "     sort_keys: 2",
                "    -> scan",
                "       x: 1",
            ])
        );
    }
}
