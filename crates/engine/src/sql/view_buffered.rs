//! 評価後射影形ビュー（本文に `LIMIT`・`ORDER BY`・集計・JOIN を含む
//! `CREATE VIEW`。TABLE-18・Issue #1192）の外側処理。
//!
//! 責務境界: `core.rs` の `Statement::BufferedView` アームが、ビュー本文
//! （`Scan`／`Aggregate`／`Join`／`SetOperation`）を**参照したセッション自身**の
//! `PolicyContext` で既存の実行経路により評価して [`QueryResult`] を得た後、本
//! モジュールが外側クエリの後処理（`WHERE` の絞り込み・`ORDER BY` の並べ替え・
//! 列射影・`LIMIT`／`OFFSET` の切り出し。Issue #1360）を**評価済みセル**に対して
//! 行う（第 2 の実行器を作らない。cursor の DECLARE/FETCH と同じく「内側の文を既存
//! 経路で実行し、結果を後処理する」方式）。Describe（拡張クエリ）は
//! [`plan_outer`] だけを呼び（本文の列メタデータに対して束縛・解決を行い）、本文は
//! 実行しない。Execute と Describe が同じ [`plan_outer`] を通るため、束縛エラーが
//! 両経路で一致する。
//!
//! 外側 `WHERE` は本文の結果列から合成したスキーマ（`TableSchema`）に対して
//! 既存の束縛器（`sql::parser::bind_scan`）で宣言的フィルタへ束縛し、評価は
//! `sql::join` が持つ評価済みセル→`ScalarRef` アダプタと同じ述語評価部品を再利用
//! する。外側 `ORDER BY` は `sql::join::values` の比較部品（NULL 位置規約つき）で
//! 安定ソートする。
//!
//! RLS-10 (b) の不変条件: 本モジュールは行の可視性判定に一切関与しない
//! （`PolicyContext` を受け取らない）。可視行の確定は本文の実行経路が参照
//! セッションの `ctx` で行うため、作成者の可視性は構造的に引き継がれない。
//! 外側 `WHERE`／`ORDER BY` が解決できるのは本文の**結果列**だけで、ビューが公開
//! していない物理キー（`id` 等）では絞り込み・並べ替えできない（filter oracle の
//! 防止）。
//!
//! 順序: `ORDER BY` が無ければ本文の順序（`ORDER BY`／`GROUP BY`／JOIN が固定した
//! 決定的な順序）をそのまま保つ。外側 `ORDER BY` は安定ソート（`sort_by`）で、
//! 同値のときは本文の順序を保つ（sort-determinism 規約。`sort_unstable*` は使わない）。

use std::cmp::Ordering;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::ScalarRef;

use super::allowlist::{
    Projection, ScalarOrderKey, SqlSurfaceError, ValidatedBufferedView, ValidatedScan,
};
use super::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use super::expr_program::StackValue;
use super::join::values::{cell_scalar, cmp_class, cmp_val, compare_order, CmpClass, CmpVal};
use super::udf_call::{ExprValue, UdfRegistry};

/// 列メタデータの名前（疑似列 `id` は `"id"`）。
fn column_name(meta: &ColumnMeta) -> &str {
    match meta {
        ColumnMeta::Id => "id",
        ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name,
    }
}

