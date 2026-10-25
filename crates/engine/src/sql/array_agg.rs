//! `array_agg([DISTINCT] <式> [ORDER BY ...])` のアキュムレータ状態（SQL-57・Issue #1801）。
//!
//! 責務境界: [`crate::sql::aggregate::Accumulator::ArrayAgg`] が保持する状態と、行ごとの
//! 観測（[`ArrayAggState::observe`]）・確定（[`ArrayAggState::finish`]）だけを担う。
//! 構文・束縛（要素型の解決・`ORDER BY` キーの解決・`DISTINCT` の `42P10`）は
//! [`crate::sql::parser::resolve_array_agg_input`] の責務で、本モジュールは束縛済みの
//! [`ArrayAggInput`] を評価するのみ。呼び出し元は `sql::aggregate`（単一行集計・索引経路）と
//! `sql::group_by`（`GROUP BY` の各グループ）で、どちらも可視行（RLS 適用済み）だけを渡す。
//! 兄弟の [`crate::sql::string_agg`] と同じ構成（溜めてから安定ソート）で、結果は
//! [`crate::row_codec::ArrayValue`]（1 次元）になる。
//!
//! 意味論（PostgreSQL 17 と同じ）:
//! - NULL の入力は NULL 要素として配列に含める。入力行が無ければ結果は NULL（空配列ではない）
//! - 内側の `ORDER BY` が無ければ入力順（行の処理順）。同値キーは入力順を保つ（安定ソート）
//! - `DISTINCT` は要素値の同値で重複を除く（NULL は 1 個だけ残る）。`ORDER BY` が無ければ
//!   束縛段が合成した引数の昇順キーで並ぶ
//!
//! 資源上限（SQL-57。確保の前に検査し、超過は `54000`。部分的な結果は返さない）:
//! - 要素数 [`MAX_ARRAY_AGG_ELEMENTS`]（`DISTINCT` では重複を除いた後の件数）
//! - 可変長要素の本文の累計 [`MAX_ARRAY_AGG_PAYLOAD_BYTES`]
//! - 溜めた状態の大きさ [`MAX_ARRAY_AGG_STATE_BYTES`]（1 件あたりの固定見積りを含む）
//!
//! 上限判定はテナントの可視行だけで決まり、他テナントの行の有無に依存しない。

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::catalog::{ArrayElemType, MAX_ARRAY_ELEMENTS};
use crate::numeric::Decimal;
use crate::row_codec::{self, ArrayValue};
use crate::sql::aggregate::{accumulator_bug, try_clone_str, RowVector};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::distinct::canon_f64;
use crate::sql::expr_program::StackValue;
use crate::sql::order_value::{
    compare_order_key, expr_value_to_order_value, extract_column_order_value_ref,
    scalar_key_ref_to_owned, NullPlacement, OrderValue,
};
use crate::sql::parser::{ArrayAggInput, ArrayAggValue, StringAggKeySource, StringAggOrderKey};
use crate::sql::udf_call::{self, ExprValue};
use crate::uuid::Uuid;

/// 結果配列の要素数の上限（配列コンストラクタ・配列列と同じ [`MAX_ARRAY_ELEMENTS`]。起動時設定
/// `--max-array-elements` を式の結果へ通すのは #1606 で対象外のため定数を使う）。
pub(crate) const MAX_ARRAY_AGG_ELEMENTS: usize = MAX_ARRAY_ELEMENTS as usize;

/// 可変長要素（text・bytea・json・jsonb）の本文の累計上限（配列コンストラクタ・配列 codec と同じ）。
pub(crate) const MAX_ARRAY_AGG_PAYLOAD_BYTES: usize = row_codec::MAX_SCALAR_PAYLOAD_LEN as usize;

/// 1 アキュムレータが溜めてよい状態（要素・キー・固定見積り）の合計バイト数。
pub(crate) const MAX_ARRAY_AGG_STATE_BYTES: usize = 16 * 1024 * 1024;

