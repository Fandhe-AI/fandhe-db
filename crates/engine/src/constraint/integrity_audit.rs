//! 整合検査（`storage::integrity`。RECOVER-13 のポインタ）向けの、制約・索引の読み取り専用監査。
//!
//! # 役割と呼び出し文脈
//!
//! 親モジュール [`super`]（`constraint.rs`）の非公開の検査部品（キー計算・FK の参照元/参照先の
//! 解決・永続一意索引の物理形式）を、DB ファイル全体を走査する整合検査の側へ**読み取り専用**で
//! 差し出す。呼び出し元は `storage::integrity` の走査ドライバだけで、テナント 1 つ・表 1 つ・
//! 制約 1 つ単位で呼ぶ。戻り値は件数だけで、キー値・行 id・テナント名を含めない。
//!
//! # 資源上限
//!
//! 重複・参照先キーの集合は呼び出し元が渡すバイト予算を超えると [`PassResult::OverBudget`] を
//! 返す。呼び出し元はキーのハッシュで空間を `parts` 個に分けて `part` ごとに走査し直す
//! （分割して再走査するだけで結果は変わらない）。

use std::collections::{BTreeSet, HashSet};
use std::hash::{Hash, Hasher};

use redb::ReadableTable;

use super::{
    decode_key_columns, foreign_key_specs_for, internal, key_bytes, parent_row_key_by_id_value,
    push_required_key, unique_index, FkKeyRecoder, KeySpec, NullPolicy, RequiredParentKeys,
    UniqueKeyPlan,
};
use crate::catalog::{ForeignKeyDef, TableSchema};
use crate::storage::rowid::{row_key, split_row_key, RowId};
use crate::tenant::TenantWriteError;

/// 集合の 1 エントリあたりの管理コストの見積り（バイト。キーの `Vec` ヘッダ・ハッシュ表のスロットと
/// 拡張時の余裕を見込んだ概算で、予算計上用の保守的な定数）。
const ENTRY_OVERHEAD: usize = 96;

/// 予算付きの 1 回の走査の結果。
pub(crate) enum PassResult<T> {
    /// 完了した。
    Done(T),
    /// 集合のバイト予算を超えた（呼び出し元が分割数を増やして再走査する）。
    OverBudget,
}

/// キーのハッシュが属する分割の番号（`parts >= 1`）。
fn part_of(key: &[u8], parts: u32) -> u32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    u32::try_from(h.finish() % u64::from(parts.max(1))).unwrap_or(0)
}

/// 表 1 つ分の一意キー計画（主キー・UNIQUE 制約・式/部分 UNIQUE 索引）。
pub(crate) struct UniqueAuditPlan {
    plan: UniqueKeyPlan,
    compiled: Option<crate::sql::expression_key_index::CompiledIndexExpressions>,
    has_primary_key: bool,
}

impl UniqueAuditPlan {
    /// 現在のカタログ宣言から計画を組み立てる。宣言を解決できなければ `Err`（呼び出し元は未検査として
    /// 報告する）。
    pub(crate) fn build(
        read_txn: &redb::ReadTransaction,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Self, TenantWriteError> {
        let decls = crate::catalog::table_expression_decls_in_read_txn(read_txn, table_name)?;
        let plan = UniqueKeyPlan::build(schema, &decls)?;
        let compiled = crate::sql::expression_key_index::CompiledIndexExpressions::compile(
            schema,
            &decls.keys,
        )?;
        Ok(Self {
            plan,
            compiled,
            has_primary_key: schema.primary_key().is_some(),
        })
    }

    /// 一意キーが 1 つも無い表か。
    pub(crate) fn is_empty(&self) -> bool {
        self.plan.is_empty()
    }
}

/// [`duplicate_keys_in_tenant`] の件数。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct DuplicateCounts {
    /// 主キーの重複（2 件目以降の行数）。
    pub(crate) primary_key: u64,
    /// UNIQUE 制約・一意索引の重複（2 件目以降の行数）。
    pub(crate) unique: u64,
    /// キーを計算できなかった行数（分割番号 0 の走査でだけ数える）。
    pub(crate) unevaluable: u64,
}

