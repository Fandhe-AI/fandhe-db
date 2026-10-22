//! `INSERT ... SELECT` の束縛と、内側 SELECT 結果行の `BoundInsert` への変換（SQL-51。親: #1731）。
//!
//! 2 つの責務を 1 モジュールに持つ。
//! - 束縛（Issue #1732）: [`bind`] が検証済みの [`ValidatedInsertSelect`] を挿入先の [`TableSchema`] と
//!   内側 SELECT の結果列メタ（[`ColumnMeta`]。`core.rs` が既存の Describe で導出する）に突き合わせ、
//!   列リストの解決・列数（`42601`）・型（`42804`）・`RETURNING` を挿入より前に検証する。純関数で、
//!   ストレージ・RLS・台帳に触れない。`core.rs` の Describe 腕・実行腕から呼ばれる。
//! - 変換（Issue #1733）: 内側 SELECT の評価（RLS の暗黙適用・行数上限）と書き込みは
//!   `core/insert_select_exec.rs` が担い、本モジュールは「結果セル（[`Cell`]）→ 列型の行値
//!   （[`Value`]）」と「結果行 → [`BoundInsert`]」の変換だけを持つ。変換後の `BoundInsert` は
//!   複数行 `VALUES`・COPY と同じ書き込み経路（`sql::exec::execute_insert_*_in`）へ渡る
//!   （第 2 の書き込み経路を作らない）。
//!
//! fail-closed 方針: 代入変換を持たない（セル種別, 列型）の組は黙って丸めず `0A000`（固定文言）で
//! 拒否する。静的な型照合（`42804` 等）は束縛側が先に行い、変換段へ到達した不整合は値を含まない
//! 固定文言のエラーにする。エラー文言には行の値・他テナントの情報を含めない。
//!
//! 呼び出し元: `core.rs`（束縛）・`core/insert_select_exec.rs` の `EngineCore::execute_insert_select`（変換）。

use super::allowlist::{SqlSurfaceError, ValidatedInsertSelect};
use super::exec::{Cell, ColumnMeta, ResultRow, ALIASED_ID_TYPE};
use super::parser::{BoundInsert, ProjectedColumn, VolatileColumn};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::numeric::Decimal;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;

/// 挿入先として解決した 1 列（内側 SELECT の出力列 1 本の行き先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetColumn {
    /// 疑似列 `id`（従来方式・移行済み表のみ）。
    Id,
    /// `schema.columns` 上の位置。
    Column(usize),
}

/// 挿入先の列リストを解決する。`columns` が `None`（列リスト省略）のときは宣言順で、従来方式の
/// 表では先頭に疑似列 `id` を置く（移行済み表・採番方式の表は `id` を含めない。移行済み表の `id` は
/// 書き込み段が IDENTITY 採番する）。指定がある場合は未知の列を `42703`、重複を `42701` で拒否する。
pub(crate) fn resolve_target_columns(
    schema: &TableSchema,
    columns: Option<&[String]>,
) -> Result<Vec<TargetColumn>, SqlSurfaceError> {
    let has_pseudo_id = schema.has_pseudo_id();
    let mut out: Vec<TargetColumn> = Vec::new();
    match columns {
        None => {
            out.try_reserve_exact(schema.columns.len().saturating_add(1))
                .map_err(|_| internal())?;
            if has_pseudo_id && !schema.is_migrated_id() {
                out.push(TargetColumn::Id);
            }
            out.extend((0..schema.columns.len()).map(TargetColumn::Column));
        }
        Some(names) => {
            out.try_reserve_exact(names.len()).map_err(|_| internal())?;
            for name in names {
                let target = if has_pseudo_id && name == "id" {
                    TargetColumn::Id
                } else {
                    let idx = schema
                        .columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| SqlSurfaceError::undefined_column(name.clone()))?;
                    TargetColumn::Column(idx)
                };
                if out.contains(&target) {
                    return Err(SqlSurfaceError::duplicate_column(name.clone()));
                }
                out.push(target);
            }
        }
    }
    if out.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT ... SELECT target column list must not be empty",
        ));
    }
    Ok(out)
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 代入変換を持たない（セル種別, 列型）の組の固定エラー（行の値・列名を含めない）。
fn unsupported_pair() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "INSERT ... SELECT does not support this value type conversion",
    )
}

