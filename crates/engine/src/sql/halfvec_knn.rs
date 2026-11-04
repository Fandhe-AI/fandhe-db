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
    OrderByForm, ScanOrderKey, SqlSurfaceError, ValidatedScan, ValidatedStatement,
};
use crate::sql::lexer::DistanceKind;
use crate::sql::plan::EvaluationOrder;
use crate::sql::udf_call::Expr;

/// `HALFVEC` 列を持つ表への距離演算子形 kNN を scan へ変換する。対象外は `Ok(None)`。
///
/// 演算子の範囲は VECTOR の kNN と同じ `<=>`（コサイン）・`<#>`（負の内積）だけで、`<->`／`<+>` は
/// `sql::exec::ensure_distance_kind_executable` と同じ `0A000` で変換の前に拒否する。
pub(crate) fn halfvec_select_as_scan(
    v: &ValidatedStatement,
    schema: &TableSchema,
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
