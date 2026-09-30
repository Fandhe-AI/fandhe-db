//! `sql::exec` の SCALAR 事前フィルタ述語が `sql::scalar_index::ScalarIndex`
//! （Issue #473）を消費できる形状かどうかの**静的判定**（Issue #474。対応 ADR:
//! `docs/design/scalar-secondary-index.md`「採用案（候補 B）の確定仕様」節）。
//!
//! `sql::hnsw_cache::classify_ann_plan`（Issue #411）と同型の純粋関数
//! （[`classify_scalar_plan`]）として設計する: `sql::exec`（実行時の索引消費・
//! 縮退判定）と `sql::explain`（`scalar_plan:` 行の静的表示）が同じ入力形状
//! （[`ScalarShapeInput`]）から同じ分類を導出することで、両者の判定基準が
//! 実装として発散しない単一情報源にする。
//!
//! **索引対応述語の狭い定義**（ADR「索引対応述語の狭い定義」節）: `sql::exec`
//! の SCALAR 段は `bound.metadata_filters`（`TEXT` 列の等価・前方一致。
//! `declarative_filter::bind_all` が構築）と `bound.expr_filters`（`WHERE` の
//! 式述語。`sql::udf_call::BoundExpr`）を宣言順に短絡評価する。索引が扱えるのは
//! 前者すべてと、後者のうち **`id <op> <数値リテラル>`**（`op` は
//! `>`/`<`/`>=`/`<=`/`=`、左右いずれの位置でも可）という狭い形だけである。
//! `expr_filters` に 1 つでもこの形に一致しない要素（`Builtin`・`WasmCall`・
//! `VectorRef` を含む式、ネストした演算等）があれば、それは評価時にエラーに
//! なりうる残余述語であり、宣言順の短絡評価という既存の fail-closed 契約
//! （`sql::exec::execute_statement_with_cache` の `on_visible_row` 参照）を
//! 崩さないよう索引経路を一切使わない（[`ScalarPlan::PlainScan`] へ縮退）。
//! `WHERE visible()`（[`crate::sql::parser::WherePredicate::PredicateCall`]）は
//! `rls_predicate_present` フラグのみを立て `metadata_filters`/`expr_filters`
//! を増やさないため、この判定には現れない（残余述語にはならない）。

use crate::declarative_filter::MetadataFilter;
use crate::sql::udf_call::{BinOp, BoundExpr};
use crate::sql::where_tree::BoundOrGroup;

/// [`classify_scalar_plan`] の分類結果。閉じた語彙（`sql::explain` の
/// `scalar_plan:` 行の値と 1 対 1）。
///
/// TASK-186・NOSQL-10 の前提として Issue #730 で公開 API へ昇格した
/// （`sql::hnsw_cache::AnnPlan` と同じ理由。`#[non_exhaustive]` により
/// クレート外からの網羅 `match` を禁止する。クレート内の網羅 `match`
/// （`sql::explain::scalar_plan_token` 等）は影響を受けない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalarPlan {
    /// 索引を使わず全可視行を走査する（既定）。
    PlainScan,
    /// 索引対応述語がちょうど 1 件で、`TEXT` 列の等価条件。
    IndexEquality,
    /// 索引対応述語がちょうど 1 件で、`TEXT` 列の前方一致条件。
    IndexPrefix,
    /// 索引対応述語がちょうど 1 件で、`id` に対する単純比較。
    IndexIdRange,
    /// 索引対応述語がちょうど 1 件で、`DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA`
    /// 列の範囲比較（`FilterOp::TypedCompare`。Issue #891・TASK-199 で
    /// production 結線済み・Issue #893 で二次索引へ接続済み。`BYTEA` は
    /// Issue #1257 で接続）。`FilterOp::Between`
    /// （SQL-24・TASK-208 ポインタ）も同じ分類へ合流する
    /// （`sql::scalar_index::ScalarIndex::candidates_for` が `Ge`∧`Le` の
    /// 積で候補を導出する）。
    IndexTypedRange,
    /// 索引対応述語がちょうど 1 件で、`TEXT`／`ENUM` 列の `IN` 条件
    /// （`FilterOp::InText`。SQL-24・TASK-208 ポインタ）。
    IndexInList,
    /// 索引対応述語が 2 件以上（交差が必要）。
    IndexConjunction,
}