fn out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::numeric_out_of_range("INSERT ... SELECT value is out of range")
}

/// 内側 SELECT のセルを、挿入先の列 `column` の行値へ代入変換する。
///
/// - `NULL`: 非 nullable 列は `23502`（`DEFAULT` は適用しない。明示 NULL と省略は別。TABLE-16）
/// - 文字列系（`VARCHAR(n)`・`CHAR(n)` の長さ検証と空白詰めを含む）・ENUM・JSON は
///   VALUES 形と同じ束縛関数で再検証する
/// - 数値は整数幅の範囲検査（`22003`）・`NUMERIC(p,s)` の再検証・整数 → 浮動小数の拡大を行う
/// - 配列・浮動小数 → 整数などこの Issue が変換を持たない組は `0A000`
pub(crate) fn cell_to_insert_value(
    cell: Cell,
    column: &ColumnDef,
) -> Result<Value, SqlSurfaceError> {
    if matches!(cell, Cell::Null) {
        if !column.nullable {
            return Err(SqlSurfaceError::not_null_violation(column.name.clone()));
        }
        return Ok(Value::Null);
    }
    match (&column.ty, cell) {
        (ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_), Cell::Text(t)) => {
            super::parser::bind_text_literal(&column.ty, &t)
        }
        (ColumnType::Enum(def), Cell::Text(t)) => {
            super::parser::bind_enum_literal(def, &t, &column.name)
        }
        (ColumnType::Json | ColumnType::Jsonb, Cell::Json(j)) => {
            super::parser::bind_json_literal(&j, &column.ty, &column.name)
        }
        (ColumnType::Vector(dim), Cell::Vector(v)) => {
            if u32::try_from(v.len()).ok() != Some(*dim) {
                return Err(SqlSurfaceError::invalid_input(
                    "INSERT ... SELECT vector dimension does not match the column",
                ));
            }
            Ok(Value::Vector(v))
        }
        (
            ColumnType::Numeric { precision, scale },
            c @ (Cell::Numeric(_) | Cell::Integer(_) | Cell::SignedInteger(_)),
        ) => {
            let d = match c {
                Cell::Numeric(d) => d,
                Cell::Integer(u) => {
                    Decimal::from_parts(i128::from(u), 0).map_err(|_| out_of_range())?
                }
                Cell::SignedInteger(i) => {
                    Decimal::from_parts(i128::from(i), 0).map_err(|_| out_of_range())?
                }
                _ => return Err(unsupported_pair()),
            };
            let fitted = super::cast_value::apply_numeric_typmod(d, Some((*precision, *scale)))?;
            Ok(Value::Numeric(fitted))
        }
        // 整数 → 浮動小数の拡大（PostgreSQL の代入変換と同じ向き。逆向きは丸め規則が要るため対象外）。
        (ColumnType::Double, Cell::Integer(u)) => Ok(Value::Double(u as f64)),
        (ColumnType::Double, Cell::SignedInteger(i)) => Ok(Value::Double(i as f64)),
        (ColumnType::Real, Cell::Integer(u)) => Ok(Value::Real(u as f32)),
        (ColumnType::Real, Cell::SignedInteger(i)) => Ok(Value::Real(i as f32)),
        // 配列は要素型の照合規則を持たないため対象外（fail-closed）。
        (ColumnType::Array(_), _) => Err(unsupported_pair()),
        (ty, cell) => match super::matview::cell_to_value(cell, ty) {
            Ok(v) => Ok(v),
            // 同じ型族の組以外は固定文言の `0A000` に写す（matview 用の文言を外へ出さない）。
            Err(SqlSurfaceError::Internal { .. }) => Err(unsupported_pair()),
            // 範囲外も matview 用の文言ではなく本文の固定文言へ写す。
            Err(SqlSurfaceError::NumericOutOfRange { .. }) => Err(out_of_range()),
            Err(e) => Err(e),
        },
    }
}

