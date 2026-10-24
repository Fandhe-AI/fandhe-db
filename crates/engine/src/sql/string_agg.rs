//! `string_agg(<値>, <区切り> [ORDER BY ...])` のアキュムレータ状態（SQL-57・Issue #1800）。
//!
//! 責務境界: [`crate::sql::aggregate::Accumulator::StringAgg`] が保持する状態と、行ごとの
//! 観測（[`StringAggState::observe`]）・確定（[`StringAggState::finish`]）だけを担う。
//! 構文・束縛（引数の型検査・`ORDER BY` キーの解決・`DISTINCT` の `42P10`）は
//! [`crate::sql::parser::resolve_string_agg_input`] の責務で、本モジュールは束縛済みの
//! [`StringAggInput`] を評価するのみ。呼び出し元は `sql::aggregate`（単一行集計・索引経路）と
//! `sql::group_by`（`GROUP BY` の各グループ）で、どちらも可視行（RLS 適用済み）だけを渡す。
//!
//! 意味論（PostgreSQL 17 と同じ）:
//! - 値が NULL の行は連結から除く（その行の区切りも使わない）。全行が NULL・入力なしなら NULL
//! - 2 つめ以降の値の前には**その行の**区切りを置く（区切りが NULL の行は何も置かない）
//! - 内側の `ORDER BY` が無ければ入力順（行の処理順）。同値キーは入力順を保つ（安定ソート）
//! - `DISTINCT` は引数の組（値, 区切り）全体で重複を除き、未指定の引数を昇順キーとして並べる
//!
//! 資源上限（SQL-57 (5)。確保の前に検査し、超過は `54000`。部分的な結果は返さない）:
//! - 結果の長さ [`MAX_STRING_AGG_RESULT_BYTES`]
//! - 溜めた状態の大きさ [`MAX_STRING_AGG_STATE_BYTES`]（1 件あたりの固定見積りを含む）
//!
//! 上限判定はテナントの可視行だけで決まり、他テナントの行の有無に依存しない。

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::row_codec;
use crate::sql::aggregate::{accumulator_bug, try_clone_str, RowVector};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::expr_program::StackValue;
use crate::sql::order_value::{
    compare_order_key, expr_value_to_order_value, extract_column_order_value_ref,
    scalar_key_ref_to_owned, NullPlacement, OrderValue,
};
use crate::sql::parser::{BoundTextArg, StringAggInput, StringAggKeySource, StringAggOrderKey};
use crate::sql::udf_call::{self, ExprValue};

/// 連結後の結果が取れる最大バイト数（`TEXT` 1 値の上限と同じ。`string_fn` の結果上限と同根拠）。
pub(crate) const MAX_STRING_AGG_RESULT_BYTES: usize = row_codec::MAX_TEXT_FIELD_LEN as usize;

/// 1 アキュムレータが溜めてよい状態（値・区切り・キー・固定見積り）の合計バイト数。
/// `ORDER BY`／`DISTINCT` のときだけ溜める（ストリーミング連結は結果長の上限で足りる）。
pub(crate) const MAX_STRING_AGG_STATE_BYTES: usize = 16 * 1024 * 1024;

/// 文全体（単一行集計の全項目の合計）で `string_agg` が保持してよい状態の上限。
/// `GROUP BY` 側の `MAX_TEXT_ACCUMULATOR_TOTAL_BYTES` と同じ規模。
pub(crate) const MAX_STRING_AGG_STATEMENT_BYTES: usize = 16 * 1024 * 1024;

/// 溜めた 1 件あたりの固定領域（`Entry` 自体＝`String`/`Option<String>`/`Vec` のヘッダと、
/// `Vec<Entry>` 内の 1 要素分。空文字列・NULL キーを大量に溜める DoS を状態の大きさで有界にする）。
const ENTRY_FIXED_BYTES: usize = std::mem::size_of::<Entry>();

/// 結果長の超過（固定の英語文言。値・列名・他テナントの情報を含めない）。
const RESULT_TOO_LARGE_DETAIL: &str = "string_agg result exceeds the allowed size";
/// 溜めた状態の超過。
const STATE_TOO_LARGE_DETAIL: &str = "string_agg accumulated state exceeds the allowed size";

/// `ORDER BY` 付きで溜める 1 行分。
#[derive(Clone)]
struct Entry {
    value: String,
    /// その行の区切り（NULL なら `None`）。
    delimiter: Option<String>,
    /// 内側 `ORDER BY` の各キーの値（NULL は `None`）。
    keys: Vec<Option<OrderValue>>,
}