/// [`classify_scalar_plan`] の入力。`sql::exec`・`sql::using_plan::
/// pre_check_bindable` の双方が同じ形へ組み立てる。TASK-186・NOSQL-10 の前提
/// として Issue #730 で公開 API へ昇格した（`metadata_filters`／
/// `expr_filters` はいずれも既に `pub` な型〔`MetadataFilter`・`BoundExpr`〕の
/// スライスで、`BoundStatement` のアクセサー経由で外部から得られるため、
/// クレート外が構造体リテラルで直接組み立てられるようにする。
/// `#[non_exhaustive]` は付けない）。
pub struct ScalarShapeInput<'a> {
    /// `ExecutionPlan::from_evaluation_order(..).scalar_prefilter`
    /// （SCALAR 段が DISTANCE 段より先に評価されるか）。`false`（`HINT ORDER`
    /// による DISTANCE 先行・SCALAR 事後フィルタ）では索引を使わない
    /// （`sql::exec` の事後フィルタ経路は索引化の対象外。モジュール
    /// ドキュメント参照）。
    pub scalar_prefilter: bool,
    pub metadata_filters: &'a [MetadataFilter],
    pub expr_filters: &'a [BoundExpr],
    /// `WHERE` の `OR` 群（TASK-208・SQL-24、Issue #912）。
    ///
    /// **BREAKING CHANGE**: 本フィールドの追加は `ScalarShapeInput` を構造体
    /// リテラルで組み立てる既存コードのコンパイルを壊す（`#[non_exhaustive]`
    /// を付けていない `pub` 型のため）。移行方針: OR 群を持たない既存呼び出しは
    /// 空スライス（`&[]`）を渡す。
    ///
    /// 索引経路（`sql::scalar_index`）は現時点で OR 群の和集合計算に対応して
    /// いない（Issue #912 のスコープでは索引最適化を対象外とし、正しさを優先
    /// して `ScalarPlan::PlainScan` へ一律縮退する。索引対応は別 Issue）。
    pub or_filters: &'a [BoundOrGroup],
}

/// `id <op> <数値リテラル>` へ正規化した式述語（左右いずれの位置で束縛されて
/// いても `id` が左辺に来る形へ揃える。左右入替時は `op` を反転する: 例えば
/// `5 > id` は `id < 5` と同値であるため `Lt` へ変換する）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IdPredicate {
    pub(crate) op: BinOp,
    pub(crate) literal: f64,
}

/// `op` を左右入替時の同値な演算子へ反転する（`L op R` を `R op' L` に書き換える
/// ときの `op'`）。`Eq` は対称なため不変。
fn flip_comparison(op: BinOp) -> BinOp {
    match op {
        BinOp::Gt => BinOp::Lt,
        BinOp::Lt => BinOp::Gt,
        BinOp::Ge => BinOp::Le,
        BinOp::Le => BinOp::Ge,
        BinOp::Eq => BinOp::Eq,
        // 算術演算子はこの関数の呼び出し元（`id_predicate_from_expr`）が比較
        // 演算子のみに絞り込んだ後にしか渡さない。
        other => other,
    }
}

/// `expr` が「`id` と数値リテラルの単純比較」（狭義の索引対応述語）である場合に
/// 限り [`IdPredicate`] を返す。それ以外（算術式・`Builtin`・`WasmCall`・
/// `VectorRef` を含む式・ネストした比較等）はすべて `None`（索引非対応。呼び出し元
/// はこれを「残余述語あり」＝[`ScalarPlan::PlainScan`] の根拠として扱う）。
pub(crate) fn id_predicate_from_expr(expr: &BoundExpr) -> Option<IdPredicate> {
    let BoundExpr::Binary { op, lhs, rhs } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq
    ) {
        return None;
    }
    match (lhs.as_ref(), rhs.as_ref()) {
        (BoundExpr::IdRef, BoundExpr::Number(literal)) => Some(IdPredicate {
            op: *op,
            literal: *literal,
        }),
        (BoundExpr::Number(literal), BoundExpr::IdRef) => Some(IdPredicate {
            op: flip_comparison(*op),
            literal: *literal,
        }),
        _ => None,
    }
}

