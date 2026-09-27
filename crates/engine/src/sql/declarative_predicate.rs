//! NoSQL `filter` 語彙拡張（範囲比較・`IN`・`OR`。Issue #945・対象ビヘイビア:
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-14）の束縛入口。
//!
//! 責務境界: SQL 表層の `WHERE` 述語束縛（`sql::parser::bind_where_predicates`・
//! `sql::where_tree`）と**同一の**束縛結果型（`declarative_filter::MetadataFilter`・
//! `udf_call::BoundExpr`・`where_tree::BoundOrGroup`）を、SQL テキストを一切
//! 経由せず、本モジュールが定義する小さな AST（[`DeclarativePredicate`]）から
//! 直接得る。呼び出し文脈: `wire-server` の `http::query::filter`（NoSQL
//! `filter` 配列の JSON 束縛）が唯一の呼び出し元。第 2 の実行器・第 2 の評価器を
//! 作らないという CLAUDE.md「委譲方針」に従い、束縛後の実行経路
//! （`sql::exec`／`sql::scan`／`sql::aggregate`）は SQL 表層と完全に共有する。

use crate::catalog::TableSchema;
use crate::declarative_filter::{self, DeclarativeFilter, MetadataFilter};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{self, BoundExpr, Expr, ExprType, UdfRegistry};
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};

/// クレート外（`wire-server`）が組み立てる、束縛前の述語ツリー 1 要素。
/// `&[DeclarativePredicate]`（本 enum を含むスライス）は常に `AND` 結合を表す
/// （SQL の `WHERE a AND b AND c` と同じ）。
#[derive(Debug, Clone, PartialEq)]
pub enum DeclarativePredicate {
    /// 宣言的（メタデータ）述語 1 件（等価・前方一致・範囲比較・`IN` 等。
    /// `declarative_filter::DeclarativeFilter` のいずれの構築子で作った値も
    /// 渡せる）。
    Leaf(DeclarativeFilter),
    /// 式述語 1 件（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への
    /// 等価・範囲比較。式レーン）。束縛結果は [`ExprType::Bool`] でなければ
    /// ならない（SQL 表層の `WHERE` 式述語と同じ契約。
    /// `sql::parser::bind_where_predicates` 参照）。
    Expr(Expr),
    /// `OR` で結ぶ分岐の集合（2 個以上。1 個の場合は呼び出し元が親の `AND` 列へ
    /// 平坦化してから渡す契約——`sql::allowlist::Parser::parse_where_or` と
    /// 同じ判断）。各分岐はそれ自体が `AND` で結ぶ [`DeclarativePredicate`] 列。
    Or(Vec<Vec<DeclarativePredicate>>),
}

/// [`bind_declarative_predicates`] の戻り値。SQL 表層の `BoundStatement`／
/// `BoundScan`／`BoundAggregate` が持つ 3 種の `WHERE` 表現（メタデータフィルタ・
/// 式述語・`OR` 群）を束ねたもの。フィールドは非公開で、呼び出し元は
/// [`Self::into_parts`] または個別アクセサ経由で読み取る。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundWhereFilters {
    metadata_filters: Vec<MetadataFilter>,
    expr_filters: Vec<BoundExpr>,
    or_filters: Vec<BoundOrGroup>,
}

impl BoundWhereFilters {
    /// SCALAR 段で適用するメタデータフィルタ一覧。
    pub fn metadata_filters(&self) -> &[MetadataFilter] {
        &self.metadata_filters
    }

    /// 式述語（`INTEGER`／`BIGINT`／`REAL`／`DOUBLE PRECISION` 列への範囲比較等）。
    pub fn expr_filters(&self) -> &[BoundExpr] {
        &self.expr_filters
    }

    /// `OR` 群。`crate::sql::where_tree` が `pub(crate) mod` のため、この
    /// スライスの要素型はクレート外から名指しできない（`BoundStatement::
    /// or_filters` と同じ、外部からは不透明な値として受け渡すだけの契約）。
    pub fn or_filters(&self) -> &[BoundOrGroup] {
        &self.or_filters
    }

