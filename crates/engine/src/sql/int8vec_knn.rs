//! INT8VEC 列への距離順 SELECT（kNN）を広域取得（`Statement::Scan`）へ書き換える
//! （TABLE-49 (1)(4)・Issue #2953）。
//!
//! `core.rs` の読み取り実行・Prepared Describe・EXPLAIN の 3 経路が、`Statement::Select`
//! （`ORDER BY <INT8VEC 列> <距離演算子> '[..]' [DESC] [LIMIT k] [OFFSET n]`）を束縛する直前に
//! [`int8vec_select_as_scan`] を呼ぶ。書き換え後は式キーの `ORDER BY` を持つ既存の scan 経路
//! （`sql::scan`）が RLS・行ポリシー・WHERE・`LIMIT`/`OFFSET`・NULL と NaN の並びを担い、距離は
//! 行ごとに `BoundExpr::Int8VecDistance`（`kernel::int8_*`。整数のまま厳密）で求める。
//!
//! 検索 SELECT の実行段（`sql::exec` の DISTANCE 段）は f32 の `VectorArena` を前提にするため
//! INT8VEC を通さない（通すと arena・ANN 索引・スカラー索引・遅延投影の全分岐が f32 バッファへ
//! 依存する）。そのため `kernel::int8_top_k` は本経路からも呼ばない（性能が必要になった時点の
//! 別 Issue。`docs/design/column-type-extension.md` の Issue #2953 追記参照）。
//!
//! fail-closed の方針: 書き換えられるのは「INT8VEC 列への距離演算子形で、`USING PLAN`・`USING MODE`・
//! `HINT ORDER` を伴わない SELECT」だけ。それ以外（hybrid の関数形・`USING PLAN` など）は `None` を
//! 返して従来経路に任せ、`parser::vector_column` の `0A000` で拒否される。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{
    OrderByForm, Projection, ScanOrderKey, SelectItem, SqlSurfaceError, ValidatedScan,
    ValidatedStatement, WherePredicate,
};
use crate::sql::lexer::DistanceKind;
use crate::sql::mode::SearchMode;
use crate::sql::parser::{parse_int8vec_literal, LiteralSkip};
use crate::sql::plan::EvaluationOrder;
use crate::sql::udf_call::Expr;