/// 内側 SELECT の結果行を `BoundInsert` 列へ変換する。省略列は `DEFAULT` を補完し、
/// `DEFAULT nextval` 系の印は束縛直後に `0A000` で拒否する（COPY と同じ fail-closed）。
/// `rows` の行数は呼び出し元が上限検査済みであることを前提に、確保は `try_reserve_exact` で行う。
pub(crate) fn rows_to_bound_inserts(
    table_name: &str,
    schema: &TableSchema,
    targets: &[TargetColumn],
    rows: Vec<ResultRow>,
    operation_id: Option<&OperationId>,
    clock: crate::sql::time_fn::StatementClock,
) -> Result<Vec<BoundInsert>, SqlSurfaceError> {
    let has_pseudo_id = schema.has_pseudo_id();
    let id_targeted = targets.contains(&TargetColumn::Id);
    // 従来方式の表は `id` が必須。移行済み表は省略時に書き込み段が採番する。
    if has_pseudo_id && !id_targeted && !schema.is_migrated_id() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT ... SELECT column list must include the id pseudo-column",
        ));
    }
    let id_omitted = has_pseudo_id && schema.is_migrated_id() && !id_targeted;

    let mut out: Vec<BoundInsert> = Vec::new();
    out.try_reserve_exact(rows.len()).map_err(|_| internal())?;
    for row in rows {
        if row.cells.len() != targets.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "INSERT ... SELECT result shape changed during the statement".to_string(),
            });
        }
        let mut values: Vec<Value> = vec![Value::Null; schema.columns.len()];
        let mut provided = vec![false; schema.columns.len()];
        let mut id: u64 = 0;
        for (target, cell) in targets.iter().zip(row.cells) {
            match target {
                TargetColumn::Id => id = cell_to_row_id(cell)?,
                TargetColumn::Column(idx) => {
                    let column = schema.columns.get(*idx).ok_or_else(internal)?;
                    let value = cell_to_insert_value(cell, column)?;
                    if let Some(slot) = values.get_mut(*idx) {
                        *slot = value;
                    }
                    if let Some(flag) = provided.get_mut(*idx) {
                        *flag = true;
                    }
                }
            }
        }
        let volatile_default_columns: Vec<VolatileColumn> =
            crate::sql::parser::fill_omitted_columns(
                &schema.columns,
                &mut values,
                &provided,
                Some(clock),
                false,
            )?;
        if volatile_default_columns.iter().any(|c| {
            matches!(
                c,
                VolatileColumn::DefaultSequence(_) | VolatileColumn::Sequence(..)
            )
        }) {
            return Err(crate::sql::parser::reject_sequence_call_literal());
        }
        out.push(BoundInsert {
            table: table_name.to_string(),
            id,
            values,
            operation_id: operation_id.cloned(),
            volatile_default_columns,
            id_omitted,
        });
    }
    Ok(out)
}

/// 疑似列 `id` へ流すセルを `u64` にする（非負整数のみ。NULL・負数は `22000`、他の型は `0A000`）。
fn cell_to_row_id(cell: Cell) -> Result<u64, SqlSurfaceError> {
    match cell {
        Cell::Integer(u) => Ok(u),
        Cell::SignedInteger(i) => u64::try_from(i).map_err(|_| {
            SqlSurfaceError::invalid_input("id pseudo-column value must not be negative")
        }),
        Cell::Null => Err(SqlSurfaceError::invalid_input(
            "id pseudo-column value must not be NULL",
        )),
        _ => Err(unsupported_pair()),
    }
}

/// [`bind`] の結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundInsertSelectTarget {
    /// 内側の出力列 i の書き込み先（列リストの順）。
    pub(crate) slots: Vec<TargetColumn>,
    /// 疑似列 `id` を列リストで省略したか（移行済みの表のみ真。採番は書き込み段）。
    pub(crate) id_omitted: bool,
    /// 列リストに無い列（`schema.columns` の添字。既定値の補完は実行側が行う）。
    pub(crate) omitted: Vec<usize>,
    /// `RETURNING` の束縛済み投影。
    pub(crate) returning: Option<Vec<ProjectedColumn>>,
}

/// 出力列の由来（疑似列 `id` への代入可否が値族で決まるため、型だけに潰さず区別する）。
enum SourceTy<'a> {
    /// 疑似列 `id` 由来（`ColumnMeta::Id`／`AliasedId`）。
    IdLike,
    Ty(&'a ColumnType),
}

