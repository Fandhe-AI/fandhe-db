//! 複数テーブル参照スコープの束縛基盤（SQL-28・RLS-10、TASK-212、Issue #924）。
//!
//! 責務境界: 許可リスト（[`crate::sql::allowlist`]）が引き続き単一テーブルの
//! `FROM <ident>` のみを受理する現状は変えない。本モジュールは、後続タスク
//! （Issue #925 以降。JOIN・複数 FROM の許可リスト開放と `EngineCore` への結線）が
//! 載せる「束縛スコープ」だけを土台として提供する。呼び出し元は複数の
//! `(TableRef, &TableSchema)` を集めて [`BindingScope::new`] へ渡し、修飾・非修飾の
//! 列参照を [`BindingScope::resolve`] で解決する。
//!
//! 単一参照スコープでの非修飾列解決は、既存の単一テーブル束縛
//! （`sql::parser` の列解決）と同じ添字を返す（`tests/multi_relation_binding.rs` の
//! 等価性テストで固定）。
//!
//! 本モジュールはカタログ照会を行わない: `relations` に渡された
//! `(TableRef, &TableSchema)` は呼び出し元が解決済みのものとして受け取る
//! （ビュー展開・FROM 解決は Issue #928 以降の管轄）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;

/// 1 クエリが持てるテーブル参照数の上限（本リポ独自の実装既定値。無制限 `Vec`
/// 確保を避ける。`.claude/rules/security.md`「不安全な設計｜無制限リソース確保
/// （DoS）」対応）。`docs/design/multi-relation-plan-foundation.md` に既定値の
/// 選定理由を記録する。
pub const MAX_TABLE_REFS: usize = 8;

/// FROM 句の 1 テーブル参照（テーブル名 + 任意の別名）。別名を付けた場合、
/// PostgreSQL と同様に元のテーブル名では修飾できなくなる（[`Self::exposed_name`]
/// が別名を優先して返すのはこのため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    table: String,
    alias: Option<String>,
}

impl TableRef {
    /// 別名なしの参照を構築する。
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            alias: None,
        }
    }

    /// 別名付きの参照を構築する。
    pub fn with_alias(table: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            alias: Some(alias.into()),
        }
    }

    /// カタログ上の実テーブル名。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 宣言された別名（無ければ `None`）。
    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    /// 修飾列参照（`<qualifier>.<col>`）が照合する公開名。別名があれば別名、
    /// 無ければテーブル名そのもの。
    pub fn exposed_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table)
    }
}

/// SELECT リスト・WHERE 句などが参照する列（修飾子は任意）。字句層の
/// `Token::QualifiedIdent`（`sql::lexer`）が返す `(qualifier, name)` をそのまま
/// 保持できる形にしている。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    qualifier: Option<String>,
    name: String,
}

impl ColumnRef {
    /// 非修飾の列参照（`<col>`）。
    pub fn unqualified(name: impl Into<String>) -> Self {
        Self {
            qualifier: None,
            name: name.into(),
        }
    }

    /// 修飾付きの列参照（`<qualifier>.<col>`）。
    pub fn qualified(qualifier: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            qualifier: Some(qualifier.into()),
            name: name.into(),
        }
    }

    pub fn qualifier(&self) -> Option<&str> {
        self.qualifier.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// 解決済み列参照が指す物理位置。`Id` は行キー由来の疑似列（既存の単一テーブル
/// 束縛 [`crate::sql::parser`] の `ProjectedColumn::Id` と同じ意味論）、`Column` は
/// [`TableSchema::columns`] の添字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnSlot {
    /// 行キー疑似列 `id`。
    Id,
    /// `TableSchema::columns` の添字。
    Column(usize),
}

/// [`BindingScope::resolve`] の解決結果。`relation` はスコープ内の参照位置
/// （`relations` の添字。自己結合で同じテーブルを 2 回参照しても別の値になる）。
#[derive(Debug, Clone)]
pub struct ResolvedColumn {
    relation: usize,
    slot: ColumnSlot,
    column_type: Option<ColumnType>,
}