/// 外側の射影を本文の結果列に対して解決し、選択する列インデックスと結果列
/// メタデータを返す。`*` は本文の全列（重複名があってもそのまま通す）。列名指定は
/// 本文の結果列名で解決し、存在しない名前は `22000`、本文に同名列が複数あり
/// 一意に決まらない名前は `42702`（fail-closed。どちらか一方を黙って選ばない）。
pub(crate) fn resolve_projection(
    body_columns: &[ColumnMeta],
    projection: &Projection,
) -> Result<(Vec<usize>, Vec<ColumnMeta>), SqlSurfaceError> {
    match projection {
        Projection::All => Ok(((0..body_columns.len()).collect(), body_columns.to_vec())),
        Projection::Columns(names) => {
            let mut indices = Vec::with_capacity(names.len());
            let mut metas = Vec::with_capacity(names.len());
            for name in names {
                let mut found: Option<usize> = None;
                for (i, meta) in body_columns.iter().enumerate() {
                    if column_name(meta) == name {
                        if found.is_some() {
                            return Err(SqlSurfaceError::ambiguous_column(name.clone()));
                        }
                        found = Some(i);
                    }
                }
                let idx = found.ok_or_else(|| SqlSurfaceError::InvalidInput {
                    detail: format!("unknown column: {name}"),
                })?;
                let meta = body_columns.get(idx).cloned().ok_or_else(internal_error)?;
                indices.push(idx);
                metas.push(meta);
            }
            Ok((indices, metas))
        }
        // 構造検証段（`validate_select_statement`）が式項目を拒否済みのため
        // 到達しない（fail-closed）。
        Projection::Items(_) => Err(SqlSurfaceError::unsupported(
            "expression projection items are not supported on this view",
        )),
    }
}

fn internal_error() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 外側 `ORDER BY` の 1 キー（本文の結果列の位置・比較クラス・降順指定）。
struct OrderKeyPlan {
    index: usize,
    class: CmpClass,
    descending: bool,
}

/// 外側 `WHERE` の束縛結果。`bound` は本文の結果列から合成したスキーマに束縛した
/// フィルタで、`types` は結果列ごとの評価用の型（合成スキーマに載せられない列は
/// `None`＝参照不能）。合成スキーマの列位置は本文の結果列位置と一致させる。
struct FilterPlan {
    bound: super::parser::BoundScan,
    types: Vec<Option<ColumnType>>,
    /// 述語が参照する結果列か（`types` と同じ位置。大文字小文字無視の名前一致で
    /// 判定し、取りこぼしより過剰側へ倒す）。未参照列は評価に使われないため
    /// 値域変換（`22003`）の対象にしない。
    referenced: Vec<bool>,
}

/// 外側の後処理の計画（[`plan_outer`] が Execute・Describe で共有する）。
pub(crate) struct OuterPlan {
    indices: Vec<usize>,
    columns: Vec<ColumnMeta>,
    filter: Option<FilterPlan>,
    order: Vec<OrderKeyPlan>,
    limit: usize,
    offset: usize,
}

impl OuterPlan {
    /// 外側の射影後の結果列メタデータ（Describe の応答になる）。
    pub(crate) fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
}

/// 外側の `WHERE`・`ORDER BY`・射影・`LIMIT`／`OFFSET` を、本文の結果列に対して
/// 束縛・解決する（Issue #1360）。エラーの優先順位は射影（`22000`／`42702`）→
/// `WHERE` の束縛 → `ORDER BY` の解決。本文の結果行は見ない（Describe からも呼ぶ）。
pub(crate) fn plan_outer(
    body_columns: &[ColumnMeta],
    view: &ValidatedBufferedView,
    udfs: &UdfRegistry,
) -> Result<OuterPlan, SqlSurfaceError> {
    let (indices, columns) = resolve_projection(body_columns, &view.projection)?;
    let filter = plan_filter(body_columns, view, udfs)?;
    let order = plan_order(body_columns, &view.order_by)?;
    Ok(OuterPlan {
        indices,
        columns,
        filter,
        order,
        limit: usize::try_from(view.limit).map_err(|_| internal_error())?,
        offset: usize::try_from(view.offset).map_err(|_| internal_error())?,
    })
}