fn source_ty(meta: &ColumnMeta) -> Result<SourceTy<'_>, SqlSurfaceError> {
    match meta {
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => Ok(SourceTy::IdLike),
        ColumnMeta::Scalar { ty, .. } => Ok(SourceTy::Ty(ty)),
        ColumnMeta::Computed { ty: Some(ty), .. } => Ok(SourceTy::Ty(ty)),
        // 静的な型を持たない式は代入可否を判定できないため拒否する（fail-closed。
        // `set_op::columns_compatible` と同じ方針）。
        ColumnMeta::Computed { ty: None, .. } => Err(SqlSurfaceError::datatype_mismatch(
            "INSERT ... SELECT source column has no static type",
        )),
    }
}

/// 疑似列 `id` へ代入できる整数型か。
fn is_integer_type(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt
    )
}

/// 型 `from` の値を列 `to` へ代入できるか。規則表の外にある型（`VECTOR`・`ENUM`）は同じ値ドメインの
/// ときだけ受理する（暗黙の型変換を作らない）。
fn assignable(from: &ColumnType, to: &ColumnType) -> bool {
    match crate::sql::cast::column_assignment_convertible(from, to) {
        Some(ok) => ok,
        None => from.same_value_domain(to),
    }
}

/// `stmt` の列リスト・列数・型・`RETURNING` を挿入先 `schema` と内側の結果列 `source` に照らして束縛する。
///
/// 判定順は決定的（PostgreSQL と同じ順）: 列リストの解決（重複・未知列・`GENERATED ALWAYS`）→
/// 疑似列 `id` の扱い → 列数（`42601`）→ 型（`42804`）→ `RETURNING`。
/// 内側の結果列メタと挿入先スキーマが同じスナップショットのものであることは本関数では要求しない
/// （実行時の再検証は write txn 内で実行側が行う）。
pub(crate) fn bind(
    stmt: &ValidatedInsertSelect,
    schema: &TableSchema,
    source: &[ColumnMeta],
) -> Result<BoundInsertSelectTarget, SqlSurfaceError> {
    crate::sql::parser::check_insert_column_list_duplicates(&stmt.columns)?;

    let has_pseudo_id = schema.has_pseudo_id();
    let mut slots = Vec::with_capacity(stmt.columns.len());
    let mut provided = vec![false; schema.columns.len()];
    for name in &stmt.columns {
        if has_pseudo_id && name == "id" {
            slots.push(TargetColumn::Id);
            continue;
        }
        let idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        let column = schema
            .columns
            .get(idx)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        crate::sql::parser::reject_generated_always_value(column)?;
        if let Some(flag) = provided.get_mut(idx) {
            *flag = true;
        }
        slots.push(TargetColumn::Column(idx));
    }

    // 疑似列 `id`: 従来方式の表は列リストに必須。移行済みの表は省略を受理する（採番は書き込み段）。
    let id_listed = slots.contains(&TargetColumn::Id);
    let id_omitted = has_pseudo_id && !id_listed && schema.is_migrated_id();
    if has_pseudo_id && !id_listed && !id_omitted {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT column list must include the id pseudo-column",
        ));
    }

    if source.len() != slots.len() {
        let detail = if source.len() > slots.len() {
            "INSERT has more expressions than target columns"
        } else {
            "INSERT has more target columns than expressions"
        };
        return Err(SqlSurfaceError::unsupported(detail));
    }

    for (meta, slot) in source.iter().zip(slots.iter()) {
        let from = source_ty(meta)?;
        let ok = match (slot, &from) {
            (TargetColumn::Id, SourceTy::IdLike) => true,
            (TargetColumn::Id, SourceTy::Ty(ty)) => is_integer_type(ty),
            (TargetColumn::Column(idx), from) => {
                let to = &schema
                    .columns
                    .get(*idx)
                    .ok_or_else(|| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?
                    .ty;
                match from {
                    SourceTy::IdLike => assignable(&ALIASED_ID_TYPE, to),
                    SourceTy::Ty(ty) => assignable(ty, to),
                }
            }
        };
        if !ok {
            return Err(SqlSurfaceError::datatype_mismatch(
                "INSERT ... SELECT column type does not match the target column",
            ));
        }
    }

    let omitted = provided
        .iter()
        .enumerate()
        .filter_map(|(i, p)| (!*p).then_some(i))
        .collect();
    let returning = crate::sql::parser::bind_returning(stmt.returning.as_ref(), schema)?;
    Ok(BoundInsertSelectTarget {
        slots,
        id_omitted,
        omitted,
        returning,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: ColumnType, nullable: bool) -> ColumnDef {
        ColumnDef::new(name, ty, nullable)
    }

    // 対象ビヘイビア: SQL-51（ポインタ）。NULL は nullable 列だけが受理する。
    #[test]
    fn null_requires_nullable_column() {
        let nn = col("a", ColumnType::Integer, false);
        let err = cell_to_insert_value(Cell::Null, &nn).unwrap_err();
        assert_eq!(err.wire_code(), "23502");
        let n = col("a", ColumnType::Integer, true);
        assert_eq!(cell_to_insert_value(Cell::Null, &n).unwrap(), Value::Null);
    }

    #[test]
    fn varchar_overflow_is_22001_and_char_is_padded() {
        let v = col("a", ColumnType::Varchar(Some(3)), true);
        let err = cell_to_insert_value(Cell::Text("abcd".into()), &v).unwrap_err();
        assert_eq!(err.wire_code(), "22001");
        let c = col("a", ColumnType::Bpchar(3), true);
        assert_eq!(
            cell_to_insert_value(Cell::Text("ab".into()), &c).unwrap(),
            Value::Text("ab ".into())
        );
    }

    #[test]
    fn integer_widths_check_range() {
        let small = col("a", ColumnType::SmallInt, true);
        let err = cell_to_insert_value(Cell::SignedInteger(70_000), &small).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
        let big = col("a", ColumnType::BigInt, true);
        assert_eq!(
            cell_to_insert_value(Cell::SignedInteger(7), &big).unwrap(),
            Value::BigInt(7)
        );
        let int = col("a", ColumnType::Integer, true);
        assert_eq!(
            cell_to_insert_value(Cell::Integer(7), &int).unwrap(),
            Value::Integer(7)
        );
    }

    #[test]
    fn integer_widens_to_numeric_and_double() {
        let n = col(
            "a",
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            true,
        );
        let v = cell_to_insert_value(Cell::SignedInteger(5), &n).unwrap();
        assert!(matches!(v, Value::Numeric(d) if d.scale() == 2));
        let overflow = col(
            "a",
            ColumnType::Numeric {
                precision: 3,
                scale: 2,
            },
            true,
        );
        let err = cell_to_insert_value(Cell::SignedInteger(500), &overflow).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
        let d = col("a", ColumnType::Double, true);
        assert_eq!(
            cell_to_insert_value(Cell::SignedInteger(3), &d).unwrap(),
            Value::Double(3.0)
        );
    }

    #[test]
    fn unsupported_pairs_are_0a000_without_values() {
        let int = col("a", ColumnType::Integer, true);
        let err = cell_to_insert_value(Cell::Float(1.5), &int).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
        assert!(!err.to_string().contains("1.5"));
        let t = col("a", ColumnType::Text, true);
        let err = cell_to_insert_value(Cell::Bool(true), &t).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn vector_dimension_must_match() {
        let v = col("e", ColumnType::Vector(2), false);
        assert!(cell_to_insert_value(Cell::Vector(vec![1.0, 2.0]), &v).is_ok());
        assert!(cell_to_insert_value(Cell::Vector(vec![1.0]), &v).is_err());
    }

    #[test]
    fn target_columns_reject_unknown_and_duplicate() {
        let schema = TableSchema::new(
            "t",
            vec![
                col("e", ColumnType::Vector(2), false),
                col("lang", ColumnType::Text, true),
            ],
        );
        let names = vec!["lang".to_string(), "nope".to_string()];
        assert_eq!(
            resolve_target_columns(&schema, Some(&names))
                .unwrap_err()
                .wire_code(),
            "42703"
        );
        let names = vec!["lang".to_string(), "lang".to_string()];
        assert_eq!(
            resolve_target_columns(&schema, Some(&names))
                .unwrap_err()
                .wire_code(),
            "42701"
        );
        let all = resolve_target_columns(&schema, None).unwrap();
        assert_eq!(
            all,
            vec![
                TargetColumn::Id,
                TargetColumn::Column(0),
                TargetColumn::Column(1)
            ]
        );
    }
}

#[cfg(test)]
mod bind_tests {
    use super::*;
    use crate::catalog::{ColumnDef, RowIdentity};
    use crate::sql::allowlist::{InsertSelectShape, Statement};

    fn col(name: &str, ty: ColumnType) -> ColumnDef {
        ColumnDef::new(name, ty, true)
    }

    fn stmt(columns: &[&str]) -> ValidatedInsertSelect {
        // `bind` は内側の `Statement` を読まない（結果列メタは別引数）ため、ダミーの文を使う。
        ValidatedInsertSelect::new(
            InsertSelectShape {
                table_name: "t".to_string(),
                columns: columns.iter().map(|c| c.to_string()).collect(),
                query_tokens: Vec::new(),
                on_conflict: None,
                returning: None,
                operation_id: None,
            },
            Statement::ShowTimeZone,
            crate::recovery::content_hash::for_insert_select(&[]).expect("hash"),
        )
    }

    fn scalar(ty: ColumnType) -> ColumnMeta {
        ColumnMeta::Scalar {
            name: "c".to_string(),
            ty,
        }
    }

    fn schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                col("a", ColumnType::BigInt),
                col("t", ColumnType::Text),
                col("v", ColumnType::Vector(3)),
            ],
        )
        .with_row_identity(RowIdentity::Allocated)
    }

    fn code(r: Result<BoundInsertSelectTarget, SqlSurfaceError>) -> String {
        r.expect_err("must fail").wire_code().to_string()
    }

    #[test]
    fn column_count_mismatch_is_42601() {
        let s = schema();
        let two = [scalar(ColumnType::Integer), scalar(ColumnType::Text)];
        assert_eq!(code(bind(&stmt(&["a"]), &s, &two)), "42601");
        assert_eq!(code(bind(&stmt(&["a", "t"]), &s, &two[..1])), "42601");
    }

    #[test]
    fn assignment_rules_follow_cast_table() {
        let s = schema();
        // INTEGER -> BIGINT は受理
        let ok = bind(&stmt(&["a"]), &s, &[scalar(ColumnType::Integer)]).expect("widening");
        assert_eq!(ok.slots, vec![TargetColumn::Column(0)]);
        assert_eq!(ok.omitted, vec![1, 2]);
        // TEXT -> BIGINT は 42804
        assert_eq!(
            code(bind(&stmt(&["a"]), &s, &[scalar(ColumnType::Text)])),
            "42804"
        );
        // 型を持たない式は 42804（fail-closed）
        let untyped = ColumnMeta::Computed {
            name: "x".to_string(),
            ty: None,
        };
        assert_eq!(code(bind(&stmt(&["a"]), &s, &[untyped])), "42804");
    }

    #[test]
    fn vector_requires_same_dimension() {
        let s = schema();
        bind(&stmt(&["v"]), &s, &[scalar(ColumnType::Vector(3))]).expect("same dim");
        assert_eq!(
            code(bind(&stmt(&["v"]), &s, &[scalar(ColumnType::Vector(4))])),
            "42804"
        );
        assert_eq!(
            code(bind(&stmt(&["v"]), &s, &[scalar(ColumnType::Text)])),
            "42804"
        );
    }

    #[test]
    fn unknown_and_duplicate_columns_are_rejected() {
        let s = schema();
        let one = [scalar(ColumnType::Integer)];
        assert_eq!(code(bind(&stmt(&["zz"]), &s, &one)), "42703");
        let two = [scalar(ColumnType::Integer), scalar(ColumnType::Integer)];
        assert_eq!(code(bind(&stmt(&["a", "a"]), &s, &two)), "22000");
    }

    #[test]
    fn pseudo_id_rules() {
        let legacy = TableSchema::new("t", vec![col("a", ColumnType::BigInt)]);
        // 従来方式は列リストに id が必須
        assert_eq!(
            code(bind(&stmt(&["a"]), &legacy, &[scalar(ColumnType::Integer)])),
            "22000"
        );
        let b = bind(
            &stmt(&["id", "a"]),
            &legacy,
            &[ColumnMeta::Id, scalar(ColumnType::Integer)],
        )
        .expect("id listed");
        assert_eq!(b.slots[0], TargetColumn::Id);
        // id に TEXT は 42804
        assert_eq!(
            code(bind(
                &stmt(&["id", "a"]),
                &legacy,
                &[scalar(ColumnType::Text), scalar(ColumnType::Integer)]
            )),
            "42804"
        );
        // 移行済みは省略を受理する
        let migrated = legacy.clone().with_row_identity(RowIdentity::MigratedId);
        let b = bind(&stmt(&["a"]), &migrated, &[scalar(ColumnType::Integer)]).expect("omit id");
        assert!(b.id_omitted);
    }
}