/// テナント 1 つ分の行を走査し、主キー・一意キーの重複を数える。索引は参照せず行だけから判定する
/// （索引が壊れていても行の一意性を独立に確かめるため）。`part`/`parts` はキーのハッシュによる分割、
/// `budget_bytes` は重複検出用の集合に使ってよいバイト数。
pub(crate) fn duplicate_keys_in_tenant<R>(
    plan: &UniqueAuditPlan,
    schema: &TableSchema,
    row_table: &R,
    tenant_id: &str,
    part: u32,
    parts: u32,
    budget_bytes: usize,
) -> Result<PassResult<DuplicateCounts>, TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let mut counts = DuplicateCounts::default();
    let mut seen: HashSet<(u16, Vec<u8>)> = HashSet::new();
    let mut used = 0usize;
    let start = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(0)));
    let end = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(u64::MAX)));
    for entry in row_table
        .range::<(&str, u64)>((start, end))
        .map_err(crate::catalog::CatalogError::from)?
    {
        let (k, v) = entry.map_err(crate::catalog::CatalogError::from)?;
        let (key_tenant, id) = split_row_key(k.value());
        if key_tenant != tenant_id {
            break;
        }
        let Ok(keys) =
            plan.plan
                .row_keys(schema, plan.compiled.as_ref(), id.legacy_id(), v.value())
        else {
            if part == 0 {
                counts.unevaluable = counts.unevaluable.saturating_add(1);
            }
            continue;
        };
        for (ordinal, key) in keys.into_iter().enumerate() {
            let Some(key) = key else { continue };
            let Ok(ordinal_u16) = u16::try_from(ordinal) else {
                if part == 0 {
                    counts.unevaluable = counts.unevaluable.saturating_add(1);
                }
                continue;
            };
            if part_of(&key, parts) != part {
                continue;
            }
            let cost = key.len().saturating_add(ENTRY_OVERHEAD);
            if seen.insert((ordinal_u16, key)) {
                // 新規のキーだけ予算に計上する（同じキーの大量重複で予算を使い果たさない）。
                used = used.saturating_add(cost);
                if used > budget_bytes {
                    return Ok(PassResult::OverBudget);
                }
            } else if ordinal == 0 && plan.has_primary_key {
                counts.primary_key = counts.primary_key.saturating_add(1);
            } else {
                counts.unique = counts.unique.saturating_add(1);
            }
        }
    }
    Ok(PassResult::Done(counts))
}

/// [`unique_index_audit`] の結果（[`unique_index::UniqueAudit`] の crate 内公開版）。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct UniqueIndexCounts {
    /// テナントのマーカーが有効か（`false` は遅延再構築の待ちで、他の件数は 0）。
    pub(crate) marker_valid: bool,
    /// 行にあるキーの正引きエントリが無い件数。
    pub(crate) missing: u64,
    /// 指す行が無い・行のキーと一致しない正引きエントリの件数。
    pub(crate) dangling: u64,
    /// 突き合わせられなかった件数。
    pub(crate) unevaluable: u64,
}

/// テナント 1 つ分の永続一意索引を行と双方向に突き合わせる（読み取り専用・メモリは行数に非依存）。
pub(crate) fn unique_index_audit<R, I>(
    plan: &UniqueAuditPlan,
    schema: &TableSchema,
    row_table: &R,
    index_table: &I,
    tenant_id: &str,
) -> Result<UniqueIndexCounts, TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>,
{
    let a = unique_index::audit_tenant_streaming(
        row_table,
        index_table,
        schema,
        &plan.plan,
        plan.compiled.as_ref(),
        tenant_id,
    )?;
    Ok(UniqueIndexCounts {
        marker_valid: a.marker_valid,
        missing: a.missing,
        dangling: a.dangling,
        unevaluable: a.unevaluable,
    })
}

/// [`foreign_key_orphans_in_tenant`] の件数。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ForeignKeyCounts {
    /// 参照先に対応する行が無い子行の数。
    pub(crate) orphans: u64,
    /// 判定できなかった行の数（分割番号 0 の走査でだけ数える）。
    pub(crate) unevaluable: u64,
    /// 参照先のキー集合が不完全（参照先の行を復号できない）で、集合に見つからない子行を孤児と
    /// 断定できなかった数。
    pub(crate) unresolved: u64,
}

/// 参照先の列がすべて参照先のスキーマに存在するか（`id` 参照は常に真）。
pub(crate) fn parent_columns_resolve(parent_schema: &TableSchema, fk: &ForeignKeyDef) -> bool {
    fk.references_parent_id()
        || fk
            .parent_columns()
            .iter()
            .all(|n| parent_schema.columns.iter().any(|c| &c.name == n))
}

