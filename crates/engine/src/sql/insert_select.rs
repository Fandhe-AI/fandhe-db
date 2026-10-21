//! `INSERT ... SELECT` の束縛（挿入先の列の解決・列数・型の整合。SQL-51・Issue #1732）。
//!
//! 構文段（`sql::allowlist` の `parse_insert_any_shape_tokens_with_limit`）と内側 SELECT の検証
//! （既存の SELECT 検証器。`core.rs::parse_tokens_in`）を通過した [`ValidatedInsertSelect`] を、挿入先の
//! [`TableSchema`] と内側 SELECT の結果列メタ（[`ColumnMeta`]。`core.rs` が既存の Describe で導出する。
//! 列の導出を第 2 の実装にしない）に突き合わせ、列数と型の不整合を挿入より前に拒否する。
//!
//! 本モジュールはストレージ・RLS・台帳のいずれにも触れない純関数だけを持つ。呼び出し元は
//! `core.rs` の Describe 腕と実行腕（現状は束縛の後に `0A000` を返すスタブ。行の実体化・RLS・挿入・
//! 台帳の内容照合は親 #1731 の後続 Issue が担い、その際に本束縛の結果〔[`BoundInsertSelectTarget`]〕を
//! 再利用する）。エラー文言は英語の固定文言とスキーマ由来の列名だけで、値・他テナントの情報を含めない
//! （fail-closed。存在オラクルを作らない）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, ValidatedInsertSelect};
use crate::sql::exec::{ColumnMeta, ALIASED_ID_TYPE};
use crate::sql::parser::ProjectedColumn;

/// 内側 SELECT の出力列 1 個が書き込まれる先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InsertSelectSlot {
    /// 疑似列 `id`（従来方式・移行済みの表のみ）。
    PseudoId,
    /// `schema.columns` の添字。
    Column(usize),
}

/// [`bind`] の結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundInsertSelectTarget {
    /// 内側の出力列 i の書き込み先（列リストの順）。
    pub(crate) slots: Vec<InsertSelectSlot>,
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
            slots.push(InsertSelectSlot::PseudoId);
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
        slots.push(InsertSelectSlot::Column(idx));
    }

    // 疑似列 `id`: 従来方式の表は列リストに必須。移行済みの表は省略を受理する（採番は書き込み段）。
    let id_listed = slots.contains(&InsertSelectSlot::PseudoId);
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
            (InsertSelectSlot::PseudoId, SourceTy::IdLike) => true,
            (InsertSelectSlot::PseudoId, SourceTy::Ty(ty)) => is_integer_type(ty),
            (InsertSelectSlot::Column(idx), from) => {
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
                returning: None,
                operation_id: None,
            },
            Statement::ShowTimeZone,
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
        assert_eq!(ok.slots, vec![InsertSelectSlot::Column(0)]);
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
        assert_eq!(b.slots[0], InsertSelectSlot::PseudoId);
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
