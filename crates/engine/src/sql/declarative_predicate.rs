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
use crate::sql::udf_call::{self, BinOp, BoundExpr, Expr, ExprType, UdfRegistry};
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
    // トップレベルの同列等価 OR を IN へ畳む（Issue #1306。SQL 側と共通。
    // `bind_one` の分岐再帰では呼ばない＝入れ子は対象外）。
    let or_filters = crate::sql::where_tree::fold_same_column_text_or_groups(
        &mut metadata_filters,
        &expr_filters,
        or_filters,
    );
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
                // `WherePredicate::Expression` 分岐と同じ分類（`42804`。Issue #1186）に揃える
                // （第 2 の評価器を作らない方針。エラー分類も共有する）。
                return Err(SqlSurfaceError::datatype_mismatch(
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

/// 連言（`AND` 列）の否定を、`Not` を `Or`／`AND` 群の上に残さない連言として
/// 返す（Issue #1197・NOSQL-14 の `not` グループ。`sql::where_negation::
/// negate_conjunction` の [`DeclarativePredicate`] 版で、同じ否定表に従う）。
///
/// 呼び出し文脈: `wire-server` の `http::query::filter` が `{"not": ...}` を束縛
/// する際に呼ぶ。二値評価器（`sql::where_tree`）は UNKNOWN を false として扱うため、
/// 群の上へ否定を置くと NULL 行で UNKNOWN が真へ反転し fail-open になる。そこで
/// De Morgan で否定を葉まで押し下げる（分配はしないので展開は線形。`Eq` 式のみ
/// 1 葉が 2 葉になる）。
///
/// | 入力 | 結果 |
/// | ---- | ---- |
/// | 空の列 | `42601`（fail-closed） |
/// | `[p]` | `¬p` |
/// | `[p1..pn]`（n≥2） | `[Or([¬p1], ..., [¬pn])]` |
/// | `Leaf(f)` | 否定を畳み込んだ `Leaf`（`Not`・`IS [NOT] NULL` は反転） |
/// | `Expr(a > b)` 等 | 演算子反転（`>`↔`<=`、`<`↔`>=`） |
/// | `Expr(a = b)` | `Or([a < b], [a > b])` |
/// | 上記以外の `Expr` | `42601` |
/// | `Or(branches)` | 各分岐の否定を連結（`AND`） |
///
/// 上限（葉数・深さ）は後段の [`bind_declarative_predicates`] が再検査する。
pub fn negate_conjunction(
    preds: Vec<DeclarativePredicate>,
) -> Result<Vec<DeclarativePredicate>, SqlSurfaceError> {
    let mut iter = preds.into_iter();
    let Some(first) = iter.next() else {
        return Err(SqlSurfaceError::unsupported(
            "NOT must be followed by a predicate",
        ));
    };
    let Some(second) = iter.next() else {
        return negate_one(first);
    };
    let mut branches = vec![negate_one(first)?, negate_one(second)?];
    for p in iter {
        branches.push(negate_one(p)?);
    }
    Ok(vec![DeclarativePredicate::Or(branches)])
}

/// 構文形 [`crate::sql::allowlist::WherePredicate`] 列（`AND` 結合）の否定を、SQL の
/// `NOT` と同一の AST で返す（`sql::where_negation::negate_conjunction` の公開入口。
/// Issue #1356・NOSQL-12 の述語形 DML）。
///
/// 呼び出し文脈: `wire-server` の `http::query::filter::bind_filter_where_predicates`
/// が NoSQL `update`／`delete` の `{"not": ...}`・数値列 `ne` を構文形へ写す際に呼ぶ。
/// 述語形 DML の `content_hash` は束縛前の構文形をハッシュ源にする（RECOVER-10）ため、
/// SQL パーサと同じ否定の押し下げ結果をバイト単位で再現する必要がある。wire 側で否定を
/// 再実装すると第 2 の評価器になり AST もずれるので、この関数を唯一の入口とする。
/// `budget` は式ノード予算（[`udf_call::MAX_EXPR_NODES`] で初期化し filter 全体で共有する。
/// 枯渇は `54000`）。
pub fn negate_where_conjunction(
    preds: Vec<crate::sql::allowlist::WherePredicate>,
    budget: &mut usize,
) -> Result<Vec<crate::sql::allowlist::WherePredicate>, SqlSurfaceError> {
    crate::sql::where_negation::negate_conjunction(preds, budget)
}

/// [`negate_conjunction`] の 1 要素分（否定表の各行）。
fn negate_one(pred: DeclarativePredicate) -> Result<Vec<DeclarativePredicate>, SqlSurfaceError> {
    match pred {
        DeclarativePredicate::Or(branches) => {
            let mut out = Vec::new();
            for branch in branches {
                out.extend(negate_conjunction(branch)?);
            }
            Ok(out)
        }
        DeclarativePredicate::Leaf(filter) => {
            Ok(vec![DeclarativePredicate::Leaf(filter.negate_folded())])
        }
        DeclarativePredicate::Expr(Expr::Binary { op, lhs, rhs }) => match op {
            BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => {
                let flipped = match op {
                    BinOp::Gt => BinOp::Le,
                    BinOp::Lt => BinOp::Ge,
                    BinOp::Ge => BinOp::Lt,
                    _ => BinOp::Gt,
                };
                Ok(vec![DeclarativePredicate::Expr(Expr::Binary {
                    op: flipped,
                    lhs,
                    rhs,
                })])
            }
            BinOp::Eq => {
                let lt = DeclarativePredicate::Expr(Expr::Binary {
                    op: BinOp::Lt,
                    lhs: lhs.clone(),
                    rhs: rhs.clone(),
                });
                let gt = DeclarativePredicate::Expr(Expr::Binary {
                    op: BinOp::Gt,
                    lhs,
                    rhs,
                });
                Ok(vec![DeclarativePredicate::Or(vec![vec![lt], vec![gt]])])
            }
            _ => Err(SqlSurfaceError::unsupported(
                "NOT must be followed by a comparison",
            )),
        },
        DeclarativePredicate::Expr(_) => Err(SqlSurfaceError::unsupported(
            "NOT must be followed by a comparison",
        )),
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

    fn eq_branch(col: &str, v: &str) -> Vec<DeclarativePredicate> {
        vec![DeclarativePredicate::Leaf(DeclarativeFilter::equals(
            col, v,
        ))]
    }

    #[test]
    fn folds_same_column_eq_or_into_in_text() {
        let preds = vec![DeclarativePredicate::Or(vec![
            eq_branch("lang", "ja"),
            eq_branch("lang", "en"),
            eq_branch("lang", "fr"),
        ])];
        let bound =
            bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default()).unwrap();
        assert_eq!(bound.metadata_filters().len(), 1);
        assert!(bound.or_filters().is_empty());
    }

    #[test]
    fn does_not_fold_nested_or() {
        let preds = vec![DeclarativePredicate::Or(vec![
            eq_branch("lang", "ja"),
            vec![DeclarativePredicate::Or(vec![
                eq_branch("lang", "en"),
                eq_branch("lang", "fr"),
            ])],
        ])];
        let bound =
            bind_declarative_predicates(&preds, &schema(), &UdfRegistry::default()).unwrap();
        assert!(bound.metadata_filters().is_empty());
        assert_eq!(bound.or_filters().len(), 1);
    }

    #[test]
    fn does_not_fold_when_top_level_expr_present() {
        let preds = vec![
            DeclarativePredicate::Or(vec![eq_branch("lang", "ja"), eq_branch("lang", "en")]),
            DeclarativePredicate::Expr(Expr::Binary {
                op: udf_call::BinOp::Eq,
                lhs: Box::new(Expr::Ident("lang".to_string())),
                rhs: Box::new(Expr::String("ja".to_string())),
            }),
        ];
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
        assert_eq!(err.wire_code(), "42804");
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

    fn eq_leaf(col: &str, v: &str) -> DeclarativePredicate {
        DeclarativePredicate::Leaf(DeclarativeFilter::equals(col, v))
    }

    fn cmp_expr(op: udf_call::BinOp, col: &str, n: &str) -> DeclarativePredicate {
        DeclarativePredicate::Expr(Expr::Binary {
            op,
            lhs: Box::new(Expr::Ident(col.to_string())),
            rhs: Box::new(Expr::Number(n.to_string())),
        })
    }

    #[test]
    fn negate_leaf_wraps_and_double_negation_folds() {
        let once = negate_conjunction(vec![eq_leaf("lang", "ja")]).unwrap();
        assert_eq!(
            once,
            vec![DeclarativePredicate::Leaf(
                DeclarativeFilter::equals("lang", "ja").negate()
            )]
        );
        let twice = negate_conjunction(once).unwrap();
        assert_eq!(twice, vec![eq_leaf("lang", "ja")]);
    }

    #[test]
    fn negate_is_null_flips_to_is_not_null() {
        let out = negate_conjunction(vec![DeclarativePredicate::Leaf(
            DeclarativeFilter::is_null("count"),
        )])
        .unwrap();
        assert_eq!(
            out,
            vec![DeclarativePredicate::Leaf(DeclarativeFilter::is_not_null(
                "count"
            ))]
        );
    }

    #[test]
    fn negate_expr_flips_ordering_and_splits_eq() {
        use udf_call::BinOp;
        assert_eq!(
            negate_conjunction(vec![cmp_expr(BinOp::Gt, "count", "1")]).unwrap(),
            vec![cmp_expr(BinOp::Le, "count", "1")]
        );
        assert_eq!(
            negate_conjunction(vec![cmp_expr(BinOp::Ge, "count", "1")]).unwrap(),
            vec![cmp_expr(BinOp::Lt, "count", "1")]
        );
        assert_eq!(
            negate_conjunction(vec![cmp_expr(BinOp::Eq, "count", "1")]).unwrap(),
            vec![DeclarativePredicate::Or(vec![
                vec![cmp_expr(BinOp::Lt, "count", "1")],
                vec![cmp_expr(BinOp::Gt, "count", "1")],
            ])]
        );
    }

    #[test]
    fn negate_conjunction_applies_de_morgan() {
        let a = eq_leaf("lang", "ja");
        let b = eq_leaf("lang", "en");
        let not = |p: &DeclarativePredicate| match p {
            DeclarativePredicate::Leaf(f) => DeclarativePredicate::Leaf(f.clone().negate()),
            _ => unreachable!(),
        };
        // NOT (a AND b) = Or([NOT a], [NOT b])
        assert_eq!(
            negate_conjunction(vec![a.clone(), b.clone()]).unwrap(),
            vec![DeclarativePredicate::Or(vec![vec![not(&a)], vec![not(&b)]])]
        );
        // NOT (a OR b) = [NOT a, NOT b]
        assert_eq!(
            negate_conjunction(vec![DeclarativePredicate::Or(vec![
                vec![a.clone()],
                vec![b.clone()]
            ])])
            .unwrap(),
            vec![not(&a), not(&b)]
        );
    }

    #[test]
    fn negate_rejects_empty_and_non_comparison_expr() {
        let err = negate_conjunction(vec![]).expect_err("empty must be rejected");
        assert_eq!(err.wire_code(), "42601");
        let err = negate_conjunction(vec![DeclarativePredicate::Expr(Expr::Number(
            "1".to_string(),
        ))])
        .expect_err("non-comparison expr must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }
}
