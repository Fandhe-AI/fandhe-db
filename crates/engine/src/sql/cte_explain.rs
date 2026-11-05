//! CTE・派生表と WHERE の部分クエリの `EXPLAIN` 用の段の木の組み立て
//! （Issue #2048・SQL-75 (2) ポインタ）。
//!
//! 呼び出し元は `core.rs` の `Statement::Explain` アーム（`run_cte_explain` と、単一表の広域取得・集計に
//! 部分クエリが付く場合の `run_subquery_explain`）。`sql::subquery` が内側を実行せずに静的検証して集めた
//! 記述（[`SubqueryStageSpec`]）と、`core.rs` が作る走査分類の行を受け取り、`sql::explain_tree` の
//! [`ExplainNode`] を返す純粋関数だけを置く。行・ロック・台帳・世代・キャッシュには触れない。
//!
//! 出力語彙は `sql::join::explain`・`sql::set_op_explain` と同じ閉じた集合に
//! `cte`・`cte_source`・`cte_mode`（`inline`／`materialized`／`recursive`）・`cte_shared`・
//! `subquery`・`subquery_kind`・`correlated` を足したもの。具体化 CTE・再帰 CTE・CTE／派生表を
//! 入力にする JOIN（Issue #2976）は、同じ純粋関数群に `recursive_union`（`union`・`search`・`cycle`）・
//! `work_table_scan`・`result`（`source: constant`）の段を足して組み立てる。呼び出し元は
//! `core.rs` の `run_buffered_cte_explain`・`join_explain_node`。
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

/// `cte_source` 行の固定トークン。CTE 名・別名は出さない。
fn source_token(source: CteExplainSource) -> &'static str {
    match source {
        CteExplainSource::With => "with",
        CteExplainSource::DerivedTable => "derived_table",
    }
}

/// `limit` 段（`limit`・`offset` の有無のみ）。
fn limit_node(has_limit: bool, has_offset: bool) -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage(
        "limit".to_string(),
        vec![
            format!("limit: {}", present(has_limit)),
            format!("offset: {}", present(has_offset)),
        ],
    )
}

/// `sort` 段（キー数のみ）。
fn sort_node(sort_keys: usize) -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage("sort".to_string(), vec![format!("sort_keys: {sort_keys}")])
}

/// `filter` 段（残りの述語数のみ）。
fn filter_node(predicates: usize) -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage(
        "filter".to_string(),
        vec![format!("residual_predicates: {predicates}")],
    )
}

/// `parent` の下に `child` を 1 つ吊るして `parent` を返す。
fn with_child(
    mut parent: ExplainNode,
    child: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    parent.push_child(child)?;
    Ok(parent)
}

/// `scan`（走査分類の行）の上に、本文の `aggregate` → `sort` → `limit` を積む。
/// インライン展開の `cte` 節・具体化 CTE の本文・JOIN 入力 slot の `Scan` が共有する積み方
/// （第 2 の実装を持たない）。
pub(crate) fn main_shape_tree(
    shape: &CteMainShape,
    scan_lines: Vec<String>,
) -> Result<ExplainNode, ExplainTreeError> {
    main_shape_tree_with_subqueries(shape, scan_lines, Vec::new())
}

/// [`main_shape_tree`] に、内側の WHERE の部分クエリ（Issue #2977）を `scan` の子として出現順で足す版。
fn main_shape_tree_with_subqueries(
    shape: &CteMainShape,
    scan_lines: Vec<String>,
    subqueries: Vec<SubqueryStageSpec>,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut current = ExplainNode::try_stage("scan".to_string(), scan_lines)?;
    for spec in subqueries {
        current.push_child(subquery_node(spec)?)?;
    }
    if let Some(agg) = &shape.aggregate {
        current = with_child(aggregate_node(agg)?, current)?;
    }
    if shape.sort_keys > 0 {
        current = with_child(sort_node(shape.sort_keys)?, current)?;
    }
    if shape.has_limit || shape.has_offset {
        current = with_child(limit_node(shape.has_limit, shape.has_offset)?, current)?;
    }
    Ok(current)
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
    let root = ExplainNode::try_stage(
        "cte".to_string(),
        vec![
            format!("cte_source: {}", source_token(source)),
            "cte_mode: inline".to_string(),
        ],
    )?;
    with_child(
        root,
        main_shape_tree_with_subqueries(shape, scan_lines, subqueries)?,
    )
}

