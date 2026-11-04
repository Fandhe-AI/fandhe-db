//! `EXPLAIN` の段の木（Issue #2045・SQL-75 (2) ポインタ）。
//!
//! `sql::explain` の各 builder（単一表の検索・集計・広域取得）は、これまで「平らな
//! 行の並び」を直接組み立てていた。結合・集合演算・CTE・部分クエリの `EXPLAIN`
//! （親 #2044 配下の #2046〜#2048）は段を入れ子にして深さを字下げで示す必要があるため、
//! 親子関係を持つ節 [`ExplainNode`] と、字下げ付きの行への描画をここに置く。
//! 単一表の経路は「見出しなし・子なしのルート 1 節」として載せ替え、出力は
//! バイト単位で不変（後方互換）。
//!
//! 責務境界: 本モジュールは描画だけを担い新しい情報源を持たない。節に載せる行は
//! 呼び出し元が SQL-27 の契約（閉じた語彙のトークンと検証済みの索引名のみ。行数・
//! 可視件数・カーディナリティ等のテナント存在情報は載せない。SQL-75 (5)）に従って
//! 作る。`EXPLAIN` が検索本体を実行しない性質も変わらない。NoSQL 表層は対象外。
//!
//! 字下げの規則（安定契約。一度出したら変えない）:
//! - 深さ 0: 見出しがあれば `{stage}`、属性行は前置なし。
//! - 深さ d >= 1: 見出し行は `"  " x (d-1) + "-> " + stage`、属性行は
//!   `"  " x (d-1) + "   " + line`。見出しなしの子は見出し行を出さず属性行のみ字下げする。
//!
//! DoS 対策として深さ・節の数・行の数に上限を置き、超えたら出力を途中で切らず
//! [`ExplainTreeError`] で fail-closed にする。描画は再帰を使わず明示スタックで行う。

use crate::sql::exec::QueryResult;

/// 木の深さの上限（根 = 0）。構文段の入れ子上限（表参照 8・部分クエリ 4・CTE 4・
/// 集合演算の括弧 4・ビュー 4 の合計 24）に、段ごとの付帯の段（並べ替え・件数制限・
/// 窓 等）の余裕を見込んだ値。
pub(crate) const MAX_EXPLAIN_TREE_DEPTH: usize = 64;
/// 木の節の数の上限（無制限確保の防止）。
pub(crate) const MAX_EXPLAIN_TREE_NODES: usize = 1024;
/// 木が描画する行の数の上限（無制限確保の防止）。
pub(crate) const MAX_EXPLAIN_TREE_LINES: usize = 8192;

const _: () = assert!(MAX_EXPLAIN_TREE_DEPTH >= 8 + 4 + 4 + 4 + 4);

/// 有無を表す固定トークン（`present`／`none`）。`limit`・`offset`・`having`・`join_filter` 等の
/// 段の属性行が値（リテラル・件数）を出さずに有無だけを示すために、JOIN（#2046）と集合演算
/// （#2047）で共有する（SQL-75 (5)）。
pub(crate) fn present(b: bool) -> &'static str {
    if b {
        "present"
    } else {
        "none"
    }
}

/// 木の構築が上限・形式に反したときのエラー。メッセージは固定文で入力の値を含めない。
/// `wire_code` への写像は呼び出し元（#2046〜#2048）が決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplainTreeError {
    /// 深さが [`MAX_EXPLAIN_TREE_DEPTH`] を超える。
    TooDeep,
    /// 節の数が [`MAX_EXPLAIN_TREE_NODES`] を超える。
    TooManyNodes,
    /// 行の数が [`MAX_EXPLAIN_TREE_LINES`] を超える。
    TooManyLines,
    /// 見出し・属性行に改行が含まれる（字下げ構造を壊すため拒否）。
    InvalidLine,
}