impl ResolvedColumn {
    /// スコープ内の参照位置（[`BindingScope`] 構築時の `relations` 添字）。
    pub fn relation(&self) -> usize {
        self.relation
    }

    pub fn slot(&self) -> ColumnSlot {
        self.slot
    }

    /// 列型（[`ColumnSlot::Id`] は行キー疑似列のため `None`）。
    pub fn column_type(&self) -> Option<&ColumnType> {
        self.column_type.as_ref()
    }
}

/// 複数テーブル参照の束縛スコープ（SQL-28・RLS-10、Issue #924）。呼び出し元が
/// 解決済みの `(TableRef, &TableSchema)` を渡して構築し、修飾・非修飾の列参照を
/// [`Self::resolve`] で解決する。カタログ照会・RLS 判定は一切行わない
/// （責務は「参照集合内での名前解決」のみ）。
#[derive(Debug)]
pub struct BindingScope<'a> {
    relations: Vec<(TableRef, &'a TableSchema)>,
}

impl<'a> BindingScope<'a> {
    /// 参照集合から束縛スコープを構築する。検証順序（決定的。同じ入力には常に
    /// 同じエラー）:
    /// 1. 参照数を `1..=MAX_TABLE_REFS` で検証する（`Vec` 確保前。超過は `54000`）
    /// 2. 公開名（[`TableRef::exposed_name`]）の重複を検出する（ERR-6 に専用の
    ///    「重複相関名」行が無いため、既存の許可リスト外エラー `42601` へ
    ///    fail-closed に倒す）
    pub fn new(relations: Vec<(TableRef, &'a TableSchema)>) -> Result<Self, SqlSurfaceError> {
        if relations.is_empty() || relations.len() > MAX_TABLE_REFS {
            return Err(SqlSurfaceError::payload_too_large(format!(
                "too many table references: {} (max {MAX_TABLE_REFS})",
                relations.len()
            )));
        }
        for (i, (r, _)) in relations.iter().enumerate() {
            let exposed = r.exposed_name();
            if relations[..i]
                .iter()
                .any(|(other, _)| other.exposed_name() == exposed)
            {
                return Err(SqlSurfaceError::unsupported(format!(
                    "duplicate table reference name: {exposed}"
                )));
            }
        }
        Ok(Self { relations })
    }

    /// スコープが保持する参照数。
    pub fn len(&self) -> usize {
        self.relations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.relations.is_empty()
    }

    /// スコープ内の参照（`(TableRef, &TableSchema)`）を宣言順に返す。
    pub fn relations(&self) -> &[(TableRef, &'a TableSchema)] {
        &self.relations
    }

    /// 列参照を解決する。判定規則:
    /// - 修飾ありの場合: 修飾子が公開名に一致する参照が無ければ
    ///   [`SqlSurfaceError::undefined_table`]（`42P01`。カタログを照会しないため
    ///   存在オラクルにならない）。一致した参照内で列（`id` 疑似列を含む）を
    ///   探し、見つからなければ [`SqlSurfaceError::invalid_input`]（`22000`。
    ///   既存 SELECT 束縛の未知列エラーと同じ分類）。
    /// - 修飾なしの場合: 全参照を左から走査し、ヒットが 2 件以上なら
    ///   [`SqlSurfaceError::ambiguous_column`]（`42702`。`id` は全参照が持つ
    ///   疑似列のため、参照が 2 つ以上あれば非修飾 `id` は常にこの分類になる）。
    ///   0 件なら `22000`。
    ///
    /// 単一参照スコープでの非修飾解決は、既存の単一テーブル束縛の列解決と同じ
    /// 添字を返す（実カラムを疑似列 `id` より優先して照合する。
    /// `sql::parser::bind_projection` と同じ規則。`tests/multi_relation_binding.rs`
    /// の等価性テストで固定）。
    pub fn resolve(&self, column: &ColumnRef) -> Result<ResolvedColumn, SqlSurfaceError> {
        match column.qualifier() {
            Some(qualifier) => {
                let position = self
                    .relations
                    .iter()
                    .position(|(r, _)| r.exposed_name() == qualifier)
                    .ok_or_else(|| SqlSurfaceError::undefined_table(qualifier))?;
                let (_, schema) = &self.relations[position];
                self.resolve_in_relation(position, schema, column.name())
                    .ok_or_else(|| {
                        SqlSurfaceError::invalid_input(format!("unknown column: {}", column.name()))
                    })
            }
            None => {
                let mut found: Option<ResolvedColumn> = None;
                for (position, (_, schema)) in self.relations.iter().enumerate() {
                    if let Some(resolved) =
                        self.resolve_in_relation(position, schema, column.name())
                    {
                        if found.is_some() {
                            return Err(SqlSurfaceError::ambiguous_column(column.name()));
                        }
                        found = Some(resolved);
                    }
                }
                found.ok_or_else(|| {
                    SqlSurfaceError::invalid_input(format!("unknown column: {}", column.name()))
                })
            }
        }
    }

    /// 単一参照内での列解決（実カラム優先、次に `id` 疑似列。`sql::parser` の
    /// 単一テーブル列解決と同じ規則）。
    fn resolve_in_relation(
        &self,
        position: usize,
        schema: &TableSchema,
        name: &str,
    ) -> Option<ResolvedColumn> {
        if let Some(index) = schema.columns.iter().position(|c| c.name == name) {
            return Some(ResolvedColumn {
                relation: position,
                slot: ColumnSlot::Column(index),
                column_type: Some(schema.columns[index].ty.clone()),
            });
        }
        if name == "id" {
            return Some(ResolvedColumn {
                relation: position,
                slot: ColumnSlot::Id,
                column_type: None,
            });
        }
        None
    }
}

/// 明示トランザクション（SQL-31・TASK-221）内で、`relations` のいずれかが
/// 既に書き込み済みかどうかを検査する。1 つでも該当すれば既存の単一テーブル
/// 経路と同じ `0A000`（[`SqlSurfaceError::transaction_feature_not_supported`]）で
/// fail-closed に拒否する。
///
/// 本 Issue の時点では本番の実行経路から呼ばれない（EngineCore への結線は
/// Issue #925 以降）。`pub` で公開し、結合テストから直接検証できるようにする。
pub fn ensure_relations_not_written(
    txn: &crate::sql::transaction::SessionTransaction<'_>,
    relations: &[TableRef],
) -> Result<(), SqlSurfaceError> {
    for r in relations {
        if txn.table_already_written(r.table()) {
            return Err(SqlSurfaceError::transaction_feature_not_supported(
                "statement reads a table already written in this transaction",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;
    use crate::error_format::ClassifiedError;

    fn schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
        TableSchema::new(name, columns)
    }

    #[test]
    fn qualified_resolution_picks_matching_relation() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("y", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)])
            .expect("scope");

        let resolved = scope
            .resolve(&ColumnRef::qualified("b", "y"))
            .expect("resolve");
        assert_eq!(resolved.relation(), 1);
        assert_eq!(resolved.slot(), ColumnSlot::Column(0));
    }

    #[test]
    fn unqualified_resolution_with_single_match_succeeds() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("y", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)])
            .expect("scope");