/// INT8VEC 列への距離順 SELECT を、式キー `ORDER BY` の広域取得へ書き換える。
///
/// 戻り値は、書き換えた場合 `Ok(Some(scan))`、対象外（INT8VEC 列が無い・`USING PLAN`・距離演算子形
/// でない・ORDER BY の列が INT8VEC でない・`USING MODE`／`HINT ORDER` 付き）なら `Ok(None)`。
/// 対象でありながら実行できない形（`<+>`・precision モード・投影の距離列のクエリ不一致）は `0A000`。
///
/// `skip` は Prepared Describe の `$n` 由来ダミー値の位置（実行経路は [`LiteralSkip::NONE`]）。
/// ダミー値のリテラルは解析も比較もしない。エラー文言にリテラルの値は含めない。
pub(crate) fn int8vec_select_as_scan(
    v: &ValidatedStatement,
    schema: &TableSchema,
    session_mode: Option<SearchMode>,
    skip: &LiteralSkip,
    limits: &crate::result_limits::ResultLimits,
) -> Result<Option<ValidatedScan>, SqlSurfaceError> {
    if !schema
        .columns
        .iter()
        .any(|c| matches!(c.ty, ColumnType::Int8Vec(_)))
    {
        return Ok(None);
    }
    if v.using_plan.is_some() {
        return Ok(None);
    }
    let OrderByForm::Distance { column, literal } = &v.order_by else {
        return Ok(None);
    };
    let Some(dim) = schema
        .columns
        .iter()
        .find(|c| &c.name == column)
        .and_then(|c| match c.ty {
            ColumnType::Int8Vec(dim) => Some(dim),
            _ => None,
        })
    else {
        return Ok(None);
    };
    // `USING MODE`・`HINT ORDER` は INT8VEC では意味を持たない。従来経路の拒否に任せる。
    if v.search_mode.is_some() || v.evaluation_order != EvaluationOrder::DEFAULT {
        return Ok(None);
    }
    if v.distance_kind == DistanceKind::L1 {
        return Err(SqlSurfaceError::feature_not_supported(
            "distance operator <+> is not supported on int8vec columns",
        ));
    }
    if session_mode == Some(SearchMode::Precision) {
        return Err(SqlSurfaceError::feature_not_supported(
            "precision mode is not supported on int8vec columns",
        ));
    }
    // 変換先の scan 束縛は LIMIT／OFFSET を個別にしか検証しないため、通常の kNN 束縛と同じ
    // 合計上限（`LIMIT + OFFSET` ≦ `--max-result-rows`）を先に適用する（fail-closed。HALFVEC と同じ）。
    // 暗黙 LIMIT は scan 経路の行数上限検査が担うので対象外。実行・Describe・EXPLAIN で共有する。
    if !v.limit_implicit {
        crate::sql::parser::validate_search_limit_offset_with(v.limit, v.offset, limits)?;
    }
    check_projected_queries(v, column, literal, dim, skip)?;

    // Describe の `$n` 由来ダミー値だけを、列型に合う有効なゼロベクトルへ置き換えて束縛する。
    // 実リテラル（WHERE・投影・ORDER BY）はここで置換せず、通常どおり束縛で検証する。
    let order_literal = if skip.order_by {
        zero_vector_literal(dim)
    } else {
        literal.clone()
    };
    let projection = match &v.projection {
        Projection::Items(items) => Projection::Items(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    if skip.skips_projection_item(index) {
                        placeholder_item(item, schema)
                    } else {
                        item.clone()
                    }
                })
                .collect(),
        ),
        other => other.clone(),
    };

    Ok(Some(ValidatedScan {
        table_name: v.table_name.clone(),
        projection,
        where_predicates: placeholder_predicates(&v.where_predicates, schema, skip),
        limit: v.limit,
        limit_implicit: v.limit_implicit,
        order_by: Vec::new(),
        offset: v.offset,
        window_items: Vec::new(),
        order_keys: vec![ScanOrderKey::Expr {
            expr: Expr::VectorDistance {
                kind: v.distance_kind,
                lhs: Box::new(Expr::Ident(column.clone())),
                rhs: Box::new(Expr::String(order_literal)),
            },
            descending: v.distance_descending,
            nulls: None,
        }],
        scalar_subquery_items: Vec::new(),
        physical_order_keys: Vec::new(),
        embedded_scalar_subqueries: Vec::new(),
        with_ties: false,
    }))
}

/// 次元 `dim` のゼロベクトルのリテラル（Describe の `$n` 由来ダミー値の置換先）。
fn zero_vector_literal(dim: u32) -> String {
    let mut out = String::from("[");
    for i in 0..dim {
        if i > 0 {
            out.push(',');
        }
        out.push('0');
    }
    out.push(']');
    out
}

/// Prepared Describe の Scan 形（`ORDER BY` が距離でない半径検索 `WHERE v <-> $1 < r ORDER BY id` 等）の
/// 束縛前に、`$n` 由来の距離オペランド（WHERE・投影）だけを INT8VEC 列の次元に合うゼロベクトルへ置き換える
/// （Issue #2953。`core.rs` の Describe の `Statement::Scan` 分岐が呼ぶ）。実リテラルは置換せず、束縛で
/// 通常どおり検証する。INT8VEC 列が無い表・印が無い文では入力をそのまま複製して返す。
pub(crate) fn int8vec_scan_placeholders(
    scan: &ValidatedScan,
    schema: &TableSchema,
    skip: &LiteralSkip,
) -> ValidatedScan {
    if !schema
        .columns
        .iter()
        .any(|c| matches!(c.ty, ColumnType::Int8Vec(_)))
    {
        return scan.clone();
    }
    let mut out = scan.clone();
    out.where_predicates = placeholder_predicates(&scan.where_predicates, schema, skip);
    if let Projection::Items(items) = &scan.projection {
        out.projection = Projection::Items(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    if skip.skips_projection_item(index) {
                        placeholder_item(item, schema)
                    } else {
                        item.clone()
                    }
                })
                .collect(),
        );
    }
    out
}

