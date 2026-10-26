//! `JOIN ... USING (...)`／`NATURAL JOIN` の束縛と結合列の統合規則（Issue #1826・#1827・SQL-60 (1)）。
//!
//! 責務境界: `sql::join::plan` の段ループから呼ばれ、(a) USING 列リストの検証
//! （重複・存在・型）と等価結合キーへの脱糖、(b) 統合列（結合列を 1 列にまとめた出力列）
//! を考慮した列解決（[`JoinScope`]）を担う。行は一切読まない。PostgreSQL 17 の規則:
//! - 統合列の値は INNER／LEFT が左側、RIGHT が右側、FULL が `COALESCE(左, 右)`。
//! - `SELECT *` の列順は「統合列（USING 順）→ 左の残り → 右の残り」。
//! - 非修飾の列名は統合列を指し（統合元の列は候補から外れる）、修飾付き `a.x`・`b.x` は
//!   引き続き元の列を指す。
//!
//! FULL の統合列（複数ソース）は「投影」と「非集計形の `ORDER BY`」だけが値を組み立てられる
//! （`OutputColumn`／`PlainOrder` の代替ソース）。WHERE・GROUP BY・集計・後続段の結合キーからの
//! 参照は `0A000` で fail-closed に拒否する（`docs/design/join-using.md`）。USING を 1 度も
//! 使わない JOIN では [`JoinScope`] は [`BindingScope`] への完全委譲で、挙動は変わらない。
//! NATURAL JOIN は [`natural_using_names`] が両入力の共通列名を導出し、USING と同じ
//! [`bind_using_step`] へ渡す（共通列 0 個は統合なしの直積）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{JoinKind, SqlSurfaceError, MAX_JOIN_CONDITIONS};
use crate::sql::relation::{BindingScope, ColumnRef, ColumnSlot, ResolvedColumn};

use super::values::{key_class, JoinKeyClass};

/// 結合結果の可視列（`SELECT *` の並びであり、非修飾名の候補集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VisibleCol {
    /// relation `rel` の元の列。
    Base { rel: usize, slot: ColumnSlot },
    /// [`JoinScope::merged`] の添字が指す統合列。
    Merged(usize),
}

/// USING で統合された列。`sources` は COALESCE の評価順で、INNER／LEFT は左ソース 1 個、
/// RIGHT は右ソース 1 個、FULL は「左ソース ++ 右ソース」。
#[derive(Debug, Clone)]
pub(super) struct MergedColumn {
    pub(super) name: String,
    pub(super) sources: Vec<(usize, ColumnSlot)>,
    /// 列型（疑似列 `id` は `None`）。全ソースで完全一致する（束縛時に検証済み）。
    pub(super) ty: Option<ColumnType>,
}

/// 非修飾名の照合結果 1 件。
enum Hit {
    Base(ResolvedColumn),
    Merged(usize),
}

/// 統合列を考慮した列解決スコープ。`has_using == false` のとき [`BindingScope::resolve`] へ
/// 完全に委譲する（USING を使わない JOIN の挙動を 1 ビットも変えない）。
pub(super) struct JoinScope<'a> {
    base: BindingScope<'a>,
    merged: Vec<MergedColumn>,
    visible: Vec<VisibleCol>,
    has_using: bool,
}

/// FULL 統合列を値解決できない文脈で参照したときの固定文言（列名・行データを含めない）。
fn full_using_unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "reference to a FULL JOIN USING column is not supported here",
    )
}

/// スロットが指す列型（疑似列 `id` は `None`）。
pub(super) fn slot_type(schema: &TableSchema, slot: ColumnSlot) -> Option<ColumnType> {
    match slot {
        ColumnSlot::Id => None,
        ColumnSlot::Column(i) => schema.columns.get(i).map(|c| c.ty.clone()),
    }
}

/// スロットが指す列名（疑似列 `id` は `"id"`）。
fn slot_name(schema: &TableSchema, slot: ColumnSlot) -> Result<&str, SqlSurfaceError> {
    match slot {
        ColumnSlot::Id => Ok("id"),
        ColumnSlot::Column(i) => schema
            .columns
            .get(i)
            .map(|c| c.name.as_str())
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN column index out of range".to_string(),
            }),
    }
}