/// 文全体（単一行集計の全項目の合計）で `array_agg` が保持してよい状態の上限。
/// `GROUP BY` 側の `MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` と同じ規模。
pub(crate) const MAX_ARRAY_AGG_STATEMENT_BYTES: usize = 16 * 1024 * 1024;

/// 溜めた 1 件あたりの固定領域（`Entry` 自体と `Vec<Entry>` 内の 1 要素分。NULL 要素・空値を
/// 大量に溜める DoS を状態の大きさで有界にする）。
const ENTRY_FIXED_BYTES: usize = std::mem::size_of::<Entry>();

/// 要素数の超過（固定の英語文言。値・列名・他テナントの情報を含めない）。
const TOO_MANY_ELEMENTS_DETAIL: &str = "array_agg result exceeds the allowed element count";
/// 本文の累計の超過。
const PAYLOAD_TOO_LARGE_DETAIL: &str = "array_agg result exceeds the allowed size";
/// 溜めた状態の超過。
const STATE_TOO_LARGE_DETAIL: &str = "array_agg accumulated state exceeds the allowed size";

/// 所有された要素値 1 個（[`ArrayValue`] の要素型 variant と 1 対 1）。
#[derive(Clone)]
enum Elem {
    Text(String),
    Bool(bool),
    Integer(i32),
    BigInt(i64),
    Real(f32),
    Double(f64),
    Date(i32),
    Timestamp(i64),
    Uuid(Uuid),
    Numeric(Decimal),
    Bytea(Vec<u8>),
    Json(String),
    Jsonb(String),
}

impl Elem {
    /// 可変長本文のバイト数（固定長の型は 0）。
    fn payload_len(&self) -> usize {
        match self {
            Elem::Text(s) | Elem::Json(s) | Elem::Jsonb(s) => s.len(),
            Elem::Bytea(b) => b.len(),
            _ => 0,
        }
    }

    /// `DISTINCT` の重複判定キー（同値の要素は同じバイト列になる正準形）。
    /// 型は列で固定のため型タグは持たず、浮動小数は `-0.0`・NaN を正規化する。
    ///
    /// `jsonb` は格納表記ではなく値等価の正規形（[`crate::json::canonical_equality_text`]。
    /// UNIQUE 制約と共通）で比べる（`1`・`1.0`・`1e0` は同値）。`numeric` は末尾の 0 を
    /// 落として scale 違いの同値（`1` と `1.0`）を同じキーにする。出力要素は元の表記のまま。
    fn canonical(&self, out: &mut Vec<u8>) -> Result<(), SqlSurfaceError> {
        match self {
            Elem::Jsonb(s) => {
                let canon = crate::json::canonical_equality_text(s).map_err(|_| {
                    accumulator_bug("array_agg observed an invalid stored jsonb value")
                })?;
                out.try_reserve(canon.len())
                    .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
                out.extend_from_slice(canon.as_bytes());
            }
            Elem::Text(s) | Elem::Json(s) => out.extend_from_slice(s.as_bytes()),
            Elem::Bytea(b) => out.extend_from_slice(b),
            Elem::Bool(b) => out.push(u8::from(*b)),
            Elem::Integer(v) | Elem::Date(v) => out.extend_from_slice(&v.to_be_bytes()),
            Elem::BigInt(v) | Elem::Timestamp(v) => out.extend_from_slice(&v.to_be_bytes()),
            Elem::Real(v) => out.extend_from_slice(&canon_f64(f64::from(*v))),
            Elem::Double(v) => out.extend_from_slice(&canon_f64(*v)),
            Elem::Uuid(u) => out.extend_from_slice(u.as_bytes()),
            Elem::Numeric(d) => {
                let (mut unscaled, mut scale) = (d.unscaled(), d.scale());
                while scale > 0 && unscaled % 10 == 0 {
                    unscaled /= 10;
                    scale -= 1;
                }
                out.push(scale);
                out.extend_from_slice(&unscaled.to_be_bytes());
            }
        }
        Ok(())
    }
}

