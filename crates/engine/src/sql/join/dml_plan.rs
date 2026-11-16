//! 束縛済み `UPDATE ... FROM`／`DELETE ... USING` から結合 DML の実行入力（[`JoinedDmlPlan`]）への
//! 変換器（SQL-53 (b)・RLS-10、Issue #3201。親 #3200）。
//!
//! 責務境界: `sql::dml_from` の束縛結果（[`BoundUpdateFrom`]・[`BoundDeleteUsing`]）と、呼び出し元が
//! 同じスナップショットから取得したスキーマだけを入力にする純関数で、行・RLS・台帳・書き込み
//! トランザクションには一切触れない。実行は `sql::join::dml`（[`JoinedProbe`]）と
//! `core::dml_join_exec`、SQL 経路への結線は `core::dml_from_exec`（この変換器を呼ぶ）が担う。
//!
//! source の畳み込み（Issue #3294）: FROM／USING が 2 表以上、または `JOIN ... ON` の段を持つときは、
//! source 側のリレーション列と結合段から合成した JOIN 文（[`FoldedSource`]）を結合実行器で評価した
//! 出力 1 つを source 行源として扱い、対象表はその行源と結合キーで突き合わせる。畳み込みスキーマは
//! 「SET・結合キー・`RETURNING` が読む source 列」だけを疑似列 `id` なしで並べた合成スキーマで、
//! `JoinedDmlSource` の添字系列（`columns`・`EquiKey::source_column`・セル位置）はその添字を指す。
//! source が 1 表で段を持たない形は従来の単一表走査を使う。
//!
//! WHERE の振り分け（最上位の `AND` で平坦化した conjunct ごと。判定は解決済みの relation 添字だけで
//! 行い、表名では行わない。自己結合と疑似列 `id`／実列 `id` の取り違えを防ぐ）:
//! - 対象表と source の列同士の等値 → [`EquiKey`]（結合キー。0 本でもよく、その場合は純粋な交差結合で
//!   一致ペア数の上限〔`54000`〕が実行時に効く）
//! - 対象側だけを参照する述語（`OR` を含んでよい） → 対象側フィルタ（既存の述語形 DML と同じ束縛器）
//! - source 側だけを参照する述語 → 単一表 source は [`JoinedDmlSource::where_predicates`]（走査へ押し下げ）、
//!   畳み込み source は合成 JOIN 文の `WHERE`（外部結合の意味論は結合計画器が守る。複数の source に
//!   またがる述語・列同士の比較も可）
//! - それ以外（対象表と source をまたぐ非等値・`OR`、対象側の列比較）はすべて `0A000` で拒否する。
//!   取りこぼした述語は書き込み範囲を広げる（fail-open になる）ため、分類は全域にして未分類を必ず
//!   拒否する。
//!
//! SET: リテラルと対象表の列だけを使う式はそのまま運び、列参照（右辺が source 列のものを含む）と
//! source 列を含む式（行値代入の要素を含む。Issue #3296）は「対象列 ++ source の取り出し列」の
//! 合成スキーマ上の式として束縛する。エラー文言は固定文言と
//! 入力された名前だけで、他テナントの存在・件数・行 id を含めない（変換はデータに依存しない）。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::{
    JoinProjection, JoinStep, JoinWhereExpr, JoinWherePredicate, SqlSurfaceError, ValidatedJoin,
    WherePredicate,
};
use crate::sql::dml_from::{build_scope, BoundDeleteUsing, BoundUpdateFrom};
use crate::sql::dml_from_returning::BoundFromReturning;
use crate::sql::relation::{ColumnRef, ColumnSlot, ResolvedColumn, TableRef};
use crate::sql::udf_call::{BinOp, Expr, UdfRegistry};

use super::dml::{
    EquiKey, FoldedSource, JoinedCopyItem, JoinedDmlPlan, JoinedDmlSource, JoinedDmlTarget,
    JoinedResidual, JoinedSetItem, JoinedUpdateSet,
};
use super::dml_residual::{convert, plan_residual};
use super::plan::{collect_leaf_colrefs, flatten_conjuncts, slot_name};
use super::using::JoinScope;
use super::values::{cell_scalar_supports, cmp_class, key_class};
use super::JoinLimits;