impl ExplainTreeError {
    /// 固定の英語メッセージ（入力の値は含めない）。
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::TooDeep => "EXPLAIN plan is nested too deeply",
            Self::TooManyNodes => "EXPLAIN plan has too many stages",
            Self::TooManyLines => "EXPLAIN plan has too many lines",
            Self::InvalidLine => "EXPLAIN plan line contains a line break",
        }
    }
}

/// `EXPLAIN` の段の木の 1 節。フィールドは非公開で、組み立てはコンストラクタと
/// [`ExplainNode::push_child`] だけで行うため、上限内の木であることが型で保証される。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplainNode {
    stage: Option<String>,
    lines: Vec<String>,
    children: Vec<ExplainNode>,
    height: usize,
    node_count: usize,
    line_count: usize,
}

fn has_break(s: &str) -> bool {
    s.contains(['\n', '\r'])
}

impl ExplainNode {
    /// 見出しなしの葉。`sql::explain` の単一表の builder が使う。行は呼び出し元が作る
    /// 固定語彙（最大でも十数行）で改行を含まないことを前提とした内部用コンストラクタ。
    /// 外部由来の文字列を載せる場合は [`ExplainNode::try_stage`] を使う。
    pub(crate) fn leaf(lines: Vec<String>) -> Self {
        let line_count = lines.len();
        Self {
            stage: None,
            lines,
            children: Vec::new(),
            height: 0,
            node_count: 1,
            line_count,
        }
    }

    /// 見出し付きの節（兄弟 Issue が結合・集合演算・CTE の段を足すときに使う）。
    /// 改行を含む入力と行数の超過を拒否する。
    pub(crate) fn try_stage(stage: String, lines: Vec<String>) -> Result<Self, ExplainTreeError> {
        if has_break(&stage) || lines.iter().any(|l| has_break(l)) {
            return Err(ExplainTreeError::InvalidLine);
        }
        let line_count = lines
            .len()
            .checked_add(1)
            .filter(|n| *n <= MAX_EXPLAIN_TREE_LINES)
            .ok_or(ExplainTreeError::TooManyLines)?;
        Ok(Self {
            stage: Some(stage),
            lines,
            children: Vec::new(),
            height: 0,
            node_count: 1,
            line_count,
        })
    }

    /// 子を末尾へ足す。上限を超えるときは木を変えずに `Err` を返す。
    pub(crate) fn push_child(&mut self, child: ExplainNode) -> Result<(), ExplainTreeError> {
        let height = child
            .height
            .checked_add(1)
            .filter(|h| *h <= MAX_EXPLAIN_TREE_DEPTH)
            .ok_or(ExplainTreeError::TooDeep)?;
        let node_count = self
            .node_count
            .checked_add(child.node_count)
            .filter(|n| *n <= MAX_EXPLAIN_TREE_NODES)
            .ok_or(ExplainTreeError::TooManyNodes)?;
        let line_count = self
            .line_count
            .checked_add(child.line_count)
            .filter(|n| *n <= MAX_EXPLAIN_TREE_LINES)
            .ok_or(ExplainTreeError::TooManyLines)?;
        self.height = self.height.max(height);
        self.node_count = node_count;
        self.line_count = line_count;
        self.children.push(child);
        Ok(())
    }