/// 溜める 1 行分。
#[derive(Clone)]
struct Entry {
    /// 要素（NULL は `None`）。
    elem: Option<Elem>,
    /// 内側 `ORDER BY` の各キーの値（NULL は `None`）。
    keys: Vec<Option<OrderValue>>,
}

/// `array_agg` 1 項目（`GROUP BY` ならグループごと）の状態。`Clone` は
/// [`crate::sql::aggregate::Accumulator`] の派生に必要（ウィンドウ非対応のため実際には複製されない）。
#[derive(Clone)]
pub(crate) struct ArrayAggState {
    elem_ty: ArrayElemType,
    /// `ORDER BY`・`DISTINCT` のいずれかでキーを評価して並べ替えるか（`false` は入力順）。
    collect: bool,
    distinct: bool,
    /// 各キーの（降順, NULL 位置）。`finish` の安定ソートが使う。
    specs: Vec<(bool, NullPlacement)>,
    entries: Vec<Entry>,
    entries_bytes: usize,
    /// 可変長要素の本文の累計。
    payload_bytes: usize,
    /// `DISTINCT` の重複判定キーと、その累計バイト数。
    seen: HashSet<Vec<u8>>,
    seen_bytes: usize,
}

impl ArrayAggState {
    /// 束縛済み入力から空の状態を作る。
    pub(crate) fn new(input: &ArrayAggInput) -> Self {
        Self {
            elem_ty: input.elem,
            collect: !input.keys.is_empty(),
            distinct: input.distinct,
            specs: input
                .keys
                .iter()
                .map(|k| (k.key.descending, k.key.nulls))
                .collect(),
            entries: Vec::new(),
            entries_bytes: 0,
            payload_bytes: 0,
            seen: HashSet::new(),
            seen_bytes: 0,
        }
    }

    /// 呼び出し元が before/after の差分で予算（`GROUP BY` の 16 MiB・単一行の文全体予算）へ
    /// 計上する、保持中の状態のバイト数。
    pub(crate) fn text_len(&self) -> usize {
        self.entries_bytes
    }

    /// `DISTINCT` の重複判定キーの（件数, 累計バイト数）。`COUNT(DISTINCT)` と同じ形で
    /// [`crate::sql::distinct::DistinctBudget`]（文全体の上限。SQL-25 (c)）へ計上される。
    pub(crate) fn distinct_footprint(&self) -> (usize, usize) {
        (self.seen.len(), self.seen_bytes)
    }

    /// 可視行 1 件を観測する。値・`ORDER BY` の式は行ごとに評価し、評価エラー（`22012` 等）は
    /// 文全体のエラーとして伝播する（値が NULL の行でも評価する）。
    pub(crate) fn observe(
        &mut self,
        input: &ArrayAggInput,
        id: u64,
        vector: &RowVector<'_>,
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
    ) -> Result<(), SqlSurfaceError> {
        let embedding = embedding_for(input, vector)?;
        let elem = eval_elem(input, id, embedding, scanned, scratch)?;
        let keys = if self.collect {
            self.eval_keys(input, id, embedding, scanned, scratch)?
        } else {
            Vec::new()
        };
        self.push_entry(elem, keys)
    }

    /// `ORDER BY` の各キーの値を評価する（`DISTINCT` の合成キーを含む）。
    fn eval_keys(
        &self,
        input: &ArrayAggInput,
        id: u64,
        embedding: &[f32],
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
    ) -> Result<Vec<Option<OrderValue>>, SqlSurfaceError> {
        let mut out = Vec::new();
        out.try_reserve_exact(input.keys.len())
            .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        for k in &input.keys {
            out.push(eval_key(k, id, embedding, scanned, scratch)?);
        }
        Ok(out)
    }