/// 振り分けられない述語の固定文言（`0A000`）。
const UNSUPPORTED_WHERE: &str =
    "UPDATE ... FROM / DELETE ... USING WHERE supports only equality join keys and single-table predicates";

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// WHERE の振り分け結果。
struct Split {
    target: Vec<WherePredicate>,
    /// 単一表 source の述語（走査へ押し下げる）。
    source: Vec<WherePredicate>,
    /// 畳み込み source の述語（修飾済みの論理木。合成 JOIN 文の `WHERE` へ渡す）。
    source_join: Vec<JoinWhereExpr>,
    keys: Vec<EquiKey>,
    /// 対象と単一表 source をまたぐ残余（Issue #3295。畳み込み source では拒否する）。
    residual: Option<JoinedResidual>,
}

/// source 側の列を、`JoinedDmlSource` の添字系列（`columns`・`EquiKey::source_column`）へ写像する。
/// 単一表 source は実表の列添字そのもの（疑似列 `id` は `None`）、畳み込み source は
/// 「必要な (relation, slot) を初出順に並べた畳み込みスキーマ」の添字（疑似列 `id` も 1 列として載る）。
enum SourceMap {
    Single,
    /// 畳み込み列の出自（relation は FROM／USING 側の 0 起点）。
    Folded(Vec<(usize, ColumnSlot)>),
}

impl SourceMap {
    fn is_folded(&self) -> bool {
        matches!(self, Self::Folded(_))
    }

    /// (DML 束縛スコープの relation 添字〔1 起点〕, slot) の列の添字を返す（畳み込みでは未登録なら登録する）。
    fn column(
        &mut self,
        relation: usize,
        slot: ColumnSlot,
    ) -> Result<Option<usize>, SqlSurfaceError> {
        let rel = relation
            .checked_sub(1)
            .ok_or_else(|| internal("source relation index out of range"))?;
        match self {
            Self::Single => {
                if rel != 0 {
                    return Err(internal("source relation index out of range"));
                }
                Ok(match slot {
                    ColumnSlot::Id => None,
                    ColumnSlot::Column(i) => Some(i),
                })
            }
            Self::Folded(cols) => {
                if let Some(p) = cols.iter().position(|&c| c == (rel, slot)) {
                    return Ok(Some(p));
                }
                cols.push((rel, slot));
                Ok(Some(cols.len() - 1))
            }
        }
    }

    /// 登録済みの列の slot を、source 側の添字系列の slot へ写す（`RETURNING` のセル位置の解決用）。
    fn slot_of(&self, relation: usize, slot: ColumnSlot) -> Result<ColumnSlot, SqlSurfaceError> {
        match self {
            Self::Single => Ok(slot),
            Self::Folded(cols) => {
                let rel = relation
                    .checked_sub(1)
                    .ok_or_else(|| internal("source relation index out of range"))?;
                cols.iter()
                    .position(|&c| c == (rel, slot))
                    .map(ColumnSlot::Column)
                    .ok_or_else(|| internal("RETURNING column missing from the folded source"))
            }
        }
    }
}

/// 部分木が参照する relation（対象側を含むか・source 側を含むか）。
fn relation_set(
    expr: &JoinWhereExpr,
    scope: &JoinScope<'_>,
) -> Result<(bool, bool), SqlSurfaceError> {
    let mut refs: Vec<&ColumnRef> = Vec::new();
    collect_leaf_colrefs(expr, &mut refs);
    let (mut target, mut source) = (false, false);
    for c in refs {
        if scope.resolve(c)?.relation() == 0 {
            target = true;
        } else {
            source = true;
        }
    }
    Ok((target, source))
}

/// 結合キー列 1 辺の検査（照合は 0A000）。
fn check_key_side(schema: &TableSchema, slot: ColumnSlot) -> Result<(), SqlSurfaceError> {
    if let ColumnSlot::Column(i) = slot {
        // ハッシュ照合はバイト等価のため、非バイト列順の照合の列はキーにできない。
        crate::sql::udf_call::require_wired_collation(
            schema.columns.get(i).and_then(|c| c.collation),
        )?;
    }
    Ok(())
}