/// [`ScalarShapeInput`] から [`ScalarPlan`] を決定する（純粋関数・副作用なし）。
/// `sql::exec`（索引消費の可否判定）と `sql::explain`（`scalar_plan:` 行）が
/// この関数だけを単一情報源として使う（モジュールドキュメント参照）。TASK-186・
/// NOSQL-10 の前提として Issue #730 で公開 API へ昇格した。
pub fn classify_scalar_plan(input: &ScalarShapeInput<'_>) -> ScalarPlan {
    if !input.scalar_prefilter {
        return ScalarPlan::PlainScan;
    }
    if input.metadata_filters.is_empty()
        && input.expr_filters.is_empty()
        && input.or_filters.is_empty()
    {
        return ScalarPlan::PlainScan;
    }
    // TASK-208・SQL-24（Issue #912）: OR 群を含む述語は索引の和集合計算に
    // 未対応のため一律 `PlainScan` へ縮退する（モジュールドキュメント
    // 「索引対応述語の狭い定義」と同じ判断: 対応できない形は安全側の全走査に
    // 倒す。索引最適化〔`ScalarPlan::IndexDisjunction` 相当〕は別 Issue）。
    if !input.or_filters.is_empty() {
        return ScalarPlan::PlainScan;
    }
    // BOOLEAN 列の等価述語が 1 つでも含まれる場合は索引経路へ進まない
    // （`ScalarIndex::build` が BOOLEAN 列を索引対象から除外するため。
    // `IndexConjunction` の複合述語に紛れ込んで誤って索引被覆済みと
    // 判定されるのを防ぐ単一情報源。Issue #883・D-e）。
    if input
        .metadata_filters
        .iter()
        .any(|f| matches!(f.op(), crate::declarative_filter::FilterOp::BoolEquals(_)))
    {
        return ScalarPlan::PlainScan;
    }
    // `Not`／`IsNull`／`IsNotNull`／`InTyped`（SQL-24・TASK-208 ポインタ）は
    // 索引未対応（`sql::scalar_index::ScalarIndex::candidates_for` が `None` を
    // 返す）。`mask_trusted_defer`／`count_star_only` は「索引が返した候補集合が
    // 厳密一致」を前提に再評価を省略するため、索引側が対応しない op が複合述語
    // へ紛れ込んだまま `IndexConjunction` に分類されるのを防ぐ単一の事前ゲート
    // （`BoolEquals` と同じ設計）。
    if input.metadata_filters.iter().any(|f| {
        matches!(
            f.op(),
            crate::declarative_filter::FilterOp::Not(_)
                | crate::declarative_filter::FilterOp::IsNull
                | crate::declarative_filter::FilterOp::IsNotNull
                | crate::declarative_filter::FilterOp::InTyped(_)
                // 配列・JSON 列の等価／`IN`（Issue #1193）も索引未対応。事前ゲートに
                // 含めないと `lang = 'ja' AND tags = '{a}'` のような複合述語が
                // `IndexConjunction` に分類され、`mask_trusted_defer` が再評価を
                // 省略して誤った一致を返す（fail-open）。
                | crate::declarative_filter::FilterOp::ArrayEquals(_)
                | crate::declarative_filter::FilterOp::InArray(_)
                | crate::declarative_filter::FilterOp::JsonEquals(_)
                | crate::declarative_filter::FilterOp::InJson(_)
        )
    }) {
        return ScalarPlan::PlainScan;
    }
    // `LIKE` の一般形（中間一致・後方一致・`_`。SQL-24／TASK-208、Issue #914）は
    // 二次索引が対応しない（`sql::scalar_index::ScalarIndex::candidates_for` が
    // `None` を返す）。純粋な前方一致・完全一致は束縛時に `StartsWith`／
    // `Equals` へ既に振り分け済みのため、ここへ到達する `Like` は必ず
    // 一般形——`BoolEquals` と同じ理由で、複合述語に
    // 紛れて誤って索引被覆済みと判定されるのを防ぐ単一情報源として先頭で
    // plain scan へ倒す。
    if input
        .metadata_filters
        .iter()
        .any(|f| matches!(f.op(), crate::declarative_filter::FilterOp::Like(_)))
    {
        return ScalarPlan::PlainScan;
    }
    let mut id_predicate_count = 0usize;
    for expr in input.expr_filters {
        if id_predicate_from_expr(expr).is_none() {
            // 索引非対応の残余述語（評価時にエラーになりうる式を含む）が
            // 1 つでもあれば、索引経路を使わず宣言順の逐次評価
            // （既存 fail-closed 契約）へ全面的に委ねる。
            return ScalarPlan::PlainScan;
        }
        id_predicate_count += 1;
    }
    let total = input.metadata_filters.len() + id_predicate_count;
    if total >= 2 {
        return ScalarPlan::IndexConjunction;
    }
    // ここに到達する時点で `total == 1`（`total == 0` は上の空判定で除外済み）。
    if id_predicate_count == 1 {
        ScalarPlan::IndexIdRange
    } else {
        match input.metadata_filters[0].op() {
            crate::declarative_filter::FilterOp::Equals(_) => ScalarPlan::IndexEquality,
            crate::declarative_filter::FilterOp::StartsWith(_) => ScalarPlan::IndexPrefix,
            // BOOLEAN 列の等価述語は索引化しない（`ScalarIndex::build` が
            // BOOLEAN 列を索引対象から除外する。Issue #883・D-e）ため、常に
            // plain scan へ倒す。`mask_trusted_defer`／`count_star_only`／
            // `observe_group_count_only` が BOOLEAN 述語を「索引で完全被覆
            // 済み」と誤って信頼しないための単一情報源での保証。
            crate::declarative_filter::FilterOp::BoolEquals(_) => ScalarPlan::PlainScan,
            // `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID`／`BYTEA` 列の範囲比較。
            // Issue #891・TASK-199 の production 結線・Issue #893 の二次索引
            // 接続（`BYTEA` は Issue #1257）により索引対応述語として扱う。
            crate::declarative_filter::FilterOp::TypedCompare { .. }
            // `Between`（SQL-24・TASK-208 ポインタ）。
            | crate::declarative_filter::FilterOp::Between { .. } => ScalarPlan::IndexTypedRange,
            // `TEXT`／`ENUM` 列の `IN`（SQL-24・TASK-208 ポインタ）。
            crate::declarative_filter::FilterOp::InText(_) => ScalarPlan::IndexInList,
            // `Compare`／`InListLiteral`／`BetweenLiteral`（未束縛）は `bind`
            // 済み `MetadataFilter` には現れない契約だが網羅性のため plain
            // scan へ倒す（fail-closed の保険腕）。`Not`／`IsNull`／
            // `IsNotNull`／`InTyped` は上の事前ゲートで既に `PlainScan` 済み
            // のためここへは到達しない。
            //
            // `Like` は上の事前判定で既に `PlainScan` を返し済みのため
            // ここへは到達しないが、網羅性のため保険腕として同じ結果を返す
            // （SQL-24／TASK-208、Issue #914）。`LikeUnbound`（未束縛）は
            // `bind` 済み `MetadataFilter` には現れない契約（`Compare` と同じ
            // fail-closed の保険腕）。
            crate::declarative_filter::FilterOp::Compare { .. }
            | crate::declarative_filter::FilterOp::InListLiteral { .. }
            | crate::declarative_filter::FilterOp::BetweenLiteral { .. }
            | crate::declarative_filter::FilterOp::Not(_)
            | crate::declarative_filter::FilterOp::IsNull
            | crate::declarative_filter::FilterOp::IsNotNull
            | crate::declarative_filter::FilterOp::InTyped(_)
            | crate::declarative_filter::FilterOp::ArrayEquals(_)
            | crate::declarative_filter::FilterOp::InArray(_)
            | crate::declarative_filter::FilterOp::JsonEquals(_)
            | crate::declarative_filter::FilterOp::InJson(_)
            | crate::declarative_filter::FilterOp::Like(_)
            | crate::declarative_filter::FilterOp::LikeUnbound(_) => ScalarPlan::PlainScan,
        }
    }
}

