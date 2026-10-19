//! 式索引（`CREATE INDEX ... ((<式>))`）の式キー索引（INDEX-10、Issue #1648）。
//!
//! 役割と責務境界:
//! - `sql::scalar_index::ScalarIndex` が構築時に呼ぶ。RLS 適用済みスナップショット
//!   （[`SqlArenaSnapshot`]）の各スロットで式を評価し、結果型に応じたキー
//!   （`TEXT` はバイト列順、数値は全順序の `u64` ビット表現、日付・時刻は `i64`）で
//!   `(キー, スロット)` を昇順に整列して保持する。NULL 結果のスロットはキーに入れず
//!   [`ExpressionKeyIndex::null_slots`] へ分ける。
//! - 式テキストは `catalog` に保存された正規化 SQL で、untrusted として扱う。再解析
//!   （`allowlist::parse_index_expression_text`）→ 再束縛（`index_expr::bind_index_expression`）を
//!   通し、往復で同じテキストに戻らなければ使わない。
//! - fail-soft かつ「狭めるだけ」: 再束縛の失敗・1 行でも式の評価エラー・非有限の `f64`・予算超過・
//!   確保失敗のいずれでも、その式キーだけを作らない（部分的な索引は作らない）。既存の列索引や
//!   クエリの成否・結果には影響しない。現時点の消費者はテスト（順序付き走査 API）だけで、
//!   プランナ選択・EXPLAIN 表示・DML 維持は後続 Issue #1649〜#1651 の責務。
//! - 構築元は RLS 適用済みスナップショットだけで、候補は構造的に可視集合の部分集合になる
//!   （他テナントの行を読まない）。

use crate::catalog::{DeclaredExpressionKey, TableSchema};
use crate::sql::arena_cache::SqlArenaSnapshot;
use crate::sql::scalar_index::ScalarIndexBuildError;
use crate::sql::udf_call::{ExprType, ExprValue};

/// 式の結果型ごとのキー列（`(キー, スロット)` をキー昇順・同値はスロット昇順に整列済み）。
#[derive(Debug)]
pub(crate) enum ExpressionKeys {
    /// `TEXT` 結果。UTF-8 バイト列の辞書順（`Collation::DEFAULT`）。
    Bytes(Vec<(Vec<u8>, u32)>),
    /// 数値結果。`normalize_zero` 済みの有限 `f64` を全順序の `u64` へ写像したキー。
    F64Sortable(Vec<(u64, u32)>),
    /// `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` 結果。
    I64(Vec<(i64, u32)>),
}

/// 式索引の式要素 1 個分のキー索引。
#[derive(Debug)]
pub(crate) struct ExpressionKeyIndex {
    index_name: String,
    element: usize,
    keys: ExpressionKeys,
    /// 式が NULL を返したスロット（昇順）。
    null_slots: Vec<u32>,
}

/// 確保失敗を [`ScalarIndexBuildError::AllocationFailed`] にして `Vec` を事前確保する。
fn reserve_exact<T>(v: &mut Vec<T>, n: usize) -> Result<(), ScalarIndexBuildError> {
    v.try_reserve_exact(n)
        .map_err(|_| ScalarIndexBuildError::AllocationFailed)
}