/// 具体化された CTE・派生表の結果に重ねる外側の層の形（件数・有無のみ。Issue #2976）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OuterLayerShape {
    pub(crate) has_limit: bool,
    pub(crate) has_offset: bool,
    pub(crate) sort_keys: usize,
    /// 外側が集計のときの形。
    pub(crate) aggregate: Option<AggregateShape>,
    /// 外側 `WHERE` の述語数（0 なら `filter` 段を作らない）。
    pub(crate) filter_predicates: usize,
}

/// 具体化された `cte` 節（`cte_mode: materialized`）の下に本文の木 `body` を吊るす。
pub(crate) fn cte_materialized_node(
    source: CteExplainSource,
    body: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    let root = ExplainNode::try_stage(
        "cte".to_string(),
        vec![
            format!("cte_source: {}", source_token(source)),
            "cte_mode: materialized".to_string(),
        ],
    )?;
    with_child(root, body)
}

/// 具体化された結果（`inner`。`cte` 節）の上に、外側の `filter` → `aggregate` → `sort` → `limit` を
/// 積む。外側の段が 1 つも無ければ `inner` をそのまま返す。外側の層（具体化 CTE の主クエリ）と
/// JOIN 入力の派生 slot の層が共有する。
pub(crate) fn outer_layer_stages(
    outer: &OuterLayerShape,
    inner: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut current = inner;
    if outer.filter_predicates > 0 {
        current = with_child(filter_node(outer.filter_predicates)?, current)?;
    }
    if let Some(agg) = &outer.aggregate {
        current = with_child(aggregate_node(agg)?, current)?;
    }
    if outer.sort_keys > 0 {
        current = with_child(sort_node(outer.sort_keys)?, current)?;
    }
    if outer.has_limit || outer.has_offset {
        current = with_child(limit_node(outer.has_limit, outer.has_offset)?, current)?;
    }
    Ok(current)
}

/// JOIN の入力になる CTE・派生表 slot の `cte` 節。`relation` が `Some` のときは JOIN の側の
/// 位置（1 起点）を先頭に出す。`shared` は文内で評価結果を共有するか（`MATERIALIZED` または
/// 2 回以上の参照）。名前は出さず `cte_source` だけを出す。
pub(crate) fn cte_input_node(
    relation: Option<usize>,
    source: CteExplainSource,
    shared: bool,
    slot_tree: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    let mut lines = Vec::with_capacity(4);
    if let Some(rel) = relation {
        lines.push(format!("relation: {rel}"));
    }
    lines.push(format!("cte_source: {}", source_token(source)));
    lines.push("cte_mode: materialized".to_string());
    lines.push(format!("cte_shared: {shared}"));
    let root = ExplainNode::try_stage("cte".to_string(), lines)?;
    with_child(root, slot_tree)
}

/// 再帰 CTE の `recursive_union` 段の属性（固定トークンになるものだけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecursiveStageSpec {
    pub(crate) union_all: bool,
    /// `SEARCH` 句。`Some(false)` が深さ優先、`Some(true)` が幅優先。
    pub(crate) search: Option<bool>,
    pub(crate) cycle: bool,
}

/// 再帰 CTE の `cte` 節（`cte_mode: recursive`）の下に `recursive_union` を吊るし、その下に
/// アンカー項・再帰項の木を並べる。`search`／`cycle` は有無と方式だけで、列名・値は出さない。
pub(crate) fn recursive_cte_node(
    source: CteExplainSource,
    spec: &RecursiveStageSpec,
    anchor: ExplainNode,
    recursive: ExplainNode,
) -> Result<ExplainNode, ExplainTreeError> {
    let search = match spec.search {
        None => "none",
        Some(false) => "depth_first",
        Some(true) => "breadth_first",
    };
    let mut union = ExplainNode::try_stage(
        "recursive_union".to_string(),
        vec![
            format!("union: {}", if spec.union_all { "all" } else { "distinct" }),
            format!("search: {search}"),
            format!("cycle: {}", present(spec.cycle)),
        ],
    )?;
    union.push_child(anchor)?;
    union.push_child(recursive)?;
    let root = ExplainNode::try_stage(
        "cte".to_string(),
        vec![
            format!("cte_source: {}", source_token(source)),
            "cte_mode: recursive".to_string(),
        ],
    )?;
    with_child(root, union)
}