/// 子表 1 つ・`FOREIGN KEY` 1 つ・テナント 1 つについて、参照先に存在しない子行（孤児）を数える。
///
/// 参照先の行はテナント内の可視性を問わず母集合とする（書き込み側の検査と同じ。RLS-10 (c)）。
/// 列参照はキー集合を `part`/`parts` で分割して `budget_bytes` 内に収め、`id` 参照は点照会で
/// メモリを使わない。`parent_rows` が `None` は参照先の行ストア未作成（行 0 件）を表す。
#[allow(clippy::too_many_arguments)] // 子/親の表とスキーマ・分割・予算を呼び出し側が明示する検査部品。
pub(crate) fn foreign_key_orphans_in_tenant<C, P>(
    child_schema: &TableSchema,
    parent_schema: &TableSchema,
    fk: &ForeignKeyDef,
    child_rows: &C,
    parent_rows: Option<&P>,
    tenant_id: &str,
    part: u32,
    parts: u32,
    budget_bytes: usize,
) -> Result<PassResult<ForeignKeyCounts>, TenantWriteError>
where
    C: ReadableTable<(&'static str, u64), &'static [u8]>,
    P: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let mut counts = ForeignKeyCounts::default();
    let (specs, child_mask) = foreign_key_specs_for(child_schema, fk)?;
    let by_id = fk.references_parent_id();
    let recoder = FkKeyRecoder::new(child_schema, parent_schema, fk)?;

    // 参照先のキー集合（列参照のみ。このハッシュ分割に属するキーだけを持つ）。
    let mut parent_keys: HashSet<Vec<u8>> = HashSet::new();
    let mut used = 0usize;
    // 参照先の行を復号・キー計算できず、キー集合が不完全か（分割に関わらず全区画に影響する）。
    let mut parent_incomplete = false;
    if !by_id {
        let mut indices = Vec::with_capacity(fk.parent_columns().len());
        let mut pmask = vec![false; parent_schema.columns.len()];
        for name in fk.parent_columns() {
            let idx = parent_schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| internal("referenced column not found in parent schema"))?;
            if let Some(slot) = pmask.get_mut(idx) {
                *slot = true;
            }
            indices.push(idx);
        }
        let pspec = KeySpec::new(indices, NullPolicy::Skip);
        if let Some(parent_rows) = parent_rows {
            let start = std::ops::Bound::Included((tenant_id, 0u64));
            let end = std::ops::Bound::Included((tenant_id, u64::MAX));
            for entry in parent_rows
                .range::<(&str, u64)>((start, end))
                .map_err(crate::catalog::CatalogError::from)?
            {
                let (k, v) = entry.map_err(crate::catalog::CatalogError::from)?;
                if k.value().0 != tenant_id {
                    break;
                }
                let Ok(values) = decode_key_columns(parent_schema, &pmask, v.value()) else {
                    parent_incomplete = true;
                    if part == 0 {
                        counts.unevaluable = counts.unevaluable.saturating_add(1);
                    }
                    continue;
                };
                let key = match key_bytes(&pspec, &values) {
                    Ok(Some(key)) => key,
                    // 構成列が NULL の行は参照先になり得ない（MATCH SIMPLE）。
                    Ok(None) => continue,
                    Err(_) => {
                        parent_incomplete = true;
                        if part == 0 {
                            counts.unevaluable = counts.unevaluable.saturating_add(1);
                        }
                        continue;
                    }
                };
                if part_of(&key, parts) != part {
                    continue;
                }
                let cost = key.len().saturating_add(ENTRY_OVERHEAD);
                if parent_keys.insert(key) {
                    used = used.saturating_add(cost);
                    if used > budget_bytes {
                        return Ok(PassResult::OverBudget);
                    }
                }
            }
        }
    }

    let spec = specs;
    let start = std::ops::Bound::Included((tenant_id, 0u64));
    let end = std::ops::Bound::Included((tenant_id, u64::MAX));
    for entry in child_rows
        .range::<(&str, u64)>((start, end))
        .map_err(crate::catalog::CatalogError::from)?
    {
        let (k, v) = entry.map_err(crate::catalog::CatalogError::from)?;
        if k.value().0 != tenant_id {
            break;
        }
        let Ok(values) = decode_key_columns(child_schema, &child_mask, v.value()) else {
            if part == 0 {
                counts.unevaluable = counts.unevaluable.saturating_add(1);
            }
            continue;
        };
        let mut required = RequiredParentKeys::new(fk);
        match push_required_key(&spec, &values, &mut required) {
            Ok(()) => {}
            Err(TenantWriteError::ForeignKeyViolation) => {
                // 負の `id` 値・MATCH FULL の部分 NULL は参照先が存在し得ない。
                if part == 0 {
                    counts.orphans = counts.orphans.saturating_add(1);
                }
                continue;
            }
            Err(_) => {
                if part == 0 {
                    counts.unevaluable = counts.unevaluable.saturating_add(1);
                }
                continue;
            }
        }
        match required {
            RequiredParentKeys::Ids(ids) => {
                if part != 0 {
                    continue;
                }
                for id in ids {
                    let Ok(key) = parent_row_key_by_id_value(parent_schema, tenant_id, id) else {
                        counts.unevaluable = counts.unevaluable.saturating_add(1);
                        continue;
                    };
                    let exists = match parent_rows {
                        Some(p) => p
                            .get(key)
                            .map_err(crate::catalog::CatalogError::from)?
                            .is_some(),
                        None => false,
                    };
                    if !exists {
                        counts.orphans = counts.orphans.saturating_add(1);
                    }
                }
            }
            RequiredParentKeys::Keys(keys) => {
                let keys: BTreeSet<Vec<u8>> = keys;
                for key in keys {
                    match recoder.child_to_parent(key) {
                        Ok(Some(parent_key)) => {
                            if part_of(&parent_key, parts) == part
                                && !parent_keys.contains(&parent_key)
                            {
                                if parent_incomplete {
                                    counts.unresolved = counts.unresolved.saturating_add(1);
                                } else {
                                    counts.orphans = counts.orphans.saturating_add(1);
                                }
                            }
                        }
                        Ok(None) => {
                            if part == 0 {
                                counts.orphans = counts.orphans.saturating_add(1);
                            }
                        }
                        Err(_) => {
                            if part == 0 {
                                counts.unevaluable = counts.unevaluable.saturating_add(1);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(PassResult::Done(counts))
}