/// relation 1 個の可視列（`SELECT *` の展開規則と同じ。疑似列 `id` が先頭、続いて宣言列）。
pub(super) fn relation_columns(rel: usize, schema: &TableSchema) -> Vec<VisibleCol> {
    let mut out = Vec::with_capacity(schema.columns.len().saturating_add(1));
    if schema.has_pseudo_id() {
        out.push(VisibleCol::Base {
            rel,
            slot: ColumnSlot::Id,
        });
    }
    for i in 0..schema.columns.len() {
        out.push(VisibleCol::Base {
            rel,
            slot: ColumnSlot::Column(i),
        });
    }
    out
}

impl<'a> JoinScope<'a> {
    /// USING を含まないスコープ（全 relation の全列が可視）。
    pub(super) fn plain(base: BindingScope<'a>) -> Self {
        let mut visible = Vec::new();
        for (rel, (_, schema)) in base.relations().iter().enumerate() {
            visible.extend(relation_columns(rel, schema));
        }
        Self {
            base,
            merged: Vec::new(),
            visible,
            has_using: false,
        }
    }

    pub(super) fn new(
        base: BindingScope<'a>,
        merged: Vec<MergedColumn>,
        visible: Vec<VisibleCol>,
        has_using: bool,
    ) -> Self {
        Self {
            base,
            merged,
            visible,
            has_using,
        }
    }

    /// 可視列 1 件の列名（元の列は所属 relation のスキーマ、統合列は統合名）。疑似列 `id` は `"id"`。
    fn visible_name(&self, v: &VisibleCol) -> Result<&str, SqlSurfaceError> {
        match v {
            VisibleCol::Merged(i) => Ok(self.merged(*i)?.name.as_str()),
            VisibleCol::Base { rel, slot } => {
                let (_, schema) =
                    self.base
                        .relations()
                        .get(*rel)
                        .ok_or_else(|| SqlSurfaceError::Internal {
                            detail: "JOIN relation index out of range".to_string(),
                        })?;
                slot_name(schema, *slot)
            }
        }
    }

    pub(super) fn has_using(&self) -> bool {
        self.has_using
    }

    pub(super) fn visible(&self) -> &[VisibleCol] {
        &self.visible
    }

    pub(super) fn merged(&self, idx: usize) -> Result<&MergedColumn, SqlSurfaceError> {
        self.merged
            .get(idx)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN merged column index out of range".to_string(),
            })
    }

    /// 非修飾名に一致する可視列（relation ごとに高々 1 件の元の列＋統合列）。元の列は
    /// 統合で消費されていれば候補にならない。
    fn lookup(&self, name: &str) -> Vec<Hit> {
        let mut hits = Vec::new();
        for (rel, (table_ref, _)) in self.base.relations().iter().enumerate() {
            let Ok(r) = self
                .base
                .resolve(&ColumnRef::qualified(table_ref.exposed_name(), name))
            else {
                continue;
            };
            let slot = r.slot();
            if self
                .visible
                .iter()
                .any(|v| matches!(v, VisibleCol::Base { rel: vr, slot: vs } if *vr == rel && *vs == slot))
            {
                hits.push(Hit::Base(r));
            }
        }
        for v in &self.visible {
            if let VisibleCol::Merged(i) = v {
                if self.merged.get(*i).is_some_and(|m| m.name == name) {
                    hits.push(Hit::Merged(*i));
                }
            }
        }
        hits
    }

    /// 列参照を解決する。修飾付きは元の列、非修飾は可視列（統合列を含む）。統合されて
    /// いない同名列が複数見えるときは `42702`。FULL の統合列は `0A000`。
    pub(super) fn resolve(&self, column: &ColumnRef) -> Result<ResolvedColumn, SqlSurfaceError> {
        if !self.has_using || column.qualifier().is_some() {
            return self.base.resolve(column);
        }
        let name = column.name();
        let hits = self.lookup(name);
        match hits.as_slice() {
            [] => Err(SqlSurfaceError::undefined_column(name)),
            [Hit::Base(r)] => Ok(r.clone()),
            [Hit::Merged(i)] => {
                let m = self.merged(*i)?;
                match m.sources.as_slice() {
                    [(rel, slot)] => Ok(ResolvedColumn::new(*rel, *slot, m.ty.clone())),
                    _ => Err(full_using_unsupported()),
                }
            }
            _ => Err(SqlSurfaceError::ambiguous_column(name)),
        }
    }

    /// 非修飾名が FULL の統合列（複数ソース）を一意に指すとき、その統合列の添字を返す
    /// （投影・非集計 `ORDER BY` が [`Self::resolve`] より先に呼ぶ）。
    pub(super) fn resolve_coalesce(
        &self,
        column: &ColumnRef,
    ) -> Result<Option<usize>, SqlSurfaceError> {
        if !self.has_using || column.qualifier().is_some() {
            return Ok(None);
        }
        let name = column.name();
        let hits = self.lookup(name);
        match hits.as_slice() {
            [Hit::Merged(i)] => {
                let m = self.merged(*i)?;
                Ok((m.sources.len() > 1).then_some(*i))
            }
            [_, _, ..] => Err(SqlSurfaceError::ambiguous_column(name)),
            _ => Ok(None),
        }
    }
}

