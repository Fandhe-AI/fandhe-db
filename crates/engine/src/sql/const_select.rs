//! `FROM` なし定数式 `SELECT`（SQL-43・Issue #1531）の束縛と実行。
//!
//! 責務境界: `sql::allowlist` が構文検証した [`ValidatedConstSelect`] を、列参照・
//! 集計の禁止検査（構文木の網羅走査）→ 項目ごとの型付けの順で束縛し、1 行を返す。
//! 式の評価は `sql::udf_call` の束縛器・評価器をそのまま使う（第 2 の式評価器を
//! 作らない）。呼び出し元は `core.rs` の実行入口（`execute_validated_in_session`）と
//! Describe（`describe_parsed_in_session`）。wire-server への接続確認は別 Issue
//! （#1532）の担当。
//!
//! `WHERE`／`LIMIT`／`OFFSET`（Issue #2329）は PostgreSQL と同じくダミー入力 1 行への
//! 絞り込みとして扱い、結果は 0 行か 1 行になる。`WHERE` は定数条件のみ（偽・`NULL` は
//! 0 行、非真偽型は `42804`）、`LIMIT`／`OFFSET` の負値は実行時に `22000`。
//! 投影は絞り込み結果に関わらず全項目を評価し、エラー有無がフィルタに左右されない
//! ようにする（`--max-search-k` 上限は適用しない。結果が最大 1 行のため）。
//!
//! テーブル・ストレージ・RLS 述語のいずれにも触れないため、他テナントのデータや
//! 存在情報に依存する経路が無い。列参照は常に同一の固定文言で `42703`（Issue #2328。
//! `FROM` なしでは全識別子が一律に未定義）とし、識別子名・テーブル名をエラーに含めない
//! （fail-closed）。集計は `42601` のまま。

