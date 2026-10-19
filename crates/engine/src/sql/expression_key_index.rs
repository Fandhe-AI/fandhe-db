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
//!   クエリの成否・結果には影響しない（多層防御。通常は下記の DML ゲートで起こらない）。
//!   現時点の消費者はテスト（順序付き走査 API）だけで、プランナ選択・EXPLAIN 表示は後続
//!   Issue #1650〜#1651 の責務。
//! - DML 維持（Issue #1649）: 式キー索引はコミット済みの行からテーブル世代ごとに作り直す派生物
//!   なので、維持の本体は「全行で式が評価でき索引化できる」不変条件の強制になる。
//!   [`CompiledIndexExpressions`] が `constraint::enforce_row_constraints_in_txn`（全書き込み経路の
//!   単一検査点）と `catalog::Storage::create_index_bound`（既存行の検証）から呼ばれ、評価エラー・
//!   非有限値・型不一致を commit 前に拒否する。値の分類は [`classify_expression_value`] を
//!   構築側（[`ExpressionKeyIndex::build`]）と共有し、2 系統に分けない。
//! - 構築元は RLS 適用済みスナップショットだけで、候補は構造的に可視集合の部分集合になる
//!   （他テナントの行を読まない）。

use crate::catalog::{DeclaredExpressionKey, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::arena_cache::SqlArenaSnapshot;
use crate::sql::scalar_index::ScalarIndexBuildError;
use crate::sql::udf_call::{ExprType, ExprValue};
use crate::tenant::TenantWriteError;

/// 式の結果値 1 個を索引キーへ分類した結果（構築側と DML ゲートで共有する）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClassifiedKey<'a> {
    /// NULL（キーに入れず NULL スロットへ分ける。索引には含める）。
    Null,
    /// `TEXT` のバイト列キー。
    Bytes(&'a [u8]),
    /// 数値の全順序 `u64` ビット表現（`normalize_zero` 済みの有限値）。
    F64Sortable(u64),
    /// `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` の `i64` キー。
    I64(i64),
}

/// [`classify_expression_value`] が索引化できないと判断した理由。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClassifyError {
    /// 非有限の数値（キーのエンコードが表せない）。
    NonFinite,
    /// 静的な結果型と実行時の値の型が食い違った（不変条件の破れ）。
    TypeMismatch,
}

/// 式の結果値を結果型 `ty` に応じた索引キーへ分類する。[`ExpressionKeyIndex::build`] と
/// DML ゲート（[`CompiledIndexExpressions::enforce`]）が共有する唯一の分類規則。
pub(crate) fn classify_expression_value<'a>(
    ty: ExprType,
    value: &'a ExprValue<'_>,
) -> Result<ClassifiedKey<'a>, ClassifyError> {
    if matches!(value, ExprValue::Null) {
        return Ok(ClassifiedKey::Null);
    }
    match (ty, value) {
        (ExprType::Text, ExprValue::Text(s)) => Ok(ClassifiedKey::Bytes(s.as_bytes())),
        (ExprType::Scalar, ExprValue::Scalar(x)) => {
            if !x.is_finite() {
                return Err(ClassifyError::NonFinite);
            }
            Ok(ClassifiedKey::F64Sortable(
                crate::sql::scalar_index::f64_to_sortable_bits(
                    crate::sql::scalar_index::normalize_zero(*x),
                ),
            ))
        }
        (
            ExprType::Date | ExprType::Timestamp | ExprType::Time | ExprType::TimestampTz,
            ExprValue::Date(d),
        ) => Ok(ClassifiedKey::I64(i64::from(*d))),
        (
            ExprType::Date | ExprType::Timestamp | ExprType::Time | ExprType::TimestampTz,
            ExprValue::Timestamp(t) | ExprValue::Time(t) | ExprValue::TimestampTz(t),
        ) => Ok(ClassifiedKey::I64(*t)),
        _ => Err(ClassifyError::TypeMismatch),
    }
}