/// `string_agg` 1 項目（`GROUP BY` ならグループごと）の状態。`Clone` は
/// [`crate::sql::aggregate::Accumulator`] の派生に必要（ウィンドウ経路向けの複製。string_agg は
/// ウィンドウ非対応のため実際には複製されない）。
#[derive(Clone)]
pub(crate) struct StringAggState {
    /// `ORDER BY`・`DISTINCT` のいずれかで溜めてから並べるか（`false` は入力順の逐次連結）。
    collect: bool,
    distinct: bool,
    /// 各キーの（降順, NULL 位置）。`finish` の安定ソートが使う。
    specs: Vec<(bool, NullPlacement)>,
    // 逐次連結（`collect == false`）。
    buf: String,
    has_value: bool,
    // 溜める形（`collect == true`）。
    entries: Vec<Entry>,
    entries_bytes: usize,
    /// 値の長さの合計（結果長の下限。早期拒否に使う）。
    value_bytes: usize,
    /// `DISTINCT` の重複判定キー（値, 区切り の正準バイト列）と、そのキーの累計バイト数。
    seen: HashSet<Vec<u8>>,
    seen_bytes: usize,
}

impl StringAggState {
    /// 束縛済み入力から空の状態を作る。キーが 1 つでもあれば（`DISTINCT` は常にキーを持つ）
    /// 溜める形、無ければ逐次連結。
    pub(crate) fn new(input: &StringAggInput) -> Self {
        Self {
            collect: !input.keys.is_empty(),
            distinct: input.distinct,
            specs: input
                .keys
                .iter()
                .map(|k| (k.key.descending, k.key.nulls))
                .collect(),
            buf: String::new(),
            has_value: false,
            entries: Vec::new(),
            entries_bytes: 0,
            value_bytes: 0,
            seen: HashSet::new(),
            seen_bytes: 0,
        }
    }

    /// 呼び出し元が before/after の差分で予算（`GROUP BY` の 16 MiB・単一行の文全体予算）へ
    /// 計上する、保持中の状態のバイト数。
    pub(crate) fn text_len(&self) -> usize {
        if self.collect {
            self.entries_bytes
        } else {
            self.buf.len()
        }
    }

    /// `DISTINCT` の重複判定キーの（件数, 累計バイト数）。`COUNT(DISTINCT)` と同じ形で
    /// [`crate::sql::distinct::DistinctBudget`]（文全体で 100,000 件・16 MiB。SQL-25 (c)）へ計上される。
    pub(crate) fn distinct_footprint(&self) -> (usize, usize) {
        (self.seen.len(), self.seen_bytes)
    }