/// 本文の結果列のうち、述語が評価できる型（`VECTOR`・`ARRAY` 以外）を返す。
fn evaluable_type(meta: &ColumnMeta) -> Option<&ColumnType> {
    // 公開されている `id` 結果列は NULL を取りうる（LEFT JOIN の右辺・集合演算）ため、
    // 疑似列ではなく NULL 可能な `BIGINT` の実列として合成スキーマへ載せる
    // （束縛器は同名の実列を疑似列 `id` より優先する。NULL を 0 として評価しない）。
    static ID_TYPE: ColumnType = ColumnType::BigInt;
    let ty = match meta {
        ColumnMeta::Id => &ID_TYPE,
        ColumnMeta::Scalar { ty, .. } => ty,
        ColumnMeta::Computed { ty: Some(ty), .. } => ty,
        ColumnMeta::Computed { ty: None, .. } => return None,
    };
    match ty {
        ColumnType::Vector(_) | ColumnType::Array(_) => None,
        _ => Some(ty),
    }
}

/// 外側 `WHERE` の束縛。述語が参照する名前が本文の結果列に複数ある場合は `42702`
/// （どちらか一方を黙って選ばない）、無ければ束縛器の `22000`。合成スキーマに載らない
/// 列（`id` メタ・型なし列・`VECTOR`／`ARRAY`）は到達できないプレースホルダ名にして、
/// 参照を `22000` で拒否する（ビューが公開していない物理キーでの絞り込みや、型の
/// 取り違えを作らない）。
fn plan_filter(
    body_columns: &[ColumnMeta],
    view: &ValidatedBufferedView,
    udfs: &UdfRegistry,
) -> Result<Option<FilterPlan>, SqlSurfaceError> {
    if view.where_predicates.is_empty() {
        return Ok(None);
    }
    let mut idents = std::collections::HashSet::new();
    super::parser::collect_where_predicate_idents(&view.where_predicates, &mut idents);
    for ident in &idents {
        let count = body_columns
            .iter()
            .filter(|m| column_name(m) == ident.as_str())
            .count();
        if count > 1 {
            return Err(SqlSurfaceError::ambiguous_column(ident.clone()));
        }
    }
    // 束縛器は `id` を（同名の実列が無ければ）疑似列として解決してしまうため、
    // 本文が `id` を結果列として公開していない限り `id` の参照を拒否する
    // （ビューが公開していない物理キーでの絞り込みを許さない）。
    if idents.contains("id") && !body_columns.iter().any(|m| column_name(m) == "id") {
        return Err(SqlSurfaceError::InvalidInput {
            detail: "unknown column: id".to_string(),
        });
    }
    let mut defs = Vec::with_capacity(body_columns.len());
    let mut types = Vec::with_capacity(body_columns.len());
    for (i, meta) in body_columns.iter().enumerate() {
        match evaluable_type(meta) {
            Some(ty) => {
                defs.push(ColumnDef::new(column_name(meta), ty.clone(), true));
                types.push(Some(ty.clone()));
            }
            None => {
                // NUL 文字を含む名前は SQL の識別子として書けないため参照できない。
                defs.push(ColumnDef::new(
                    format!("\u{0}unreachable#{i}"),
                    ColumnType::Text,
                    true,
                ));
                types.push(None);
            }
        }
    }
    let schema = TableSchema::new(view.view_name.clone(), defs);
    let scan = ValidatedScan {
        table_name: view.view_name.clone(),
        projection: Projection::All,
        where_predicates: view.where_predicates.clone(),
        limit: 1,
        order_by: Vec::new(),
        offset: 0,
        window_items: Vec::new(),
        order_keys: Vec::new(),
    };
    let bound = super::parser::bind_scan(&scan, &schema, udfs)?;
    let referenced = body_columns
        .iter()
        .map(|m| {
            idents
                .iter()
                .any(|ident| column_name(m).eq_ignore_ascii_case(ident))
        })
        .collect();
    Ok(Some(FilterPlan {
        bound,
        types,
        referenced,
    }))
}