/// 保存済みの式要素を再解析・再束縛し、往復一致を確認した評価器と参照列マスク。
struct CompiledElement {
    bound: crate::sql::index_expr::BoundIndexExpression,
    mask: Vec<bool>,
}

/// テーブルの式索引の式要素群を write txn 内でコンパイルした DML ゲート用の評価器
/// （INDEX-10、Issue #1649）。1 文につき 1 回だけコンパイルし、`written_ids` の各行で
/// [`CompiledIndexExpressions::enforce`] を呼ぶ。`sql::check_constraint::CompiledChecks` と同型。
pub(crate) struct CompiledIndexExpressions {
    elements: Vec<CompiledElement>,
}

/// 保存済みの式の漂流（再解析・再束縛・往復一致の失敗）を fail-closed の `XX000` へ写像する。
fn drifted() -> TenantWriteError {
    TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(
        "stored index expression no longer binds".to_string(),
    ))
}

impl CompiledIndexExpressions {
    /// `decls` をコンパイルする。宣言が空なら `None`（ゲート不要）。保存テキストは untrusted で、
    /// 再解析・再束縛を通り往復で同じテキストに戻らなければ [`TenantWriteError::Catalog`]
    /// （`CorruptSchema`）で拒否する。
    pub(crate) fn compile(
        schema: &TableSchema,
        decls: &[DeclaredExpressionKey],
    ) -> Result<Option<Self>, TenantWriteError> {
        if decls.is_empty() {
            return Ok(None);
        }
        let mut elements: Vec<CompiledElement> = Vec::new();
        elements
            .try_reserve_exact(decls.len())
            .map_err(|_| drifted())?;
        for decl in decls {
            let expr = crate::sql::allowlist::parse_index_expression_text(&decl.sql)
                .map_err(|_| drifted())?;
            let bound = crate::sql::index_expr::bind_index_expression(&expr, schema)
                .map_err(|_| drifted())?;
            if bound.sql != decl.sql {
                return Err(drifted());
            }
            let mut mask = vec![false; schema.columns.len()];
            crate::sql::udf_call::mark_referenced_scalar_columns(&bound.bound, &mut mask);
            elements.push(CompiledElement { bound, mask });
        }
        Ok(Some(Self { elements }))
    }