    /// 可視行 1 件を観測する。値・区切り・`ORDER BY` の式は PostgreSQL と同じく行ごとに評価し、
    /// 評価エラー（`22012` 等）は文全体のエラーとして伝播する（値が NULL の行でも評価する）。
    pub(crate) fn observe(
        &mut self,
        input: &StringAggInput,
        id: u64,
        vector: &RowVector<'_>,
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
    ) -> Result<(), SqlSurfaceError> {
        let embedding = embedding_for(input, vector)?;
        let value = eval_text(&input.value, id, embedding, scanned, scratch)?;
        let delimiter = eval_text(&input.delimiter, id, embedding, scanned, scratch)?;
        // ORDER BY のキーも値が NULL の行で評価し、エラーを PostgreSQL と同じく伝播させる。
        let keys = if self.collect {
            self.eval_keys(input, id, embedding, scanned, scratch, &value, &delimiter)?
        } else {
            Vec::new()
        };
        let Some(value) = value else {
            return Ok(());
        };
        let delimiter = delimiter.as_deref();
        if value.len() > MAX_STRING_AGG_RESULT_BYTES {
            return Err(SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL));
        }
        if self.collect {
            self.push_entry(&value, delimiter, keys)
        } else {
            self.append_streaming(&value, delimiter)
        }
    }

    /// 逐次連結（`ORDER BY`・`DISTINCT` なし）。最初の値の区切りは使わず、2 つめ以降は
    /// その行の区切り（NULL なら何も置かない）を値の前に置く。結果長の上限は確保の前に検査する。
    fn append_streaming(
        &mut self,
        value: &str,
        delimiter: Option<&str>,
    ) -> Result<(), SqlSurfaceError> {
        let delim_len = if self.has_value {
            delimiter.map_or(0, str::len)
        } else {
            0
        };
        let add = delim_len
            .checked_add(value.len())
            .ok_or_else(|| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
        let total = self
            .buf
            .len()
            .checked_add(add)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
        if total > MAX_STRING_AGG_RESULT_BYTES {
            return Err(SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL));
        }
        self.buf
            .try_reserve(add)
            .map_err(|_| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
        if self.has_value {
            if let Some(d) = delimiter {
                self.buf.push_str(d);
            }
        }
        self.buf.push_str(value);
        self.has_value = true;
        Ok(())
    }

    /// 溜める形で 1 行を追加する。`DISTINCT` は（値, 区切り）が既出なら何もしない。上限
    /// （結果長の下限・状態の大きさ）は確保の前に検査する。
    fn push_entry(
        &mut self,
        value: &str,
        delimiter: Option<&str>,
        keys: Vec<Option<OrderValue>>,
    ) -> Result<(), SqlSurfaceError> {
        // `DISTINCT` の既出（値, 区切り）は捨てられる行なので、状態・結果長の予算を
        // 消費させる前に判定して戻る（重複行で誤って 54000 にしない）。
        let canon = if self.distinct {
            let canon = canonical_pair(value, delimiter)?;
            if self.seen.contains(&canon) {
                return Ok(());
            }
            Some(canon)
        } else {
            None
        };
        // キー配列の固定領域（NULL キーを含む全スロット分）＋各キーのヒープ内容。
        let mut key_bytes = keys
            .len()
            .saturating_mul(std::mem::size_of::<Option<OrderValue>>());
        for k in keys.iter().flatten() {
            key_bytes = key_bytes.saturating_add(order_value_heap_bytes(k));
        }
        let entry_bytes = ENTRY_FIXED_BYTES
            .checked_add(value.len())
            .and_then(|n| n.checked_add(delimiter.map_or(0, str::len)))
            .and_then(|n| n.checked_add(key_bytes))
            .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        let next_state = self
            .entries_bytes
            .checked_add(entry_bytes)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        if next_state > MAX_STRING_AGG_STATE_BYTES {
            return Err(SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL));
        }
        let next_values = self
            .value_bytes
            .checked_add(value.len())
            .ok_or_else(|| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
        if next_values > MAX_STRING_AGG_RESULT_BYTES {
            return Err(SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL));
        }
        if let Some(canon) = canon {
            self.seen_bytes = self
                .seen_bytes
                .checked_add(canon.len())
                .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
            self.seen.insert(canon);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        let value = try_clone_str(value)?;
        let delimiter = match delimiter {
            Some(d) => Some(try_clone_str(d)?),
            None => None,
        };
        self.entries.push(Entry {
            value,
            delimiter,
            keys,
        });
        self.entries_bytes = next_state;
        self.value_bytes = next_values;
        Ok(())
    }

    /// `ORDER BY` の各キーの値を評価する（`DISTINCT` の合成キー・値／区切りの再利用を含む）。
    #[allow(clippy::too_many_arguments)]
    fn eval_keys(
        &self,
        input: &StringAggInput,
        id: u64,
        embedding: &[f32],
        scanned: &[Option<row_codec::ScalarRef<'_>>],
        scratch: &mut Vec<StackValue>,
        value: &Option<String>,
        delimiter: &Option<String>,
    ) -> Result<Vec<Option<OrderValue>>, SqlSurfaceError> {
        let mut out = Vec::new();
        out.try_reserve_exact(input.keys.len())
            .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
        for k in &input.keys {
            out.push(eval_key(
                k, id, embedding, scanned, scratch, value, delimiter,
            )?);
        }
        Ok(out)
    }

    /// 確定する。空集合・全行 NULL は `None`（呼び出し元が `Cell::Null` にする）。溜めた形は
    /// キーの安定ソート後に連結し、結果長は `checked_add`・`try_reserve` で上限検査する。
    pub(crate) fn finish(mut self) -> Result<Option<String>, SqlSurfaceError> {
        if !self.collect {
            return Ok(if self.has_value { Some(self.buf) } else { None });
        }
        if self.entries.is_empty() {
            return Ok(None);
        }
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
        let mut total = 0usize;
        for (i, e) in self.entries.iter().enumerate() {
            let delim = if i == 0 {
                0
            } else {
                e.delimiter.as_deref().map_or(0, str::len)
            };
            total = total
                .checked_add(delim)
                .and_then(|n| n.checked_add(e.value.len()))
                .ok_or_else(|| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
            if total > MAX_STRING_AGG_RESULT_BYTES {
                return Err(SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL));
            }
        }
        let mut out = String::new();
        out.try_reserve_exact(total)
            .map_err(|_| SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL))?;
        for (i, e) in self.entries.iter().enumerate() {
            if i > 0 {
                if let Some(d) = &e.delimiter {
                    out.push_str(d);
                }
            }
            out.push_str(&e.value);
        }
        Ok(Some(out))
    }
}