/// 外側 `ORDER BY` のキーを本文の結果列名で解決する。未知列は `22000`、同名の結果列が
/// 複数ある名前は `42702`、並べ替えできない型（`VECTOR`・`ARRAY`・`JSON`・型なし）は
/// `22000`。疑似列 `id` は本文が `id` 結果列（`ColumnMeta::Id`）を公開している
/// ときだけ解決できる。
fn plan_order(
    body_columns: &[ColumnMeta],
    keys: &[ScalarOrderKey],
) -> Result<Vec<OrderKeyPlan>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let mut found: Option<usize> = None;
        for (i, meta) in body_columns.iter().enumerate() {
            if column_name(meta) == key.column {
                if found.is_some() {
                    return Err(SqlSurfaceError::ambiguous_column(key.column.clone()));
                }
                found = Some(i);
            }
        }
        let index = found.ok_or_else(|| SqlSurfaceError::InvalidInput {
            detail: format!("unknown column: {}", key.column),
        })?;
        let unsupported = || {
            SqlSurfaceError::invalid_input(format!(
                "unsupported ORDER BY column type: {}",
                key.column
            ))
        };
        let class = match body_columns.get(index).ok_or_else(internal_error)? {
            ColumnMeta::Id => cmp_class(None),
            ColumnMeta::Scalar { ty, .. } | ColumnMeta::Computed { ty: Some(ty), .. } => {
                cmp_class(Some(ty))
            }
            ColumnMeta::Computed { ty: None, .. } => None,
        }
        .ok_or_else(unsupported)?;
        out.push(OrderKeyPlan {
            index,
            class,
            descending: key.descending,
        });
    }
    Ok(out)
}

/// 述語評価用に本文のセルを列の宣言型へ合わせる。集計本文は `COUNT` を
/// `Cell::Integer(u64)`＋`BIGINT`、`SUM(id)` 等を `Cell::Integer`＋`NUMERIC` で返すが、
/// 述語アダプタ（`cell_scalar`）は `BIGINT` に符号付き整数、`NUMERIC` に十進数しか
/// 受け付けない。変換できない値は `22003`（黙って NULL にしない）。それ以外のセルは
/// そのまま通す（不整合は `cell_scalar` が `Internal` で拒否する）。
fn normalize_cell<'a>(
    cell: &'a Cell,
    ty: &ColumnType,
) -> Result<std::borrow::Cow<'a, Cell>, SqlSurfaceError> {
    use std::borrow::Cow;
    let Cell::Integer(u) = cell else {
        return Ok(Cow::Borrowed(cell));
    };
    let out_of_range = || SqlSurfaceError::numeric_out_of_range("integer value out of range");
    Ok(match ty {
        ColumnType::Integer | ColumnType::BigInt => Cow::Owned(Cell::SignedInteger(
            i64::try_from(*u).map_err(|_| out_of_range())?,
        )),
        ColumnType::Numeric { .. } => Cow::Owned(Cell::Numeric(
            crate::numeric::Decimal::from_parts(i128::from(*u), 0).map_err(|_| out_of_range())?,
        )),
        ColumnType::Real | ColumnType::Double => Cow::Owned(Cell::Float(*u as f64)),
        _ => Cow::Borrowed(cell),
    })
}