/// `id_as_finite_scalar`（`sql::udf_call`）が許容する `id` の上限と同じ境界
/// （`2^53`。`f64` の仮数部が整数値を正確に表現できる上限）。`id_index` は
/// 全行がこの上限を満たす場合に限り `Some` になるため（`scalar_index.rs`
/// モジュールドキュメント参照）、リテラル側の判定にも同じ境界を使う。
const MAX_EXACT_ID: u64 = 1u64 << 53;

/// `id` 比較述語 [`IdPredicate`] を `ScalarIndex::candidates_id_range` が受け取る
/// `(Bound<u64>, Bound<u64>)` へ変換する。`sql::udf_call::eval_binary` の
/// `(id as f64) op literal` 評価とビット同値になるよう導出する
/// （本モジュールの [`tests::id_bounds_matches_eval_binary_property`] で固定）。
///
/// `None` は「判定不能（呼び出し元は索引を使わず全走査へ縮退する）」であり
/// 「一致 0 件」ではない。文法上 `literal` は非負かつ有限のみ生成されうる
/// （単項マイナスを持たない構文・`sql::udf_call::parse_number_literal` の
/// `is_finite` 検査。`sql::lexer::lex_number` 参照）が、将来の構文拡張に備え
/// 非有限値は防御的に `None` へ倒す。負のリテラルも構文上は到達しないが、
/// 同じ防御のため正しい境界（`Gt`/`Ge` は全件、`Lt`/`Le`/`Eq` は空集合）を返す。
pub(crate) fn id_bounds(
    pred: &IdPredicate,
) -> Option<(std::ops::Bound<u64>, std::ops::Bound<u64>)> {
    use std::ops::Bound;

    if !pred.literal.is_finite() {
        return None;
    }
    let l = pred.literal;
    let max = MAX_EXACT_ID as f64;
    // `lower > upper` になるよう構成した正準の「一致 0 件」（`candidates_id_range`
    // の `in_lower && in_upper` 判定が常に偽になる）。
    const EMPTY: (Bound<u64>, Bound<u64>) = (Bound::Included(1), Bound::Included(0));

    Some(match pred.op {
        BinOp::Gt => {
            if l < 0.0 {
                (Bound::Unbounded, Bound::Unbounded)
            } else if l >= max {
                EMPTY
            } else {
                let lower = (l.floor() as u64).saturating_add(1);
                (Bound::Included(lower), Bound::Unbounded)
            }
        }
        BinOp::Ge => {
            if l < 0.0 {
                (Bound::Unbounded, Bound::Unbounded)
            } else if l > max {
                EMPTY
            } else {
                (Bound::Included(l.ceil() as u64), Bound::Unbounded)
            }
        }
        BinOp::Lt => {
            if l <= 0.0 {
                EMPTY
            } else if l > max {
                (Bound::Unbounded, Bound::Unbounded)
            } else {
                let upper = if l.fract() == 0.0 {
                    (l as u64).saturating_sub(1)
                } else {
                    l.floor() as u64
                };
                (Bound::Unbounded, Bound::Included(upper))
            }
        }
        BinOp::Le => {
            if l < 0.0 {
                EMPTY
            } else if l >= max {
                (Bound::Unbounded, Bound::Unbounded)
            } else {
                (Bound::Unbounded, Bound::Included(l.floor() as u64))
            }
        }
        BinOp::Eq => {
            if !(0.0..=max).contains(&l) || l.fract() != 0.0 {
                EMPTY
            } else {
                let v = l as u64;
                (Bound::Included(v), Bound::Included(v))
            }
        }
        // `id_predicate_from_expr` が比較演算子のみへ絞り込み済みのため到達
        // しない。
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => return None,
    })
}