/// 式のどれかが embedding を参照するときだけ `vector.values` を要求する
/// （未デコードなら `ReferencedColumns` の導出漏れという実装バグとして fail-closed）。
fn embedding_for<'a>(
    input: &StringAggInput,
    vector: &RowVector<'a>,
) -> Result<&'a [f32], SqlSurfaceError> {
    let needs = udf_call::references_embedding(&input.value.source)
        || udf_call::references_embedding(&input.delimiter.source)
        || input.keys.iter().any(|k| match &k.source {
            StringAggKeySource::Expr { source, .. } => udf_call::references_embedding(source),
            _ => false,
        });
    if !needs {
        return Ok(&[]);
    }
    vector.values.ok_or_else(|| {
        accumulator_bug(
            "string_agg references the VECTOR column but the row's embedding was not decoded",
        )
    })
}

/// text 引数 1 つを評価して所有文字列にする（NULL は `None`）。束縛が text 以外を拒否済みの
/// ため、他の型の値は実装バグとして `XX000`。
fn eval_text(
    arg: &BoundTextArg,
    id: u64,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
) -> Result<Option<String>, SqlSurfaceError> {
    match arg.program.eval(id, embedding, scanned, scratch)? {
        ExprValue::Text(t) => {
            if t.len() > MAX_STRING_AGG_RESULT_BYTES {
                return Err(SqlSurfaceError::payload_too_large(RESULT_TOO_LARGE_DETAIL));
            }
            Ok(Some(try_clone_str(&t)?))
        }
        ExprValue::Null => Ok(None),
        _ => Err(accumulator_bug(
            "text-typed string_agg argument evaluated to a non-text value",
        )),
    }
}

/// `ORDER BY` キー 1 つの値を評価する（NULL は `None`）。
fn eval_key(
    k: &StringAggOrderKey,
    id: u64,
    embedding: &[f32],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
    scratch: &mut Vec<StackValue>,
    value: &Option<String>,
    delimiter: &Option<String>,
) -> Result<Option<OrderValue>, SqlSurfaceError> {
    let text_key = |text: &Option<String>| -> Result<Option<OrderValue>, SqlSurfaceError> {
        match text {
            Some(t) => expr_value_to_order_value(
                &ExprValue::Text(std::borrow::Cow::Borrowed(t.as_str())),
                k.key.kind,
                k.key.collation,
            ),
            None => Ok(None),
        }
    };
    match &k.source {
        StringAggKeySource::Value => text_key(value),
        StringAggKeySource::Delimiter => text_key(delimiter),
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
    }
}

/// `DISTINCT` の重複判定キー（値, 区切り）の正準バイト列。NULL と空文字列を区別し、長さ接頭辞で
/// 境界の曖昧さをなくす（`("a","bc")` と `("ab","c")` を同一視しない）。
fn canonical_pair(value: &str, delimiter: Option<&str>) -> Result<Vec<u8>, SqlSurfaceError> {
    let mut key = Vec::new();
    let cap = value
        .len()
        .checked_add(delimiter.map_or(0, str::len))
        .and_then(|n| n.checked_add(10))
        .ok_or_else(|| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    key.try_reserve_exact(cap)
        .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    let vlen = u32::try_from(value.len())
        .map_err(|_| SqlSurfaceError::payload_too_large(STATE_TOO_LARGE_DETAIL))?;
    key.push(1);
    key.extend_from_slice(&vlen.to_be_bytes());
    key.extend_from_slice(value.as_bytes());
    match delimiter {
        Some(d) => {
            key.push(1);
            key.extend_from_slice(d.as_bytes());
        }
        None => key.push(0),
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

    /// 空文字列の値と NULL キー複数でも、1 件ごとに `Entry` 自体とキー配列の固定領域が
    /// 状態の予算へ計上され、`MAX_STRING_AGG_STATE_BYTES` で `54000` になる（SQL-57 (5)）。
    #[test]
    fn empty_values_with_null_keys_are_bounded_by_fixed_footprint() {
        let mut st = StringAggState {
            collect: true,
            distinct: false,
            specs: Vec::new(),
            buf: String::new(),
            has_value: false,
            entries: Vec::new(),
            entries_bytes: 0,
            value_bytes: 0,
            seen: HashSet::new(),
            seen_bytes: 0,
        };
        let per_entry = ENTRY_FIXED_BYTES + 4 * std::mem::size_of::<Option<OrderValue>>();
        let mut pushed = 0usize;
        let err = loop {
            match st.push_entry("", None, vec![None, None, None, None]) {
                Ok(()) => pushed += 1,
                Err(e) => break e,
            }
        };
        assert_eq!(err.wire_code(), "54000");
        assert_eq!(pushed, MAX_STRING_AGG_STATE_BYTES / per_entry);
        assert!(st.text_len() <= MAX_STRING_AGG_STATE_BYTES);
    }
}