/// 1 行が外側 `WHERE` を満たすか。`scanned` は合成スキーマ長で、評価できない列
/// （型なし・`VECTOR`・`ARRAY`・`id` メタ）は `None`（束縛で参照が拒否済み）。
fn row_matches(
    plan: &FilterPlan,
    row: &ResultRow,
    scratch: &mut Vec<StackValue>,
) -> Result<bool, SqlSurfaceError> {
    let mut normalized: Vec<Option<std::borrow::Cow<'_, Cell>>> =
        Vec::with_capacity(plan.types.len());
    for ((cell, ty), referenced) in row
        .cells
        .iter()
        .zip(plan.types.iter())
        .zip(plan.referenced.iter())
    {
        // 述語が参照しない列は変換しない（`i64::MAX` 超の `COUNT` 等が未参照列に
        // あっても、その列を使わない述語を `22003` で失敗させない）。
        normalized.push(match ty {
            Some(ty) if *referenced => Some(normalize_cell(cell, ty)?),
            _ => None,
        });
    }
    // 疑似列 `id` は合成スキーマの実列（本文が公開する `id` 結果列）としてだけ参照
    // でき、本文の行 id は述語・式に渡さない（公開していない物理キーの参照は束縛前に
    // 拒否済み）。
    let id = 0u64;
    let mut scanned: Vec<Option<ScalarRef<'_>>> = vec![None; plan.types.len()];
    for ((slot, cell), ty) in scanned
        .iter_mut()
        .zip(normalized.iter())
        .zip(plan.types.iter())
    {
        if let (Some(cell), Some(ty)) = (cell, ty) {
            *slot = cell_scalar(cell.as_ref(), ty)?;
        }
    }
    if !crate::declarative_filter::matches_all(&plan.bound.metadata_filters, &scanned) {
        return Ok(false);
    }
    for group in &plan.bound.or_filters {
        if !group.matches(&scanned, id, &[], 0, scratch)? {
            return Ok(false);
        }
    }
    // 式述語（`n >= 2` のような整数・浮動小数の比較は式として束縛される）。NULL
    // （UNKNOWN）は非該当（`sql::scan` と同じ扱い）。`VECTOR` 列は合成スキーマに
    // 載せないため embedding は空。
    for program in &plan.bound.expr_filter_programs {
        match program.eval(id, &[], &scanned, scratch)? {
            ExprValue::Bool(true) => {}
            ExprValue::Bool(false) | ExprValue::Null => return Ok(false),
            _ => {
                return Err(SqlSurfaceError::invalid_input(
                    "WHERE expression did not evaluate to a boolean",
                ))
            }
        }
    }
    Ok(true)
}

/// 外側 `ORDER BY` 用の比較値。`COUNT` 等の `Cell::Integer`（`u64`）が浮動小数・
/// `NUMERIC` クラスの列に載る場合は、クラスの値へ寄せる。
fn order_val<'a>(cell: &'a Cell, class: &CmpClass) -> Result<Option<CmpVal<'a>>, SqlSurfaceError> {
    match (class, cell) {
        (CmpClass::Numeric, Cell::Integer(u)) => Ok(Some(CmpVal::Num(
            crate::numeric::Decimal::from_parts(i128::from(*u), 0)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("integer value out of range"))?,
        ))),
        (CmpClass::Float, Cell::Integer(u)) => Ok(Some(CmpVal::Float(*u as f64))),
        _ => cmp_val(cell, class),
    }
}

/// 本文の実行結果に外側の `WHERE`・`ORDER BY`・列射影・`OFFSET`／`LIMIT` を適用する
/// （適用順は WHERE → ORDER BY → OFFSET／LIMIT → 射影。PostgreSQL と同じ）。
/// `ORDER BY` は安定ソートで、同値の行は本文の順序を保つ。列インデックスが
/// 行のセル数を超える場合は内部エラー（fail-closed）。
pub(crate) fn apply_outer(
    result: QueryResult,
    plan: &OuterPlan,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut rows = result.rows;
    if let Some(filter) = &plan.filter {
        let mut scratch: Vec<StackValue> = Vec::new();
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if row_matches(filter, &row, &mut scratch)? {
                kept.push(row);
            }
        }
        rows = kept;
    }
    if !plan.order.is_empty() {
        rows = sort_rows(rows, &plan.order)?;
    }
    let mut out_rows = Vec::new();
    for row in rows.into_iter().skip(plan.offset).take(plan.limit) {
        let mut cells = Vec::with_capacity(plan.indices.len());
        for &i in &plan.indices {
            cells.push(row.cells.get(i).cloned().ok_or_else(internal_error)?);
        }
        out_rows.push(ResultRow {
            id: row.id,
            score: row.score,
            cells,
        });
    }
    Ok(QueryResult {
        columns: plan.columns.clone(),
        rows: out_rows,
    })
}

