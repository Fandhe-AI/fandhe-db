//! ビューの `WITH [CASCADED | LOCAL] CHECK OPTION`（TABLE-43 (2)・Issue #1673）の述語の束縛と評価。
//!
//! 責務境界: `sql::view::resolve_from` が入れ子の規則で選んだ検査対象の述語（各段の `WHERE`。
//! 基表の物理列名で書かれたもの）を、書き込み txn の中で基表スキーマに束縛し、書き込んだ行ごとに
//! 評価する。呼び出し元は 2 つ。
//!
//! - `constraint::enforce_row_constraints_in_txn`（DML の唯一の検査点）が [`CompiledViewChecks`] を
//!   1 文につき 1 回コンパイルし、`written_ids` の各行で [`CompiledViewChecks::enforce`] を呼ぶ。
//!   述語が TRUE でない行（FALSE・NULL）は `44000` で文全体を拒否する（CHECK 制約と違い NULL も違反）。
//! - `core` の `CREATE VIEW` 実行段が [`check_declarable`] で、宣言時点で検査できない形の述語を
//!   `0A000` にする（fail-closed）。
//!
//! 束縛は `WHERE` と同じ経路（`sql::parser::bind_where_predicates`）で、評価も部分索引の述語と同じ
//! `BoundOrGroup::matches` を使う（第 2 の評価器は作らない）。可視性（`visible()`）・部分クエリを含む
//! 述語は検査に可視性判定や再帰実行を混ぜないため `0A000` で拒否する（PostgreSQL との差）。
//! 評価エラー（0 除算等）は `44000` へ丸めず SQLSTATE を保ったまま透過する。

use crate::catalog::TableSchema;
use crate::sql::allowlist::{SqlSurfaceError, TableLookup, WherePredicate};
use crate::sql::index_predicate::{map_predicate_eval_error, PredicateEvalError};
use crate::sql::where_tree::{BoundConjunction, BoundOrGroup};
use crate::tenant::TenantWriteError;

/// 検査できない形の述語を拒否する固定文言（述語の内容を含めない）。
const MSG_UNSUPPORTED_PREDICATE: &str =
    "WITH CHECK OPTION is not supported for this view predicate";

fn unsupported_predicate() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(MSG_UNSUPPORTED_PREDICATE)
}

/// 1 段の述語を束縛した評価器。
struct CompiledLayer {
    group: BoundOrGroup,
    /// 評価に必要な列のデコードマスク（`row_codec::scan_scalar_columns_masked` へ渡す）。
    column_mask: Vec<bool>,
    references_embedding: bool,
}

impl CompiledLayer {
    /// 部分クエリ・RLS 述語呼び出しを含む形は `0A000`。それ以外の束縛エラー（未知列等）は透過する。
    fn bind(preds: &[WherePredicate], schema: &TableSchema) -> Result<Self, SqlSurfaceError> {
        // 部分クエリの検出は列写像の共有走査器へ恒等写像を渡し、部分クエリ葉で呼ばれる
        // `on_subquery` のエラーで行う（第 2 の走査器を作らない）。
        for p in preds {
            crate::sql::check_constraint::map_predicate_columns(
                p,
                &|c: &str| c.to_string(),
                &unsupported_predicate,
            )?;
        }
        let udfs = crate::sql::udf_call::UdfRegistry::default();
        let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
        let (metadata_filters, expr_filters, rls_present, or_filters) =
            crate::sql::parser::bind_where_predicates(preds, schema, &udfs, &mut budget, &[])?;
        if rls_present {
            return Err(unsupported_predicate());
        }
        let group = BoundOrGroup::new(vec![BoundConjunction::new(
            metadata_filters,
            expr_filters,
            or_filters,
        )]);
        let mut column_mask = vec![false; schema.columns.len()];
        group.visit_column_indices(&mut |i| {
            if let Some(slot) = column_mask.get_mut(i) {
                *slot = true;
            }
        });
        let references_embedding = group.references_embedding();
        Ok(CompiledLayer {
            group,
            column_mask,
            references_embedding,
        })
    }

    /// 1 行で述語を評価する。TRUE のときだけ `true`（FALSE・NULL は `false`）。
    fn eval(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
        scratch: &mut Vec<crate::sql::expr_program::StackValue>,
    ) -> Result<bool, PredicateEvalError> {
        let scanned =
            crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&self.column_mask))
                .map_err(|e| PredicateEvalError::Decode(e.to_string()))?;
        let emb: &[f32] = if self.references_embedding {
            embedding
        } else {
            &[]
        };
        self.group
            .matches(&scanned, id, emb, emb.len(), scratch)
            .map_err(PredicateEvalError::Eval)
    }
}

/// 検査対象の全段をコンパイルした DML ゲート用の評価器。
pub(crate) struct CompiledViewChecks {
    layers: Vec<CompiledLayer>,
}

impl CompiledViewChecks {
    /// `layers`（内側のビューが先頭）を `schema` に束縛する。検査対象が無ければ `None`（ゲート不要）。
    pub(crate) fn compile(
        schema: &TableSchema,
        layers: &[Vec<WherePredicate>],
    ) -> Result<Option<Self>, SqlSurfaceError> {
        if layers.is_empty() {
            return Ok(None);
        }
        let mut compiled = Vec::with_capacity(layers.len());
        for preds in layers {
            compiled.push(CompiledLayer::bind(preds, schema)?);
        }
        Ok(Some(Self { layers: compiled }))
    }

