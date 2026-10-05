//! 評価後射影形ビュー（本文に `LIMIT`・`ORDER BY`・集計・JOIN を含む
//! `CREATE VIEW`。TABLE-18・Issue #1192）の外側処理。
//!
//! 責務境界: `core.rs` の `Statement::BufferedView` アームが、ビュー本文
//! （`Scan`／`Aggregate`／`Join`／`SetOperation`）を**参照したセッション自身**の
//! `PolicyContext` で既存の実行経路により評価して [`QueryResult`] を得た後、本
//! モジュールが外側クエリの後処理を**評価済みセル**に対して行う（第 2 の実行器を
//! 作らない。cursor の DECLARE/FETCH と同じく「内側の文を既存経路で実行し、結果を
//! 後処理する」方式）。外側の形は 2 つある（[`BufferedOuter`]）。
//!
//! - 行形（Issue #1360・Issue #1411）: `WHERE` の絞り込み・ウィンドウ関数・`ORDER BY`
//!   （列キーと式キー）の並べ替え・列射影・`LIMIT`／`OFFSET` の切り出し。ウィンドウ関数は
//!   `sql::window::CellWindowEvaluator`（ストレージ走査経路と同じキー抽出・状態予算・
//!   パーティション評価）へ、外側 `WHERE` を通った行だけを母集合として流し込む。
//! - 集計形（Issue #1411）: `COUNT`／`SUM`／`AVG`／`MIN`／`MAX`・`GROUP BY`・`HAVING`・
//!   `SELECT DISTINCT`（脱糖後の集計形）。`sql::group_by::GroupedRowAccumulator`（ストレージ
//!   走査経路と同じグループ表・予算・`HAVING`・`ORDER BY` の終端処理）へ評価済み行を
//!   1 行ずつ流し込む。
//!
//! Describe（拡張クエリ）は [`plan_outer`] だけを呼び（本文の列メタデータに対して束縛・
//! 解決を行い）、本文は実行しない。Execute と Describe が同じ [`plan_outer`] を通る
//! ため、束縛エラーが両経路で一致する。
//!
//! 外側の式・述語・集計は、本文の結果列から合成したスキーマ（`TableSchema`。列の位置は
//! 本文の結果列の位置と一致させる）に対して既存の束縛器（`sql::parser::bind_scan`・
//! `bind_aggregate`）で束縛し、評価は評価済みセル→`ScalarRef` アダプタ
//! （`sql::join::values::cell_scalar`）経由で既存の述語・式・アキュムレータを再利用する。
//!
//! RLS-10 (b) の不変条件: 本モジュールは行の可視性判定に一切関与しない
//! （`PolicyContext` を受け取らない）。可視行の確定は本文の実行経路が参照
//! セッションの `ctx` で行うため、作成者の可視性は構造的に引き継がれない。外側の集計・
//! ソートの母集合は RLS 適用後の本文結果だけで、他テナント行はグループ・件数に現れない。
//! 外側が解決できるのは本文の**結果列**だけで、ビューが公開していない物理キー
//! （`id` 等）では絞り込み・並べ替え・グループ化・集計できない（filter oracle の
//! 防止。[`check_outer_scope`]）。
//!
//! 順序: `ORDER BY` が無ければ本文の順序（`ORDER BY`／`GROUP BY`／JOIN が固定した
//! 決定的な順序）をそのまま保つ。外側 `ORDER BY` は安定ソート（`sort_by`）で、
//! 同値のときは本文の順序を保つ（sort-determinism 規約。`sort_unstable*` は使わない）。

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::ScalarRef;

use super::allowlist::{
    AggregateArg, AggregateSelectItem, BufferedOuter, BufferedRows, Projection, ScalarOrderKey,
    ScanOrderKey, SqlSurfaceError, ValidatedAggregate, ValidatedBufferedView, ValidatedScan,
};
use super::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use super::expr_program::StackValue;
use super::join::values::{cell_scalar, cmp_class, cmp_val, compare_order, CmpClass, CmpVal};
use super::order_value::{
    compare_order_key, expr_value_to_order_value, extract_order_value_ref, scalar_key_ref_to_owned,
    OrderValue,
};
use super::parser::{BoundAggregate, BoundOrderKey, BoundOrderTarget, BoundScan, BoundWindowItem};
use super::udf_call::{ExprValue, UdfRegistry};
use super::window::{window_result_type, CellWindowEvaluator};

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