impl ExpressionKeyIndex {
    /// `decl`（保存済みの式要素）を `snapshot` の全スロットで評価して構築する。`running_bytes` は
    /// 呼び出し元がすでに計上している概算バイト量で、予算（`check_scalar_index_budget`）は
    /// 確保の**前**に検査する。失敗は呼び出し元が「この式キーだけ作らない」へ縮退する。
    pub(crate) fn build(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        decl: &DeclaredExpressionKey,
        running_bytes: usize,
    ) -> Result<Self, ScalarIndexBuildError> {
        // 永続データは untrusted。再解析・再束縛し、保存形へ描き戻して一致することを確認する。
        let expr = crate::sql::allowlist::parse_index_expression_text(&decl.sql)
            .map_err(|_| ScalarIndexBuildError::Unbuildable)?;
        let bound = crate::sql::index_expr::bind_index_expression(&expr, schema)
            .map_err(|_| ScalarIndexBuildError::Unbuildable)?;
        if bound.sql != decl.sql {
            return Err(ScalarIndexBuildError::Unbuildable);
        }
        let mut mask = vec![false; schema.columns.len()];
        crate::sql::udf_call::mark_referenced_scalar_columns(&bound.bound, &mut mask);

        let arena = snapshot.arena();
        let row_count = arena.len();
        let entry_size = match bound.ty {
            ExprType::Text => std::mem::size_of::<(Vec<u8>, u32)>(),
            ExprType::Scalar => std::mem::size_of::<(u64, u32)>(),
            _ => std::mem::size_of::<(i64, u32)>(),
        };
        // 配列は `row_count` 件ちょうどを一括確保する（確保前に予算へ計上して検査する）。
        let mut used = row_count.saturating_mul(entry_size);
        crate::sql::scalar_index::check_scalar_index_budget(running_bytes, used)?;
        let mut keys = match bound.ty {
            ExprType::Text => {
                let mut v = Vec::new();
                reserve_exact(&mut v, row_count)?;
                ExpressionKeys::Bytes(v)
            }
            ExprType::Scalar => {
                let mut v = Vec::new();
                reserve_exact(&mut v, row_count)?;
                ExpressionKeys::F64Sortable(v)
            }
            ExprType::Date | ExprType::Timestamp | ExprType::Time | ExprType::TimestampTz => {
                let mut v = Vec::new();
                reserve_exact(&mut v, row_count)?;
                ExpressionKeys::I64(v)
            }
            // `bind_index_expression` が結果型ゲートで弾く型。到達しない多層防御。
            ExprType::Vector
            | ExprType::Bool
            | ExprType::Interval
            | ExprType::Json
            | ExprType::JsonText
            | ExprType::Numeric
            | ExprType::Uuid
            | ExprType::Bytea
            | ExprType::Array => return Err(ScalarIndexBuildError::Unbuildable),
        };
        let mut null_slots: Vec<u32> = Vec::new();
        let mut scratch = Vec::new();
        let ids = arena.ids();
        for slot in 0..row_count {
            let slot_u32 = u32::try_from(slot).map_err(|_| ScalarIndexBuildError::SlotOverflow)?;
            let metadata = snapshot
                .metadata()
                .get(slot)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let scanned =
                crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&mask))
                    .map_err(ScalarIndexBuildError::RowDecode)?;
            let id = ids.get(slot).copied().unwrap_or(0);
            let embedding: &[f32] = if bound.references_embedding {
                arena.vector(slot).unwrap_or(&[])
            } else {
                &[]
            };
            // 1 行でも評価に失敗したら、この式キーは作らない（部分的な索引は「狭める」前提を壊す）。
            let value = bound
                .program
                .eval(id, embedding, &scanned, &mut scratch)
                .map_err(|_| ScalarIndexBuildError::Unbuildable)?;
            match (&mut keys, value) {
                (_, ExprValue::Null) => {
                    reserve_exact_one(&mut null_slots)?;
                    used = used.saturating_add(std::mem::size_of::<u32>());
                    null_slots.push(slot_u32);
                }
                (ExpressionKeys::Bytes(v), ExprValue::Text(s)) => {
                    let bytes = s.as_bytes();
                    crate::sql::scalar_index::check_scalar_index_budget(
                        running_bytes.saturating_add(used),
                        bytes.len(),
                    )?;
                    let mut owned: Vec<u8> = Vec::new();
                    reserve_exact(&mut owned, bytes.len())?;
                    owned.extend_from_slice(bytes);
                    used = used.saturating_add(bytes.len());
                    v.push((owned, slot_u32));
                }
                (ExpressionKeys::F64Sortable(v), ExprValue::Scalar(x)) => {
                    if !x.is_finite() {
                        return Err(ScalarIndexBuildError::Unbuildable);
                    }
                    v.push((
                        crate::sql::scalar_index::f64_to_sortable_bits(
                            crate::sql::scalar_index::normalize_zero(x),
                        ),
                        slot_u32,
                    ));
                }
                (ExpressionKeys::I64(v), ExprValue::Date(d)) => v.push((i64::from(d), slot_u32)),
                (
                    ExpressionKeys::I64(v),
                    ExprValue::Timestamp(t) | ExprValue::Time(t) | ExprValue::TimestampTz(t),
                ) => v.push((t, slot_u32)),
                // 静的な結果型と実行時の値の型が食い違った（不変条件の破れ）: 作らない。
                _ => return Err(ScalarIndexBuildError::Unbuildable),
            }
        }
        match &mut keys {
            ExpressionKeys::Bytes(v) => v.sort_unstable(), // sort-determinism: allow (バイト列キー, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
            ExpressionKeys::F64Sortable(v) => v.sort_unstable(), // sort-determinism: allow (u64 ソート可能ビット表現, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
            ExpressionKeys::I64(v) => v.sort_unstable(), // sort-determinism: allow (i64 キー, u32 スロット) のタプル全順序でスロットが明示的タイブレーク
        }
        Ok(Self {
            index_name: decl.index_name.clone(),
            element: decl.element,
            keys,
            null_slots,
        })
    }

    /// 索引名（`CREATE INDEX` で付けた名前）。
    pub(crate) fn index_name(&self) -> &str {
        &self.index_name
    }

    /// 索引内の要素位置（0 始まり）。
    pub(crate) fn element(&self) -> usize {
        self.element
    }

    /// 式が NULL を返したスロット（昇順）。
    pub(crate) fn null_slots(&self) -> &[u32] {
        &self.null_slots
    }

    /// 非 NULL のスロットを式の結果順（キー昇順・同値はスロット昇順）に並べ、NULL のスロットを
    /// 末尾に置く（PostgreSQL 既定の `ASC NULLS LAST`）。確保失敗は `None`。
    pub(crate) fn ordered_slots(&self) -> Option<Vec<u32>> {
        let non_null = match &self.keys {
            ExpressionKeys::Bytes(v) => v.len(),
            ExpressionKeys::F64Sortable(v) => v.len(),
            ExpressionKeys::I64(v) => v.len(),
        };
        let mut out: Vec<u32> = Vec::new();
        out.try_reserve_exact(non_null.saturating_add(self.null_slots.len()))
            .ok()?;
        match &self.keys {
            ExpressionKeys::Bytes(v) => out.extend(v.iter().map(|(_, s)| *s)),
            ExpressionKeys::F64Sortable(v) => out.extend(v.iter().map(|(_, s)| *s)),
            ExpressionKeys::I64(v) => out.extend(v.iter().map(|(_, s)| *s)),
        }
        out.extend(self.null_slots.iter().copied());
        Some(out)
    }

    /// `TEXT` 結果の範囲照会（`[lower, upper]`。結果はスロット昇順）。結果型が `TEXT` でなければ `None`。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn range_slots_bytes(
        &self,
        lower: std::ops::Bound<&[u8]>,
        upper: std::ops::Bound<&[u8]>,
    ) -> Option<Vec<u32>> {
        match &self.keys {
            ExpressionKeys::Bytes(v) => {
                crate::sql::scalar_index::range_slots_bytes(v, lower, upper)
            }
            ExpressionKeys::F64Sortable(_) | ExpressionKeys::I64(_) => None,
        }
    }

    /// `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` 結果の範囲照会。結果型が一致しなければ `None`。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn range_slots_i64(
        &self,
        lower: std::ops::Bound<i64>,
        upper: std::ops::Bound<i64>,
    ) -> Option<Vec<u32>> {
        match &self.keys {
            ExpressionKeys::I64(v) => crate::sql::scalar_index::range_slots(v, lower, upper),
            ExpressionKeys::Bytes(_) | ExpressionKeys::F64Sortable(_) => None,
        }
    }

    /// 数値結果の範囲照会（境界は有限の `f64`。`-0.0` は `+0.0` として扱う）。結果型が一致しなければ
    /// `None`。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn range_slots_f64(
        &self,
        lower: std::ops::Bound<f64>,
        upper: std::ops::Bound<f64>,
    ) -> Option<Vec<u32>> {
        use std::ops::Bound;
        let key = |x: f64| {
            x.is_finite().then(|| {
                crate::sql::scalar_index::f64_to_sortable_bits(
                    crate::sql::scalar_index::normalize_zero(x),
                )
            })
        };
        let map = |b: Bound<f64>| -> Option<Bound<u64>> {
            Some(match b {
                Bound::Unbounded => Bound::Unbounded,
                Bound::Included(x) => Bound::Included(key(x)?),
                Bound::Excluded(x) => Bound::Excluded(key(x)?),
            })
        };
        match &self.keys {
            ExpressionKeys::F64Sortable(v) => {
                crate::sql::scalar_index::range_slots(v, map(lower)?, map(upper)?)
            }
            ExpressionKeys::Bytes(_) | ExpressionKeys::I64(_) => None,
        }
    }

    /// 確保容量ベースの概算ヒープバイト量（`ScalarIndex::approx_heap_bytes` に加算する）。
    pub(crate) fn approx_heap_bytes(&self) -> usize {
        let keys_bytes = match &self.keys {
            ExpressionKeys::Bytes(v) => v
                .capacity()
                .saturating_mul(std::mem::size_of::<(Vec<u8>, u32)>())
                .saturating_add(
                    v.iter()
                        .fold(0usize, |acc, (k, _)| acc.saturating_add(k.capacity())),
                ),
            ExpressionKeys::F64Sortable(v) => v
                .capacity()
                .saturating_mul(std::mem::size_of::<(u64, u32)>()),
            ExpressionKeys::I64(v) => v
                .capacity()
                .saturating_mul(std::mem::size_of::<(i64, u32)>()),
        };
        keys_bytes
            .saturating_add(
                self.null_slots
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
            .saturating_add(self.index_name.capacity())
    }
}

/// `v` に 1 要素分の余地を確保する（NULL スロットの蓄積用。確保失敗は構築失敗として扱う）。
fn reserve_exact_one(v: &mut Vec<u32>) -> Result<(), ScalarIndexBuildError> {
    v.try_reserve(1)
        .map_err(|_| ScalarIndexBuildError::AllocationFailed)
}