/// 対象表と source の列の等値を [`EquiKey`] にする（向きは対象側 `target_column`・source 側
/// `source_column` に正規化）。型クラスが比較できなければ `42804`。キー化できない型・非バイト列順の
/// 照合は `None`（残余として通常の WHERE と同じ評価へ回す。Issue #3295）。
/// `source_schemas` は FROM／USING 側の実表スキーマ（宣言順）。
fn equi_key(
    lhs: &ResolvedColumn,
    rhs: &ResolvedColumn,
    target_schema: &TableSchema,
    source_schemas: &[&TableSchema],
    map: &mut SourceMap,
) -> Result<Option<EquiKey>, SqlSurfaceError> {
    let (t, s) = match (lhs.relation() == 0, rhs.relation() == 0) {
        (true, false) => (lhs, rhs),
        (false, true) => (rhs, lhs),
        _ => return Err(internal("join key columns are not on distinct relations")),
    };
    match (cmp_class(t.column_type()), cmp_class(s.column_type())) {
        (Some(a), Some(b)) if a == b => {}
        (Some(_), Some(_)) => {
            return Err(SqlSurfaceError::datatype_mismatch(
                "JOIN key columns have incompatible types",
            ))
        }
        _ => return Ok(None),
    }
    let (Ok(tc), Ok(sc)) = (key_class(t.column_type()), key_class(s.column_type())) else {
        return Ok(None);
    };
    if tc != sc {
        return Ok(None);
    }
    if check_key_side(target_schema, t.slot()).is_err() {
        return Ok(None);
    }
    let source_schema = s
        .relation()
        .checked_sub(1)
        .and_then(|r| source_schemas.get(r))
        .ok_or_else(|| internal("source relation index out of range"))?;
    if check_key_side(source_schema, s.slot()).is_err() {
        return Ok(None);
    }
    let target_column = match t.slot() {
        ColumnSlot::Id => None,
        ColumnSlot::Column(i) => Some(i),
    };
    Ok(Some(EquiKey {
        target_column,
        source_column: map.column(s.relation(), s.slot())?,
    }))
}

/// 解決済みの列参照を、公開名で修飾した列参照へ書き換える（畳み込み source の合成 JOIN 文は
/// 修飾済みの参照だけを受け取り、DML 側の非修飾解決〔対象表を含むスコープ〕に依存しない）。
fn qualify_ref(
    c: &ColumnRef,
    scope: &JoinScope<'_>,
    relations: &[TableRef],
    schemas: &[&TableSchema],
) -> Result<ColumnRef, SqlSurfaceError> {
    let r = scope.resolve(c)?;
    let table = relations
        .get(r.relation())
        .ok_or_else(|| internal("relation index out of range in DML join WHERE"))?;
    let schema = schemas
        .get(r.relation())
        .ok_or_else(|| internal("relation index out of range in DML join WHERE"))?;
    Ok(ColumnRef::qualified(
        table.exposed_name(),
        slot_name(r.slot(), schema)?,
    ))
}

/// 論理木の全ての列参照を [`qualify_ref`] で書き換える。
fn qualify_expr(
    expr: &JoinWhereExpr,
    scope: &JoinScope<'_>,
    relations: &[TableRef],
    schemas: &[&TableSchema],
) -> Result<JoinWhereExpr, SqlSurfaceError> {
    let q = |c: &ColumnRef| qualify_ref(c, scope, relations, schemas);
    Ok(match expr {
        JoinWhereExpr::And(ch) => JoinWhereExpr::And(
            ch.iter()
                .map(|e| qualify_expr(e, scope, relations, schemas))
                .collect::<Result<_, _>>()?,
        ),
        JoinWhereExpr::Or(ch) => JoinWhereExpr::Or(
            ch.iter()
                .map(|e| qualify_expr(e, scope, relations, schemas))
                .collect::<Result<_, _>>()?,
        ),
        JoinWhereExpr::Leaf(p) => JoinWhereExpr::Leaf(match p {
            JoinWherePredicate::Equality { column, value } => JoinWherePredicate::Equality {
                column: q(column)?,
                value: value.clone(),
            },
            JoinWherePredicate::Prefix { column, pattern } => JoinWherePredicate::Prefix {
                column: q(column)?,
                pattern: pattern.clone(),
            },
            JoinWherePredicate::Compare { column, op, value } => JoinWherePredicate::Compare {
                column: q(column)?,
                op: *op,
                value: value.clone(),
            },
            JoinWherePredicate::BoolEquality { column, value } => {
                JoinWherePredicate::BoolEquality {
                    column: q(column)?,
                    value: *value,
                }
            }
            JoinWherePredicate::BoolColumn { column } => {
                JoinWherePredicate::BoolColumn { column: q(column)? }
            }
            JoinWherePredicate::InList { column, values } => JoinWherePredicate::InList {
                column: q(column)?,
                values: values.clone(),
            },
            JoinWherePredicate::ColumnCompare { lhs, op, rhs } => {
                JoinWherePredicate::ColumnCompare {
                    lhs: q(lhs)?,
                    op: *op,
                    rhs: q(rhs)?,
                }
            }
            JoinWherePredicate::NumericCompare { column, op, value } => {
                JoinWherePredicate::NumericCompare {
                    column: q(column)?,
                    op: *op,
                    value: value.clone(),
                }
            }
            JoinWherePredicate::IsNull { column, negated } => JoinWherePredicate::IsNull {
                column: q(column)?,
                negated: *negated,
            },
        }),
    })
}