/// 外側 `ORDER BY`（列キーのみ）の 1 キー（本文の結果列の位置・比較クラス・降順指定）。
struct OrderKeyPlan {
    index: usize,
    class: CmpClass,
    descending: bool,
}

/// 本文の結果列から合成したスキーマに束縛した外側の述語・式キー。`types` は結果列ごとの
/// 評価用の型（合成スキーマに載せられない列は `None`＝参照不能）。合成スキーマの列位置は
/// 本文の結果列位置と一致させる。
struct ScanPlan {
    schema: TableSchema,
    bound: BoundScan,
    types: Vec<Option<ColumnType>>,
    /// 外側が参照する結果列か（`types` と同じ位置。大文字小文字無視の名前一致で
    /// 判定し、取りこぼしより過剰側へ倒す）。未参照列は評価に使われないため
    /// 値域変換（`22003`）の対象にしない。
    referenced: Vec<bool>,
    /// 外側 `WHERE` があるか（無ければ行ごとの述語評価を省く）。
    has_where: bool,
}

/// 外側の並べ替え指定。
enum RowOrder {
    /// 並べ替えなし（本文の順序を保つ）。
    None,
    /// 列キーのみ（比較クラスは本文の結果列の型から決める。Issue #1360）。
    Columns(Vec<OrderKeyPlan>),
    /// 式キーを含む（`ScanPlan::bound` の `order_by`／`order_exprs` が正本。Issue #1411）。
    Keys,
}

/// 行形の外側の出力列 1 つの出所（Issue #1411。ウィンドウ項目は SELECT リスト内の位置へ
/// 差し込む）。
#[derive(Clone, Copy)]
enum OutCol {
    /// 本文の結果列（セルの添字）。
    Plain(usize),
    /// ウィンドウ項目（`ScanPlan::bound.windows()` の添字）。
    Window(usize),
}

/// 行形の外側の計画。
struct RowsPlan {
    out: Vec<OutCol>,
    scan: ScanPlan,
    order: RowOrder,
    limit: usize,
    offset: usize,
}

/// 集計形の外側の計画。
struct AggregatePlan {
    schema: TableSchema,
    bound: BoundAggregate,
    types: Vec<Option<ColumnType>>,
    referenced: Vec<bool>,
}

enum OuterKind {
    Rows(Box<RowsPlan>),
    Aggregate(Box<AggregatePlan>),
}

/// 外側の後処理の計画（[`plan_outer`] が Execute・Describe で共有する）。
pub(crate) struct OuterPlan {
    columns: Vec<ColumnMeta>,
    kind: OuterKind,
}

impl OuterPlan {
    /// 外側の後処理後の結果列メタデータ（Describe の応答になる）。
    pub(crate) fn columns(&self) -> &[ColumnMeta] {
        &self.columns
    }
}

/// 外側のクエリを、本文の結果列に対して束縛・解決する（Issue #1360・Issue #1411）。
/// 行形のエラーの優先順位は射影（`22000`／`42702`）→ 公開範囲（`22000`）→ 同名列
/// （`42702`）→ `WHERE`・`ORDER BY` の束縛。本文の結果行は見ない（Describe からも呼ぶ）。
pub(crate) fn plan_outer(
    body_columns: &[ColumnMeta],
    view: &ValidatedBufferedView,
    udfs: &UdfRegistry,
) -> Result<OuterPlan, SqlSurfaceError> {
    match &view.outer {
        BufferedOuter::Rows(rows) => plan_rows(body_columns, &view.view_name, rows, udfs),
        BufferedOuter::Aggregate(agg) => plan_aggregate(body_columns, agg, udfs),
    }
}

