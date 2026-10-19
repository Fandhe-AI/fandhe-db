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
//!   消費者は 2 つ: WHERE の式述語を候補スロットへ写す [`ExpressionKeyIndex::candidates_for_comparison`]
//!   （Issue #1650。`sql::scalar_index::ScalarIndex::resolve_candidates` が呼ぶ）と、テストの順序付き
//!   走査 API。ORDER BY の式一致による索引順走査は後続 Issue の責務。UNIQUE の強制と列変更への追随は Issue #1651
//!   （[`CompiledIndexExpressions::push_unique_key_component`] が一意キーの式成分を作る）。
//! - プランナ選択（Issue #1650）: [`ExpressionIndexMatcher`] が保存済み宣言を再束縛した式木を持ち、
//!   WHERE 側の束縛済み式木と構造一致した述語だけを索引対応にする。分類（`sql::scalar_plan`）・
//!   実行（`sql::exec` 等）・EXPLAIN（`core.rs`）が同じ一致判定 [`ExpressionIndexMatcher::find`] を
//!   通る単一情報源にする。
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
use crate::sql::udf_call::{BinOp, BoundExpr, ExprType, ExprValue};
use crate::tenant::TenantWriteError;
use std::ops::Bound;

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
        for position in 0..self.elements.len() {
            self.eval_element(
                position,
                schema,
                id,
                embedding,
                metadata,
                &mut scratch,
                |_| {},
            )?;
        }
        Ok(())
    }

    /// 式要素 `position` を 1 行で評価し、分類した値を `use_key` へ渡す。評価・分類のエラー写像は
    /// [`Self::enforce`] と共通（2 系統に分けない）。
    #[allow(clippy::too_many_arguments)]
    fn eval_element<R>(
        &self,
        position: usize,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
        scratch: &mut Vec<crate::sql::expr_program::StackValue>,
        use_key: impl FnOnce(&ClassifiedKey<'_>) -> R,
    ) -> Result<R, TenantWriteError> {
        let element = self.elements.get(position).ok_or_else(drifted)?;
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
            .eval(id, emb, &scanned, scratch)
            .map_err(TenantWriteError::IndexExpressionEvaluationFailed)?;
        match classify_expression_value(element.bound.ty, &value) {
            Ok(key) => Ok(use_key(&key)),
            Err(ClassifyError::NonFinite) => {
                Err(TenantWriteError::IndexExpressionEvaluationFailed(
                    SqlSurfaceError::NumericOutOfRange {
                        detail: "index expression value is out of range".to_string(),
                    },
                ))
            }
            Err(ClassifyError::TypeMismatch) => Err(
                TenantWriteError::IndexExpressionEvaluationFailed(SqlSurfaceError::Internal {
                    detail: "index expression result type mismatch".to_string(),
                }),
            ),
        }
    }

    /// 式要素 `position` の値を一意キー用の正準成分として `out` へ足す（INDEX-9・INDEX-10、
    /// Issue #1651）。NULL なら何も足さず `Ok(false)`（NULLS DISTINCT: 呼び出し元はキーを持たない
    /// 行として扱う）。分類は [`classify_expression_value`] を共有し、成分は列要素の成分
    /// （`constraint::push_canonical_component`）と衝突しないタグ付き・長さ前置の形にする。
    pub(crate) fn push_unique_key_component(
        &self,
        position: usize,
        schema: &TableSchema,
        id: u64,
        embedding: &[f32],
        metadata: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<bool, TenantWriteError> {
        let mut scratch = Vec::new();
        self.eval_element(
            position,
            schema,
            id,
            embedding,
            metadata,
            &mut scratch,
            |key| match key {
                ClassifiedKey::Null => false,
                ClassifiedKey::Bytes(b) => {
                    push_expression_component(out, EXPR_KEY_TAG_BYTES, b);
                    true
                }
                ClassifiedKey::F64Sortable(bits) => {
                    push_expression_component(out, EXPR_KEY_TAG_F64, &bits.to_be_bytes());
                    true
                }
                ClassifiedKey::I64(v) => {
                    push_expression_component(out, EXPR_KEY_TAG_I64, &v.to_be_bytes());
                    true
                }
            },
        )
    }

    /// 式要素 `position` の識別を `out` へ足す（一意キー索引のマーカーのシグネチャ用）。正規化 SQL・
    /// 結果型・参照列の名前と型の指紋を含めるため、式の変更・参照列の改名・型変更で必ず変わる。
    pub(crate) fn push_signature(
        &self,
        position: usize,
        schema: &TableSchema,
        out: &mut Vec<u8>,
    ) -> Result<(), TenantWriteError> {
        let element = self.elements.get(position).ok_or_else(drifted)?;
        push_len_prefixed_u32(out, element.bound.sql.as_bytes());
        out.push(expr_type_tag(element.bound.ty));
        for (column, referenced) in schema.columns.iter().zip(&element.mask) {
            if !*referenced {
                continue;
            }
            push_len_prefixed_u32(out, column.name.as_bytes());
            let (tag, param) = column.catalog_fields().map_err(TenantWriteError::Catalog)?;
            push_len_prefixed_u32(out, tag.as_bytes());
            push_len_prefixed_u32(out, param.as_bytes());
        }
        Ok(())
    }
}

/// 式成分のタグ（`TEXT` のバイト列）。列要素のタグ（`ColumnType::unique_key_tag` は 0..=14、配列の
/// 要素タグは別の入れ子）と重ならない値にする。
pub(crate) const EXPR_KEY_TAG_BYTES: u8 = 0xE1;
/// 式成分のタグ（数値の全順序 `u64` ビット表現）。
pub(crate) const EXPR_KEY_TAG_F64: u8 = 0xE2;
/// 式成分のタグ（`DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` の `i64`）。
pub(crate) const EXPR_KEY_TAG_I64: u8 = 0xE3;

/// `[tag: u8][len: u32 BE][payload]`。長さ前置で成分の境界が曖昧にならない。
fn push_expression_component(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);
    push_len_prefixed_u32(out, payload);
}