/// 再帰項の作業表の走査（`work_table_scan`）。`lines` は JOIN の側なら `relation: k`、
/// 単一関係の再帰項なら [`work_table_filter_lines`]。反復回数・累積行数はデータ依存なので出さない。
pub(crate) fn work_table_scan_node(lines: Vec<String>) -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage("work_table_scan".to_string(), lines)
}

/// 単一関係の再帰項の `work_table_scan` 行。
pub(crate) fn work_table_filter_lines(has_filter: bool) -> Vec<String> {
    vec![format!("filter: {}", present(has_filter))]
}

/// `FROM` なし定数式のアンカー項の葉（`result`。`source: constant`）。
pub(crate) fn const_result_leaf() -> Result<ExplainNode, ExplainTreeError> {
    ExplainNode::try_stage("result".to_string(), vec!["source: constant".to_string()])
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

    #[test]
    fn materialized_cte_sits_between_outer_and_body_stages() {
        let body_shape = CteMainShape {
            has_limit: true,
            has_offset: false,
            sort_keys: 0,
            aggregate: None,
        };
        let body = main_shape_tree(&body_shape, lines(&["x: 1"])).unwrap();
        let cte = cte_materialized_node(CteExplainSource::DerivedTable, body).unwrap();
        let outer = OuterLayerShape {
            has_limit: false,
            has_offset: false,
            sort_keys: 1,
            aggregate: None,
            filter_predicates: 2,
        };
        let out = outer_layer_stages(&outer, cte).unwrap().into_lines();
        assert_eq!(
            out,
            lines(&[
                "sort",
                "sort_keys: 1",
                "-> filter",
                "   residual_predicates: 2",
                "  -> cte",
                "     cte_source: derived_table",
                "     cte_mode: materialized",
                "    -> limit",
                "       limit: present",
                "       offset: none",
                "      -> scan",
                "         x: 1",
            ])
        );
    }

    #[test]
    fn no_outer_layers_leaves_the_cte_as_the_root() {
        let outer = OuterLayerShape {
            has_limit: false,
            has_offset: false,
            sort_keys: 0,
            aggregate: None,
            filter_predicates: 0,
        };
        let cte = cte_materialized_node(
            CteExplainSource::With,
            ExplainNode::try_stage("scan".to_string(), lines(&["x: 1"])).unwrap(),
        )
        .unwrap();
        let out = outer_layer_stages(&outer, cte).unwrap().into_lines();
        assert_eq!(out.first().map(String::as_str), Some("cte"));
    }

    #[test]
    fn recursive_cte_hangs_both_terms_under_the_union() {
        let spec = RecursiveStageSpec {
            union_all: false,
            search: Some(true),
            cycle: true,
        };
        let anchor = const_result_leaf().unwrap();
        let rec = work_table_scan_node(work_table_filter_lines(false)).unwrap();
        let out = recursive_cte_node(CteExplainSource::With, &spec, anchor, rec)
            .unwrap()
            .into_lines();
        assert_eq!(
            out,
            lines(&[
                "cte",
                "cte_source: with",
                "cte_mode: recursive",
                "-> recursive_union",
                "   union: distinct",
                "   search: breadth_first",
                "   cycle: present",
                "  -> result",
                "     source: constant",
                "  -> work_table_scan",
                "     filter: none",
            ])
        );
    }

    #[test]
    fn cte_input_node_omits_relation_for_nested_parents() {
        let leaf = ExplainNode::try_stage("scan".to_string(), lines(&["x: 1"])).unwrap();
        let out = cte_input_node(None, CteExplainSource::With, true, leaf)
            .unwrap()
            .into_lines();
        assert_eq!(out[1], "cte_source: with");
        assert!(out.iter().any(|l| l == "cte_shared: true"));
        assert!(!out.iter().any(|l| l.starts_with("relation")));
    }
}