/// WHERE を結合キー・対象側述語・source 側述語へ振り分ける（判定順は文書冒頭のとおり決定的）。
/// WHERE の省略は全ての述語が空（結合キーも 0 本）で、純粋な交差結合になる。
fn split_where(
    relations: &[TableRef],
    where_clause: Option<&JoinWhereExpr>,
    target_schema: &TableSchema,
    other_schemas: &[&TableSchema],
    map: &mut SourceMap,
    other_visible: &[Option<Vec<String>>],
    udfs: &UdfRegistry,
) -> Result<Split, SqlSurfaceError> {
    let (Some(target_ref), others) = (relations.first(), relations.get(1..).unwrap_or(&[])) else {
        return Err(internal("DML join without a target relation"));
    };
    if others.is_empty() || others.len() != other_schemas.len() {
        return Err(internal("DML join schema count mismatch"));
    }
    let mut split = Split {
        target: Vec::new(),
        source: Vec::new(),
        source_join: Vec::new(),
        keys: Vec::new(),
        residual: None,
    };
    let Some(where_clause) = where_clause else {
        return Ok(split);
    };
    let scope = JoinScope::plain(build_scope(
        target_ref,
        target_schema,
        others,
        other_schemas,
        other_visible,
    )?);

    let mut conjuncts: Vec<&JoinWhereExpr> = Vec::new();
    flatten_conjuncts(where_clause, &mut conjuncts);
    let mut schemas: Vec<&TableSchema> = Vec::with_capacity(other_schemas.len().saturating_add(1));
    schemas.push(target_schema);
    schemas.extend(other_schemas.iter().copied());
    let unsupported = || SqlSurfaceError::feature_not_supported(UNSUPPORTED_WHERE);
    let mut residual: Vec<&JoinWhereExpr> = Vec::new();
    // 単一 relation で完結する部分木の列名解決（解決済みの relation 添字で行う）。
    let mut name_of = |r: &ColumnRef| -> Result<String, SqlSurfaceError> {
        let resolved = scope.resolve(r)?;
        let schema = schemas
            .get(resolved.relation())
            .ok_or_else(|| internal("relation index out of range in DML join WHERE"))?;
        slot_name(resolved.slot(), schema)
    };
    for c in conjuncts {
        // 判定は解決済みの relation 添字だけで行い、全域にする（取りこぼしは書き込み範囲を広げる）。
        match relation_set(c, &scope)? {
            (true, false) => split.target.extend(convert(c, &mut name_of)?),
            (false, true) if map.is_folded() => {
                split
                    .source_join
                    .push(qualify_expr(c, &scope, relations, &schemas)?);
            }
            (false, true) => split.source.extend(convert(c, &mut name_of)?),
            (true, true) => {
                // 列同士の等値は結合キー。キー化できない型・等値以外・`OR` などは残余（Issue #3295）。
                if let JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare { lhs, op, rhs }) = c {
                    if *op == BinOp::Eq {
                        let (l, r) = (scope.resolve(lhs)?, scope.resolve(rhs)?);
                        if let Some(k) = equi_key(&l, &r, target_schema, other_schemas, map)? {
                            split.keys.push(k);
                            continue;
                        }
                    }
                }
                residual.push(c);
            }
            (false, false) => return Err(unsupported()),
        }
    }
    if !residual.is_empty() {
        // 残余は単一表 source 専用（畳み込み source は対象行 × 畳み込み行の評価ビューを持たない）。
        let ([source_schema], false) = (other_schemas, map.is_folded()) else {
            return Err(unsupported());
        };
        split.residual = Some(plan_residual(
            &residual,
            &scope,
            target_schema,
            source_schema,
            udfs,
        )?);
    }
    Ok(split)
}
/// 対象側述語を対象表のスキーマで束縛して [`JoinedDmlTarget`] を作る（既存の述語形 DML と同じ束縛器。
/// embedding を参照する述語は候補列挙の前提が崩れるため `0A000`）。
fn build_target(
    table: &str,
    schema: &TableSchema,
    preds: &[WherePredicate],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlTarget, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (metadata_filters, expr_filters, _rls, or_filters) =
        crate::sql::parser::bind_where_predicates(preds, schema, udfs, &mut node_budget, &[])?;
    if expr_filters
        .iter()
        .any(crate::sql::udf_call::references_embedding)
        || or_filters.iter().any(|g| g.references_embedding())
    {
        return Err(SqlSurfaceError::feature_not_supported(
            "UPDATE ... FROM / DELETE ... USING WHERE cannot reference a VECTOR column",
        ));
    }
    Ok(JoinedDmlTarget {
        table: table.to_string(),
        schema: schema.clone(),
        metadata_filters,
        expr_filters,
        or_filters,
    })
}