fn plan_rows(
    body_columns: &[ColumnMeta],
    view_name: &str,
    rows: &BufferedRows,
    udfs: &UdfRegistry,
) -> Result<OuterPlan, SqlSurfaceError> {
    let (indices, metas) = resolve_projection(body_columns, &rows.projection)?;
    let names = body_column_names(body_columns);
    // `WHERE`・`ORDER BY` がウィンドウ別名を参照する形は、ストレージ走査経路と同じく束縛段が
    // `42601` で拒否する（`22000` の「未知列」にしない）。そのため公開範囲の検査には別名も加える。
    // 別名と同名の列（`id` 等）を外側が参照できるようになるわけではない: 別名と同名の識別子は
    // 束縛段が拒否するか、実際の並べ替え・絞り込みが本文の結果列だけで解決する。
    let mut names_with_aliases = names.clone();
    for item in &rows.window_items {
        names_with_aliases.push(
            item.alias
                .clone()
                .unwrap_or_else(|| item.func.default_alias().to_string()),
        );
    }
    super::view::check_columns_within_view(
        Some(&names_with_aliases),
        &Projection::All,
        &rows.where_predicates,
        &rows.order_by,
    )?;
    super::view::check_order_exprs_within_view(Some(&names), &rows.order_keys)?;
    super::view::check_window_columns_within_view(Some(&names), &rows.window_items)?;
    let mut idents = HashSet::new();
    for item in &rows.window_items {
        idents.extend(item.partition_by.iter().cloned());
        idents.extend(item.order_by.iter().map(|(c, _)| c.clone()));
        if let Some(AggregateArg::Expr(e)) = &item.arg {
            super::parser::collect_expr_idents(e, &mut idents);
        }
    }
    super::parser::collect_where_predicate_idents(&rows.where_predicates, &mut idents);
    for key in &rows.order_by {
        idents.insert(key.column.clone());
    }
    for key in &rows.order_keys {
        match key {
            ScanOrderKey::Column(k) => {
                idents.insert(k.column.clone());
            }
            ScanOrderKey::Expr { expr, .. } => {
                super::parser::collect_expr_idents(expr, &mut idents)
            }
        }
    }
    check_unambiguous(body_columns, &idents)?;
    let scan = bind_scan_plan(body_columns, view_name, rows, &idents, udfs)?;
    let (out, columns) = assemble_output(&indices, metas, scan.bound.windows())?;
    let order = if !rows.order_keys.is_empty() {
        RowOrder::Keys
    } else if !rows.order_by.is_empty() {
        RowOrder::Columns(plan_order(body_columns, &rows.order_by)?)
    } else {
        RowOrder::None
    };
    Ok(OuterPlan {
        columns,
        kind: OuterKind::Rows(Box::new(RowsPlan {
            out,
            scan,
            order,
            limit: usize::try_from(rows.limit).map_err(|_| internal_error())?,
            offset: usize::try_from(rows.offset).map_err(|_| internal_error())?,
        })),
    })
}

fn plan_aggregate(
    body_columns: &[ColumnMeta],
    agg: &ValidatedAggregate,
    udfs: &UdfRegistry,
) -> Result<OuterPlan, SqlSurfaceError> {
    let names = body_column_names(body_columns);
    super::view::check_aggregate_columns_within_view(
        Some(&names),
        &agg.items,
        agg.group_by.as_ref(),
        &agg.where_predicates,
    )?;
    // 外側が参照する本文の結果列（`WHERE`・集計引数・`GROUP BY` 列）。`HAVING`・`ORDER BY` の
    // 項目名はグループ出力の名前で、本文の結果列は参照しない（式内の識別子は
    // `check_aggregate_columns_within_view` が公開列に限定済み）。
    let mut idents = HashSet::new();
    super::parser::collect_where_predicate_idents(&agg.where_predicates, &mut idents);
    for item in &agg.items {
        match item {
            AggregateSelectItem::Aggregate(a) => {
                if let AggregateArg::Expr(e) = &a.arg {
                    super::parser::collect_expr_idents(e, &mut idents);
                }
            }
            AggregateSelectItem::GroupKey { column, .. } => {
                idents.insert(column.clone());
            }
        }
    }
    if let Some(gb) = &agg.group_by {
        for c in &gb.columns {
            idents.insert(c.clone());
        }
    }
    check_unambiguous(body_columns, &idents)?;
    let (schema, types) = synth_schema(&agg.table_name, body_columns);
    let referenced = referenced_mask(body_columns, &idents);
    let bound = super::parser::bind_aggregate(agg, &schema, udfs)?;
    let columns = super::aggregate::aggregate_projection_columns(&bound, &schema)?;
    Ok(OuterPlan {
        columns,
        kind: OuterKind::Aggregate(Box::new(AggregatePlan {
            schema,
            bound,
            types,
            referenced,
        })),
    })
}