    /// 前順（子は出現順）に字下げ付きの行へ描画する。再帰は使わない。
    pub(crate) fn into_lines(self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.line_count);
        let mut stack = vec![(self, 0usize)];
        while let Some((node, depth)) = stack.pop() {
            let ExplainNode {
                stage,
                lines,
                children,
                ..
            } = node;
            let pad = "  ".repeat(depth.saturating_sub(1));
            if depth == 0 {
                if let Some(stage) = stage {
                    out.push(stage);
                }
                out.extend(lines);
            } else {
                if let Some(stage) = stage {
                    out.push(format!("{pad}-> {stage}"));
                }
                out.extend(lines.into_iter().map(|l| format!("{pad}   {l}")));
            }
            for child in children.into_iter().rev() {
                stack.push((child, depth.saturating_add(1)));
            }
        }
        out
    }

    /// 描画して `QUERY PLAN` 単一列の [`QueryResult`] にする。
    pub(crate) fn into_query_result(self) -> QueryResult {
        super::explain::lines_to_query_result(self.into_lines())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn stage(name: &str, lines: &[&str]) -> ExplainNode {
        ExplainNode::try_stage(s(name), lines.iter().map(|l| s(l)).collect()).unwrap()
    }

    #[test]
    fn leaf_renders_lines_verbatim() {
        let lines = vec![s("a: 1"), s("b: 2")];
        assert_eq!(ExplainNode::leaf(lines.clone()).into_lines(), lines);
    }

    #[test]
    fn nested_rendering_indents_by_depth() {
        let mut root = stage("root", &["k: v"]);
        let mut a = stage("a", &["x: 1"]);
        a.push_child(stage("a1", &["y: 2"])).unwrap();
        root.push_child(a).unwrap();
        root.push_child(stage("b", &["z: 3"])).unwrap();
        root.push_child(ExplainNode::leaf(vec![s("bare")])).unwrap();
        let expected = [
            "root",
            "k: v",
            "-> a",
            "   x: 1",
            "  -> a1",
            "     y: 2",
            "-> b",
            "   z: 3",
            "   bare",
        ];
        assert_eq!(root.clone().into_lines(), expected);
        assert_eq!(root.clone().into_lines(), root.into_lines());
    }

    fn chain(depth: usize) -> ExplainNode {
        let mut node = stage("n", &[]);
        for _ in 0..depth {
            let mut parent = stage("n", &[]);
            parent.push_child(node).unwrap();
            node = parent;
        }
        node
    }

    #[test]
    fn depth_limit_boundary() {
        let at_limit = chain(MAX_EXPLAIN_TREE_DEPTH);
        let mut over = stage("n", &[]);
        assert_eq!(
            over.push_child(at_limit.clone()),
            Err(ExplainTreeError::TooDeep)
        );
        assert!(over.children.is_empty());
        assert_eq!(over.node_count, 1);
        // 深い直線の木でも明示スタックで描画できる。
        assert_eq!(at_limit.into_lines().len(), MAX_EXPLAIN_TREE_DEPTH + 1);
    }

    #[test]
    fn node_limit_boundary() {
        let mut root = stage("r", &[]);
        for _ in 1..MAX_EXPLAIN_TREE_NODES {
            root.push_child(stage("c", &[])).unwrap();
        }
        let before = root.clone();
        assert_eq!(
            root.push_child(stage("c", &[])),
            Err(ExplainTreeError::TooManyNodes)
        );
        assert_eq!(root, before);
    }

    #[test]
    fn line_limit_boundary() {
        let big = |n: usize| ExplainNode::try_stage(s("c"), vec![s("l"); n]).unwrap();
        let mut root = stage("r", &[]);
        root.push_child(big(MAX_EXPLAIN_TREE_LINES - 2)).unwrap();
        assert_eq!(root.line_count, MAX_EXPLAIN_TREE_LINES);
        let before = root.clone();
        assert_eq!(root.push_child(big(0)), Err(ExplainTreeError::TooManyLines));
        assert_eq!(root, before);
        assert_eq!(
            ExplainNode::try_stage(s("r"), vec![s("l"); MAX_EXPLAIN_TREE_LINES]),
            Err(ExplainTreeError::TooManyLines)
        );
    }

    #[test]
    fn line_breaks_are_rejected() {
        assert_eq!(
            ExplainNode::try_stage(s("a\nb"), vec![]),
            Err(ExplainTreeError::InvalidLine)
        );
        assert_eq!(
            ExplainNode::try_stage(s("a"), vec![s("x\ry")]),
            Err(ExplainTreeError::InvalidLine)
        );
        assert!(!ExplainTreeError::InvalidLine.message().is_empty());
    }
}