/// source 側述語を検証する（単一表 source。束縛結果は捨て、実行時の側スキャンと同じ型解析をここで
/// 前倒しする）。
fn build_source(
    table: &str,
    schema: &TableSchema,
    preds: Vec<WherePredicate>,
    columns: Vec<usize>,
    returning_columns: Vec<usize>,
    include_pseudo_id: bool,
    udfs: &UdfRegistry,
) -> Result<JoinedDmlSource, SqlSurfaceError> {
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    crate::sql::parser::bind_where_predicates(&preds, schema, udfs, &mut node_budget, &[])?;
    Ok(JoinedDmlSource {
        table: table.to_string(),
        schema: schema.clone(),
        where_predicates: preds,
        columns,
        returning_columns,
        include_pseudo_id,
        fold: None,
    })
}

/// 畳み込み source を組み立てる（Issue #3294）。畳み込みスキーマは疑似列 `id` を持たない採番方式
/// （`cte_schema` と同じ作法）で、列の型は元の列の複製（source の疑似列 `id` は整数クラスの
/// `BIGINT` 列として載せ、セルは整数のまま運ぶ）。合成 JOIN 文の射影は実行時に差し替える。
fn build_folded_source(
    others: &[TableRef],
    steps: &[JoinStep],
    other_schemas: &[&TableSchema],
    cols: Vec<(usize, ColumnSlot)>,
    source_join: Vec<JoinWhereExpr>,
    columns: Vec<usize>,
    returning_columns: Vec<usize>,
) -> Result<JoinedDmlSource, SqlSurfaceError> {
    if steps.len().saturating_add(1) != others.len() || others.len() != other_schemas.len() {
        return Err(internal("DML join step count mismatch"));
    }
    let mut defs: Vec<ColumnDef> = Vec::with_capacity(cols.len());
    for (pos, (rel, slot)) in cols.iter().enumerate() {
        let schema = other_schemas
            .get(*rel)
            .ok_or_else(|| internal("folded source relation out of range"))?;
        let ty = match slot {
            ColumnSlot::Id => ColumnType::BigInt,
            ColumnSlot::Column(i) => schema
                .columns
                .get(*i)
                .map(|c| c.ty.clone())
                .ok_or_else(|| internal("folded source column out of range"))?,
        };
        defs.push(ColumnDef::new(synthetic_name(pos), ty, true));
    }
    let schema = TableSchema::new("\u{0}folded".to_string(), defs)
        .with_row_identity(crate::catalog::RowIdentity::Allocated);
    let where_clause = match source_join.len() {
        0 => None,
        1 => source_join.into_iter().next().map(Box::new),
        _ => Some(Box::new(JoinWhereExpr::And(source_join))),
    };
    let join = ValidatedJoin {
        relations: others.to_vec(),
        steps: steps.to_vec(),
        projection: JoinProjection::Columns(Vec::new()),
        column_aliases: Vec::new(),
        where_clause,
        order_by: Vec::new(),
        aggregate: None,
        limit: None,
        offset: 0,
        cte_inputs: None,
    };
    let real = others
        .iter()
        .zip(other_schemas.iter())
        .map(|(r, s)| (r.table().to_string(), (*s).clone()))
        .collect();
    Ok(JoinedDmlSource {
        table: String::new(),
        schema,
        where_predicates: Vec::new(),
        columns,
        returning_columns,
        include_pseudo_id: false,
        fold: Some(Box::new(FoldedSource { join, cols, real })),
    })
}

/// `RETURNING` が参照する source（relation 1 以上）の列を、SET 用の列とは別の `returning_columns` へ
/// 重複なしで集める。SET の式評価ビュー（`source_scalar`。VECTOR 等は写像できない）へ載せないため
/// 分離する。単一表 source で疑似列 `id` が参照されていれば真を返す（畳み込み source の疑似列 `id` は
/// 通常の畳み込み列として `returning_columns` に載る）。列型は制限しない（セルを複製して返すだけ）。
fn returning_source_columns(
    returning: Option<&BoundFromReturning>,
    map: &mut SourceMap,
    returning_columns: &mut Vec<usize>,
) -> Result<bool, SqlSurfaceError> {
    let mut include_pseudo_id = false;
    let Some(ret) = returning else {
        return Ok(include_pseudo_id);
    };
    for col in &ret.columns {
        if col.relation == 0 {
            continue;
        }
        match map.column(col.relation, col.slot)? {
            None => include_pseudo_id = true,
            Some(i) => {
                if !returning_columns.contains(&i) {
                    returning_columns.push(i);
                }
            }
        }
    }
    Ok(include_pseudo_id)
}