/// 本文の結果列名の一覧（疑似列 `id` を公開している本文は `"id"` を含む）。外側が
/// 参照してよい列の許可集合として `sql::view` の列スコープ検査へ渡す。ビューが公開して
/// いない物理キー `id` は含まれず、外側の `id` 参照は `22000` で拒否される。
fn body_column_names(body_columns: &[ColumnMeta]) -> Vec<String> {
    body_columns
        .iter()
        .map(|m| column_name(m).to_string())
        .collect()
}

/// 外側が参照する識別子のうち、本文の結果列に同名が複数あるものは `42702`
/// （どちらか一方を黙って選ばない）。
fn check_unambiguous(
    body_columns: &[ColumnMeta],
    idents: &HashSet<String>,
) -> Result<(), SqlSurfaceError> {
    for ident in idents {
        let count = body_columns
            .iter()
            .filter(|m| column_name(m) == ident.as_str())
            .count();
        if count > 1 {
            return Err(SqlSurfaceError::ambiguous_column(ident.clone()));
        }
    }
    Ok(())
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

/// 本文の結果列から合成スキーマ（列位置は本文の結果列位置と一致）と、結果列ごとの評価用の
/// 型を作る。合成スキーマに載らない列（`id` メタ・型なし列・`VECTOR`／`ARRAY`）は到達できない
/// プレースホルダ名にして、参照を束縛段の `22000` で拒否する（ビューが公開していない物理
/// キーでの絞り込みや、型の取り違えを作らない）。
fn synth_schema(
    view_name: &str,
    body_columns: &[ColumnMeta],
) -> (TableSchema, Vec<Option<ColumnType>>) {
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
    (TableSchema::new(view_name.to_string(), defs), types)
}

/// 外側が参照する結果列のマスク（`synth_schema` の `types` と同じ位置。大文字小文字無視の
/// 名前一致で判定し、取りこぼしより過剰側へ倒す）。
fn referenced_mask(body_columns: &[ColumnMeta], idents: &HashSet<String>) -> Vec<bool> {
    body_columns
        .iter()
        .map(|m| {
            idents
                .iter()
                .any(|ident| column_name(m).eq_ignore_ascii_case(ident))
        })
        .collect()
}

/// 外側 `WHERE`・式キー付き `ORDER BY` の束縛（合成スキーマに対する [`super::parser::bind_scan`]）。
/// 参照名の公開範囲・同名列は呼び出し元が検査済み。
fn bind_scan_plan(
    body_columns: &[ColumnMeta],
    view_name: &str,
    rows: &BufferedRows,
    idents: &HashSet<String>,
    udfs: &UdfRegistry,
) -> Result<ScanPlan, SqlSurfaceError> {
    let (schema, types) = synth_schema(view_name, body_columns);
    // 外側の列 `ORDER BY`（`rows.order_by`）はウィンドウ別名の参照を拒否する束縛規則
    // （`sql::parser::bind_scan`）にだけ渡す（比較自体は [`plan_order`] が行う）。
    let scan = ValidatedScan {
        table_name: view_name.to_string(),
        projection: Projection::All,
        where_predicates: rows.where_predicates.clone(),
        limit: 1,
        order_by: rows.order_by.clone(),
        offset: 0,
        window_items: rows.window_items.clone(),
        scalar_subquery_items: Vec::new(),
        order_keys: rows.order_keys.clone(),
    };
    let bound = super::parser::bind_scan(&scan, &schema, udfs)?;
    Ok(ScanPlan {
        schema,
        bound,
        types,
        referenced: referenced_mask(body_columns, idents),
        has_where: !rows.where_predicates.is_empty(),
    })
}

/// 通常の射影列（本文の結果列 `indices`・メタデータ `metas`）とウィンドウ項目を、ウィンドウ項目の
/// SELECT リスト内位置（`BoundWindowItem::position`）に従って 1 本の出力列へ合成する
/// （ストレージ走査経路の `sql::window::build_result`・`sql::describe::scan_columns` と同じ
/// 割り当て規則）。位置が範囲外・重複なら束縛の不変条件違反として `Internal`。
fn assemble_output(
    indices: &[usize],
    metas: Vec<ColumnMeta>,
    windows: &[BoundWindowItem],
) -> Result<(Vec<OutCol>, Vec<ColumnMeta>), SqlSurfaceError> {
    if windows.is_empty() {
        return Ok((indices.iter().map(|&i| OutCol::Plain(i)).collect(), metas));
    }
    let total = indices
        .len()
        .checked_add(windows.len())
        .ok_or_else(internal_error)?;
    let mut slots: Vec<Option<(OutCol, Option<ColumnMeta>)>> = vec![None; total];
    for (k, item) in windows.iter().enumerate() {
        let slot = slots.get_mut(item.position).ok_or_else(internal_error)?;
        if slot.is_some() {
            return Err(internal_error());
        }
        *slot = Some((
            OutCol::Window(k),
            Some(ColumnMeta::Computed {
                name: item.name.clone(),
                ty: window_result_type(item),
            }),
        ));
    }
    let mut plain = indices.iter().copied().zip(metas);
    let mut out = Vec::with_capacity(total);
    let mut columns = Vec::with_capacity(total);
    for slot in slots {
        match slot {
            Some((col, Some(meta))) => {
                out.push(col);
                columns.push(meta);
            }
            Some((_, None)) => return Err(internal_error()),
            None => {
                let (index, meta) = plain.next().ok_or_else(internal_error)?;
                out.push(OutCol::Plain(index));
                columns.push(meta);
            }
        }
    }
    if plain.next().is_some() {
        return Err(internal_error());
    }
    Ok((out, columns))
}

/// 外側 `ORDER BY`（列キーのみ）のキーを本文の結果列名で解決する。未知列は `22000`、同名の
/// 結果列が複数ある名前は `42702`、並べ替えできない型（`VECTOR`・`ARRAY`・`JSON`・型なし）は
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
fn normalize_cell<'a>(cell: &'a Cell, ty: &ColumnType) -> Result<Cow<'a, Cell>, SqlSurfaceError> {
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

