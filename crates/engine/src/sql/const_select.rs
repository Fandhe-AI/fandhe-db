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
//! （fail-closed）。
//!
//! トップレベルの集計項目（`count(*)`・`sum(<式>)` 等。Issue #2330）は、`GROUP BY` なしの
//! 集計として入力（ダミー 1 行を `WHERE` で絞った 0 か 1 行）を畳み込み、集計を 1 つでも含む
//! 文は常に 1 行を出す（`LIMIT`／`OFFSET` は出力行に適用）。`FROM` あり側の集計実行
//! （`sql::aggregate`）には依存せず、入力が高々 1 行であることを使って直接評価する。結果型は
//! PostgreSQL 準拠（`sum(int4)`→`bigint` 等。型表は [`const_aggregate_result_type`]）。
//! 集計を含む式・`WHERE` 内の集計・入れ子集計は構文段で `42601`。
//!
//! `HAVING`（Issue #2331）は `GROUP BY` なしの 1 グループへの条件で、条件中の集計は内部名の
//! 識別子へ置き換え済み（構文段）。束縛段は内部名だけを許す禁止検査 → 集計の束縛 →
//! 内部名を列名・集計結果型を列型とする合成スキーマでの型付け（真偽型でなければ `42804`）の
//! 順で行い、実行段は `WHERE` の結果に関わらず投影・`HAVING` の集計引数を評価してから条件を
//! 評価する。`HAVING` があれば集計項目が無くても集計問い合わせ（常に 1 グループ）として扱い、
//! 条件が偽・`NULL` なら 0 行。結果型が `numeric` の集計は `HAVING` では未対応（`42601`）。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::aggregate::numeric_avg_result_scale;
use crate::sql::allowlist::{
    aggregate_placeholder, aggregate_placeholder_index, is_aggregate_function_name, AggregateArg,
    AggregateFunc, AggregateItem, ConstSelectCondition, ConstSelectHaving, ConstSelectItem,
    SqlSurfaceError, ValidatedConstSelect,
};
use crate::sql::exec::{scalar_cell_for, Cell, ColumnMeta, QueryResult, ResultRow};
use crate::sql::udf_call::{
    bind_const_expr, bind_const_projection_expr, bind_projection_expr, classify_integer_literal,
    eval, eval_with_scalars, into_owned_vector, BoundExpr, Expr, ExprType, ExprValue, NumericKind,
    UdfRegistry, MAX_EXPR_NODES,
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
    /// トップレベルの集計項目（Issue #2330）。型は束縛時に `meta` へ確定済み。
    Aggregate {
        meta: ColumnMeta,
        func: AggregateFunc,
        arg: BoundAggArg,
    },
}

/// 束縛済みの集計引数。
#[derive(Debug, Clone)]
enum BoundAggArg {
    /// `count(*)`。
    Star,
    /// 数値リテラル引数（束縛時に型とセルが確定する）。
    Literal { cell: Cell, ty: ColumnType },
    /// 式引数（`WHERE` の結果に関わらず常に評価する）。
    Expr {
        expr: BoundExpr,
        ty: Option<ColumnType>,
    },
}

impl BoundConstItem {
    fn meta(&self) -> &ColumnMeta {
        match self {
            BoundConstItem::Literal { meta, .. }
            | BoundConstItem::Expr { meta, .. }
            | BoundConstItem::Aggregate { meta, .. } => meta,
        }
    }
}

/// 束縛済みの定数式 SELECT。列メタデータは評価を伴わず導出できる（Describe は
/// 本体を実行しない契約）。
#[derive(Debug, Clone)]
pub struct BoundConstSelect {
    items: Vec<BoundConstItem>,
    where_clause: Option<BoundCondition>,
    having: Option<BoundHaving>,
    limit: Option<i64>,
    offset: Option<i64>,
    /// 集計問い合わせか（集計項目を 1 つでも含む、または `HAVING` がある）。`GROUP BY` なしの集計文は入力 0 行でも 1 行を出す。
    has_aggregate: bool,
}

/// 束縛済みの `WHERE` 条件。リテラルは束縛時に確定し、式は実行時に評価する。
#[derive(Debug, Clone)]
enum BoundCondition {
    /// `Some(b)` は真偽リテラル、`None` は `NULL`。
    Const(Option<bool>),
    Expr(BoundExpr),
}