/// 計画の走査投影が確定した後に、`RETURNING` の source 側の列が読むセル位置を確定する。
fn resolve_returning(
    plan: &mut JoinedDmlPlan,
    map: &SourceMap,
    returning: Option<BoundFromReturning>,
) -> Result<(), SqlSurfaceError> {
    plan.returning = match returning {
        None => None,
        Some(mut ret) => {
            ret.assign_source_cells(|relation, slot| {
                plan.source_cell_position(map.slot_of(relation, slot)?)
            })?;
            Some(ret)
        }
    };
    Ok(())
}
/// source 列が SET 式の入力として結合ビューへ写像できる型か（`sql::join::dml::source_scalar` が
/// 扱う型と一致させる。写像できない型は実行時の `XX000` にせず変換の段階で拒否する）。
fn source_type_supported(ty: &ColumnType) -> bool {
    cell_scalar_supports(ty)
        || matches!(
            ty,
            ColumnType::Time
                | ColumnType::TimestampTz
                | ColumnType::Interval
                | ColumnType::Jsonb
                | ColumnType::Array(_)
        )
}

/// 式としての SET 代入（`bind_set_expr`）が受理する代入先の列型か。受理しない型（JSONB／JSON／
/// UUID／BYTEA／NUMERIC／ENUM 等）への source 列の単純コピーは式を経由せず [`JoinedCopyItem`] で運ぶ。
fn set_expr_accepts_target(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::SmallInt
            | ColumnType::Integer
            | ColumnType::BigInt
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Text
            | ColumnType::Varchar(_)
            | ColumnType::Bpchar(_)
            | ColumnType::Boolean
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::TimestampTz
            | ColumnType::Interval
            | ColumnType::Time
            | ColumnType::Array(_)
            | ColumnType::Bit(_)
    )
}

/// 結合ビュー上の source 列の合成名。利用者の識別子と衝突しない綴り（NUL を含む）にする。
fn synthetic_name(position: usize) -> String {
    format!("\u{0}src{position}")
}

/// SET 句を [`JoinedUpdateSet`] へ変換し、取り出す source 列（初出順・重複なし）を返す。
fn plan_set(
    bound: &BoundUpdateFrom,
    target_schema: &TableSchema,
    other_schemas: &[&TableSchema],
    map: &mut SourceMap,
) -> Result<(JoinedUpdateSet, Vec<usize>), SqlSurfaceError> {
    let mut set = JoinedUpdateSet {
        copies: Vec::new(),
        literal_assignments: bound.set_literals().to_vec(),
        items: bound
            .set_exprs()
            .iter()
            .map(|(i, e)| JoinedSetItem {
                column_index: *i,
                set: e.clone(),
            })
            .collect(),
    };
    // 合成スキーマ: 対象表 ++ source の取り出し列。対象列の添字は不変で、末尾への追記だけを行う。
    let mut combined = target_schema.clone();
    let mut columns: Vec<usize> = Vec::new();
    for (target_idx, resolved) in bound.set_columns() {
        let target_col = target_schema
            .columns
            .get(*target_idx)
            .ok_or_else(|| internal("SET column out of range"))?;
        let mut copy_pos: Option<usize> = None;
        let name = if resolved.relation() == 0 {
            slot_name(resolved.slot(), target_schema)?
        } else {
            let (pos, col_ty) =
                register_source_column(resolved, other_schemas, map, &mut columns, &mut combined)?;
            if !set_expr_accepts_target(&target_col.ty) {
                // 式経路が受理しない型の単純コピー。型が一致する組だけを運ぶ。
                if col_ty != target_col.ty {
                    return Err(SqlSurfaceError::datatype_mismatch(format!(
                        "column {:?} cannot be assigned a value of a different type",
                        target_col.name
                    )));
                }
                copy_pos = Some(pos);
            }
            synthetic_name(pos)
        };
        if let Some(source_pos) = copy_pos {
            set.copies.push(JoinedCopyItem {
                column_index: *target_idx,
                source_pos,
                target_ty: target_col.ty.clone(),
                nullable: target_col.nullable,
                column_name: target_col.name.clone(),
            });
            continue;
        }
        let bound_set = crate::sql::set_expr::bind_set_expr(
            &target_col.name,
            &Expr::Ident(name),
            target_col,
            &combined,
        )?;
        set.items.push(JoinedSetItem {
            column_index: *target_idx,
            set: bound_set,
        });
    }
    // FROM 側の列を含む式（Issue #3296）。参照ごとに合成スキーマ上の名前へ写してから、単一表 SET と
    // 同じ `bind_set_expr` で束縛する（型の照合・暗黙キャスト・式非対応の代入先型の `0A000` も同じ）。
    for (target_idx, expr, refs) in bound.set_joined_exprs() {
        let target_col = target_schema
            .columns
            .get(*target_idx)
            .ok_or_else(|| internal("SET column out of range"))?;
        let mut names = Vec::with_capacity(refs.len());
        for resolved in refs {
            names.push(match resolved.relation() {
                0 => slot_name(resolved.slot(), target_schema)?,
                _ => {
                    let (pos, _) = register_source_column(
                        resolved,
                        other_schemas,
                        map,
                        &mut columns,
                        &mut combined,
                    )?;
                    synthetic_name(pos)
                }
            });
        }
        let mapped = crate::sql::dml_from::restore_set_refs(expr, &names);
        let bound_set =
            crate::sql::set_expr::bind_set_expr(&target_col.name, &mapped, target_col, &combined)?;
        set.items.push(JoinedSetItem {
            column_index: *target_idx,
            set: bound_set,
        });
    }
    Ok((set, columns))
}