    /// 1 行（`id`・`embedding`・`metadata`）が全段の述語を満たすか検査する。満たさなければ
    /// [`TenantWriteError::WithCheckOptionViolation`]（固定文言。どの段かは出さない）。評価エラーは
    /// SQLSTATE を保って透過し、行のデコード失敗は `XX000`。
    pub(crate) fn enforce(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), TenantWriteError> {
        let mut scratch = Vec::new();
        for layer in &self.layers {
            let ok = layer
                .eval(schema, id, embedding, metadata, &mut scratch)
                .map_err(map_predicate_eval_error)?;
            if !ok {
                return Err(TenantWriteError::WithCheckOptionViolation);
            }
        }
        Ok(())
    }
}

/// 宣言時の試し束縛（`CREATE VIEW ... WITH CHECK OPTION`）。検査できない形なら `0A000`。
pub(crate) fn check_declarable(
    schema: &TableSchema,
    layers: &[Vec<WherePredicate>],
) -> Result<(), SqlSurfaceError> {
    CompiledViewChecks::compile(schema, layers).map(|_| ())
}

/// 宣言先のビュー `name`（まだカタログに無い、または `OR REPLACE` で置換前）を新定義 `def` で
/// 上書きして見せる [`TableLookup`] ラッパー。新定義を連鎖の解決（`resolve_from`）と更新可能性の
/// 判定（`view_updatability`）に通すために使い、第 2 の判定器・解決器を作らない。
struct NewViewLookup<'a> {
    inner: &'a crate::storage::Storage,
    name: &'a str,
    def: crate::catalog::ViewDef,
}

impl TableLookup for NewViewLookup<'_> {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        TableLookup::table_exists(self.inner, name)
    }

    fn view_definition(
        &self,
        name: &str,
    ) -> Result<Option<crate::catalog::ViewDef>, SqlSurfaceError> {
        if name == self.name {
            return Ok(Some(self.def.clone()));
        }
        TableLookup::view_definition(self.inner, name)
    }

    fn table_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        TableLookup::table_columns(self.inner, name)
    }

    fn table_star_columns(&self, name: &str) -> Result<Option<Vec<String>>, SqlSurfaceError> {
        TableLookup::table_star_columns(self.inner, name)
    }

    fn table_is_allocated(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        TableLookup::table_is_allocated(self.inner, name)
    }

    fn materialized_view_columns(
        &self,
        name: &str,
    ) -> Result<Option<Vec<crate::catalog::ColumnDef>>, SqlSurfaceError> {
        TableLookup::materialized_view_columns(self.inner, name)
    }
}

/// `CREATE [OR REPLACE] VIEW <name> ... WITH CHECK OPTION` の宣言時の検査（TABLE-43 (2)・
/// Issue #1673）。`core` の `CreateView` 実行段が DDL 権限ゲートの後・カタログ書き込みの前に呼ぶ。
///
/// 1. 宣言先が自動更新できる形か（基底が評価後射影形ビュー・matview なら `0A000`。判定は
///    `sql::view_updatable::view_updatability` を新定義込みで通す）
/// 2. 入れ子の規則で検査対象になる述語（自身の段と、`CASCADED` のとき下位の段）が検査できる形か
///    （基表スキーマへの試し束縛。部分クエリ・`visible()` は `0A000`）
///
/// 宣言なしは何もしない。基底が存在しない・自己参照の場合は作成段（カタログ）の `42P01`・`42P17`
/// に任せる。解決エラー（`XX000`・`54000`）はそのまま返す（fail-closed）。宣言の後で内側のビューが
/// 更新できない形に置換された場合は、書き込み時に既存の `55000` で拒否される（置換時の依存検査は
/// 追加しない）。
pub(crate) fn check_declaration(
    storage: &crate::storage::Storage,
    name: &str,
    def: crate::catalog::ViewDef,
) -> Result<(), SqlSurfaceError> {
    use crate::catalog::ViewCheckOption;
    use crate::sql::view::{resolve_from, Resolved};
    use crate::sql::view_updatable::{view_updatability, ViewUpdatability};
    if def.check_option == ViewCheckOption::None || def.base_relation == name {
        return Ok(());
    }
    let base = def.base_relation.clone();
    if TableLookup::view_definition(storage, &base)?.is_none()
        && !TableLookup::table_exists(storage, &base)?
    {
        if TableLookup::materialized_view_columns(storage, &base)?.is_some() {
            return Err(crate::sql::allowlist::not_auto_updatable_check_option());
        }
        // 基底が存在しない: 作成段が `42P01` を返す。
        return Ok(());
    }
    let lookup = NewViewLookup {
        inner: storage,
        name,
        def,
    };
    match view_updatability(&lookup, name)? {
        ViewUpdatability::AutoUpdatable => {}
        ViewUpdatability::NotAutoUpdatable(_) => {
            return Err(crate::sql::allowlist::not_auto_updatable_check_option());
        }
    }
    let Resolved::View {
        base_table,
        check_predicates,
        ..
    } = resolve_from(&lookup, name)?
    else {
        return Err(SqlSurfaceError::Internal {
            detail: "invalid view definition".to_string(),
        });
    };
    let schema = storage.get_table_schema(&base_table).map_err(|e| match e {
        crate::catalog::CatalogError::TableNotFound(missing) => {
            SqlSurfaceError::UndefinedTable { name: missing }
        }
        other => crate::catalog::table_lookup_error(other),
    })?;
    check_declarable(&schema, &check_predicates)
}
