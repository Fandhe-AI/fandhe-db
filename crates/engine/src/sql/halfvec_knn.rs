//! `HALFVEC` 列を持つ表への距離演算子形 kNN（`ORDER BY h <=> '[..]' LIMIT k`）を、全走査ソートの
//! scan 経路へ写す変換（Issue #2015・TABLE-46。SQL-33 のポインタ）。
//!
//! 通常の kNN（`Statement::Select`）は arena の embedding スロット（`VECTOR` 専用）を読む。HALFVEC は
//! スカラーペイロードに f16 で保存されるため arena に載らない。そこで `core.rs` の読み取り経路
//! （`execute_read_statement_with_limits` と Prepared の Describe）が、束縛の直前に本モジュールで
//! `Statement::Scan` 相当へ変換し、関数形 `ORDER BY l2_distance(..)` が既に使っている式キー ORDER BY
//! の実行器へ合流させる。行ポリシー（RLS）・WHERE・LIMIT/OFFSET・NULL/NaN の並びは scan 経路が担う。
//!
//! fail-closed: 変換の対象外（`USING PLAN`・hybrid・`USING MODE`・`HINT ORDER`）や HALFVEC を持たない表は
//! `Ok(None)` で呼び出し元の従来経路へ戻し、従来経路が `0A000`／既存の契約で拒否する。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{
    OrderByForm, Projection, ScanOrderKey, SelectItem, SqlSurfaceError, ValidatedScan,
    ValidatedStatement,
};
use crate::sql::lexer::DistanceKind;
use crate::sql::mode::SearchMode;
use crate::sql::parser::LiteralSkip;
use crate::sql::plan::EvaluationOrder;
use crate::sql::udf_call::Expr;

/// `HALFVEC` 列を持つ表への距離演算子形 kNN を scan へ変換する。対象外は `Ok(None)`。
///
/// 演算子の範囲は VECTOR の kNN と同じ `<=>`（コサイン）・`<#>`（負の内積）だけで、`<->`／`<+>` は
/// `sql::exec::ensure_distance_kind_executable` と同じ `0A000` で変換の前に拒否する。
///
/// `session_mode` は `SET search_mode` の値。文中の `USING MODE` が無く実効モードが `precision` の
/// 組は、確信度ゲートを scan 経路が持たないため `0A000` で拒否する（Execute／Describe 共通。fail-closed）。
/// `skip` は Prepared Describe の `$n` 由来ダミー値を比較から外すための印で、実リテラルの投影距離列は
/// `ORDER BY` と HALFVEC 丸め後のクエリベクトルが一致しない組を `0A000` で拒否する（SQL-33 (7)）。
pub(crate) fn halfvec_select_as_scan(
    v: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
    skip: &LiteralSkip,
) -> Result<Option<ValidatedScan>, SqlSurfaceError> {
    if !schema
        .columns
        .iter()
        .any(|c| matches!(c.ty, ColumnType::HalfVec(_)))
    {
        return Ok(None);
    }
    if v.using_plan().is_some() {
        return Ok(None);
    }
    let OrderByForm::Distance { column, literal } = v.order_by() else {
        return Ok(None);
    };
    if !schema
        .columns
        .iter()
        .any(|c| &c.name == column && matches!(c.ty, ColumnType::HalfVec(_)))
    {
        return Ok(None);
    }
    let kind = v.distance_kind();
    if let DistanceKind::L2 | DistanceKind::L1 = kind {
        return Err(SqlSurfaceError::feature_not_supported(format!(
            "distance operator {} is not supported yet",
            kind.operator()
        )));
    }
    if v.search_mode().is_some() || v.evaluation_order() != EvaluationOrder::DEFAULT {
        return Ok(None);
    }
    if session_mode == Some(SearchMode::Precision) {
        return Err(SqlSurfaceError::feature_not_supported(
            "precision mode is not supported on halfvec columns",
        ));
    }
    if !skip.order_by {
        check_projected_query_matches(v, kind, literal, skip)?;
    }
    Ok(Some(ValidatedScan {
        table_name: v.table_name().to_string(),
        projection: v.projection().clone(),
        where_predicates: v.where_predicates().to_vec(),
        limit: v.limit(),
        limit_implicit: v.limit_implicit,
        order_by: Vec::new(),
        offset: v.offset(),
        window_items: Vec::new(),
        order_keys: vec![ScanOrderKey::Expr {
            expr: Expr::VectorDistance {
                kind,
                lhs: Box::new(Expr::Ident(column.clone())),
                rhs: Box::new(Expr::String(literal.clone())),
            },
            descending: v.distance_descending(),
            nulls: None,
        }],
        scalar_subquery_items: Vec::new(),
        physical_order_keys: Vec::new(),
        embedded_scalar_subqueries: Vec::new(),
        with_ties: false,
    }))
}

/// 投影の距離列のクエリベクトルが `ORDER BY` のものと（f16 丸め後に）一致することを検査する。
/// 通常の kNN 経路の `check_projected_distance` は scan 変換後は呼ばれないため、同じ契約をここで守る。
/// リテラルがベクトルとして解釈できない場合は比較せず、後段の束縛が既存のエラーで拒否する。
fn check_projected_query_matches(
    v: &ValidatedStatement,
    kind: DistanceKind,
    order_literal: &str,
    skip: &LiteralSkip,
) -> Result<(), SqlSurfaceError> {
    let Projection::Items(items) = v.projection() else {
        return Ok(());
    };
    let rounded = |lit: &str| -> Option<Vec<u16>> {
        let values = crate::sql::parser::parse_vector_literal_any_dim(lit).ok()?;
        Some(
            values
                .iter()
                .map(|x| crate::f16::f32_to_f16_bits(*x))
                .collect(),
        )
    };
    let order_bits = rounded(order_literal);
    for (index, item) in items.iter().enumerate() {
        if skip.skips_projection_item(index) {
            continue;
        }
        let SelectItem::Expr {
            expr:
                Expr::VectorDistance {
                    kind: item_kind,
                    lhs,
                    rhs,
                },
            ..
        } = item
        else {
            continue;
        };
        // リテラルは左右どちらにも置ける（`'[..]'::halfvec <=> h` 等）。キャストは外して中の文字列を見る。
        let Some(item_literal) =
            distance_operand_literal(rhs).or_else(|| distance_operand_literal(lhs))
        else {
            continue;
        };
        if *item_kind != kind {
            return Err(SqlSurfaceError::feature_not_supported(
                "a projected distance must use the same operator as the distance ORDER BY",
            ));
        }
        if let (Some(a), Some(b)) = (&order_bits, rounded(item_literal)) {
            if *a != b {
                return Err(SqlSurfaceError::feature_not_supported(
                    "a projected distance must use the same query vector as the distance ORDER BY",
                ));
            }
        }
    }
    Ok(())
}

/// 距離式の被演算子が（キャストで包まれていてもよい）文字列リテラルなら、その文字列を返す。
fn distance_operand_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::String(s) => Some(s.as_str()),
        Expr::Cast { expr, .. } => distance_operand_literal(expr),
        _ => None,
    }
}