/// 解決済みの source 側の列（relation > 0）を取り出し列（`columns`。初出順・重複なし）へ登録し、合成スキーマ
/// `combined` へ合成名つきの列を追記して、取り出し列内の位置と列型を返す。結合ビューは source の疑似列
/// `id` を運ばないため、それを指す参照と、写像できない型は `0A000`。単純コピー・式（Issue #3296）の双方が
/// 共有する（source の列を合成スキーマへ写す箇所をここに閉じ込める）。
fn register_source_column(
    resolved: &ResolvedColumn,
    other_schemas: &[&TableSchema],
    map: &mut SourceMap,
    columns: &mut Vec<usize>,
    combined: &mut TableSchema,
) -> Result<(usize, ColumnType), SqlSurfaceError> {
    let real_idx = match resolved.slot() {
        ColumnSlot::Id => {
            return Err(SqlSurfaceError::feature_not_supported(
                "UPDATE ... FROM: the pseudo column id of the FROM table cannot be used in SET",
            ));
        }
        ColumnSlot::Column(i) => i,
    };
    let source_schema = resolved
        .relation()
        .checked_sub(1)
        .and_then(|r| other_schemas.get(r))
        .ok_or_else(|| internal("SET relation index out of range"))?;
    let col = source_schema
        .columns
        .get(real_idx)
        .ok_or_else(|| internal("source column out of range"))?;
    if !source_type_supported(&col.ty) {
        return Err(SqlSurfaceError::feature_not_supported(
            "UPDATE ... FROM: the FROM-table column type is not supported in SET",
        ));
    }
    let i = map
        .column(resolved.relation(), resolved.slot())?
        .ok_or_else(|| internal("source column has no index"))?;
    if let Some(p) = columns.iter().position(|&c| c == i) {
        return Ok((p, col.ty.clone()));
    }
    columns.push(i);
    combined.columns.push(ColumnDef::new(
        synthetic_name(columns.len() - 1),
        col.ty.clone(),
        true,
    ));
    Ok((columns.len() - 1, col.ty.clone()))
}

/// 組み立てた計画の形を検証して返す（添字の範囲外は変換の段階で `Internal` にする）。畳み込み source は、
/// 合成 JOIN 文の束縛（`ON` の列・型の検証、リレーション数）もここで前倒しして行い、Describe と
/// Execute で拒否コードがそろうようにする（行は読まない）。
fn finish(plan: JoinedDmlPlan, udfs: &UdfRegistry) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    plan.validate(&JoinLimits::default())?;
    if let Some(fold) = &plan.source.fold {
        fold.prepare(&plan.scan_columns(), udfs)?;
    }
    Ok(plan)
}