    /// 1 行（`id`・`embedding`・`metadata`）で全式要素を評価し、索引化できなければ拒否する。
    /// 評価エラーは [`TenantWriteError::IndexExpressionEvaluationFailed`] で透過し、非有限値は
    /// `22003`、型不一致は内部事象（`XX000`）にする。NULL は可。
    pub(crate) fn enforce(
        &self,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Result<(), TenantWriteError> {
        let mut scratch = Vec::new();
        for element in &self.elements {
            let scanned =
                crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&element.mask))
                    .map_err(|e| {
                        TenantWriteError::Catalog(crate::catalog::CatalogError::CorruptSchema(
                            e.to_string(),
                        ))
                    })?;
            let emb: &[f32] = if element.bound.references_embedding {
                embedding
            } else {
                &[]
            };
            let value = element
                .bound
                .program
                .eval(id, emb, &scanned, &mut scratch)
                .map_err(TenantWriteError::IndexExpressionEvaluationFailed)?;
            match classify_expression_value(element.bound.ty, &value) {
                Ok(_) => {}
                Err(ClassifyError::NonFinite) => {
                    return Err(TenantWriteError::IndexExpressionEvaluationFailed(
                        SqlSurfaceError::NumericOutOfRange {
                            detail: "index expression value is out of range".to_string(),
                        },
                    ));
                }
                Err(ClassifyError::TypeMismatch) => {
                    return Err(TenantWriteError::IndexExpressionEvaluationFailed(
                        SqlSurfaceError::Internal {
                            detail: "index expression result type mismatch".to_string(),
                        },
                    ));
                }
            }
        }
        Ok(())
    }
}

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
        // 索引名の複製（`approx_heap_bytes` が計上する分）も合わせて見込む。
        let mut used = row_count
            .saturating_mul(entry_size)
            .saturating_add(decl.index_name.len());
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
            match (&mut keys, classify_expression_value(bound.ty, &value)) {
                (_, Ok(ClassifiedKey::Null)) => {
                    // 確保前に増分（容量が伸びる分は倍々なので最大 2 倍まで見込む）を予算へ計上する。
                    let grow = if null_slots.len() == null_slots.capacity() {
                        null_slots
                            .capacity()
                            .max(4)
                            .saturating_mul(std::mem::size_of::<u32>())
                    } else {
                        0
                    };
                    crate::sql::scalar_index::check_scalar_index_budget(
                        running_bytes.saturating_add(used),
                        grow.max(std::mem::size_of::<u32>()),
                    )?;
                    let before = null_slots.capacity();
                    reserve_exact_one(&mut null_slots)?;
                    used = used.saturating_add(
                        null_slots
                            .capacity()
                            .saturating_sub(before)
                            .saturating_mul(std::mem::size_of::<u32>()),
                    );
                    null_slots.push(slot_u32);
                }
                (ExpressionKeys::Bytes(v), Ok(ClassifiedKey::Bytes(bytes))) => {
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
                (ExpressionKeys::F64Sortable(v), Ok(ClassifiedKey::F64Sortable(bits))) => {
                    v.push((bits, slot_u32));
                }
                (ExpressionKeys::I64(v), Ok(ClassifiedKey::I64(x))) => v.push((x, slot_u32)),
                // 非有限値、または静的な結果型と実行時の値の型が食い違った（不変条件の破れ）: 作らない。
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
#[cfg(test)]
mod tests {
    use super::*;

    /// 値の分類（INDEX-10、Issue #1649）: NULL は索引に含め、非有限と型不一致は拒否する。
    #[test]
    fn classify_covers_null_text_numeric_and_temporal() {
        assert_eq!(
            classify_expression_value(ExprType::Text, &ExprValue::Null),
            Ok(ClassifiedKey::Null)
        );
        assert_eq!(
            classify_expression_value(ExprType::Scalar, &ExprValue::Null),
            Ok(ClassifiedKey::Null)
        );
        assert_eq!(
            classify_expression_value(ExprType::Text, &ExprValue::Text("ab".into())),
            Ok(ClassifiedKey::Bytes(b"ab"))
        );
        // `-0.0` と `+0.0` は同じキーになる。
        assert_eq!(
            classify_expression_value(ExprType::Scalar, &ExprValue::Scalar(-0.0)),
            classify_expression_value(ExprType::Scalar, &ExprValue::Scalar(0.0))
        );
        assert_eq!(
            classify_expression_value(ExprType::Scalar, &ExprValue::Scalar(f64::INFINITY)),
            Err(ClassifyError::NonFinite)
        );
        assert_eq!(
            classify_expression_value(ExprType::Scalar, &ExprValue::Scalar(f64::NAN)),
            Err(ClassifyError::NonFinite)
        );
        assert_eq!(
            classify_expression_value(ExprType::Date, &ExprValue::Date(7)),
            Ok(ClassifiedKey::I64(7))
        );
        assert_eq!(
            classify_expression_value(ExprType::Timestamp, &ExprValue::Timestamp(9)),
            Ok(ClassifiedKey::I64(9))
        );
        // 静的な結果型と値の型の食い違いは不変条件の破れ。
        assert_eq!(
            classify_expression_value(ExprType::Text, &ExprValue::Scalar(1.0)),
            Err(ClassifyError::TypeMismatch)
        );
        assert_eq!(
            classify_expression_value(ExprType::Scalar, &ExprValue::Text("x".into())),
            Err(ClassifyError::TypeMismatch)
        );
    }
}