    /// 1 行を追加する。`DISTINCT` は既出の要素なら何もしない（状態・件数の予算を消費する前に
    /// 捨てるため、重複行で誤って `54000` にならない）。上限は確保の前に検査する。
    fn push_entry(
        &mut self,
        elem: Option<Elem>,
        keys: Vec<Option<OrderValue>>,
    ) -> Result<(), SqlSurfaceError> {
        let payload = elem.as_ref().map_or(0, Elem::payload_len);
        if payload > MAX_ARRAY_AGG_PAYLOAD_BYTES {
            return Err(SqlSurfaceError::payload_too_large(PAYLOAD_TOO_LARGE_DETAIL));
        }
        let canon = if self.distinct {
            let canon = canonical_key(elem.as_ref(), payload)?;
            if self.seen.contains(&canon) {
                return Ok(());
            }
            Some(canon)
        } else {
            None
        };
        if self.entries.len() >= MAX_ARRAY_AGG_ELEMENTS {
            return Err(SqlSurfaceError::payload_too_large(TOO_MANY_ELEMENTS_DETAIL));
        }
        let next_payload = self
            .payload_bytes
            .checked_add(payload)
            .filter(|n| *n <= MAX_ARRAY_AGG_PAYLOAD_BYTES)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(PAYLOAD_TOO_LARGE_DETAIL))?;
        // キー配列の固定領域（NULL キーを含む全スロット分）＋各キーのヒープ内容。
        let mut key_bytes = keys
            .len()
            .saturating_mul(std::mem::size_of::<Option<OrderValue>>());
        for k in keys.iter().flatten() {
            key_bytes = key_bytes.saturating_add(order_value_heap_bytes(k));
        }
        let entry_bytes = ENTRY_FIXED_BYTES
            .checked_add(payload)
            .and_then(|n| n.checked_add(key_bytes))
            .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        let next_state = self
            .entries_bytes
            .checked_add(entry_bytes)
            .filter(|n| *n <= MAX_ARRAY_AGG_STATE_BYTES)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        if let Some(canon) = &canon {
            self.seen_bytes = self
                .seen_bytes
                .checked_add(canon.len())
                .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        if let Some(canon) = canon {
            self.seen.insert(canon);
        }
        self.entries.push(Entry { elem, keys });
        self.entries_bytes = next_state;
        self.payload_bytes = next_payload;
        Ok(())
    }