use crate::catalog::ColumnType;
use crate::sql::allowlist::{
    is_aggregate_function_name, ConstSelectCondition, ConstSelectItem, SqlSurfaceError,
    ValidatedConstSelect,
};
use crate::sql::exec::{scalar_cell_for, Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::{
    bind_const_expr, classify_integer_literal, eval, into_owned_vector, BoundExpr, Expr, ExprType,
    ExprValue, NumericKind, UdfRegistry, MAX_EXPR_NODES,
};

/// 束縛済みの 1 項目。
#[derive(Debug, Clone)]
enum BoundConstItem {
    /// リテラル定数（束縛時に型とセルが確定する）。
    Literal { meta: ColumnMeta, cell: Cell },
    /// 式（実行時に評価する。型は束縛時に `meta` へ確定済み）。
    Expr {
        meta: ColumnMeta,
        expr: BoundExpr,
        result_ty: Option<ColumnType>,
    },
}

impl BoundConstItem {
    fn meta(&self) -> &ColumnMeta {
        match self {
            BoundConstItem::Literal { meta, .. } | BoundConstItem::Expr { meta, .. } => meta,
        }
    }
}

/// 束縛済みの定数式 SELECT。列メタデータは評価を伴わず導出できる（Describe は
/// 本体を実行しない契約）。
#[derive(Debug, Clone)]
pub struct BoundConstSelect {
    items: Vec<BoundConstItem>,
    where_clause: Option<BoundCondition>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// 束縛済みの `WHERE` 条件。リテラルは束縛時に確定し、式は実行時に評価する。
#[derive(Debug, Clone)]
enum BoundCondition {
    /// `Some(b)` は真偽リテラル、`None` は `NULL`。
    Const(Option<bool>),
    Expr(BoundExpr),
}

impl BoundConstSelect {
    /// 結果列のメタデータ（出現順）。
    pub fn columns(&self) -> Vec<ColumnMeta> {
        self.items.iter().map(|i| i.meta().clone()).collect()
    }
}

/// 列参照・集計関数を含む式を拒否する（`Expr` の全 variant を網羅し、将来 variant が
/// 増えたらコンパイルエラーで検査漏れに気付けるようにする）。
fn reject_non_constant(expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_) => Ok(()),
        Expr::Ident(_) => Err(SqlSurfaceError::ColumnReferenceWithoutFrom),
        Expr::Call { name, args } => {
            if is_aggregate_function_name(name) {
                return Err(SqlSurfaceError::unsupported(
                    "aggregate functions require a FROM clause",
                ));
            }
            // テナント変数の上書き（RLS-6）に使われうる `set_config` は引数の検査より先に
            // 構文拒否（`42601`）で固定する。引数中の `false` 等が列参照扱いで `42703` に
            // なると RLS-6 の拒否契約（`42601`）が崩れるため。
            if name.eq_ignore_ascii_case("set_config") {
                return Err(SqlSurfaceError::unsupported("set_config is not supported"));
            }
            args.iter().try_for_each(reject_non_constant)
        }
        Expr::Binary { lhs, rhs, .. } => {
            reject_non_constant(lhs)?;
            reject_non_constant(rhs)
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                reject_non_constant(cond)?;
                reject_non_constant(result)?;
            }
            match else_result {
                Some(e) => reject_non_constant(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => args.iter().try_for_each(reject_non_constant),
        Expr::NullIf(lhs, rhs) => {
            reject_non_constant(lhs)?;
            reject_non_constant(rhs)
        }
    }
}

/// 数値リテラルを PostgreSQL 同様の型（`integer` → `bigint` → `numeric`）で定数化する。
/// 整数形（先頭 `-` 任意＋数字のみ）は i32／i64 に収まる最小の整数型、それ以外
/// （小数・指数・i64 超）は厳密な `numeric` にする。
fn literal_number(raw: &str, name: &str) -> Result<(ColumnMeta, Cell), SqlSurfaceError> {
    if let Some((v, kind)) = classify_integer_literal(raw) {
        let ty = if kind == NumericKind::Int4 {
            ColumnType::Integer
        } else {
            ColumnType::BigInt
        };
        return Ok((
            ColumnMeta::Computed {
                name: name.to_string(),
                ty: Some(ty),
            },
            Cell::SignedInteger(v),
        ));
    }
    match crate::numeric::parse_literal_exact(raw) {
        Ok(d) => Ok((
            ColumnMeta::Computed {
                name: name.to_string(),
                ty: Some(ColumnType::Numeric {
                    precision: crate::numeric::MAX_PRECISION,
                    scale: d.scale(),
                }),
            },
            Cell::Numeric(d),
        )),
        Err(crate::numeric::NumericError::Malformed(detail)) => {
            Err(SqlSurfaceError::invalid_text_representation(detail))
        }
        Err(crate::numeric::NumericError::OutOfRange) => Err(
            SqlSurfaceError::numeric_out_of_range("numeric literal is out of range"),
        ),
    }
}

/// 項目の列名: `AS` 別名 → 既存の既定名（関数呼び出し・`CASE` 等）→ `?column?`。
fn item_name(item: &ConstSelectItem) -> String {
    if let Some(alias) = item.alias() {
        return alias.to_string();
    }
    crate::sql::parser::default_expr_alias(item.expr())
}

/// 定数式 SELECT を束縛する。禁止検査（列参照・集計）を全項目に先に適用してから
/// 型付けする。式ノード予算は文全体で共有する。
pub fn bind(
    validated: &ValidatedConstSelect,
    registry: &UdfRegistry,
) -> Result<BoundConstSelect, SqlSurfaceError> {
    validated
        .items()
        .iter()
        .try_for_each(|i| reject_non_constant(i.expr()))?;
    if let Some(ConstSelectCondition::Expr(e)) = validated.where_clause() {
        reject_non_constant(e)?;
    }
    let mut budget = MAX_EXPR_NODES;
    let mut items = Vec::with_capacity(validated.items().len());
    for item in validated.items() {
        let name = item_name(item);
        let bound = match item.expr() {
            Expr::Number(raw) => {
                let (meta, cell) = literal_number(raw, &name)?;
                BoundConstItem::Literal { meta, cell }
            }
            expr => {
                let (bound, _ty, result_ty) = bind_const_expr(expr, registry, &mut budget)?;
                BoundConstItem::Expr {
                    meta: ColumnMeta::Computed {
                        name,
                        ty: result_ty.clone(),
                    },
                    expr: bound,
                    result_ty,
                }
            }
        };
        items.push(bound);
    }
    let where_clause = match validated.where_clause() {
        None => None,
        Some(ConstSelectCondition::Bool(b)) => Some(BoundCondition::Const(Some(*b))),
        Some(ConstSelectCondition::Null) => Some(BoundCondition::Const(None)),
        Some(ConstSelectCondition::Expr(e)) => {
            let (bound, ty, _) = bind_const_expr(e, registry, &mut budget)?;
            if ty != ExprType::Bool {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "argument of WHERE must be type boolean",
                ));
            }
            Some(BoundCondition::Expr(bound))
        }
    };
    Ok(BoundConstSelect {
        items,
        where_clause,
        limit: validated.limit(),
        offset: validated.offset(),
    })
}