/// WHERE 述語内の `$n` 由来の距離オペランド（Prepared Describe の仮値）を、INT8VEC 列の次元に合う
/// ゼロベクトルへ置き換える（Issue #2953。半径述語 `v <-> $1 < r` の Describe 用）。
///
/// 置換対象は値ではなく位置で決める: [`LiteralSkip::where_distance_mask`] は元トークン列の距離演算子の
/// 出現順に「被演算子が `$n` か」を持ち、述語木の `Expr::VectorDistance` を同じ出現順に走査して対応づける。
/// 実リテラルは値が `'0'` でも置換しない（束縛で検証し 22P02 等を返す）。距離式の個数が印の個数と
/// 合わない形（対応づけ不能）は何も置換しない（fail-closed。仮値のまま束縛され拒否される）。
fn placeholder_predicates(
    preds: &[WherePredicate],
    schema: &TableSchema,
    skip: &LiteralSkip,
) -> Vec<WherePredicate> {
    if !skip.where_distance_mask.iter().any(|f| *f) {
        return preds.to_vec();
    }
    let mut counter = PlaceholderWalker {
        schema,
        mask: &[],
        ordinal: 0,
        replace: false,
        seen: Vec::new(),
    };
    for p in preds {
        counter.pred(p);
    }
    let expanded;
    let mask: &[bool] = if counter.ordinal == skip.where_distance_mask.len() {
        &skip.where_distance_mask
    } else if let Some(m) = expand_mask_by_copies(
        counter.seen.len(),
        &skip.where_distance_mask,
        &skip.where_distance_copies,
    ) {
        expanded = m;
        &expanded
    } else {
        return preds.to_vec();
    };
    let mut walker = PlaceholderWalker {
        schema,
        mask,
        ordinal: 0,
        replace: true,
        seen: Vec::new(),
    };
    preds.iter().map(|p| walker.pred(p)).collect()
}

/// [`placeholder_predicates`] の走査器。`replace` が偽のときは距離式を数えるだけで木を変えない。
struct PlaceholderWalker<'a> {
    schema: &'a TableSchema,
    mask: &'a [bool],
    ordinal: usize,
    replace: bool,
    /// 走査で出会った距離式（出現順）。脱糖で複製された式の対応づけ用。
    seen: Vec<Expr>,
}

/// BETWEEN／IN の脱糖で左辺の距離式が複製され、AST 上の距離式数がトークン上の距離演算子数
/// （`mask.len()`）を超える場合に、トークン単位の印を AST の距離式の出現順へ展開する（PR #2956 指摘）。
/// 各トークンの複製数（`copies`。トークン位置から [`crate::sql::params::where_distance_copy_counts`]
/// が決める）だけ印を繰り返す。構造一致での推定はしない（Describe では異なる `$n` が同じ仮値になり、
/// 独立した距離式を 1 つの塊と取り違えるため）。複製数が不明（0）・長さ不一致・総数が AST の距離式数と
/// 合わない形は `None`（fail-closed。何も置換しない）。
fn expand_mask_by_copies(seen_len: usize, mask: &[bool], copies: &[usize]) -> Option<Vec<bool>> {
    if copies.len() != mask.len() {
        return None;
    }
    let mut out: Vec<bool> = Vec::with_capacity(seen_len);
    for (flag, n) in mask.iter().zip(copies) {
        if *n == 0 || out.len().checked_add(*n)? > seen_len {
            return None;
        }
        out.extend(std::iter::repeat_n(*flag, *n));
    }
    (out.len() == seen_len).then_some(out)
}