/// 1 行を合成スキーマ長の `scanned`（評価できない列〔型なし・`VECTOR`・`ARRAY`・`id` メタ〕と
/// 外側が参照しない列は `None`）へ変換して `f` に渡す。参照しない列は変換しない
/// （`i64::MAX` 超の `COUNT` 等が未参照列にあっても、その列を使わない外側を `22003` で
/// 失敗させない）。疑似列 `id` は合成スキーマの実列（本文が公開する `id` 結果列）としてだけ
/// 参照でき、本文の行 id は述語・式に渡さない。
fn with_scanned<T>(
    types: &[Option<ColumnType>],
    referenced: &[bool],
    row: &ResultRow,
    f: impl FnOnce(&[Option<ScalarRef<'_>>]) -> Result<T, SqlSurfaceError>,
) -> Result<T, SqlSurfaceError> {
    let mut normalized: Vec<Option<Cow<'_, Cell>>> = Vec::with_capacity(types.len());
    for ((cell, ty), referenced) in row.cells.iter().zip(types.iter()).zip(referenced.iter()) {
        normalized.push(match ty {
            Some(ty) if *referenced => Some(normalize_cell(cell, ty)?),
            _ => None,
        });
    }
    let mut scanned: Vec<Option<ScalarRef<'_>>> = vec![None; types.len()];
    for ((slot, cell), ty) in scanned.iter_mut().zip(normalized.iter()).zip(types.iter()) {
        if let (Some(cell), Some(ty)) = (cell, ty) {
            *slot = cell_scalar(cell.as_ref(), ty)?;
        }
    }
    f(&scanned)
}

/// `scanned` が外側 `WHERE`（宣言的フィルタ・`OR` 群・式述語）を満たすか。NULL（UNKNOWN）は
/// 非該当（`sql::scan` と同じ扱い）。`VECTOR` 列は合成スキーマに載せないため embedding は空。
fn where_matches(
    metadata_filters: &[crate::declarative_filter::MetadataFilter],
    or_filters: &[super::where_tree::BoundOrGroup],
    expr_programs: &[super::expr_program::ExprProgram],
    scanned: &[Option<ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
) -> Result<bool, SqlSurfaceError> {
    let id = 0u64;
    if !crate::declarative_filter::matches_all(metadata_filters, scanned) {
        return Ok(false);
    }
    for group in or_filters {
        if !group.matches(scanned, id, &[], 0, scratch)? {
            return Ok(false);
        }
    }
    for program in expr_programs {
        match program.eval(id, &[], scanned, scratch)? {
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

/// 本文の実行結果に外側の後処理を適用する。行形の適用順は WHERE → ORDER BY →
/// OFFSET／LIMIT → 射影（PostgreSQL と同じ）。`ORDER BY` は安定ソートで、同値の行は本文の
/// 順序を保つ。列インデックスが行のセル数を超える場合は内部エラー（fail-closed）。
/// 集計形は WHERE → 集計（`HAVING`・`ORDER BY`・`OFFSET`／`LIMIT` を含む）。
pub(crate) fn apply_outer(
    result: QueryResult,
    plan: &OuterPlan,
) -> Result<QueryResult, SqlSurfaceError> {
    match &plan.kind {
        OuterKind::Rows(rows) => apply_rows(result, rows, &plan.columns),
        OuterKind::Aggregate(agg) => apply_aggregate(result, agg),
    }
}

fn apply_rows(
    result: QueryResult,
    plan: &RowsPlan,
    columns: &[ColumnMeta],
) -> Result<QueryResult, SqlSurfaceError> {
    let mut rows = result.rows;
    if plan.scan.has_where {
        let mut scratch: Vec<StackValue> = Vec::new();
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let keep = with_scanned(&plan.scan.types, &plan.scan.referenced, &row, |scanned| {
                where_matches(
                    &plan.scan.bound.metadata_filters,
                    &plan.scan.bound.or_filters,
                    &plan.scan.bound.expr_filter_programs,
                    scanned,
                    &mut scratch,
                )
            })?;
            if keep {
                kept.push(row);
            }
        }
        rows = kept;
    }
    // ウィンドウ値は外側 `WHERE` を通った行を母集合に計算し、行ごとに本文のセルの後ろへ
    // 付ける（並べ替え・切り出しはその後。文全体の `ORDER BY` はウィンドウ計算の後）。
    let windows = plan.scan.bound.windows();
    let body_len = plan.scan.types.len();
    if !windows.is_empty() {
        let mut evaluator = CellWindowEvaluator::new(windows)?;
        for row in &rows {
            with_scanned(&plan.scan.types, &plan.scan.referenced, row, |scanned| {
                evaluator.push_row(scanned)
            })?;
        }
        let values = evaluator.finish()?;
        for (r, row) in rows.iter_mut().enumerate() {
            for item_values in &values {
                row.cells
                    .push(item_values.get(r).cloned().ok_or_else(internal_error)?);
            }
        }
    }
    match &plan.order {
        RowOrder::None => {}
        RowOrder::Columns(order) => rows = sort_rows(rows, order)?,
        RowOrder::Keys => rows = sort_rows_by_keys(rows, &plan.scan)?,
    }
    let mut out_rows = Vec::new();
    for row in rows.into_iter().skip(plan.offset).take(plan.limit) {
        let mut cells = Vec::with_capacity(plan.out.len());
        for col in &plan.out {
            let index = match col {
                OutCol::Plain(i) => *i,
                OutCol::Window(k) => body_len.checked_add(*k).ok_or_else(internal_error)?,
            };
            cells.push(row.cells.get(index).cloned().ok_or_else(internal_error)?);
        }
        out_rows.push(ResultRow {
            id: row.id,
            score: row.score,
            cells,
        });
    }
    Ok(QueryResult {
        columns: columns.to_vec(),
        rows: out_rows,
    })
}

/// 集計形の実行。`WHERE` を通過した行を [`super::group_by::GroupedRowAccumulator`] へ流す。
fn apply_aggregate(
    result: QueryResult,
    plan: &AggregatePlan,
) -> Result<QueryResult, SqlSurfaceError> {
    let mut acc = super::group_by::GroupedRowAccumulator::new(
        &plan.bound,
        &plan.schema,
        super::aggregate::MAX_AGGREGATE_RESULT_BYTES,
    )?;
    let mut scratch: Vec<StackValue> = Vec::new();
    for row in &result.rows {
        with_scanned(&plan.types, &plan.referenced, row, |scanned| {
            if !where_matches(
                &plan.bound.metadata_filters,
                &plan.bound.or_filters,
                &plan.bound.expr_filter_programs,
                scanned,
                &mut scratch,
            )? {
                return Ok(());
            }
            acc.observe(scanned)
        })?;
    }
    acc.finish()
}

/// 行を外側 `ORDER BY`（列キーのみ）のキーで安定ソートする。キーの比較値は先に行ごとへ展開し
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
    reorder(rows, &idx)
}

/// 行を式キーを含む外側 `ORDER BY`（`ScanPlan::bound.order_by`）で安定ソートする
/// （Issue #1411）。キーの比較値（式キーは行ごとに評価。評価エラー `22012`／`22003` 等は本文の
/// 可視行の値だけから生じ、そのまま fail-closed で返す）は先に所有値へ展開し、比較規約
/// （NULL 位置・降順）は広域取得の式 `ORDER BY` と同じ [`compare_order_key`] を使う。
fn sort_rows_by_keys(
    rows: Vec<ResultRow>,
    scan: &ScanPlan,
) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let spec: &[BoundOrderKey] = &scan.bound.order_by;
    let mut scratch: Vec<StackValue> = Vec::new();
    let mut keys: Vec<Vec<Option<OrderValue>>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let k = with_scanned(&scan.types, &scan.referenced, row, |scanned| {
            let mut out: Vec<Option<OrderValue>> = Vec::with_capacity(spec.len());
            for key in spec {
                match key.target {
                    BoundOrderTarget::Expr(i) => {
                        let (_, program) =
                            scan.bound.order_exprs.get(i).ok_or_else(internal_error)?;
                        let value = program.eval(0, &[], scanned, &mut scratch)?;
                        out.push(expr_value_to_order_value(&value, key.kind)?);
                    }
                    BoundOrderTarget::Column(_) => {
                        out.push(
                            match extract_order_value_ref(&scan.schema, key, 0, scanned)? {
                                Some(r) => Some(scalar_key_ref_to_owned(r)?),
                                None => None,
                            },
                        );
                    }
                    // 本文が公開していない物理キーは束縛前に拒否済み。公開している `id` は
                    // 同名の実列が優先されるため疑似列には解決されない（到達は配線不備のみ）。
                    BoundOrderTarget::Id => return Err(internal_error()),
                }
            }
            Ok(out)
        })?;
        keys.push(k);
    }
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|&a, &b| {
        let (Some(ka), Some(kb)) = (keys.get(a), keys.get(b)) else {
            return Ordering::Equal;
        };
        for (n, key) in spec.iter().enumerate() {
            let va = ka.get(n).and_then(Option::as_ref);
            let vb = kb.get(n).and_then(Option::as_ref);
            let ord = compare_order_key(va, vb, key.descending);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    reorder(rows, &idx)
}

/// 行を添字列 `idx` の順に並べ替える（添字は `0..rows.len()` の順列）。
fn reorder(rows: Vec<ResultRow>, idx: &[usize]) -> Result<Vec<ResultRow>, SqlSurfaceError> {
    let mut slots: Vec<Option<ResultRow>> = rows.into_iter().map(Some).collect();
    let mut sorted = Vec::with_capacity(slots.len());
    for &i in idx {
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
    use crate::sql::allowlist::{AggregateFunc, AggregateItem, Statement, WherePredicate};
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
            outer: BufferedOuter::Rows(Box::new(BufferedRows {
                projection,
                where_predicates,
                order_by,
                order_keys: Vec::new(),
                window_items: Vec::new(),
                limit,
                offset,
            })),
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

    fn agg_view(
        items: Vec<AggregateSelectItem>,
        where_predicates: Vec<WherePredicate>,
    ) -> ValidatedBufferedView {
        ValidatedBufferedView {
            view_name: "v".to_string(),
            body: Box::new(Statement::SetSearchMode {
                value: "recall".to_string(),
            }),
            outer: BufferedOuter::Aggregate(Box::new(ValidatedAggregate {
                table_name: "v".to_string(),
                items,
                where_predicates,
                group_by: None,
            })),
        }
    }

    fn agg_item(func: AggregateFunc, column: Option<&str>) -> AggregateSelectItem {
        AggregateSelectItem::Aggregate(AggregateItem {
            func,
            arg: match column {
                Some(c) => AggregateArg::Expr(Expr::Ident(c.to_string())),
                None => AggregateArg::Star,
            },
            alias: None,
            distinct: false,
        })
    }

    #[test]
    fn aggregate_without_group_by_counts_and_sums_evaluated_cells() {
        let r = result();
        let v = agg_view(
            vec![
                agg_item(AggregateFunc::Count, None),
                agg_item(AggregateFunc::Sum, Some("count")),
            ],
            vec![],
        );
        let out = run(r, &v).unwrap();
        // `count` 列は 0..5（合計 10）。`COUNT` 列の `Cell::Integer(u64)` は BIGINT へ正規化される。
        assert_eq!(
            out.rows[0].cells,
            vec![Cell::Integer(5), Cell::SignedInteger(10)]
        );
        assert_eq!(out.columns.len(), 2);
    }

    #[test]
    fn aggregate_over_zero_rows_yields_one_row() {
        let mut r = result();
        r.rows.clear();
        let v = agg_view(
            vec![
                agg_item(AggregateFunc::Count, None),
                agg_item(AggregateFunc::Max, Some("a")),
            ],
            vec![],
        );
        let out = run(r, &v).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].cells, vec![Cell::Integer(0), Cell::Null]);
    }

    #[test]
    fn aggregate_over_duplicate_body_column_is_42702() {
        let mut r = result();
        r.columns.push(text_col("a"));
        for row in &mut r.rows {
            row.cells.push(Cell::Null);
        }
        let v = agg_view(vec![agg_item(AggregateFunc::Max, Some("a"))], vec![]);
        let err = run(r, &v).err().unwrap();
        assert_eq!(err.wire_code(), "42702");
    }

    #[test]
    fn aggregate_over_unexposed_id_or_unknown_column_is_22000() {
        for col in ["id", "nope"] {
            let v = agg_view(vec![agg_item(AggregateFunc::Count, Some(col))], vec![]);
            let err = run(result(), &v).err().unwrap();
            assert_eq!(err.wire_code(), "22000", "column={col}");
        }
    }

    #[test]
    fn aggregate_sum_overflow_of_count_cells_is_22003() {
        let mut r = result();
        if let Some(row) = r.rows.get_mut(0) {
            row.cells[1] = Cell::Integer(u64::MAX);
        }
        let v = agg_view(vec![agg_item(AggregateFunc::Sum, Some("count"))], vec![]);
        let err = run(r, &v).err().unwrap();
        assert_eq!(err.wire_code(), "22003");
    }

    #[test]
    fn expression_order_key_sorts_stably_and_rejects_unexposed_id() {
        let expr_key = |column: &str, descending: bool| ScanOrderKey::Expr {
            expr: Expr::Call {
                name: "lower".to_string(),
                args: vec![Expr::Ident(column.to_string())],
            },
            descending,
        };
        let mk = |keys: Vec<ScanOrderKey>| ValidatedBufferedView {
            view_name: "v".to_string(),
            body: Box::new(Statement::SetSearchMode {
                value: "recall".to_string(),
            }),
            outer: BufferedOuter::Rows(Box::new(BufferedRows {
                projection: Projection::All,
                where_predicates: vec![],
                order_by: vec![],
                order_keys: keys,
                window_items: vec![],
                limit: 10,
                offset: 0,
            })),
        };
        let out = run(result(), &mk(vec![expr_key("a", true)])).unwrap();
        let ids: Vec<u64> = out.rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![4, 3, 2, 1, 0]);
        let err = run(result(), &mk(vec![expr_key("id", false)]))
            .err()
            .unwrap();
        assert_eq!(err.wire_code(), "22000");
    }
}