    /// `metadata_filters`・`expr_filters`・`or_filters` の 3 つ組へ分解する
    /// （`BoundScan::new`／`BoundAggregate::new` 等、SQL 表層と同型の
    /// constructor へそのまま渡すための形）。
    pub fn into_parts(self) -> (Vec<MetadataFilter>, Vec<BoundExpr>, Vec<BoundOrGroup>) {
        (self.metadata_filters, self.expr_filters, self.or_filters)
    }
}

/// 葉（[`DeclarativePredicate::Leaf`]／[`DeclarativePredicate::Expr`]）の
/// ツリー全体での総数上限。[`declarative_filter::MAX_METADATA_FILTERS`]
/// （SQL 表層 `sql::allowlist::MAX_WHERE_LEAVES` と同値）をそのまま採用し、
/// 二重定義しない。
const MAX_LEAVES: usize = declarative_filter::MAX_METADATA_FILTERS;

/// `Or` のネスト深さ上限。SQL 表層 `sql::allowlist::MAX_WHERE_GROUP_DEPTH`
/// （`udf_call::MAX_EXPR_DEPTH` と同値）をそのまま採用する。
const MAX_OR_DEPTH: usize = udf_call::MAX_EXPR_DEPTH;

/// `preds`（1 つの `AND` 列。ネストした `Or` を含みうる）の葉総数・深さが
/// 上限を超えないか、`Vec` 確保・束縛より**前**に検査する（多層防御。
/// `wire-server::http::query::filter` が JSON 走査時点で同じ検査を行うのが
/// 一次防御で、本関数は engine 側の Rust API 直接呼び出し経路も含めて
/// fail-closed に保つための二次防御）。
fn check_predicate_limits(
    preds: &[DeclarativePredicate],
    depth: usize,
    leaves: &mut usize,
) -> Result<(), SqlSurfaceError> {
    if depth > MAX_OR_DEPTH {
        return Err(SqlSurfaceError::payload_too_large(
            "filter nesting exceeds the allowed depth",
        ));
    }
    for pred in preds {
        match pred {
            DeclarativePredicate::Leaf(_) | DeclarativePredicate::Expr(_) => {
                let next = leaves.checked_add(1).ok_or_else(|| {
                    SqlSurfaceError::payload_too_large(
                        "filter leaf count exceeds the allowed limit",
                    )
                })?;
                if next > MAX_LEAVES {
                    return Err(SqlSurfaceError::payload_too_large(
                        "filter leaf count exceeds the allowed limit",
                    ));
                }
                *leaves = next;
            }
            DeclarativePredicate::Or(branches) => {
                // `wire-server::http::query::filter::map_element` は JSON 走査時点で
                // 分岐数（`declarative_filter::check_filter_count`）・空配列
                // （`branches_json.is_empty()`）を既に拒否しているが、engine の
                // 公開 API を直接呼ぶ経路（本関数のドキュメント冒頭参照）はその
                // 一次防御を経ない。分岐数を `Vec::with_capacity`（`bind_one`）
                // より前に上限検査しないと確保量が無制限になり、空分岐を拒否
                // しないと空の `AND` 列（`BoundConjunction`）が恒真として評価され
                // （`sql::where_tree::BoundConjunction::matches` は空の
                // `metadata_filters`／`expr_filters`／`or_groups` を全て
                // 素通りさせ `Ok(true)` を返す契約）`OR` 全体を無条件で真にできて
                // しまうため、ここで二重に検査する。
                if branches.len() < 2 {
                    return Err(SqlSurfaceError::invalid_input(
                        "OR group must have at least 2 branches",
                    ));
                }
                if branches.len() > MAX_LEAVES {
                    return Err(SqlSurfaceError::payload_too_large(
                        "OR branch count exceeds the allowed limit",
                    ));
                }
                for branch in branches {
                    if branch.is_empty() {
                        return Err(SqlSurfaceError::invalid_input(
                            "OR branch must not be empty",
                        ));
                    }
                    check_predicate_limits(branch, depth + 1, leaves)?;
                }
            }
        }
    }
    Ok(())
}