/// 束縛済みの定数式 SELECT を評価し、1 行の結果を返す。評価エラー（0 除算等）は
/// そのまま伝播する（fail-closed）。
pub fn execute(bound: &BoundConstSelect) -> Result<QueryResult, SqlSurfaceError> {
    // 負値は実行時に拒否する（PostgreSQL と同じく Describe は成功し Execute で失敗）。
    if bound.limit.is_some_and(|l| l < 0) {
        return Err(SqlSurfaceError::negative_row_count("LIMIT"));
    }
    if bound.offset.is_some_and(|o| o < 0) {
        return Err(SqlSurfaceError::negative_row_count("OFFSET"));
    }
    let passes = match &bound.where_clause {
        None => true,
        Some(BoundCondition::Const(v)) => v.unwrap_or(false),
        Some(BoundCondition::Expr(expr)) => match eval(expr, 0, &[])? {
            ExprValue::Bool(b) => b,
            ExprValue::Null => false,
            // 束縛段で Bool 型を保証済み。到達不能だが panic せず fail-closed に倒す。
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "WHERE did not evaluate to boolean".to_string(),
                })
            }
        },
    };
    let mut cells = Vec::with_capacity(bound.items.len());
    for item in &bound.items {
        match item {
            BoundConstItem::Literal { cell, .. } => cells.push(cell.clone()),
            BoundConstItem::Expr {
                expr, result_ty, ..
            } => {
                let cell = match eval(expr, 0, &[])? {
                    ExprValue::Scalar(f) => scalar_cell_for(f, result_ty.as_ref())?,
                    ExprValue::Bool(b) => Cell::Bool(b),
                    ExprValue::Text(t) => Cell::Text(t.into_owned()),
                    ExprValue::Date(d) => Cell::Date(d),
                    ExprValue::Timestamp(t) => Cell::Timestamp(t),
                    ExprValue::Null => Cell::Null,
                    ExprValue::Vector(v) => Cell::Vector(into_owned_vector(v)?),
                };
                cells.push(cell);
            }
        }
    }
    let selected = passes && bound.offset.unwrap_or(0) == 0 && bound.limit.is_none_or(|l| l > 0);
    let rows = if selected {
        vec![ResultRow {
            id: 0,
            score: 0.0,
            cells,
        }]
    } else {
        Vec::new()
    };
    Ok(QueryResult {
        columns: bound.columns(),
        rows,
    })
}

/// 束縛と実行をまとめた入口（`core.rs::execute_validated_in_session` が呼ぶ）。
pub fn run(
    validated: &ValidatedConstSelect,
    registry: &UdfRegistry,
) -> Result<QueryResult, SqlSurfaceError> {
    let bound = bind(validated, registry)?;
    execute(&bound)
}