impl PlaceholderWalker<'_> {
    fn pred(&mut self, pred: &WherePredicate) -> WherePredicate {
        match pred {
            WherePredicate::Expression(e) => WherePredicate::Expression(self.expr(e)),
            WherePredicate::Not(inner) => WherePredicate::Not(Box::new(self.pred(inner))),
            WherePredicate::Or(branches) => WherePredicate::Or(
                branches
                    .iter()
                    .map(|b| b.iter().map(|p| self.pred(p)).collect())
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    fn expr(&mut self, expr: &Expr) -> Expr {
        if let Expr::VectorDistance { kind, lhs, rhs } = expr {
            let flagged = self.mask.get(self.ordinal).copied().unwrap_or(false);
            if !self.replace {
                self.seen.push(expr.clone());
            }
            self.ordinal = self.ordinal.saturating_add(1);
            if self.replace && flagged {
                let operands = match (lhs.as_ref(), rhs.as_ref()) {
                    (Expr::Ident(n), Expr::String(_)) => Some((n, true)),
                    (Expr::String(_), Expr::Ident(n)) => Some((n, false)),
                    _ => None,
                };
                let dim = operands.and_then(|(name, _)| {
                    self.schema
                        .columns
                        .iter()
                        .find(|c| &c.name == name)
                        .and_then(|c| match c.ty {
                            ColumnType::Int8Vec(d) => Some(d),
                            _ => None,
                        })
                });
                if let (Some((name, lhs_is_col)), Some(dim)) = (operands, dim) {
                    let column = Box::new(Expr::Ident(name.clone()));
                    let zero = Box::new(Expr::String(zero_vector_literal(dim)));
                    let (lhs, rhs) = if lhs_is_col {
                        (column, zero)
                    } else {
                        (zero, column)
                    };
                    return Expr::VectorDistance {
                        kind: *kind,
                        lhs,
                        rhs,
                    };
                }
            }
        }
        crate::sql::parser::map_expr_children(expr, &mut |e| self.expr(e))
    }
}

/// `$n` 由来の投影距離列 `<列> <距離演算子> <ダミー>` のリテラルを、その列の型に合うゼロベクトルへ
/// 置き換える。形が合わない・列型が距離対象でない項目はそのまま返す（束縛が通常どおり判定する）。
fn placeholder_item(item: &SelectItem, schema: &TableSchema) -> SelectItem {
    let SelectItem::Expr {
        expr: Expr::VectorDistance { kind, lhs, rhs },
        alias,
    } = item
    else {
        return item.clone();
    };
    let (name, lhs_is_col) = match (lhs.as_ref(), rhs.as_ref()) {
        (Expr::Ident(n), Expr::String(_)) => (n, true),
        (Expr::String(_), Expr::Ident(n)) => (n, false),
        _ => return item.clone(),
    };
    let dim = schema
        .columns
        .iter()
        .find(|c| &c.name == name)
        .and_then(|c| match c.ty {
            ColumnType::Int8Vec(d) | ColumnType::Vector(d) | ColumnType::HalfVec(d) => Some(d),
            _ => None,
        });
    let Some(dim) = dim else {
        return item.clone();
    };
    let column = Box::new(Expr::Ident(name.clone()));
    let zero = Box::new(Expr::String(zero_vector_literal(dim)));
    let (lhs, rhs) = if lhs_is_col {
        (column, zero)
    } else {
        (zero, column)
    };
    SelectItem::Expr {
        expr: Expr::VectorDistance {
            kind: *kind,
            lhs,
            rhs,
        },
        alias: alias.clone(),
    }
}

/// 投影トップレベルの距離列（SQL-33 (7)）のクエリが `ORDER BY` と同じ値かを検査する。
/// 構文段が列・演算子の一致を保証するため、ここではリテラルの解析結果の一致だけを見る。
/// 不一致は `0A000`（値はエラーに含めない）。`$n` 由来（`skip`）の項目は比較しない。
fn check_projected_queries(
    v: &ValidatedStatement,
    column: &str,
    order_literal: &str,
    dim: u32,
    skip: &LiteralSkip,
) -> Result<(), SqlSurfaceError> {
    let Projection::Items(items) = &v.projection else {
        return Ok(());
    };
    let mut order_query: Option<Vec<i8>> = None;
    for (index, item) in items.iter().enumerate() {
        let SelectItem::Expr {
            expr: Expr::VectorDistance { lhs, rhs, .. },
            ..
        } = item
        else {
            continue;
        };
        if skip.order_by || skip.skips_projection_item(index) {
            continue;
        }
        let item_literal = match (lhs.as_ref(), rhs.as_ref()) {
            (Expr::Ident(name), Expr::String(s)) | (Expr::String(s), Expr::Ident(name))
                if name == column =>
            {
                s
            }
            _ => continue,
        };
        if order_query.is_none() {
            order_query = Some(parse_int8vec_literal(order_literal, dim)?);
        }
        let item_query = parse_int8vec_literal(item_literal, dim)?;
        if order_query.as_deref() != Some(item_query.as_slice()) {
            return Err(SqlSurfaceError::feature_not_supported(
                "a projected distance must use the same query vector as the distance ORDER BY",
            ));
        }
    }
    Ok(())
}