    /// 確定する。空集合は `None`（呼び出し元が `Cell::Null` にする）。キーがあれば安定ソートして
    /// から要素型の配列へ詰める。
    pub(crate) fn finish(mut self) -> Result<Option<ArrayValue>, SqlSurfaceError> {
        if self.entries.is_empty() {
            return Ok(None);
        }
        if self.collect {
            let specs = std::mem::take(&mut self.specs);
            // 安定ソート（同値キーは入力順を保つ。`sort_unstable*` は決定性検査が禁止している）。
            self.entries.sort_by(|a, b| {
                for (idx, (descending, nulls)) in specs.iter().enumerate() {
                    let ord = compare_order_key(
                        a.keys.get(idx).and_then(Option::as_ref),
                        b.keys.get(idx).and_then(Option::as_ref),
                        *descending,
                        *nulls,
                    );
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        }
        let elems: Vec<Option<Elem>> = self.entries.into_iter().map(|e| e.elem).collect();
        build_array(self.elem_ty, elems).map(Some)
    }
}

/// 要素型に対応する [`ArrayValue`] へ詰める。要素型と食い違う値は束縛の不変条件違反として
/// `XX000`（fail-closed）。
fn build_array(ty: ArrayElemType, elems: Vec<Option<Elem>>) -> Result<ArrayValue, SqlSurfaceError> {
    fn collect<T>(
        elems: Vec<Option<Elem>>,
        f: impl Fn(Elem) -> Option<T>,
    ) -> Result<Vec<Option<T>>, SqlSurfaceError> {
        let mut out = Vec::new();
        out.try_reserve_exact(elems.len())
            .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        for e in elems {
            match e {
                None => out.push(None),
                Some(e) => match f(e) {
                    Some(v) => out.push(Some(v)),
                    None => {
                        return Err(accumulator_bug(
                            "array_agg element does not match the declared element type",
                        ))
                    }
                },
            }
        }
        Ok(out)
    }
    Ok(match ty {
        ArrayElemType::Text => ArrayValue::Text(collect(elems, |e| match e {
            Elem::Text(s) => Some(s),
            _ => None,
        })?),
        ArrayElemType::Bool => ArrayValue::Bool(collect(elems, |e| match e {
            Elem::Bool(b) => Some(b),
            _ => None,
        })?),
        ArrayElemType::Integer => ArrayValue::Integer(collect(elems, |e| match e {
            Elem::Integer(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::BigInt => ArrayValue::BigInt(collect(elems, |e| match e {
            Elem::BigInt(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Real => ArrayValue::Real(collect(elems, |e| match e {
            Elem::Real(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Double => ArrayValue::Double(collect(elems, |e| match e {
            Elem::Double(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Date => ArrayValue::Date(collect(elems, |e| match e {
            Elem::Date(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Timestamp => ArrayValue::Timestamp(collect(elems, |e| match e {
            Elem::Timestamp(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Uuid => ArrayValue::Uuid(collect(elems, |e| match e {
            Elem::Uuid(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Numeric { .. } => ArrayValue::Numeric(collect(elems, |e| match e {
            Elem::Numeric(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Bytea => ArrayValue::Bytea(collect(elems, |e| match e {
            Elem::Bytea(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Json => ArrayValue::Json(collect(elems, |e| match e {
            Elem::Json(v) => Some(v),
            _ => None,
        })?),
        ArrayElemType::Jsonb => ArrayValue::Jsonb(collect(elems, |e| match e {
            Elem::Jsonb(v) => Some(v),
            _ => None,
        })?),
        // 束縛段は ENUM 要素を `0A000` で拒否する。到達したら束縛層の実装バグ。
        ArrayElemType::Enum => {
            return Err(accumulator_bug(
                "array_agg over ENUM elements reached finish",
            ))
        }
    })
}

/// 式のどれかが embedding を参照するときだけ `vector.values` を要求する
/// （未デコードなら `ReferencedColumns` の導出漏れという実装バグとして fail-closed）。
fn embedding_for<'a>(
    input: &ArrayAggInput,
    vector: &RowVector<'a>,
) -> Result<&'a [f32], SqlSurfaceError> {
    let value_needs = match &input.value {
        ArrayAggValue::Expr { source, .. } => udf_call::references_embedding(source),
        ArrayAggValue::Column { .. } => false,
    };
    let needs = value_needs
        || input.keys.iter().any(|k| match &k.source {
            StringAggKeySource::Expr { source, .. } => udf_call::references_embedding(source),
            _ => false,
        });
    if !needs {
        return Ok(&[]);
    }
    vector.values.ok_or_else(|| {
        accumulator_bug(
            "array_agg references the VECTOR column but the row's embedding was not decoded",
        )
    })
}

/// 要素 1 個を評価して所有値にする（NULL は `None`）。裸の列は `ScalarRef` からネイティブに
/// 複製し（`bigint` を f64 へ落とさない）、式は評価結果を要素型へ写す。
fn eval_elem(
    input: &ArrayAggInput,
    id: u64,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
) -> Result<Option<Elem>, SqlSurfaceError> {
    match &input.value {
        ArrayAggValue::Column { index } => match scanned.get(*index) {
            Some(Some(v)) => elem_from_scalar(input.elem, v).map(Some),
            Some(None) | None => Ok(None),
        },
        ArrayAggValue::Expr { program, .. } => {
            let v = program.eval(id, embedding, scanned, scratch)?;
            elem_from_expr(input.elem, v)
        }
    }
}

/// 列の借用値を所有要素へ複製する（可変長は `try_reserve_exact` で確保失敗を `54000` にする）。
fn elem_from_scalar(
    ty: ArrayElemType,
    v: &row_codec::ScalarRef<'_>,
) -> Result<Elem, SqlSurfaceError> {
    use row_codec::ScalarRef as S;
    let mismatch = || accumulator_bug("array_agg column value does not match its element type");
    Ok(match (ty, v) {
        (ArrayElemType::Text, S::Text(s)) => Elem::Text(try_clone_str(s)?),
        (ArrayElemType::Bool, S::Bool(b)) => Elem::Bool(*b),
        (ArrayElemType::Integer, S::Integer(x)) => Elem::Integer(*x),
        (ArrayElemType::BigInt, S::BigInt(x)) => Elem::BigInt(*x),
        (ArrayElemType::Real, S::Real(x)) => Elem::Real(*x),
        (ArrayElemType::Double, S::Double(x)) => Elem::Double(*x),
        (ArrayElemType::Date, S::Date(x)) => Elem::Date(*x),
        (ArrayElemType::Timestamp, S::Timestamp(x)) => Elem::Timestamp(*x),
        (ArrayElemType::Uuid, S::Uuid(u)) => Elem::Uuid(*u),
        (ArrayElemType::Numeric { .. }, S::Numeric(d)) => Elem::Numeric(*d),
        (ArrayElemType::Bytea, S::Bytes(b)) => Elem::Bytea(try_clone_bytes(b)?),
        (ArrayElemType::Json, S::Json(s)) => Elem::Json(try_clone_str(s)?),
        (ArrayElemType::Jsonb, S::Json(s)) => Elem::Jsonb(try_clone_str(s)?),
        _ => return Err(mismatch()),
    })
}

/// 式の評価値を要素型の所有要素へ写す（`values_to_array` と同じ対応）。NULL は `None`。
fn elem_from_expr(ty: ArrayElemType, v: ExprValue<'_>) -> Result<Option<Elem>, SqlSurfaceError> {
    let mismatch = || accumulator_bug("array_agg expression value does not match its element type");
    let int_of = |x: f64| -> Result<i64, SqlSurfaceError> {
        if x.is_finite() && x.fract() == 0.0 {
            Ok(x as i64)
        } else {
            Err(mismatch())
        }
    };
    if matches!(v, ExprValue::Null) {
        return Ok(None);
    }
    Ok(Some(match (ty, v) {
        (ArrayElemType::Text, ExprValue::Text(s)) => Elem::Text(try_clone_str(&s)?),
        (ArrayElemType::Bool, ExprValue::Bool(b)) => Elem::Bool(b),
        (ArrayElemType::Integer, ExprValue::Scalar(x)) => {
            let n = i32::try_from(int_of(x)?)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("integer out of range"))?;
            Elem::Integer(n)
        }
        (ArrayElemType::BigInt, ExprValue::Scalar(x)) => Elem::BigInt(int_of(x)?),
        (ArrayElemType::Real, ExprValue::Scalar(x)) => Elem::Real(x as f32),
        (ArrayElemType::Double, ExprValue::Scalar(x)) => Elem::Double(x),
        (ArrayElemType::Date, ExprValue::Date(d)) => Elem::Date(d),
        (ArrayElemType::Timestamp, ExprValue::Timestamp(t)) => Elem::Timestamp(t),
        (ArrayElemType::Uuid, ExprValue::Uuid(u)) => Elem::Uuid(u),
        (ArrayElemType::Numeric { .. }, ExprValue::Numeric(d)) => Elem::Numeric(d),
        (ArrayElemType::Bytea, ExprValue::Bytes(b)) => Elem::Bytea(try_clone_bytes(&b)?),
        (ArrayElemType::Json, ExprValue::JsonText(s)) => Elem::Json(try_clone_str(&s)?),
        (ArrayElemType::Jsonb, ExprValue::Json(s)) => Elem::Jsonb(try_clone_str(&s)?),
        _ => return Err(mismatch()),
    }))
}

/// バイト列の複製（確保失敗を `54000` にする。`try_clone_str` のバイト列版）。
fn try_clone_bytes(value: &[u8]) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    owned.extend_from_slice(value);
    Ok(owned)
}

/// `ORDER BY` キー 1 つの値を評価する（NULL は `None`）。`array_agg` は値・区切りの再利用キーを
/// 作らない（束縛段が全キーを自由キーとして束縛する）ため、それらが来たら束縛層の実装バグ。
fn eval_key(
    k: &StringAggOrderKey,
    id: u64,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
) -> Result<Option<OrderValue>, SqlSurfaceError> {
    match &k.source {
        StringAggKeySource::Id => Ok(Some(OrderValue::Id(id))),
        StringAggKeySource::Column { index, ty } => {
            match extract_column_order_value_ref(ty, *index, &k.key, scanned)? {
                Some(r) => Ok(Some(scalar_key_ref_to_owned(r)?)),
                None => Ok(None),
            }
        }
        StringAggKeySource::Expr { program, .. } => {
            let v = program.eval(id, embedding, scanned, scratch)?;
            expr_value_to_order_value(&v, k.key.kind, k.key.collation)
        }
        StringAggKeySource::Value | StringAggKeySource::Delimiter => Err(accumulator_bug(
            "array_agg order key refers to a string_agg argument",
        )),
    }
}

/// `DISTINCT` の重複判定キー。先頭 1 バイトで NULL（0）と非 NULL（1）を区別し、続けて要素の
/// 正準形を置く。確保は `try_reserve_exact` で行い失敗は `54000`。
fn canonical_key(elem: Option<&Elem>, payload: usize) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut key = Vec::new();
    // 固定長の型は高々 17 バイト（numeric: scale 1 + i128 16）。jsonb の正規形は元の表記より
    // 長くなりうるため、本文の確保は `Elem::canonical` が正規形の長さで別途検査する。
    let cap = payload
        .checked_add(18)
        .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    key.try_reserve_exact(cap)
        .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    match elem {
        None => key.push(0),
        Some(e) => {
            key.push(1);
            e.canonical(&mut key)?;
        }
    }
    Ok(key)
}

/// 溜めたキー値のヒープ内容のバイト数（固定領域は呼び出し側が `size_of` で計上する）。
fn order_value_heap_bytes(v: &OrderValue) -> usize {
    match v {
        OrderValue::Bytes(b) | OrderValue::JsonbKey(b) => b.len(),
        OrderValue::Collated { text, .. } => text.len(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(elem_ty: ArrayElemType, distinct: bool) -> ArrayAggState {
        ArrayAggState {
            elem_ty,
            collect: false,
            distinct,
            specs: Vec::new(),
            entries: Vec::new(),
            entries_bytes: 0,
            payload_bytes: 0,
            seen: HashSet::new(),
            seen_bytes: 0,
        }
    }

    /// 要素数が上限を超えると確保の前に `54000` になる（SQL-57）。
    #[test]
    fn element_count_is_bounded() {
        let mut st = state(ArrayElemType::Integer, false);
        for i in 0..MAX_ARRAY_AGG_ELEMENTS {
            st.push_entry(Some(Elem::Integer(i as i32)), Vec::new())
                .expect("within limit");
        }
        let err = st
            .push_entry(Some(Elem::Integer(0)), Vec::new())
            .expect_err("must exceed the element limit");
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(st.entries.len(), MAX_ARRAY_AGG_ELEMENTS);
    }

    /// NULL 要素を大量に溜めても 1 件ごとの固定見積りが計上され、状態は有界（要素数の上限が先に効く）。
    #[test]
    fn null_elements_are_bounded() {
        let mut st = state(ArrayElemType::Text, false);
        let err = loop {
            match st.push_entry(None, vec![None, None, None, None]) {
                Ok(()) => {}
                Err(e) => break e,
            }
        };
        assert_eq!(err.wire_code(), "54000");
        assert!(st.text_len() <= MAX_ARRAY_AGG_STATE_BYTES);
        assert_eq!(st.entries.len(), MAX_ARRAY_AGG_ELEMENTS);
    }

    /// 可変長本文の累計が上限を超えると `54000`。
    #[test]
    fn payload_total_is_bounded() {
        let mut st = state(ArrayElemType::Text, false);
        let chunk = "x".repeat(MAX_ARRAY_AGG_PAYLOAD_BYTES / 2 + 1);
        st.push_entry(Some(Elem::Text(chunk.clone())), Vec::new())
            .expect("first chunk");
        let err = st
            .push_entry(Some(Elem::Text(chunk)), Vec::new())
            .expect_err("second chunk exceeds the payload limit");
        assert_eq!(err.wire_code(), "54000");
    }

    /// `DISTINCT` は `-0.0` と `0.0`、NaN どうしを同一視し、NULL は 1 個だけ残す。重複行は
    /// 件数の予算を消費しない。
    #[test]
    fn distinct_canonicalizes_floats_and_nulls() {
        let mut st = state(ArrayElemType::Double, true);
        for v in [
            Some(0.0),
            Some(-0.0),
            Some(f64::NAN),
            Some(f64::from_bits(0x7ff8_0000_0000_0001)),
            None,
            None,
            Some(1.0),
        ] {
            st.push_entry(v.map(Elem::Double), Vec::new())
                .expect("push");
        }
        assert_eq!(st.entries.len(), 4);
        // 同一値 2,000 行でも重複除去後は 1 件。
        let mut dup = state(ArrayElemType::Integer, true);
        for _ in 0..2_000 {
            dup.push_entry(Some(Elem::Integer(7)), Vec::new())
                .expect("duplicates are dropped");
        }
        assert_eq!(dup.entries.len(), 1);
    }

    /// `jsonb` の `DISTINCT` は格納表記でなく値等価で重複を除く（`1`・`1.0`・`1e0` は同値）。
    /// 出力要素は最初の表記のまま。重複だけの大量入力で `54000` にならない。
    #[test]
    fn distinct_jsonb_uses_value_equality() {
        let mut st = state(ArrayElemType::Jsonb, true);
        for v in [
            "1",
            "1.0",
            "1e0",
            "{\"a\":1,\"b\":2}",
            "{\"b\":2.0,\"a\":1}",
        ] {
            st.push_entry(Some(Elem::Jsonb(v.to_string())), Vec::new())
                .expect("push");
        }
        assert_eq!(st.entries.len(), 2);
        let mut dup = state(ArrayElemType::Jsonb, true);
        for i in 0..(MAX_ARRAY_AGG_ELEMENTS + 10) {
            let v = if i % 2 == 0 { "1" } else { "1.00" };
            dup.push_entry(Some(Elem::Jsonb(v.to_string())), Vec::new())
                .expect("duplicates are dropped");
        }
        assert_eq!(dup.entries.len(), 1);
    }

    /// `numeric` の `DISTINCT` は scale 違いの同値（`1` と `1.0`）を同一視する。
    #[test]
    fn distinct_numeric_ignores_scale() {
        let d = |u, sc| Elem::Numeric(Decimal::from_parts(u, sc).expect("decimal"));
        let mut st = state(
            ArrayElemType::Numeric {
                precision: 20,
                scale: 4,
            },
            true,
        );
        for e in [d(1, 0), d(10, 1), d(100, 2), d(0, 0), d(0, 3), d(15, 1)] {
            st.push_entry(Some(e), Vec::new()).expect("push");
        }
        assert_eq!(st.entries.len(), 3);
    }

    /// 空集合は `None`、NULL のみの入力は NULL 要素の配列になる。
    #[test]
    fn finish_distinguishes_empty_from_all_null() {
        let st = state(ArrayElemType::Integer, false);
        assert!(st.finish().expect("finish").is_none());
        let mut st = state(ArrayElemType::Integer, false);
        st.push_entry(None, Vec::new()).expect("push");
        st.push_entry(None, Vec::new()).expect("push");
        assert_eq!(
            st.finish().expect("finish"),
            Some(ArrayValue::Integer(vec![None, None]))
        );
    }
}