/// 束縛済みの `HAVING`（Issue #2331）。`cond` 中の列参照 `index` は `aggregates` の添字。
#[derive(Debug, Clone)]
struct BoundHaving {
    cond: BoundCondition,
    aggregates: Vec<(AggregateFunc, BoundAggArg, Option<ColumnType>)>,
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
    reject_non_constant_with(expr, false)
}

/// [`reject_non_constant`] の本体。`allow_placeholders` は `HAVING` 条件用で、構文段が生成した
/// 集計の内部名（利用者は書けない）の識別子だけを通す。それ以外の識別子は常に `42703`。
fn reject_non_constant_with(expr: &Expr, allow_placeholders: bool) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::ValueFunction { .. } => Ok(()),
        Expr::Ident(n) if allow_placeholders && aggregate_placeholder_index(n).is_some() => Ok(()),
        Expr::Ident(_) => Err(SqlSurfaceError::ColumnReferenceWithoutFrom),
        Expr::Call { name, args } => {
            // 集計項目はトップレベルのみ構文段が受理し、束縛段は引数だけをこの検査へ
            // 渡すため、ここへ届く集計名は防御線（到達しない想定）。
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
            args.iter()
                .try_for_each(|a| reject_non_constant_with(a, allow_placeholders))
        }
        Expr::Binary { lhs, rhs, .. } | Expr::JsonAccess { lhs, rhs, .. } => {
            reject_non_constant_with(lhs, allow_placeholders)?;
            reject_non_constant_with(rhs, allow_placeholders)
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                reject_non_constant_with(cond, allow_placeholders)?;
                reject_non_constant_with(result, allow_placeholders)?;
            }
            match else_result {
                Some(e) => reject_non_constant_with(e, allow_placeholders),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => args
            .iter()
            .try_for_each(|a| reject_non_constant_with(a, allow_placeholders)),
        Expr::NullIf(lhs, rhs) => {
            reject_non_constant_with(lhs, allow_placeholders)?;
            reject_non_constant_with(rhs, allow_placeholders)
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

/// 項目の列名: `AS` 別名 → 集計の既定名（`count` 等）→ 既存の既定名（関数呼び出し・
/// `CASE` 等）→ `?column?`。
fn item_name(item: &ConstSelectItem) -> String {
    if let Some(alias) = item.alias() {
        return alias.to_string();
    }
    if let Some(agg) = item.aggregate() {
        return agg.func.default_alias().to_string();
    }
    crate::sql::parser::default_expr_alias(item.expr())
}

/// 集計の結果型表（Issue #2330。PostgreSQL 17 準拠。`FROM` あり側の
/// `aggregate_result_type` は `sum(int8)`→`numeric` へ是正済み（Issue #2271）だが、
/// `avg(int)`→`double` のままで、是正は TASK-330 のポインタ。本経路は先に PG の型へ
/// そろえるため AVG のみ一時的に食い違う）。
/// `arg_ty` は引数の型（`count(*)`・型不明は `None`）。非対応の組は暗黙変換せず `42883`。
/// `avg(整数)` の numeric は scale 16 で公告し、値も同 scale で返す（PG は 20 桁。
/// 表示桁の完全一致は対象外）。
fn const_aggregate_result_type(
    func: AggregateFunc,
    arg_ty: Option<&ColumnType>,
) -> Result<ColumnType, SqlSurfaceError> {
    use crate::numeric::MAX_PRECISION;
    let unsupported = || {
        SqlSurfaceError::undefined_function(format!(
            "function {}(argument type) does not exist",
            func.default_alias()
        ))
    };
    if func == AggregateFunc::Count {
        return Ok(ColumnType::BigInt);
    }
    let ty = arg_ty.ok_or_else(unsupported)?;
    match (func, ty) {
        (AggregateFunc::Sum, ColumnType::Integer) => Ok(ColumnType::BigInt),
        (AggregateFunc::Sum, ColumnType::BigInt) => Ok(ColumnType::Numeric {
            precision: MAX_PRECISION,
            scale: 0,
        }),
        (AggregateFunc::Sum, ColumnType::Numeric { scale, .. }) => Ok(ColumnType::Numeric {
            precision: MAX_PRECISION,
            scale: *scale,
        }),
        (AggregateFunc::Sum, ColumnType::Real) => Ok(ColumnType::Real),
        (AggregateFunc::Sum, ColumnType::Double) => Ok(ColumnType::Double),
        (AggregateFunc::Avg, ColumnType::Integer | ColumnType::BigInt) => Ok(ColumnType::Numeric {
            precision: MAX_PRECISION,
            scale: AVG_INT_SCALE,
        }),
        (AggregateFunc::Avg, ColumnType::Numeric { precision, scale }) => Ok(ColumnType::Numeric {
            precision: MAX_PRECISION,
            scale: numeric_avg_result_scale(*precision, *scale),
        }),
        (AggregateFunc::Avg, ColumnType::Real | ColumnType::Double) => Ok(ColumnType::Double),
        (
            AggregateFunc::Min | AggregateFunc::Max,
            ColumnType::Integer
            | ColumnType::BigInt
            | ColumnType::Numeric { .. }
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Text
            | ColumnType::Date
            | ColumnType::Timestamp,
        ) => Ok(ty.clone()),
        _ => Err(unsupported()),
    }
}

/// `avg(integer|bigint)` の numeric 結果の scale。
const AVG_INT_SCALE: u8 = 16;

/// 集計呼び出しの引数を束縛し、結果型を確定する（投影項目と `HAVING` で共有）。
/// 戻り値は（束縛済み引数、引数の型、結果型）。
fn bind_aggregate_call(
    agg: &AggregateItem,
    registry: &UdfRegistry,
    budget: &mut usize,
) -> Result<(BoundAggArg, ColumnType), SqlSurfaceError> {
    let (arg, arg_ty) = match &agg.arg {
        AggregateArg::Star => (BoundAggArg::Star, None),
        AggregateArg::Expr(Expr::Number(raw)) => {
            let (meta, cell) = literal_number(raw, "")?;
            let ColumnMeta::Computed { ty: Some(ty), .. } = meta else {
                return Err(SqlSurfaceError::Internal {
                    detail: "numeric literal has no type".to_string(),
                });
            };
            (
                BoundAggArg::Literal {
                    cell,
                    ty: ty.clone(),
                },
                Some(ty),
            )
        }
        AggregateArg::Expr(e) => {
            let (bound, _ty, result_ty) = bind_const_expr(e, registry, budget)?;
            (
                BoundAggArg::Expr {
                    expr: bound,
                    ty: result_ty.clone(),
                },
                result_ty,
            )
        }
    };
    let result = const_aggregate_result_type(agg.func, arg_ty.as_ref())?;
    Ok((arg, result))
}

/// 集計項目を束縛する（引数の型付けと結果列メタの確定）。
fn bind_aggregate_item(
    agg: &AggregateItem,
    name: String,
    registry: &UdfRegistry,
    budget: &mut usize,
) -> Result<BoundConstItem, SqlSurfaceError> {
    let (arg, result) = bind_aggregate_call(agg, registry, budget)?;
    Ok(BoundConstItem::Aggregate {
        meta: ColumnMeta::Computed {
            name,
            ty: Some(result),
        },
        func: agg.func,
        arg,
    })
}

/// `HAVING` を束縛する（Issue #2331）。禁止検査 → 集計の束縛 → 合成スキーマでの条件の
/// 型付けの順。`numeric` 結果の集計は束縛器が名前入りの `22000` で拒否する前に、固定文言の
/// `42601` で拒否する（内部名をエラーに出さない・fail-closed）。
fn bind_having(
    having: &ConstSelectHaving,
    registry: &UdfRegistry,
    budget: &mut usize,
) -> Result<BoundHaving, SqlSurfaceError> {
    if let ConstSelectCondition::Expr(e) = having.condition() {
        reject_non_constant_with(e, true)?;
    }
    having
        .aggregates()
        .iter()
        .try_for_each(|agg| match &agg.arg {
            AggregateArg::Star => Ok(()),
            AggregateArg::Expr(e) => reject_non_constant(e),
        })?;
    let mut aggregates = Vec::with_capacity(having.aggregates().len());
    let mut columns = Vec::with_capacity(having.aggregates().len());
    for (j, agg) in having.aggregates().iter().enumerate() {
        let (arg, result) = bind_aggregate_call(agg, registry, budget)?;
        if matches!(result, ColumnType::Numeric { .. }) {
            return Err(SqlSurfaceError::unsupported(
                "numeric aggregate results are not supported in HAVING without FROM",
            ));
        }
        columns.push(ColumnDef::new(
            aggregate_placeholder(j),
            result.clone(),
            true,
        ));
        aggregates.push((agg.func, arg, Some(result)));
    }
    let cond = match having.condition() {
        ConstSelectCondition::Bool(b) => BoundCondition::Const(Some(*b)),
        ConstSelectCondition::Null => BoundCondition::Const(None),
        ConstSelectCondition::Expr(e) => {
            let schema = TableSchema::new("__const_having", columns);
            let (bound, ty, _) = bind_projection_expr(e, &schema, registry, budget)?;
            if ty != ExprType::Bool {
                return Err(SqlSurfaceError::datatype_mismatch(
                    "argument of HAVING must be type boolean",
                ));
            }
            BoundCondition::Expr(bound)
        }
    };
    Ok(BoundHaving { cond, aggregates })
}

/// 評価結果 [`ExprValue`] を応答セルへ変換する（式項目・集計引数で共有）。
fn expr_value_to_cell(v: ExprValue<'_>, ty: Option<&ColumnType>) -> Result<Cell, SqlSurfaceError> {
    Ok(match v {
        ExprValue::Scalar(f) => scalar_cell_for(f, ty)?,
        ExprValue::Bool(b) => Cell::Bool(b),
        ExprValue::Text(t) => Cell::Text(t.into_owned()),
        ExprValue::Date(d) => Cell::Date(d),
        ExprValue::Timestamp(t) => Cell::Timestamp(t),
        ExprValue::Null => Cell::Null,
        ExprValue::Vector(v) => Cell::Vector(into_owned_vector(v)?),
        ExprValue::Json(j) => Cell::Json(j.into_owned()),
    })
}

fn numeric_cell(unscaled: i128, scale: u8) -> Result<Cell, SqlSurfaceError> {
    crate::numeric::Decimal::from_parts(unscaled, scale)
        .map(Cell::Numeric)
        .map_err(|_| SqlSurfaceError::numeric_out_of_range("numeric value is out of range"))
}

/// 入力が 1 行（引数値 `cell` が非 NULL）のときの集計値。入力 0 行・引数 NULL は呼び出し元が
/// `NULL` にする。`arg_ty` は引数の型（結果型の表 [`const_aggregate_result_type`] と対）。
fn single_row_aggregate(
    func: AggregateFunc,
    arg_ty: Option<&ColumnType>,
    cell: Cell,
) -> Result<Cell, SqlSurfaceError> {
    match (func, arg_ty, cell) {
        // `sum(bigint)` は numeric（scale 0）。
        (AggregateFunc::Sum, Some(ColumnType::BigInt), Cell::SignedInteger(v)) => {
            numeric_cell(i128::from(v), 0)
        }
        (AggregateFunc::Avg, _, Cell::SignedInteger(v)) => {
            let u =
                crate::numeric::avg_unscaled(i128::from(v), 1, 0, AVG_INT_SCALE).map_err(|_| {
                    SqlSurfaceError::numeric_out_of_range("numeric value is out of range")
                })?;
            numeric_cell(u, AVG_INT_SCALE)
        }
        // numeric の avg は入力 1 行なら値そのもの（結果 scale は入力 scale 以上で、
        // リテラル引数では一致する）。
        (_, _, c) => Ok(c),
    }
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
        .try_for_each(|i| match i.aggregate() {
            // 集計項目は内部名の `expr` ではなく引数だけを検査する（`count(x)` は `42703`）。
            Some(agg) => match &agg.arg {
                AggregateArg::Star => Ok(()),
                AggregateArg::Expr(e) => reject_non_constant(e),
            },
            None => reject_non_constant(i.expr()),
        })?;
    if let Some(ConstSelectCondition::Expr(e)) = validated.where_clause() {
        reject_non_constant(e)?;
    }
    let mut budget = MAX_EXPR_NODES;
    let mut items = Vec::with_capacity(validated.items().len());
    for item in validated.items() {
        let name = item_name(item);
        let bound = match item.expr() {
            _ if item.aggregate().is_some() => {
                let Some(agg) = item.aggregate() else {
                    return Err(SqlSurfaceError::Internal {
                        detail: "aggregate item vanished".to_string(),
                    });
                };
                bind_aggregate_item(agg, name, registry, &mut budget)?
            }
            Expr::Number(raw) => {
                let (meta, cell) = literal_number(raw, &name)?;
                BoundConstItem::Literal { meta, cell }
            }
            expr => {
                let (bound, _ty, result_ty) =
                    bind_const_projection_expr(expr, registry, &mut budget)?;
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
    let having = match validated.having() {
        None => None,
        Some(h) => Some(bind_having(h, registry, &mut budget)?),
    };
    Ok(BoundConstSelect {
        items,
        where_clause,
        having,
        limit: validated.limit(),
        offset: validated.offset(),
        has_aggregate: validated.having().is_some()
            || validated.items().iter().any(|i| i.aggregate().is_some()),
    })
}

/// 集計 1 つの値を求める（投影項目と `HAVING` で共有）。入力行数はダミー 1 行が `WHERE` を
/// 通れば 1、そうでなければ 0。引数は `WHERE` の結果に関わらず評価する（エラー有無を
/// フィルタに左右させない）。
fn eval_const_aggregate(
    func: AggregateFunc,
    arg: &BoundAggArg,
    passes: bool,
) -> Result<Cell, SqlSurfaceError> {
    let n: i64 = i64::from(passes);
    let (value, arg_ty) = match arg {
        BoundAggArg::Star => (None, None),
        BoundAggArg::Literal { cell, ty } => (Some(cell.clone()), Some(ty)),
        BoundAggArg::Expr { expr, ty, .. } => (
            Some(expr_value_to_cell(eval(expr, 0, &[])?, ty.as_ref())?),
            ty.as_ref(),
        ),
    };
    let non_null = value.as_ref().map_or(n, |c| i64::from(*c != Cell::Null)) * n;
    if func == AggregateFunc::Count {
        return Ok(Cell::SignedInteger(non_null));
    }
    if non_null == 0 {
        return Ok(Cell::Null);
    }
    let v = value.ok_or_else(|| SqlSurfaceError::Internal {
        detail: "aggregate argument is missing".to_string(),
    })?;
    single_row_aggregate(func, arg_ty, v)
}

/// `HAVING` を評価する（Issue #2331）。集計は `WHERE` の結果に関わらず評価し（エラー有無を
/// フィルタに左右させない）、条件が偽・`NULL` なら不通過。真偽型以外は束縛段で除外済みの
/// ため到達不能だが、panic せず fail-closed に倒す。
fn having_passes(h: &BoundHaving, passes: bool) -> Result<bool, SqlSurfaceError> {
    let cells = h
        .aggregates
        .iter()
        .map(|(func, arg, _)| eval_const_aggregate(*func, arg, passes))
        .collect::<Result<Vec<_>, _>>()?;
    match &h.cond {
        BoundCondition::Const(v) => Ok(v.unwrap_or(false)),
        BoundCondition::Expr(expr) => {
            let scalars = cells
                .iter()
                .map(crate::sql::group_by::cell_to_scalar_ref)
                .collect::<Result<Vec<_>, _>>()?;
            match eval_with_scalars(expr, 0, &[], &scalars)? {
                ExprValue::Bool(b) => Ok(b),
                ExprValue::Null => Ok(false),
                _ => Err(SqlSurfaceError::Internal {
                    detail: "HAVING did not evaluate to boolean".to_string(),
                }),
            }
        }
    }
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
            } => cells.push(expr_value_to_cell(eval(expr, 0, &[])?, result_ty.as_ref())?),
            BoundConstItem::Aggregate { func, arg, .. } => {
                cells.push(eval_const_aggregate(*func, arg, passes)?);
            }
        }
    }
    let having_passes = match &bound.having {
        None => true,
        Some(h) => having_passes(h, passes)?,
    };
    // 集計を含む文は入力 0 行でも 1 行を出す（`GROUP BY` なし）。含まない文は `WHERE` 通過が必要。
    let produces_row = bound.has_aggregate || passes;
    let selected = produces_row
        && having_passes
        && bound.offset.unwrap_or(0) == 0
        && bound.limit.is_none_or(|l| l > 0);
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