/// [`DeclarativePredicate`] 列（1 つの `AND` 列）を `schema`・UDF レジストリ
/// `udfs` と照合して [`BoundWhereFilters`] へ束縛する（Issue #945・NOSQL-14 の
/// 公開 API）。葉は [`DeclarativeFilter::bind`]、式は [`udf_call::bind_expr`]、
/// `OR` 群は [`BoundOrGroup::new`]／[`BoundConjunction::new`] で組み立てる
/// （いずれも SQL 表層 `sql::parser::bind_where_predicates` と共有する部品。
/// 第 2 の評価器を作らない）。
pub fn bind_declarative_predicates(
    preds: &[DeclarativePredicate],
    schema: &TableSchema,
    udfs: &UdfRegistry,
) -> Result<BoundWhereFilters, SqlSurfaceError> {
    let mut leaves = 0usize;
    check_predicate_limits(preds, 0, &mut leaves)?;

    let mut node_budget = udf_call::MAX_EXPR_NODES;
    let mut metadata_filters = Vec::new();
    let mut expr_filters = Vec::new();
    let mut or_filters = Vec::new();
    for pred in preds {
        bind_one(
            pred,
            schema,
            udfs,
            &mut node_budget,
            &mut metadata_filters,
            &mut expr_filters,
            &mut or_filters,
        )?;
    }
    Ok(BoundWhereFilters {
        metadata_filters,
        expr_filters,
        or_filters,
    })
}