/// USING 1 列分の等価結合キー（脱糖結果）。
pub(super) struct UsingKey {
    /// これまでの結合結果側の列（relation・スロット・型）。
    pub(super) acc: (usize, ColumnSlot, Option<ColumnType>),
    /// 新しく加わる relation 側の列。
    pub(super) new: (usize, ColumnSlot, Option<ColumnType>),
    pub(super) class: JoinKeyClass,
}

/// USING 1 段分の束縛結果。
pub(super) struct UsingBinding {
    pub(super) keys: Vec<UsingKey>,
    /// 段の適用後の統合列一覧（既存の統合列を含む）。
    pub(super) merged: Vec<MergedColumn>,
    /// 段の適用後の可視列（統合列 → 左の残り → 右の残り）。
    pub(super) visible: Vec<VisibleCol>,
}

/// USING 1 段（`names`。構文段が列数上限を検査済み）を束縛する。`left` は relation `0..new_rel`
/// だけを持つ左側スコープ、`right_base` は新しい relation `new_rel` を含む基底スコープ。
/// 列ごとに USING リスト順で判定する（PostgreSQL と同じ順序）: 重複 `42701` → 左に無い
/// `42703`／複数ある `42702` → 右に無い `42703` → 型（`key_class` 不一致は ON と同じ `42804`、
/// 同クラスで型が完全一致しない組は `0A000`）。NATURAL JOIN（#1827）は
/// [`natural_using_names`] が導出した列名列をそのまま渡す。
pub(super) fn bind_using_step(
    left: &JoinScope<'_>,
    right_base: &BindingScope<'_>,
    kind: JoinKind,
    names: &[String],
    new_rel: usize,
) -> Result<UsingBinding, SqlSurfaceError> {
    let (new_table_ref, new_schema) =
        right_base
            .relations()
            .get(new_rel)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "JOIN relation index out of range".to_string(),
            })?;
    let new_exposed = new_table_ref.exposed_name();

    let mut merged = left.merged.clone();
    let mut new_merged_idx: Vec<usize> = Vec::with_capacity(names.len());
    let mut consumed_left: Vec<VisibleCol> = Vec::with_capacity(names.len());
    let mut consumed_right: Vec<ColumnSlot> = Vec::with_capacity(names.len());
    let mut keys: Vec<UsingKey> = Vec::with_capacity(names.len());

    for (i, name) in names.iter().enumerate() {
        if names.iter().take(i).any(|n| n == name) {
            return Err(SqlSurfaceError::duplicate_column(name.as_str()));
        }
        let hits = left.lookup(name);
        let (acc, consumed) = match hits.as_slice() {
            [] => return Err(SqlSurfaceError::undefined_column(name.as_str())),
            [Hit::Base(r)] => (
                (r.relation(), r.slot(), r.column_type().cloned()),
                VisibleCol::Base {
                    rel: r.relation(),
                    slot: r.slot(),
                },
            ),
            [Hit::Merged(idx)] => {
                let m = left.merged(*idx)?;
                match m.sources.as_slice() {
                    [(rel, slot)] => ((*rel, *slot, m.ty.clone()), VisibleCol::Merged(*idx)),
                    _ => return Err(full_using_unsupported()),
                }
            }
            _ => return Err(SqlSurfaceError::ambiguous_column(name.as_str())),
        };
        let right = right_base.resolve(&ColumnRef::qualified(new_exposed, name.as_str()))?;
        let new = (new_rel, right.slot(), right.column_type().cloned());

        let acc_class = key_class(acc.2.as_ref())?;
        let new_class = key_class(new.2.as_ref())?;
        if acc_class != new_class {
            return Err(SqlSurfaceError::datatype_mismatch(
                "JOIN key type mismatch between the two sides",
            ));
        }
        // 同じ型クラスでも列型が完全一致しない組（INTEGER と BIGINT 等）は、統合列の型・
        // セル表現を一意に定められないため拒否する（共通型への寄せは後続 Issue）。
        if acc.2 != new.2 {
            return Err(SqlSurfaceError::feature_not_supported(
                "JOIN USING columns must have identical types",
            ));
        }

        let sources = match kind {
            JoinKind::Inner | JoinKind::Left => vec![(acc.0, acc.1)],
            JoinKind::Right => vec![(new.0, new.1)],
            JoinKind::Full => vec![(acc.0, acc.1), (new.0, new.1)],
        };
        merged.push(MergedColumn {
            name: name.clone(),
            sources,
            ty: acc.2.clone(),
        });
        new_merged_idx.push(merged.len().saturating_sub(1));
        consumed_left.push(consumed);
        consumed_right.push(new.1);
        keys.push(UsingKey {
            acc,
            new,
            class: acc_class,
        });
    }

    let mut visible: Vec<VisibleCol> = new_merged_idx.into_iter().map(VisibleCol::Merged).collect();
    visible.extend(
        left.visible
            .iter()
            .filter(|v| !consumed_left.contains(v))
            .copied(),
    );
    visible.extend(
        relation_columns(new_rel, new_schema)
            .into_iter()
            .filter(|v| match v {
                VisibleCol::Base { slot, .. } => !consumed_right.contains(slot),
                VisibleCol::Merged(_) => true,
            }),
    );
    Ok(UsingBinding {
        keys,
        merged,
        visible,
    })
}