fn push_len_prefixed_u32(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

/// 式の結果型の永続化用タグ（シグネチャ専用。索引化できる型だけが現れる）。
fn expr_type_tag(ty: ExprType) -> u8 {
    match ty {
        ExprType::Text => 1,
        ExprType::Scalar => 2,
        ExprType::Date => 3,
        ExprType::Timestamp => 4,
        ExprType::Time => 5,
        ExprType::TimestampTz => 6,
        _ => 0xFF,
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
    /// 式の結果型（時間型の `DATE`／`TIMESTAMP` 等をリテラルの種類と突き合わせるため、
    /// `keys` の variant だけでは区別できない型をここで保持する）。
    ty: ExprType,
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
        Self::build_for_slots(schema, snapshot, decl, running_bytes, None)
    }

    /// [`Self::build`] の本体。`slots` が `Some` のときは、そのスロット（昇順・重複なし）だけを評価して
    /// キーを作る（部分索引〔INDEX-11、Issue #1654〕の式要素を、述語 TRUE の行〔`members`〕だけで作る）。
    /// `None` は全スロット（従来どおり）。予算検査・確保の扱いは同じ。
    pub(crate) fn build_for_slots(
        schema: &TableSchema,
        snapshot: &SqlArenaSnapshot,
        decl: &DeclaredExpressionKey,
        running_bytes: usize,
        slots: Option<&[u32]>,
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
        let arena_len = arena.len();
        // 評価・格納の対象件数（`slots` 指定時はその件数）。
        let row_count = slots.map_or(arena_len, <[u32]>::len);
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
        for position in 0..row_count {
            let slot_u32 = match slots {
                Some(list) => *list
                    .get(position)
                    .ok_or(ScalarIndexBuildError::Unbuildable)?,
                None => u32::try_from(position).map_err(|_| ScalarIndexBuildError::SlotOverflow)?,
            };
            let slot = slot_u32 as usize;
            if slot >= arena_len {
                return Err(ScalarIndexBuildError::Unbuildable);
            }
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
            ty: bound.ty,
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
    pub(crate) fn range_slots_f64(
        &self,
        lower: std::ops::Bound<f64>,
        upper: std::ops::Bound<f64>,
    ) -> Option<Vec<u32>> {
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

    /// WHERE の式述語 `<式> <op> <リテラル>`（`op` は `=`／`<`／`<=`／`>`／`>=`）に一致する候補スロット
    /// （昇順）を返す（INDEX-10、Issue #1650）。リテラルの種類が式の結果型と一致しない・非有限の
    /// 数値・算術演算子は `None`（呼び出し元は全走査へ縮退する。「一致 0 件」とは別物）。NULL の
    /// スロットはキーに入っていないため、評価側の `NULL op x` が偽になる規則と一致する。
    /// 評価器との同値性: TEXT は `Collation::DEFAULT` のバイト列順と byte 等価、数値は `f64` 比較
    /// （`±0` は同値でキーは正規化済み）、時間型は同種 `i64` の比較。
    pub(crate) fn candidates_for_comparison(
        &self,
        op: BinOp,
        literal: &ExprKeyLiteral,
    ) -> Option<Vec<u32>> {
        match literal {
            ExprKeyLiteral::Text(s) => {
                let (lower, upper) = comparison_bounds(op, s.as_bytes())?;
                if self.ty != ExprType::Text {
                    return None;
                }
                self.range_slots_bytes(lower, upper)
            }
            ExprKeyLiteral::Scalar(x) => {
                if self.ty != ExprType::Scalar || !x.is_finite() {
                    return None;
                }
                let (lower, upper) = comparison_bounds(op, *x)?;
                self.range_slots_f64(lower, upper)
            }
            ExprKeyLiteral::Temporal(ty, v) => {
                if self.ty != *ty {
                    return None;
                }
                let (lower, upper) = comparison_bounds(op, *v)?;
                self.range_slots_i64(lower, upper)
            }
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

/// 比較演算子 `op` とキー `k` を `[lower, upper]` の境界対へ写す（`<式> op k`）。算術演算子は `None`。
fn comparison_bounds<K: Copy>(op: BinOp, k: K) -> Option<(Bound<K>, Bound<K>)> {
    Some(match op {
        BinOp::Eq => (Bound::Included(k), Bound::Included(k)),
        BinOp::Gt => (Bound::Excluded(k), Bound::Unbounded),
        BinOp::Ge => (Bound::Included(k), Bound::Unbounded),
        BinOp::Lt => (Bound::Unbounded, Bound::Excluded(k)),
        BinOp::Le => (Bound::Unbounded, Bound::Included(k)),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => return None,
    })
}

/// WHERE の式述語の右辺リテラル（式の結果型と同じ種類の定数だけを持つ。INDEX-10、Issue #1650）。
/// `sql::scalar_plan::ExprIndexPredicate::Expression` が保持し、[`ExpressionKeyIndex::
/// candidates_for_comparison`] が消費する。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExprKeyLiteral {
    /// `TEXT` 定数。
    Text(String),
    /// 有限の数値定数（非有限は分類側で対象外にする）。
    Scalar(f64),
    /// `DATE`／`TIMESTAMP`／`TIME`／`TIMESTAMPTZ` 定数（種類と `i64` 表現）。
    Temporal(ExprType, i64),
}

impl ExprKeyLiteral {
    /// 束縛済みの定数ノードをリテラルへ写す。定数以外・非有限の数値は `None`。
    pub(crate) fn from_bound(expr: &BoundExpr) -> Option<Self> {
        Some(match expr {
            BoundExpr::Text(s) => Self::Text(s.clone()),
            BoundExpr::Number(x) if x.is_finite() => Self::Scalar(*x),
            BoundExpr::Date(d) => Self::Temporal(ExprType::Date, i64::from(*d)),
            BoundExpr::Timestamp(t) => Self::Temporal(ExprType::Timestamp, *t),
            BoundExpr::Time(t) => Self::Temporal(ExprType::Time, *t),
            BoundExpr::TimestampTz(t) => Self::Temporal(ExprType::TimestampTz, *t),
            _ => return None,
        })
    }

    /// このリテラルが比較できる式の結果型。
    pub(crate) fn expr_type(&self) -> ExprType {
        match self {
            Self::Text(_) => ExprType::Text,
            Self::Scalar(_) => ExprType::Scalar,
            Self::Temporal(ty, _) => *ty,
        }
    }
}

/// 式の一致結果（どの式索引のどの要素か）。名前は EXPLAIN の `index=` 注記にそのまま使う。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExprMatchRef {
    pub(crate) index_name: String,
    pub(crate) element: usize,
}

/// 一致判定用に再束縛した式要素 1 個分。
#[derive(Debug)]
struct MatcherEntry {
    index_name: String,
    element: usize,
    bound: BoundExpr,
    ty: ExprType,
}

/// 対象テーブルの式索引の式要素を再束縛して持ち、WHERE の束縛済み式木との構造一致を判定する
/// （INDEX-10、Issue #1650）。`sql::scalar_index::resolve_expression_matcher_in_txn` が
/// クエリと同一の `read_txn` から作り、分類（`sql::scalar_plan`）・実行・EXPLAIN が共有する。
///
/// 一致の正当性: [`BoundExpr`] はレジストリやセッションを参照せず単独で評価できる純粋な木で、
/// 索引に入る式は決定性検査（`42P17`）を通過済みのため、木が等しければ評価結果も等しい。
/// 再束縛・往復一致・識別子検証のいずれかに失敗した宣言は黙って外す（fail-soft。一致しない＝全走査）。
#[derive(Debug, Default)]
pub(crate) struct ExpressionIndexMatcher {
    entries: Vec<MatcherEntry>,
}

/// 構造一致。`PartialEq` は数値リテラルを `f64 ==` で比べて `-0.0` と `0.0` を同一視するが、
/// `atan2` 等では符号付きゼロが結果を変えるため、デバッグ表現（`f64` を符号付きで描く）も
/// 一致することを追加で要求する。
fn exprs_identical(a: &BoundExpr, b: &BoundExpr) -> bool {
    a == b && format!("{a:?}") == format!("{b:?}")
}

impl ExpressionIndexMatcher {
    /// 何にも一致しないマッチャ（宣言なし・opt-in なし・カタログ読み取り失敗のときの値）。
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// 保存済みの式宣言を再解析・再束縛して作る。保存テキストは untrusted で、往復一致しない・
    /// 識別子として不正な索引名・確保失敗の宣言は外す。`(索引名, 要素位置)` 昇順に並べ、同じ式を
    /// 持つ索引が複数あっても選択が決定的になるようにする。
    pub(crate) fn from_decls(schema: &TableSchema, decls: &[DeclaredExpressionKey]) -> Self {
        let mut entries: Vec<MatcherEntry> = Vec::new();
        if entries.try_reserve_exact(decls.len()).is_err() {
            return Self::empty();
        }
        for decl in decls {
            if crate::catalog::validate_identifier(&decl.index_name).is_err() {
                continue;
            }
            let Ok(expr) = crate::sql::allowlist::parse_index_expression_text(&decl.sql) else {
                continue;
            };
            let Ok(bound) = crate::sql::index_expr::bind_index_expression(&expr, schema) else {
                continue;
            };
            if bound.sql != decl.sql {
                continue;
            }
            entries.push(MatcherEntry {
                index_name: decl.index_name.clone(),
                element: decl.element,
                bound: bound.bound,
                ty: bound.ty,
            });
        }
        entries.sort_by(|a, b| {
            a.index_name
                .cmp(&b.index_name)
                .then(a.element.cmp(&b.element))
        });
        Self { entries }
    }

    /// WHERE 側の式 `expr`（比較の非リテラル側）が結果型 `ty` の式要素と構造一致する最初の宣言。
    pub(crate) fn find(&self, expr: &BoundExpr, ty: ExprType) -> Option<ExprMatchRef> {
        self.entries
            .iter()
            .find(|e| e.ty == ty && exprs_identical(&e.bound, expr))
            .map(|e| ExprMatchRef {
                index_name: e.index_name.clone(),
                element: e.element,
            })
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

    /// 式成分のタグは列要素のタグ（`ColumnType::unique_key_tag` の 0..=14）と配列要素のマーク
    /// （0x80・0x40）に重ならず、互いにも異なる（Issue #1651。複合キーで成分が衝突しない前提）。
    #[test]
    fn expression_component_tags_are_disjoint_from_column_tags() {
        let expr_tags = [EXPR_KEY_TAG_BYTES, EXPR_KEY_TAG_F64, EXPR_KEY_TAG_I64];
        for (i, a) in expr_tags.iter().enumerate() {
            assert!(*a > 14, "tag {a:#x} collides with a column key tag");
            for b in &expr_tags[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // 長さ前置の成分形。
        let mut out = Vec::new();
        push_expression_component(&mut out, EXPR_KEY_TAG_BYTES, b"ab");
        assert_eq!(out, [EXPR_KEY_TAG_BYTES, 0, 0, 0, 2, b'a', b'b']);
    }

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
/// [`ExpressionIndexMatcher`] と [`ExprKeyLiteral`] の単体テスト（INDEX-10、Issue #1650）。
#[cfg(test)]
mod matcher_tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType};

    fn schema() -> TableSchema {
        TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        )
    }

    /// 式テキストを束縛し、カタログに保存される正規化 SQL と束縛済み式木を得る。
    fn bind(text: &str) -> crate::sql::index_expr::BoundIndexExpression {
        let expr = crate::sql::allowlist::parse_index_expression_text(text).expect("parse");
        crate::sql::index_expr::bind_index_expression(&expr, &schema()).expect("bind")
    }

    fn decl(name: &str, element: usize, text: &str) -> DeclaredExpressionKey {
        DeclaredExpressionKey {
            index_name: name.to_string(),
            element,
            sql: bind(text).sql,
        }
    }

    #[test]
    fn find_requires_structural_identity_and_matching_result_type() {
        let matcher = ExpressionIndexMatcher::from_decls(
            &schema(),
            &[decl("i_lower", 0, "lower(lang)"), decl("i_n", 0, "n + 1")],
        );
        let lower = bind("lower(lang)").bound;
        assert_eq!(
            matcher.find(&lower, ExprType::Text),
            Some(ExprMatchRef {
                index_name: "i_lower".to_string(),
                element: 0
            })
        );
        // 結果型が違うリテラルとは一致しない。
        assert_eq!(matcher.find(&lower, ExprType::Scalar), None);
        // 別の関数・別の引数・別の定数は一致しない。
        assert_eq!(
            matcher.find(&bind("upper(lang)").bound, ExprType::Text),
            None
        );
        assert_eq!(matcher.find(&bind("n + 2").bound, ExprType::Scalar), None);
        assert!(matcher
            .find(&bind("n + 1").bound, ExprType::Scalar)
            .is_some());
    }

    #[test]
    fn signed_zero_literals_are_not_identical() {
        let matcher = ExpressionIndexMatcher::from_decls(&schema(), &[decl("i_n", 0, "n + 0")]);
        let positive = bind("n + 0").bound;
        assert!(matcher.find(&positive, ExprType::Scalar).is_some());
        // `PartialEq` は `-0.0 == 0.0` だが、符号付きゼロが結果を変えうるため一致させない。
        let BoundExpr::Binary { op, lhs, .. } = positive else {
            panic!("expected a binary expression");
        };
        let negative = BoundExpr::Binary {
            op,
            lhs,
            rhs: Box::new(BoundExpr::Number(-0.0)),
        };
        assert_eq!(matcher.find(&negative, ExprType::Scalar), None);
    }

    #[test]
    fn broken_or_stale_declarations_are_dropped_silently() {
        let mut stale = decl("i_lower", 0, "lower(lang)");
        stale.sql = "lower(no_such_column)".to_string();
        let mut invalid_name = decl("i_ok", 0, "n + 1");
        invalid_name.index_name = "bad name".to_string();
        let mut garbage = decl("i_garbage", 0, "n + 1");
        garbage.sql = "((( not sql".to_string();
        let matcher = ExpressionIndexMatcher::from_decls(
            &schema(),
            &[stale, invalid_name, garbage, decl("i_good", 1, "n + 1")],
        );
        let found = matcher.find(&bind("n + 1").bound, ExprType::Scalar);
        assert_eq!(
            found,
            Some(ExprMatchRef {
                index_name: "i_good".to_string(),
                element: 1
            })
        );
        assert_eq!(
            matcher.find(&bind("lower(lang)").bound, ExprType::Text),
            None
        );
        assert_eq!(
            ExpressionIndexMatcher::empty().find(&bind("n + 1").bound, ExprType::Scalar),
            None
        );
    }

    #[test]
    fn identical_expressions_pick_the_smallest_index_name_deterministically() {
        let matcher = ExpressionIndexMatcher::from_decls(
            &schema(),
            &[decl("i_b", 0, "n + 1"), decl("i_a", 0, "n + 1")],
        );
        assert_eq!(
            matcher
                .find(&bind("n + 1").bound, ExprType::Scalar)
                .map(|m| m.index_name),
            Some("i_a".to_string())
        );
    }

    #[test]
    fn literal_conversion_accepts_only_finite_constants_of_each_kind() {
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Text("a".into())),
            Some(ExprKeyLiteral::Text("a".into()))
        );
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Number(1.5)),
            Some(ExprKeyLiteral::Scalar(1.5))
        );
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Number(f64::NAN)),
            None
        );
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Number(f64::INFINITY)),
            None
        );
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Date(7)),
            Some(ExprKeyLiteral::Temporal(ExprType::Date, 7))
        );
        assert_eq!(
            ExprKeyLiteral::from_bound(&BoundExpr::Timestamp(9)),
            Some(ExprKeyLiteral::Temporal(ExprType::Timestamp, 9))
        );
        assert_eq!(ExprKeyLiteral::from_bound(&BoundExpr::IdRef), None);
        assert_eq!(
            ExprKeyLiteral::Temporal(ExprType::TimestampTz, 1).expr_type(),
            ExprType::TimestampTz
        );
    }

    #[test]
    fn comparison_bounds_map_each_operator() {
        assert_eq!(
            comparison_bounds(BinOp::Eq, 5),
            Some((Bound::Included(5), Bound::Included(5)))
        );
        assert_eq!(
            comparison_bounds(BinOp::Gt, 5),
            Some((Bound::Excluded(5), Bound::Unbounded))
        );
        assert_eq!(
            comparison_bounds(BinOp::Le, 5),
            Some((Bound::Unbounded, Bound::Included(5)))
        );
        assert_eq!(comparison_bounds(BinOp::Add, 5), None);
    }
}