/// [`bind_declarative_predicates`]／`Or` 分岐の束縛が共有する 1 要素分の束縛
/// 本体。`metadata_filters`／`expr_filters`／`or_filters` はそれぞれ AND 列の
/// 現在位置へ追記する（呼び出し元が `Or` 分岐用のローカル `Vec` を渡すか、
/// トップレベルの `Vec` を渡すかで、AND 列のどの階層へ束縛するかが決まる）。
fn bind_one(
    pred: &DeclarativePredicate,
    schema: &TableSchema,
    udfs: &UdfRegistry,
    node_budget: &mut usize,
    metadata_filters: &mut Vec<MetadataFilter>,
    expr_filters: &mut Vec<BoundExpr>,
    or_filters: &mut Vec<BoundOrGroup>,
) -> Result<(), SqlSurfaceError> {
    match pred {
        DeclarativePredicate::Leaf(filter) => {
            metadata_filters.push(filter.bind(schema)?);
            Ok(())
        }
        DeclarativePredicate::Expr(expr) => {
            let (bound, ty) = udf_call::bind_expr(expr, schema, udfs, node_budget)?;
            if ty != ExprType::Bool {
                // SQL 表層 `sql::parser::bind_where_predicates` の
                // `WherePredicate::Expression` 分岐と同じ分類（`22000`）に揃える
                // （第 2 の評価器を作らない方針。エラー分類も共有する）。
                return Err(SqlSurfaceError::invalid_input(
                    "filter expression must evaluate to a boolean (use a comparison)",
                ));
            }
            expr_filters.push(bound);
            Ok(())
        }
        DeclarativePredicate::Or(branches) => {
            let mut bound_branches = Vec::with_capacity(branches.len());
            for branch in branches {
                let mut b_metadata = Vec::new();
                let mut b_expr = Vec::new();
                let mut b_or = Vec::new();
                for item in branch {
                    bind_one(
                        item,
                        schema,
                        udfs,
                        node_budget,
                        &mut b_metadata,
                        &mut b_expr,
                        &mut b_or,
                    )?;
                }
                bound_branches.push(BoundConjunction::new(b_metadata, b_expr, b_or));
            }
            or_filters.push(BoundOrGroup::new(bound_branches));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType};
    use crate::declarative_filter::CompareOp;

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("count", ColumnType::Integer, true),
                ColumnDef::new("created", ColumnType::Date, true),
            ],
        )
    }

    #[test]
    fn binds_leaf_metadata_filter() {
        let preds = vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
            "lang", "ja",
        ))];
        let bound =
            bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default()).unwrap();
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.expr_filters().is_empty());
        assert!(bound.or_filters().is_empty());
    }

    #[test]
    fn binds_bool_expr_predicate() {
        // `udf_call::bind_expr` は現時点で `INTEGER`／`BIGINT`／`REAL`／
        // `DOUBLE PRECISION` 列の式内参照を受理しない（別 Issue #891 の担当。
        // `crate::sql::udf_call` の `Ident` 解決分岐を参照）。本モジュールが
        // 汎用に式述語を束縛できることは、現時点で式レーンが受理する `TEXT`
        // 列同士の比較で確認する（Issue #945・NOSQL-14 の wire-server 側は
        // この制約を踏まえ、数値列の `eq`／範囲比較を式レーンへ渡さない）。
        let preds = vec![DeclarativePredicate::Expr(Expr::Binary {
            op: udf_call::BinOp::Eq,
            lhs: Box::new(Expr::Ident("lang".to_string())),
            rhs: Box::new(Expr::String("ja".to_string())),
        })];
        let bound =
            bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default()).unwrap();
        assert!(bound.metadata_filters().is_empty());
        assert_eq!(bound.expr_filters().len(), 1);
    }

    #[test]
    fn binds_or_group_with_two_branches() {
        let preds = vec![DeclarativePredicate::Or(vec![
            vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
                "lang", "ja",
            ))],
            vec![DeclarativePredicate::Leaf(DeclarativeFilter::compare(
                "created",
                CompareOp::Gt,
                "2024-01-01",
            ))],
        ])];
        let bound =
            bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default()).unwrap();
        assert!(bound.metadata_filters().is_empty());
        assert_eq!(bound.or_filters().len(), 1);
    }

    #[test]
    fn rejects_non_boolean_expr() {
        let preds = vec![DeclarativePredicate::Expr(Expr::Number("1".to_string()))];
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_leaf_count_over_limit() {
        let preds: Vec<DeclarativePredicate> = (0..=MAX_LEAVES)
            .map(|i| DeclarativePredicate::Leaf(DeclarativeFilter::equals("lang", i.to_string())))
            .collect();
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_or_with_empty_branch() {
        // PR #1118 codex-review P1 指摘: 空分岐（`AND` 列が 0 要素）を束縛すると
        // `BoundConjunction::matches`（`sql::where_tree`）が恒真（`Ok(true)`）を
        // 返すため、`OR` 群全体が他の分岐・条件に関わらず無条件で真になって
        // しまう。束縛前に空分岐を拒否することを確認する。
        let preds = vec![DeclarativePredicate::Or(vec![
            vec![],
            vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
                "lang", "ja",
            ))],
        ])];
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject empty OR branch");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_or_with_single_branch() {
        let preds = vec![DeclarativePredicate::Or(vec![vec![
            DeclarativePredicate::Leaf(DeclarativeFilter::equals("lang", "ja")),
        ]])];
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject single-branch OR");
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn rejects_or_branch_count_over_limit() {
        // PR #1118 codex-review P1 指摘: 分岐数を `Vec::with_capacity`
        // （`bind_one`）より前に検査しないと、大量の（空でない）分岐を渡す
        // 呼び出しで確保量が無制限になり得る。
        let branch = vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
            "lang", "ja",
        ))];
        let branches: Vec<Vec<DeclarativePredicate>> =
            (0..=MAX_LEAVES).map(|_| branch.clone()).collect();
        let preds = vec![DeclarativePredicate::Or(branches)];
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject OR branch count over limit");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn rejects_depth_over_limit() {
        let mut preds = vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
            "lang", "ja",
        ))];
        for _ in 0..=MAX_OR_DEPTH {
            preds = vec![DeclarativePredicate::Or(vec![
                preds.clone(),
                vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
                    "lang", "en",
                ))],
            ])];
        }
        let err = bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default())
            .expect_err("must reject");
        assert_eq!(err.wire_code(), "54000");
    }
}