/// NATURAL JOIN の共通列名を導出する（Issue #1827・SQL-60 (1)）。左側の可視列を並び順に
/// 走査し、新しい relation `new_rel`（スキーマ `new_schema`）に同名列がある名前を採る
/// （PostgreSQL と同じく左列順。連鎖時は統合列が先頭）。名前は完全一致で比較する（USING・
/// 他の列解決と同じ規則）。左に同名の未統合列が複数あっても導出リストへは 1 回だけ入れ、
/// 曖昧性の検出（`42702`）は [`bind_using_step`] の左照合に委ねる。共通列が 0 個なら空リスト
/// （条件なしの直積）。導出数は USING と同じ [`MAX_JOIN_CONDITIONS`] を超えると `54000`。
/// スキーマと文だけで決まり、行データには依存しない（他テナント行の有無で結果が変わらない）。
pub(super) fn natural_using_names(
    left: &JoinScope<'_>,
    new_rel: usize,
    new_schema: &TableSchema,
) -> Result<Vec<String>, SqlSurfaceError> {
    let mut right_names: Vec<&str> = Vec::new();
    for v in relation_columns(new_rel, new_schema) {
        if let VisibleCol::Base { slot, .. } = v {
            right_names.push(slot_name(new_schema, slot)?);
        }
    }
    let mut names: Vec<String> = Vec::new();
    for v in left.visible() {
        let name = left.visible_name(v)?;
        if right_names.contains(&name) && !names.iter().any(|n| n == name) {
            if names.len() >= MAX_JOIN_CONDITIONS {
                return Err(SqlSurfaceError::payload_too_large(
                    "too many NATURAL JOIN common columns",
                ));
            }
            names.push(name.to_string());
        }
    }
    Ok(names)
}