/// 結合形 DML の変換の共通入力（`plan_update_from`／`plan_delete_using` が束縛結果から写す）。
struct PlanInput<'a> {
    relations: &'a [TableRef],
    steps: &'a [JoinStep],
    where_clause: Option<&'a JoinWhereExpr>,
    update: Option<&'a BoundUpdateFrom>,
    returning: Option<&'a BoundFromReturning>,
    /// relation ごとの見える列（ビュー source の公開列。Issue #3292）。
    source_visible: &'a [Option<Vec<String>>],
}

fn plan_joined(
    input: PlanInput<'_>,
    target_schema: &TableSchema,
    other_schemas: &[&TableSchema],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    let (Some(target_ref), others) = (
        input.relations.first(),
        input.relations.get(1..).unwrap_or(&[]),
    ) else {
        return Err(internal("DML join without a target relation"));
    };
    // 畳み込みの要否は構文で決まる（source が 2 表以上、または JOIN の段を持つ）。
    let mut map = if others.len() > 1 || !input.steps.is_empty() {
        SourceMap::Folded(Vec::new())
    } else {
        SourceMap::Single
    };
    let split = split_where(
        input.relations,
        input.where_clause,
        target_schema,
        other_schemas,
        &mut map,
        input.source_visible,
        udfs,
    )?;
    let target = build_target(target_ref.table(), target_schema, &split.target, udfs)?;
    let (update, columns) = match input.update {
        Some(bound) => {
            let (u, c) = plan_set(bound, target_schema, other_schemas, &mut map)?;
            (Some(u), c)
        }
        None => (None, Vec::new()),
    };
    let mut returning_columns = Vec::new();
    let include_pseudo_id =
        returning_source_columns(input.returning, &mut map, &mut returning_columns)?;
    // 畳み込み source が何も取り出さない形（キーも `RETURNING` も無い `DELETE`）でも、走査投影が空に
    // ならないよう先頭の relation の 1 列を取り出す（行の有無と件数だけが必要）。
    if matches!(&map, SourceMap::Folded(cols) if cols.is_empty()) {
        let first = other_schemas
            .first()
            .ok_or_else(|| internal("DML join without a source relation"))?;
        let slot = if first.has_pseudo_id() {
            ColumnSlot::Id
        } else if first.columns.is_empty() {
            return Err(internal("source table without a column"));
        } else {
            ColumnSlot::Column(0)
        };
        if let Some(i) = map.column(1, slot)? {
            returning_columns.push(i);
        }
    }
    let source = match &map {
        SourceMap::Single => {
            let (Some(source_ref), Some(source_schema)) = (others.first(), other_schemas.first())
            else {
                return Err(internal("DML join without a source relation"));
            };
            build_source(
                source_ref.table(),
                source_schema,
                split.source,
                columns,
                returning_columns,
                include_pseudo_id,
                udfs,
            )?
        }
        SourceMap::Folded(cols) => build_folded_source(
            others,
            input.steps,
            other_schemas,
            cols.clone(),
            split.source_join,
            columns,
            returning_columns,
        )?,
    };
    let mut plan = JoinedDmlPlan {
        target,
        source,
        keys: split.keys,
        residual: split.residual,
        update,
        returning: None,
    };
    resolve_returning(&mut plan, &map, input.returning.cloned())?;
    finish(plan, udfs)
}

/// 束縛済みの `UPDATE ... FROM` を [`JoinedDmlPlan`] へ変換する。呼び出し元は
/// `core::dml_from_exec`（同じスナップショットのスキーマを渡す）。
pub(crate) fn plan_update_from(
    bound: &BoundUpdateFrom,
    target_schema: &TableSchema,
    from_schemas: &[&TableSchema],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    plan_joined(
        PlanInput {
            relations: bound.relations(),
            steps: bound.steps(),
            where_clause: bound.where_clause(),
            update: Some(bound),
            returning: bound.returning(),
            source_visible: bound.source_visible(),
        },
        target_schema,
        from_schemas,
        udfs,
    )
}

/// 束縛済みの `DELETE ... USING` を [`JoinedDmlPlan`] へ変換する（`update` は `None`）。
pub(crate) fn plan_delete_using(
    bound: &BoundDeleteUsing,
    target_schema: &TableSchema,
    using_schemas: &[&TableSchema],
    udfs: &UdfRegistry,
) -> Result<JoinedDmlPlan, SqlSurfaceError> {
    plan_joined(
        PlanInput {
            relations: bound.relations(),
            steps: bound.steps(),
            where_clause: bound.where_clause(),
            update: None,
            returning: bound.returning(),
            source_visible: bound.source_visible(),
        },
        target_schema,
        using_schemas,
        udfs,
    )
}
#[cfg(test)]
#[path = "dml_plan_tests.rs"]
mod tests;