// 数値・日時・`NUMERIC`・`UUID` 列（Issue #893）に対する範囲述語
// （`declarative_filter::FilterOp::TypedCompare`。Issue #891・TASK-199 で
// production 結線済み）は、`sql::scalar_index::ScalarIndex::candidates_for`
// が `metadata_filters` 経由で直接消費する（`TEXT` 列の `Equals`／
// `StartsWith` と同じ経路を共有し、本モジュール側の変換アダプタ・専用の
// 述語表現は不要——`resolve_candidates` へ渡す前に型ごとへ振り分ける必要が
// ない設計。詳細は `sql::scalar_index` モジュールドキュメント参照）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declarative_filter::DeclarativeFilter;

    fn id_gt(literal: f64) -> BoundExpr {
        BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::IdRef),
            rhs: Box::new(BoundExpr::Number(literal)),
        }
    }

    fn test_schema() -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("c0", ColumnType::Text, true),
                ColumnDef::new("c1", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("day", ColumnType::Date, true),
                ColumnDef::new("blob", ColumnType::Bytea, true),
            ],
        )
    }

    fn eq_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::equals(
                schema.columns[column_index].name.clone(),
                "v".to_string(),
            )],
            &schema,
        )
        .expect("bind equals filter");
        bound.into_iter().next().expect("one filter")
    }

    fn bool_filter(column_index: usize, value: bool) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::bool_equals(
                schema.columns[column_index].name.clone(),
                value,
            )],
            &schema,
        )
        .expect("bind bool_equals filter");
        bound.into_iter().next().expect("one filter")
    }

    /// SQL-24・TASK-208、Issue #914: `LIKE` の一般形（中間一致・後方一致・`_`）
    /// 述語。純粋な前方一致・完全一致は `bind` 時に `StartsWith`／`Equals` へ
    /// 振り分けられるため、`FilterOp::Like` を得るには `%` を中間・先頭に
    /// 置くか `_` を含める必要がある。
    fn like_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::like(
                schema.columns[column_index].name.clone(),
                "%mid%".to_string(),
            )],
            &schema,
        )
        .expect("bind like filter");
        bound.into_iter().next().expect("one filter")
    }

    fn typed_compare_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::compare(
                schema.columns[column_index].name.clone(),
                crate::declarative_filter::CompareOp::Gt,
                "2024-01-01".to_string(),
            )],
            &schema,
        )
        .expect("bind compare filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `BYTEA` 列の範囲比較用の `TypedCompare` 述語（`TypedLiteral::Bytes`）。
    fn typed_compare_bytea_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::compare(
                schema.columns[column_index].name.clone(),
                crate::declarative_filter::CompareOp::Gt,
                "\\x00".to_string(),
            )],
            &schema,
        )
        .expect("bind compare filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `TEXT` 列の `IN` 述語（SQL-24・TASK-208 ポインタ）。
    fn in_text_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::in_list(
                schema.columns[column_index].name.clone(),
                vec!["a".to_string(), "b".to_string()],
            )],
            &schema,
        )
        .expect("bind IN filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `DATE` 列の `BETWEEN` 述語（`Bytes` 以外。SQL-24・TASK-208 ポインタ）。
    fn between_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::between(
                schema.columns[column_index].name.clone(),
                "2024-01-01".to_string(),
                "2024-06-01".to_string(),
            )],
            &schema,
        )
        .expect("bind BETWEEN filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `BYTEA` 列の `BETWEEN` 述語（Issue #1257 で二次索引対応。SQL-24・
    /// TASK-208 ポインタ）。
    fn between_bytea_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::between(
                schema.columns[column_index].name.clone(),
                "\\x00".to_string(),
                "\\xff".to_string(),
            )],
            &schema,
        )
        .expect("bind BETWEEN filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `IS NULL` 述語（SQL-24・TASK-208 ポインタ）。
    fn is_null_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::is_null(
                schema.columns[column_index].name.clone(),
            )],
            &schema,
        )
        .expect("bind IS NULL filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `NOT <col> = '<lit>'` 述語（SQL-24・TASK-208 ポインタ）。
    fn not_eq_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::equals(
                schema.columns[column_index].name.clone(),
                "v".to_string(),
            )
            .negate()],
            &schema,
        )
        .expect("bind NOT filter");
        bound.into_iter().next().expect("one filter")
    }

    /// `DATE` 列の `IN` 述語（`InTyped`。SQL-24・TASK-208 ポインタ。索引
    /// 未対応のため常に `PlainScan`）。
    fn in_typed_filter(column_index: usize) -> MetadataFilter {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::in_list(
                schema.columns[column_index].name.clone(),
                vec!["2024-01-01".to_string()],
            )],
            &schema,
        )
        .expect("bind IN filter");
        bound.into_iter().next().expect("one filter")
    }

    // --- SQL-24（TASK-208 ポインタ）: IN / BETWEEN / IS NULL / NOT ------------

    #[test]
    fn index_in_list_for_single_in_text_predicate() {
        let filters = vec![in_text_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexInList);
    }

    #[test]
    fn index_typed_range_for_single_between_predicate() {
        let filters = vec![between_filter(4)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexTypedRange);
    }

    #[test]
    fn index_typed_range_for_single_bytea_between_predicate() {
        let filters = vec![between_bytea_filter(5)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexTypedRange);
    }

    #[test]
    fn index_conjunction_when_bytea_between_predicate_mixed_with_text_equality() {
        let filters = vec![eq_filter(1), between_bytea_filter(5)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexConjunction);
    }

    #[test]
    fn plain_scan_for_single_is_null_predicate() {
        let filters = vec![is_null_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_for_single_not_predicate() {
        let filters = vec![not_eq_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_for_single_in_typed_predicate() {
        let filters = vec![in_typed_filter(4)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_not_predicate_mixed_with_indexable_equality() {
        // `Not`／`IsNull`／`IsNotNull`／`InTyped` は複合述語に混在しても
        // `IndexConjunction` へ進まない（事前ゲート）。
        let filters = vec![eq_filter(1), not_eq_filter(2)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    /// Issue #1193: 配列・JSON／JSONB 列の等価／`IN` は索引未対応。TEXT 列の
    /// 索引対応述語と混在しても `IndexConjunction` へ進まず `PlainScan` になる
    /// （`mask_trusted_defer` が再評価を省略して誤った一致を返す fail-open の防止）。
    #[test]
    fn plain_scan_when_array_or_json_predicate_mixed_with_indexable_equality() {
        use crate::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                    true,
                ),
                ColumnDef::new("doc", ColumnType::Jsonb, true),
            ],
        );
        let bind = |filter: DeclarativeFilter| {
            crate::declarative_filter::bind_all(&[filter], &schema)
                .expect("bind")
                .into_iter()
                .next()
                .expect("one filter")
        };
        let text_eq = bind(DeclarativeFilter::equals("lang", "ja"));
        let predicates = [
            DeclarativeFilter::equals("tags", "{a,NULL}"),
            DeclarativeFilter::in_list("tags", vec!["{a}".to_string()]),
            DeclarativeFilter::equals("doc", "{\"a\":1}"),
            DeclarativeFilter::in_list("doc", vec!["[]".to_string()]),
        ];
        for predicate in predicates {
            let filters = vec![text_eq.clone(), bind(predicate)];
            let input = ScalarShapeInput {
                scalar_prefilter: true,
                metadata_filters: &filters,
                expr_filters: &[],
                or_filters: &[],
            };
            assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
            // 単独でも索引を使わない。
            let alone = vec![filters[1].clone()];
            let input = ScalarShapeInput {
                scalar_prefilter: true,
                metadata_filters: &alone,
                expr_filters: &[],
                or_filters: &[],
            };
            assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
        }
    }

    /// Issue #891・TASK-199（production 結線）・Issue #893（二次索引接続）:
    /// `DATE`／`TIMESTAMP`／`NUMERIC`／`UUID` 列の範囲比較（`TypedCompare`）は
    /// 単独では `IndexTypedRange` に分類される。
    #[test]
    fn index_typed_range_for_single_typed_compare_predicate() {
        let filters = vec![typed_compare_filter(4)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexTypedRange);
    }

    #[test]
    fn index_conjunction_when_typed_compare_predicate_mixed_with_text_equality() {
        let filters = vec![eq_filter(1), typed_compare_filter(4)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexConjunction);
    }

    /// `BYTEA` 列の範囲比較は Issue #1257 で [`OrderedColumnIndex::Bytes`]
    /// （`sql::scalar_index`）へ接続済みのため、単独なら `IndexTypedRange`・
    /// 複合述語の一部なら `IndexConjunction` に分類される。
    #[test]
    fn index_typed_range_for_single_bytea_typed_compare_predicate() {
        let filters = vec![typed_compare_bytea_filter(5)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexTypedRange);
    }

    #[test]
    fn index_conjunction_when_bytea_typed_compare_predicate_mixed_with_text_equality() {
        let filters = vec![eq_filter(1), typed_compare_bytea_filter(5)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexConjunction);
    }

    /// Issue #883・D-e: BOOLEAN 述語は単独でも複合述語の一部でも常に
    /// `PlainScan` に分類される（`ScalarIndex::build` が BOOLEAN 列を索引化
    /// しないため。`mask_trusted_defer`／`count_star_only`／
    /// `observe_group_count_only` が BOOLEAN 述語を「索引で完全被覆済み」と
    /// 誤って信頼しないことの単一情報源での固定）。
    /// SQL-24・TASK-208、Issue #914: `LIKE` の一般形は単独でも索引を使わず
    /// `PlainScan` に分類される（`sql::scalar_index::ScalarIndex::
    /// candidates_for` が対応しないため。索引の有無で結果が変わらない
    /// 契約を守る単一情報源での固定）。
    #[test]
    fn plain_scan_for_single_like_predicate() {
        let filters = vec![like_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_like_predicate_mixed_with_text_equality() {
        let filters = vec![eq_filter(1), like_filter(2)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    /// 純粋な前方一致（末尾 `%` のみ）は `bind` 時に `StartsWith` へ振り分け
    /// られ、従来どおり `IndexPrefix` を維持する（索引縮退の対象外）。
    #[test]
    fn index_prefix_kept_for_pure_prefix_like_pattern() {
        let schema = test_schema();
        let bound = crate::declarative_filter::bind_all(
            &[DeclarativeFilter::like(
                schema.columns[1].name.clone(),
                "src/%".to_string(),
            )],
            &schema,
        )
        .expect("bind like filter");
        let filters = bound;
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexPrefix);
    }

    #[test]
    fn plain_scan_for_single_bool_predicate() {
        let filters = vec![bool_filter(3, true)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_bool_predicate_mixed_with_text_equality() {
        // 複合述語（total >= 2）でも BOOLEAN 述語が 1 つでも含まれれば
        // IndexConjunction へ進まない。
        let filters = vec![eq_filter(1), bool_filter(3, false)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_scalar_postfilter() {
        let filters = vec![eq_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: false,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn plain_scan_when_no_predicates() {
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn index_equality_single_metadata_filter() {
        let filters = vec![eq_filter(1)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexEquality);
    }

    #[test]
    fn index_id_range_single_expr_filter() {
        let exprs = vec![id_gt(5.0)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &exprs,
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexIdRange);
    }

    #[test]
    fn index_conjunction_two_predicates() {
        let filters = vec![eq_filter(1)];
        let exprs = vec![id_gt(5.0)];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &exprs,
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::IndexConjunction);
    }

    #[test]
    fn plain_scan_on_residual_vector_ref_expr() {
        // `VectorRef` を参照する残余述語（`embedding` を直接比較する形。
        // 索引が扱えるのは `id` の単純比較のみであることを固定する）。
        let exprs = vec![BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::VectorRef),
            rhs: Box::new(BoundExpr::Number(1.0)),
        }];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &[],
            expr_filters: &exprs,
            or_filters: &[],
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    /// TASK-208・SQL-24（Issue #912）: OR 群を含む述語は、他の述語が完全に索引
    /// 対応形状であっても一律 `PlainScan` へ縮退する（索引の和集合計算は本 Issue
    /// のスコープ外。モジュールドキュメント「索引対応述語の狭い定義」と同じ判断）。
    #[test]
    fn plain_scan_when_or_filters_present_even_with_index_eligible_metadata_filters() {
        let filters = vec![eq_filter(1)];
        let or_group = crate::sql::where_tree::BoundOrGroup::new(vec![
            crate::sql::where_tree::BoundConjunction::new(
                vec![eq_filter(1)],
                Vec::new(),
                Vec::new(),
            ),
            crate::sql::where_tree::BoundConjunction::new(
                vec![eq_filter(2)],
                Vec::new(),
                Vec::new(),
            ),
        ]);
        let or_filters = vec![or_group];
        let input = ScalarShapeInput {
            scalar_prefilter: true,
            metadata_filters: &filters,
            expr_filters: &[],
            or_filters: &or_filters,
        };
        assert_eq!(classify_scalar_plan(&input), ScalarPlan::PlainScan);
    }

    #[test]
    fn left_right_swap_flips_operator() {
        // `5 > id` は `id < 5` と同値。
        let expr = BoundExpr::Binary {
            op: BinOp::Gt,
            lhs: Box::new(BoundExpr::Number(5.0)),
            rhs: Box::new(BoundExpr::IdRef),
        };
        let pred = id_predicate_from_expr(&expr).expect("id predicate");
        assert_eq!(pred.op, BinOp::Lt);
        assert_eq!(pred.literal, 5.0);
    }

    #[test]
    fn id_bounds_matches_eval_binary_property() {
        use crate::sql::udf_call::{eval_binary, ExprValue};
        let ids: Vec<u64> = (0..=20).collect();
        let literals = [0.0, 0.5, 1.0, 5.0, 5.5, 10.0, 19.5, 20.0, 20.5];
        for &literal in &literals {
            for op in [BinOp::Gt, BinOp::Lt, BinOp::Ge, BinOp::Le, BinOp::Eq] {
                let pred = IdPredicate { op, literal };
                let bounds = id_bounds(&pred).expect("finite non-negative literal");
                for &id in &ids {
                    let expected = matches!(
                        eval_binary(op, ExprValue::Scalar(id as f64), ExprValue::Scalar(literal),)
                            .expect("comparison never errors for finite operands"),
                        ExprValue::Bool(true)
                    );
                    let in_range = bound_contains(bounds, id);
                    assert_eq!(in_range, expected, "op={op:?} literal={literal} id={id}");
                }
            }
        }
    }

    /// [`tests::id_bounds_matches_eval_binary_property`] は `id ∈ 0..=20` ・
    /// 小さな literal しか網羅しないため、`id_index` が扱う実際の上限
    /// （[`MAX_EXACT_ID`]＝`2^53`）付近の境界を別途固定する。`2^53 + 1.0` は
    /// f64 の刻み幅が 1 を超えるため `2^53` へ丸められてしまう（`f64::from_str`
    /// の丸めハザード）ので、あえて `+2.0` を使い実際に異なる値になる literal を
    /// 網羅する。`id` 側は `MAX_EXACT_ID` を上限とする（`id > 2^53` の行が
    /// 1 件でもあれば `id_index` 自体が `None` になり `id_bounds` は呼ばれない
    /// 契約——`sql::udf_call::id_as_finite_scalar` 参照——ため、その領域は
    /// `eval_binary` 側の `(id as f64)` 丸めで exact 比較契約が崩れても
    /// 本関数の正しさに影響しない）。
    #[test]
    fn id_bounds_matches_eval_binary_property_near_2_53_boundary() {
        use crate::sql::udf_call::{eval_binary, ExprValue};
        let ids: Vec<u64> = vec![MAX_EXACT_ID - 2, MAX_EXACT_ID - 1, MAX_EXACT_ID];
        let literals = [
            (MAX_EXACT_ID - 1) as f64,
            MAX_EXACT_ID as f64,
            (MAX_EXACT_ID as f64) + 2.0,
        ];
        for &literal in &literals {
            for op in [BinOp::Gt, BinOp::Lt, BinOp::Ge, BinOp::Le, BinOp::Eq] {
                let pred = IdPredicate { op, literal };
                let bounds = id_bounds(&pred).expect("finite non-negative literal");
                for &id in &ids {
                    let expected = matches!(
                        eval_binary(op, ExprValue::Scalar(id as f64), ExprValue::Scalar(literal),)
                            .expect("comparison never errors for finite operands"),
                        ExprValue::Bool(true)
                    );
                    let in_range = bound_contains(bounds, id);
                    assert_eq!(in_range, expected, "op={op:?} literal={literal} id={id}");
                }
            }
        }
    }

    fn bound_contains(bounds: (std::ops::Bound<u64>, std::ops::Bound<u64>), id: u64) -> bool {
        use std::ops::Bound;
        let lower_ok = match bounds.0 {
            Bound::Included(l) => id >= l,
            Bound::Excluded(l) => id > l,
            Bound::Unbounded => true,
        };
        let upper_ok = match bounds.1 {
            Bound::Included(u) => id <= u,
            Bound::Excluded(u) => id < u,
            Bound::Unbounded => true,
        };
        lower_ok && upper_ok
    }
}