/// 行を外側 `ORDER BY` のキーで安定ソートする。キーの比較値は先に行ごとへ展開し
/// （比較器からエラーを返せないため）、行の添字列だけを `sort_by`（安定）で並べ替える。
/// 比較値の個数は「行数 × キー数（`MAX_SCALAR_ORDER_KEYS` 以下）」で、行数は本文の
/// 結果行数（既存の予算で上限済み）に等しい。
fn sort_rows(
    rows: Vec<ResultRow>,
    order: &[OrderKeyPlan],
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let idx = {
        let mut keys: Vec<Vec<Option<CmpVal<'_>>>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut k = Vec::with_capacity(order.len());
            for o in order {
                let cell = row.cells.get(o.index).ok_or_else(internal_error)?;
                k.push(order_val(cell, &o.class)?);
            }
            keys.push(k);
        }
        let mut idx: Vec<usize> = (0..rows.len()).collect();
        idx.sort_by(|&a, &b| {
            let (Some(ka), Some(kb)) = (keys.get(a), keys.get(b)) else {
                return Ordering::Equal;
            };
            for (o, (va, vb)) in order.iter().zip(ka.iter().zip(kb.iter())) {
                let ord = compare_order(va.as_ref(), vb.as_ref(), o.descending);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        idx
    };
    let mut slots: Vec<Option<ResultRow>> = rows.into_iter().map(Some).collect();
    let mut sorted = Vec::with_capacity(slots.len());
    for i in idx {
        sorted.push(
            slots
                .get_mut(i)
                .and_then(Option::take)
                .ok_or_else(internal_error)?,
        );
    }
    Ok(sorted)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::allowlist::{Statement, WherePredicate};
    use crate::sql::udf_call::{BinOp, Expr};

    fn text_col(name: &str) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: name.to_string(),
            ty: ColumnType::Text,
        }
    }

    fn count_col(name: &str) -> ColumnMeta {
        ColumnMeta::Computed {
            name: name.to_string(),
            ty: Some(ColumnType::BigInt),
        }
    }

    /// `a`（TEXT）・`count`（BIGINT。`COUNT` と同じ `Cell::Integer` で持つ）の 5 行
    /// （`count` は 0..5、`a` は `r0..r4`）。
    fn result() -> QueryResult {
        QueryResult {
            columns: vec![text_col("a"), count_col("count")],
            rows: (0..5)
                .map(|i| ResultRow {
                    id: i,
                    score: 0.0,
                    cells: vec![Cell::Text(format!("r{i}")), Cell::Integer(i)],
                })
                .collect(),
        }
    }

    fn view(
        projection: Projection,
        where_predicates: Vec<WherePredicate>,
        order_by: Vec<ScalarOrderKey>,
        limit: u32,
        offset: u32,
    ) -> ValidatedBufferedView {
        ValidatedBufferedView {
            view_name: "v".to_string(),
            // 後処理の単体テストでは本文を実行しないため、任意の文でよい。
            body: Box::new(Statement::SetSearchMode {
                value: "recall".to_string(),
            }),
            projection,
            where_predicates,
            order_by,
            limit,
            offset,
        }
    }

    fn run(r: QueryResult, v: &ValidatedBufferedView) -> Result<QueryResult, SqlSurfaceError> {
        let plan = plan_outer(&r.columns, v, &UdfRegistry::default())?;
        apply_outer(r, &plan)
    }

    /// `count >= n`（整数の比較はパーサーが式述語として生成する）。
    fn count_ge(n: u32) -> WherePredicate {
        WherePredicate::Expression(Expr::Binary {
            op: BinOp::Ge,
            lhs: Box::new(Expr::Ident("count".to_string())),
            rhs: Box::new(Expr::Number(n.to_string())),
        })
    }

    fn key(column: &str, descending: bool) -> ScalarOrderKey {
        ScalarOrderKey {
            column: column.to_string(),
            descending,
        }
    }

    #[test]
    fn slices_offset_and_limit_in_place() {
        let out = run(result(), &view(Projection::All, vec![], vec![], 2, 1)).unwrap();
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0].id, 1);
        assert_eq!(out.rows[1].id, 2);
    }

    #[test]
    fn offset_beyond_rows_yields_empty() {
        let out = run(result(), &view(Projection::All, vec![], vec![], 10, 99)).unwrap();
        assert!(out.rows.is_empty());
        assert_eq!(out.columns.len(), 2);
    }

    #[test]
    fn projects_named_columns_in_requested_order() {
        let p = Projection::Columns(vec!["count".to_string(), "a".to_string()]);
        let out = run(result(), &view(p, vec![], vec![], 1, 0)).unwrap();
        assert_eq!(out.rows[0].cells[0], Cell::Integer(0));
        assert_eq!(out.rows[0].cells[1], Cell::Text("r0".to_string()));
    }

    #[test]
    fn unknown_column_is_22000() {
        let p = Projection::Columns(vec!["nope".to_string()]);
        let err = run(result(), &view(p, vec![], vec![], 1, 0)).err().unwrap();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn duplicate_body_column_name_is_42702() {
        let mut r = result();
        r.columns.push(text_col("a"));
        for row in &mut r.rows {
            row.cells.push(Cell::Null);
        }
        let p = Projection::Columns(vec!["a".to_string()]);
        let err = run(r, &view(p, vec![], vec![], 1, 0)).err().unwrap();
        assert_eq!(err.wire_code(), "42702");
    }

    /// 外側 WHERE は本文の `LIMIT` の後の行（評価済みセル）を絞り込む。`COUNT` 型の
    /// `Cell::Integer`（`u64`）は `BIGINT` として比較できる（正規化）。
    #[test]
    fn where_filters_evaluated_cells_including_count_integers() {
        let preds = vec![count_ge(3)];
        let out = run(result(), &view(Projection::All, preds, vec![], 10, 0)).unwrap();
        let ids: Vec<u64> = out.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![3, 4]);
        let preds = vec![WherePredicate::Equality {
            column: "a".to_string(),
            value: "r2".to_string(),
        }];
        let out = run(result(), &view(Projection::All, preds, vec![], 10, 0)).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].id, 2);
    }

    #[test]
    fn where_on_unexposed_or_missing_column_is_22000() {
        // 本文の結果列に無い名前（物理キー `id` を含む）は解決できない。
        for col in ["id", "nope"] {
            let preds = vec![WherePredicate::Equality {
                column: col.to_string(),
                value: "1".to_string(),
            }];
            let err = run(result(), &view(Projection::All, preds, vec![], 10, 0))
                .err()
                .unwrap();
            assert_eq!(err.wire_code(), "22000", "column={col}");
        }
    }

    #[test]
    fn where_on_duplicate_body_column_is_42702() {
        let mut r = result();
        r.columns.push(text_col("a"));
        for row in &mut r.rows {
            row.cells.push(Cell::Null);
        }
        let preds = vec![WherePredicate::Equality {
            column: "a".to_string(),
            value: "r1".to_string(),
        }];
        let err = run(r, &view(Projection::All, preds, vec![], 10, 0))
            .err()
            .unwrap();
        assert_eq!(err.wire_code(), "42702");
    }

    #[test]
    fn where_on_vector_column_is_rejected_not_internal() {
        let mut r = result();
        r.columns.push(ColumnMeta::Scalar {
            name: "emb".to_string(),
            ty: ColumnType::Vector(2),
        });
        for row in &mut r.rows {
            row.cells.push(Cell::Null);
        }
        let preds = vec![WherePredicate::IsNull {
            column: "emb".to_string(),
            negated: false,
        }];
        let err = run(r, &view(Projection::All, preds, vec![], 10, 0))
            .err()
            .unwrap();
        assert_eq!(err.wire_code(), "22000");
    }

    #[test]
    fn integer_overflow_during_normalization_is_22003() {
        let mut r = result();
        if let Some(row) = r.rows.get_mut(0) {
            row.cells[1] = Cell::Integer(u64::MAX);
        }
        let preds = vec![count_ge(0)];
        let err = run(r, &view(Projection::All, preds, vec![], 10, 0))
            .err()
            .unwrap();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn unreferenced_overflowing_column_does_not_fail_where() {
        let mut r = result();
        if let Some(row) = r.rows.get_mut(0) {
            row.cells[1] = Cell::Integer(u64::MAX);
        }
        let preds = vec![WherePredicate::Equality {
            column: "a".to_string(),
            value: "r0".to_string(),
        }];
        let out = run(r, &view(Projection::All, preds, vec![], 10, 0)).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].cells[1], Cell::Integer(u64::MAX));
    }

    #[test]
    fn order_by_sorts_stably_and_places_nulls_by_direction() {
        let mut r = result();
        // `count` を 1,1,NULL,0,0 にする（同値の行は本文の順序を保つ）。
        let counts = [Some(1u64), Some(1), None, Some(0), Some(0)];
        for (row, c) in r.rows.iter_mut().zip(counts) {
            row.cells[1] = c.map_or(Cell::Null, Cell::Integer);
        }
        let asc = run(
            r.clone(),
            &view(Projection::All, vec![], vec![key("count", false)], 10, 0),
        )
        .unwrap();
        let ids: Vec<u64> = asc.rows.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![3, 4, 0, 1, 2],
            "ASC: NULL last, ties keep body order"
        );
        let desc = run(
            r,
            &view(Projection::All, vec![], vec![key("count", true)], 10, 0),
        )
        .unwrap();
        let ids: Vec<u64> = desc.rows.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![2, 0, 1, 3, 4],
            "DESC: NULL first, ties keep body order"
        );
    }

    #[test]
    fn order_by_unknown_or_unexposed_id_is_22000_and_applies_before_limit() {
        for col in ["id", "nope"] {
            let err = run(
                result(),
                &view(Projection::All, vec![], vec![key(col, false)], 10, 0),
            )
            .err()
            .unwrap();
            assert_eq!(err.wire_code(), "22000", "column={col}");
        }
        // ORDER BY は LIMIT の前に適用される。
        let out = run(
            result(),
            &view(Projection::All, vec![], vec![key("a", true)], 2, 0),
        )
        .unwrap();
        let ids: Vec<u64> = out.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![4, 3]);
    }

    #[test]
    fn order_by_resolves_an_exposed_id_column() {
        let mut r = result();
        r.columns.push(ColumnMeta::Id);
        for (i, row) in r.rows.iter_mut().enumerate() {
            row.cells.push(Cell::Integer(10 - i as u64));
        }
        let out = run(
            r,
            &view(Projection::All, vec![], vec![key("id", false)], 10, 0),
        )
        .unwrap();
        let ids: Vec<u64> = out.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![4, 3, 2, 1, 0]);
    }

    /// 公開された `id` 結果列の NULL は 0 として評価されない（`id = 0` に一致しない）。
    #[test]
    fn where_on_exposed_id_column_treats_null_as_unknown() {
        let mut r = result();
        r.columns.push(ColumnMeta::Id);
        for (i, row) in r.rows.iter_mut().enumerate() {
            row.cells.push(if i % 2 == 0 {
                Cell::Null
            } else {
                Cell::Integer(i as u64)
            });
        }
        let eq = |n: u32| {
            WherePredicate::Expression(Expr::Binary {
                op: BinOp::Eq,
                lhs: Box::new(Expr::Ident("id".to_string())),
                rhs: Box::new(Expr::Number(n.to_string())),
            })
        };
        let out = run(
            r.clone(),
            &view(Projection::All, vec![eq(0)], vec![], 10, 0),
        )
        .unwrap();
        assert!(out.rows.is_empty());
        let out = run(r, &view(Projection::All, vec![eq(3)], vec![], 10, 0)).unwrap();
        assert_eq!(out.rows.len(), 1);
    }
}