        let resolved = scope
            .resolve(&ColumnRef::unqualified("x"))
            .expect("resolve");
        assert_eq!(resolved.relation(), 0);
        assert_eq!(resolved.slot(), ColumnSlot::Column(0));
    }

    #[test]
    fn unqualified_id_is_ambiguous_across_two_relations() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("y", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)])
            .expect("scope");

        let err = scope.resolve(&ColumnRef::unqualified("id")).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "42702");
    }

    #[test]
    fn unqualified_column_name_collision_is_ambiguous() {
        let a = schema("a", vec![ColumnDef::new("name", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("name", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)])
            .expect("scope");

        let err = scope.resolve(&ColumnRef::unqualified("name")).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "42702");
    }

    #[test]
    fn unknown_qualifier_is_undefined_table() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a)]).expect("scope");

        let err = scope
            .resolve(&ColumnRef::qualified("nope", "x"))
            .unwrap_err();
        assert_eq!(err.error_class().wire_code(), "42P01");
    }

    #[test]
    fn unknown_column_is_invalid_input() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a)]).expect("scope");

        let err = scope.resolve(&ColumnRef::unqualified("nope")).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "22000");
    }

    #[test]
    fn aliased_relation_rejects_qualification_by_original_table_name() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::with_alias("a", "t"), &a)]).expect("scope");

        // 別名を付けた場合、元のテーブル名では修飾できない（PostgreSQL と同じ
        // 契約。`exposed_name()` が別名を優先する）。
        let err = scope.resolve(&ColumnRef::qualified("a", "x")).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "42P01");
        // 別名では解決できる。
        assert!(scope.resolve(&ColumnRef::qualified("t", "x")).is_ok());
    }

    #[test]
    fn duplicate_exposed_names_are_rejected() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("y", ColumnType::Text, false)]);
        let err = BindingScope::new(vec![
            (TableRef::with_alias("a", "t"), &a),
            (TableRef::with_alias("b", "t"), &b),
        ])
        .unwrap_err();
        assert_eq!(err.error_class().wire_code(), "42601");
    }

    #[test]
    fn too_many_relations_is_rejected_before_allocation() {
        let a = schema("a", vec![ColumnDef::new("x", ColumnType::Text, false)]);
        let relations: Vec<(TableRef, &TableSchema)> = (0..(MAX_TABLE_REFS + 1))
            .map(|i| (TableRef::with_alias("a", format!("t{i}")), &a))
            .collect();
        let err = BindingScope::new(relations).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "54000");
    }

    #[test]
    fn empty_relations_is_rejected() {
        let err = BindingScope::new(Vec::new()).unwrap_err();
        assert_eq!(err.error_class().wire_code(), "54000");
    }

    #[test]
    fn resolution_is_deterministic_across_repeated_calls() {
        let a = schema("a", vec![ColumnDef::new("name", ColumnType::Text, false)]);
        let b = schema("b", vec![ColumnDef::new("name", ColumnType::Text, false)]);
        let scope = BindingScope::new(vec![(TableRef::new("a"), &a), (TableRef::new("b"), &b)])
            .expect("scope");

        let first = scope.resolve(&ColumnRef::unqualified("name"));
        let second = scope.resolve(&ColumnRef::unqualified("name"));
        assert_eq!(
            first.unwrap_err().error_class().wire_code(),
            second.unwrap_err().error_class().wire_code()
        );
    }

    #[test]
    fn single_relation_unqualified_resolution_matches_existing_single_table_binding() {
        // 実カラムを疑似列 `id` より優先して照合する規則（`sql::parser` の単一
        // テーブル束縛と同じ）。単一参照スコープでも同じ添字を返すことを固定する。
        let schema_with_id_column = schema(
            "a",
            vec![
                ColumnDef::new("id", ColumnType::Text, false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        );
        let scope =
            BindingScope::new(vec![(TableRef::new("a"), &schema_with_id_column)]).expect("scope");
        let resolved = scope
            .resolve(&ColumnRef::unqualified("id"))
            .expect("resolve");
        // 実カラム "id"（添字 0）を疑似列より優先する。
        assert_eq!(resolved.slot(), ColumnSlot::Column(0));

        let resolved_path = scope
            .resolve(&ColumnRef::unqualified("path"))
            .expect("resolve");
        assert_eq!(resolved_path.slot(), ColumnSlot::Column(1));
    }

    #[test]
    fn ensure_relations_not_written_passes_when_idle() {
        use crate::sql::transaction::{SessionTransaction, TransactionLimits};

        // `Idle`（トランザクション外）では `table_already_written` が常に
        // `false` を返す契約（`SessionTransaction::table_already_written` の
        // ドキュメント参照）。本 Issue では実行経路から呼ばれないため、ここでは
        // 「書き込み済みテーブルが無ければ許可する」契約だけを固定する。
        let txn = SessionTransaction::new(TransactionLimits::default());
        assert!(ensure_relations_not_written(&txn, &[TableRef::new("a")]).is_ok());
    }
}
