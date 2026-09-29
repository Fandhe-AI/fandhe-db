//! 行スカラー値の「並べ替え・グルーピング用」比較値と比較規約（Issue #915・
//! Issue #1185・SQL-25・TASK-209）。
//!
//! 責務境界: [`crate::sql::scan`]（広域取得のスカラー `ORDER BY`）と
//! [`crate::sql::group_by`]（集計文のグループキー・`SELECT DISTINCT` の複数列／非
//! `TEXT` キー・集計文の `ORDER BY`）の双方が呼ぶ、型ごとの比較値
//! （[`OrderValue`]・借用版 [`ScalarKeyRef`]）と、NULL 位置・降順を含む比較器の
//! 唯一の実装。両経路で NULL 位置・NaN／±0 の扱いが食い違わないよう、比較規約は
//! ここに一本化する。RLS 適用は呼び出し元（可視行のみをここへ渡す）の責務で、
//! 本モジュールは行の可視性を一切判断しない。

use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::parser::{BoundOrderKey, BoundOrderTarget, OrderKind};
use std::cmp::Ordering;

/// 型不整合・実装バグの検出用（untrusted 入力起因ではないため `XX000`）。
fn scan_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("scan tier/projection mismatch: {detail}"),
    }
}

/// スカラー ORDER BY 1 キー分の実行時比較値（`sql::group_by` の NULL 規約とは
/// 異なる PostgreSQL 既定〔ASC 末尾・DESC 先頭〕を実装する比較器の入力）。
/// `None`（SQL NULL）は [`compare_order_key`] が型を問わず統一的に扱う。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OrderValue {
    /// 疑似列 `id`（テナント内で一意な `u64`）。
    Id(u64),
    /// `TEXT`（バイト列順）。
    Bytes(Vec<u8>),
    /// `INTEGER`／`BIGINT`／`DATE`／`TIMESTAMP`。
    SignedInt(i64),
    /// `REAL`／`DOUBLE`。
    Float(f64),
    Bool(bool),
    Numeric(crate::numeric::Decimal),
    Uuid(crate::uuid::Uuid),
    /// `ENUM`（宣言順のラベル添字）。
    EnumOrdinal(usize),
}

/// `f64` 比較（NaN はすべての非 NaN より大きく NaN 同士は等しい。`-0.0 == 0.0` は
/// IEEE754 の `PartialOrd` 実装がそのまま満たす）。`f64::total_cmp` は符号ビットまで
/// 区別する全順序のため使わない（実装既定値。§比較規約）。
pub(crate) fn compare_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// 同じ `OrderKind` 由来の 2 値を「昇順が自然な順序」として比較する（呼び出し元
/// [`compare_order_key`] が ASC/DESC・NULL 位置を適用する前段）。異なる variant の
/// 組み合わせは同一キーでは構築されない不変条件（[`extract_order_value_ref`] が
/// `BoundOrderKey::kind` に従って一意に variant を選ぶ）に反する状態のため、
/// 到達しても安全側（`Ordering::Equal`）に倒す。
pub(crate) fn compare_order_values(a: &OrderValue, b: &OrderValue) -> Ordering {
    match (a, b) {
        (OrderValue::Id(x), OrderValue::Id(y)) => x.cmp(y),
        (OrderValue::Bytes(x), OrderValue::Bytes(y)) => x.cmp(y),
        (OrderValue::SignedInt(x), OrderValue::SignedInt(y)) => x.cmp(y),
        (OrderValue::Float(x), OrderValue::Float(y)) => compare_f64(*x, *y),
        (OrderValue::Bool(x), OrderValue::Bool(y)) => x.cmp(y),
        (OrderValue::Numeric(x), OrderValue::Numeric(y)) => crate::numeric::cmp_exact(x, y),
        (OrderValue::Uuid(x), OrderValue::Uuid(y)) => x.cmp(y),
        (OrderValue::EnumOrdinal(x), OrderValue::EnumOrdinal(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// 1 キー分の最終比較（NULL 位置・降順を適用済み。ASC は NULL を末尾、DESC は
/// NULL を先頭に置く PostgreSQL 既定。§受入基準 2）。戻り値は「昇順に安定ソートすると
/// 最終的な出力順になる」意味の `Ordering`（`Less` が先頭）。
pub(crate) fn compare_order_key(
    a: Option<&OrderValue>,
    b: Option<&OrderValue>,
    descending: bool,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if descending {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if descending {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let base = compare_order_values(x, y);
            if descending {
                base.reverse()
            } else {
                base
            }
        }
    }
}

/// [`OrderValue`] の借用版（Issue #915 追加是正: codex-review・Cursor Bugbot
/// 指摘の 2 件に対応するため、経路 (B) パス 1 の候補判定を「複製せずに」行える
/// ようにする中間表現）。`Bytes` のみ `&[u8]`（デコード済みバッファからの
/// 借用）で、それ以外は `Copy` な値のため所有版と同じ表現を使う。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScalarKeyRef<'a> {
    Id(u64),
    Bytes(&'a [u8]),
    SignedInt(i64),
    Float(f64),
    Bool(bool),
    Numeric(crate::numeric::Decimal),
    Uuid(crate::uuid::Uuid),
    EnumOrdinal(usize),
}

/// 可視行 1 件から `bound.order_by` の各キーの実行時比較値を、複製せず
/// 借用のまま抽出する（Issue #915）。`scanned` は呼び出し元
/// （[`with_visible_row`]）が `DecodeTier::DimAndScalar` 以上でデコード済みの
/// 前提（`decode_tier_for` が ORDER BY のキー列を `scalar_mask` へ反映するため、
/// `bound.order_by` が非空なら常にこの前提を満たす）。列の実型が束縛段
/// （`sql::parser::bind_scalar_order_by`）で確定した `kind` と一致しない場合は
/// 実装バグとして `Internal`（`XX000`）を返す（fail-closed。untrusted 入力起因では
/// なくスキーマとキー種別の対応が壊れているケース）。
///
/// TEXT キーの複製（`Vec<u8>` への所有化）・その際の予算照合はここでは行わない
/// （呼び出し元が「この候補を実際に採用する」と決めた後にのみ
/// [`scalar_key_ref_to_owned`] を呼ぶ設計。codex-review PR #1096 追加是正:
/// 上位候補にならない大きな TEXT キーの行まで複製前に拒否してしまう問題・
/// 複数 TEXT キーの合計長を見ずに 1 キーずつしか予算照合しない問題〔Cursor
/// Bugbot 指摘〕への対応。詳細は [`predicted_heap_entry_bytes`]・
/// [`compare_candidate_ref_to_entry`] 参照）。
pub(crate) fn extract_order_value_ref<'a>(
    schema: &TableSchema,
    key: &BoundOrderKey,
    id: u64,
    scanned: &'a [Option<row_codec::ScalarRef<'a>>],
) -> Result<Option<ScalarKeyRef<'a>>, SqlSurfaceError> {
    let index = match key.target {
        BoundOrderTarget::Id => return Ok(Some(ScalarKeyRef::Id(id))),
        BoundOrderTarget::Column(index) => index,
    };
    let value = match scanned.get(index) {
        Some(Some(v)) => v,
        Some(None) | None => return Ok(None),
    };
    let column = schema
        .columns
        .get(index)
        .ok_or_else(|| scan_bug("order key column index out of range"))?;
    match (&column.ty, key.kind, value) {
        (ColumnType::Text, OrderKind::Bytes, row_codec::ScalarRef::Text(t)) => {
            Ok(Some(ScalarKeyRef::Bytes(t.as_bytes())))
        }
        (ColumnType::Integer, OrderKind::SignedInt, row_codec::ScalarRef::Integer(v)) => {
            Ok(Some(ScalarKeyRef::SignedInt(i64::from(*v))))
        }
        (ColumnType::BigInt, OrderKind::SignedInt, row_codec::ScalarRef::BigInt(v)) => {
            Ok(Some(ScalarKeyRef::SignedInt(*v)))
        }
        (ColumnType::Date, OrderKind::SignedInt, row_codec::ScalarRef::Date(v)) => {
            Ok(Some(ScalarKeyRef::SignedInt(i64::from(*v))))
        }
        (ColumnType::Timestamp, OrderKind::SignedInt, row_codec::ScalarRef::Timestamp(v)) => {
            Ok(Some(ScalarKeyRef::SignedInt(*v)))
        }
        (ColumnType::Real, OrderKind::Float, row_codec::ScalarRef::Real(v)) => {
            Ok(Some(ScalarKeyRef::Float(f64::from(*v))))
        }
        (ColumnType::Double, OrderKind::Float, row_codec::ScalarRef::Double(v)) => {
            Ok(Some(ScalarKeyRef::Float(*v)))
        }
        (ColumnType::Boolean, OrderKind::Bool, row_codec::ScalarRef::Bool(v)) => {
            Ok(Some(ScalarKeyRef::Bool(*v)))
        }
        (ColumnType::Numeric { .. }, OrderKind::Numeric, row_codec::ScalarRef::Numeric(v)) => {
            Ok(Some(ScalarKeyRef::Numeric(*v)))
        }
        (ColumnType::Uuid, OrderKind::Uuid, row_codec::ScalarRef::Uuid(v)) => {
            Ok(Some(ScalarKeyRef::Uuid(*v)))
        }
        (ColumnType::Enum(def), OrderKind::Enum, row_codec::ScalarRef::Enum(_)) => {
            let text = value
                .as_dictionary_text()
                .ok_or_else(|| scan_bug("ENUM order key scan yielded a non-dictionary scalar"))?;
            let ordinal = def
                .labels()
                .iter()
                .position(|label| label == text)
                .ok_or_else(|| scan_bug("ENUM order key value is not in the declared label set"))?;
            Ok(Some(ScalarKeyRef::EnumOrdinal(ordinal)))
        }
        _ => Err(scan_bug("order key scalar/column type mismatch")),
    }
}

/// [`ScalarKeyRef`] を所有値（[`OrderValue`]）へ変換する（採用が確定した候補
/// にのみ呼ぶ。Issue #915 追加是正）。TEXT（`Bytes`）のみ確保が必要で、
/// `try_reserve_exact` によりホスト側メモリ不足時も abort ではなく `Err` を
/// 返す（`try_alloc_text_for_budget` と同方針）。呼び出し元が予算照合
/// （[`predicted_heap_entry_bytes`] との比較）を先に済ませている前提のため、
/// ここでは長さの上限判定を重ねて行わない。
pub(crate) fn scalar_key_ref_to_owned(
    value: ScalarKeyRef<'_>,
) -> Result<OrderValue, SqlSurfaceError> {
    Ok(match value {
        ScalarKeyRef::Id(v) => OrderValue::Id(v),
        ScalarKeyRef::Bytes(b) => {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(b.len())
                .map_err(|e| SqlSurfaceError::Internal {
                    detail: format!("failed to reserve order key text field: {e}"),
                })?;
            owned.extend_from_slice(b);
            OrderValue::Bytes(owned)
        }
        ScalarKeyRef::SignedInt(v) => OrderValue::SignedInt(v),
        ScalarKeyRef::Float(v) => OrderValue::Float(v),
        ScalarKeyRef::Bool(v) => OrderValue::Bool(v),
        ScalarKeyRef::Numeric(v) => OrderValue::Numeric(v),
        ScalarKeyRef::Uuid(v) => OrderValue::Uuid(v),
        ScalarKeyRef::EnumOrdinal(v) => OrderValue::EnumOrdinal(v),
    })
}

/// 文全体のスカラー `ORDER BY` の行順序を決める単一の比較器（Issue #1189）。
/// キーを順に比較し（NULL 位置・降順は [`compare_order_key`]）、全キー同点なら
/// `id` 昇順 → `tenant_id` バイト順で確定する（§決定的な順序）。`sql::scan` の
/// 経路 (B) の `HeapEntry::order` と、`sql::window` の出力順計算の両方がこの関数を
/// 呼ぶため、ウィンドウ付き取得の行順と base scan の行順が構造上ずれない。
pub(crate) fn compare_statement_order(
    spec: &[BoundOrderKey],
    a_keys: &[Option<OrderValue>],
    a_id: u64,
    a_tenant: &[u8],
    b_keys: &[Option<OrderValue>],
    b_id: u64,
    b_tenant: &[u8],
) -> Ordering {
    for (idx, key) in spec.iter().enumerate() {
        let a = a_keys.get(idx).and_then(|o| o.as_ref());
        let b = b_keys.get(idx).and_then(|o| o.as_ref());
        let ord = compare_order_key(a, b, key.descending);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    match a_id.cmp(&b_id) {
        Ordering::Equal => a_tenant.cmp(b_tenant),
        id_ord => id_ord,
    }
}

/// [`ScalarKeyRef`] 1 件分と [`OrderValue`] 1 件分を「昇順が自然な順序」として
/// 比較する（呼び出し元 [`compare_ref_key`] が ASC/DESC・NULL 位置を適用する
/// 前段。[`compare_order_values`] の借用対応版・比較規約は完全に同一）。
pub(crate) fn compare_ref_and_owned_values(a: &ScalarKeyRef<'_>, b: &OrderValue) -> Ordering {
    match (a, b) {
        (ScalarKeyRef::Id(x), OrderValue::Id(y)) => x.cmp(y),
        (ScalarKeyRef::Bytes(x), OrderValue::Bytes(y)) => (*x).cmp(y.as_slice()),
        (ScalarKeyRef::SignedInt(x), OrderValue::SignedInt(y)) => x.cmp(y),
        (ScalarKeyRef::Float(x), OrderValue::Float(y)) => compare_f64(*x, *y),
        (ScalarKeyRef::Bool(x), OrderValue::Bool(y)) => x.cmp(y),
        (ScalarKeyRef::Numeric(x), OrderValue::Numeric(y)) => crate::numeric::cmp_exact(x, y),
        (ScalarKeyRef::Uuid(x), OrderValue::Uuid(y)) => x.cmp(y),
        (ScalarKeyRef::EnumOrdinal(x), OrderValue::EnumOrdinal(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}
impl OrderValue {
    /// 所有値を借用版へ写像する（`Bytes` のみ自身のバッファを借用し、他は `Copy`）。
    /// `sql::group_by` の所有グループキーを、行走査ループが構築する借用キー
    /// （[`ScalarKeyRef`]）と同じ比較器で扱うために使う。
    pub(crate) fn as_key_ref(&self) -> ScalarKeyRef<'_> {
        match self {
            OrderValue::Id(v) => ScalarKeyRef::Id(*v),
            OrderValue::Bytes(b) => ScalarKeyRef::Bytes(b.as_slice()),
            OrderValue::SignedInt(v) => ScalarKeyRef::SignedInt(*v),
            OrderValue::Float(v) => ScalarKeyRef::Float(*v),
            OrderValue::Bool(v) => ScalarKeyRef::Bool(*v),
            OrderValue::Numeric(v) => ScalarKeyRef::Numeric(*v),
            OrderValue::Uuid(v) => ScalarKeyRef::Uuid(*v),
            OrderValue::EnumOrdinal(v) => ScalarKeyRef::EnumOrdinal(*v),
        }
    }
}

/// [`ScalarKeyRef`] 同士を「昇順が自然な順序」で比較する（[`compare_order_values`]
/// の借用対応版で比較規約は同一。`sql::group_by` のグループキー比較の唯一の実装）。
/// 異なる variant の組は同一キーでは構築されない不変条件に反する状態のため、
/// [`compare_order_values`] と同じく安全側（`Equal`）に倒す。
pub(crate) fn compare_key_refs(a: &ScalarKeyRef<'_>, b: &ScalarKeyRef<'_>) -> Ordering {
    match (a, b) {
        (ScalarKeyRef::Id(x), ScalarKeyRef::Id(y)) => x.cmp(y),
        (ScalarKeyRef::Bytes(x), ScalarKeyRef::Bytes(y)) => x.cmp(y),
        (ScalarKeyRef::SignedInt(x), ScalarKeyRef::SignedInt(y)) => x.cmp(y),
        (ScalarKeyRef::Float(x), ScalarKeyRef::Float(y)) => compare_f64(*x, *y),
        (ScalarKeyRef::Bool(x), ScalarKeyRef::Bool(y)) => x.cmp(y),
        (ScalarKeyRef::Numeric(x), ScalarKeyRef::Numeric(y)) => crate::numeric::cmp_exact(x, y),
        (ScalarKeyRef::Uuid(x), ScalarKeyRef::Uuid(y)) => x.cmp(y),
        (ScalarKeyRef::EnumOrdinal(x), ScalarKeyRef::EnumOrdinal(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// グループキー 1 成分（[`OrderValue`]）を、列型に対応する出力セルへ復元する
/// （Issue #1185。`sql::group_by` の PROJECT 段から呼ばれる）。セル variant は
/// 広域取得（`sql::scan`）が同じ列型を投影するときと同一（`INTEGER`／`BIGINT` は
/// `SignedInteger`、`REAL` は `f64` への無損失拡大の `Float`、`ENUM` はラベルの
/// `Text`、疑似列 `id` は `Integer`）。`ty` が `None` は疑似列 `id`。値と列型の
/// 対応が崩れている場合は実装バグとして `Internal`（`XX000`）で fail-closed に倒す。
pub(crate) fn order_value_to_cell(
    value: &OrderValue,
    ty: Option<&ColumnType>,
) -> Result<crate::sql::exec::Cell, SqlSurfaceError> {
    use crate::sql::exec::Cell;
    match (value, ty) {
        (OrderValue::Id(v), None) => Ok(Cell::Integer(*v)),
        (OrderValue::Bytes(b), Some(ColumnType::Text)) => String::from_utf8(b.clone())
            .map(Cell::Text)
            .map_err(|_| scan_bug("group key TEXT value is not valid UTF-8")),
        (OrderValue::SignedInt(v), Some(ColumnType::Integer | ColumnType::BigInt)) => {
            Ok(Cell::SignedInteger(*v))
        }
        (OrderValue::SignedInt(v), Some(ColumnType::Date)) => i32::try_from(*v)
            .map(Cell::Date)
            .map_err(|_| scan_bug("group key DATE value out of range")),
        (OrderValue::SignedInt(v), Some(ColumnType::Timestamp)) => Ok(Cell::Timestamp(*v)),
        (OrderValue::Float(v), Some(ColumnType::Real | ColumnType::Double)) => Ok(Cell::Float(*v)),
        (OrderValue::Bool(v), Some(ColumnType::Boolean)) => Ok(Cell::Bool(*v)),
        (OrderValue::Numeric(v), Some(ColumnType::Numeric { .. })) => Ok(Cell::Numeric(*v)),
        (OrderValue::Uuid(v), Some(ColumnType::Uuid)) => Ok(Cell::Uuid(*v)),
        (OrderValue::EnumOrdinal(i), Some(ColumnType::Enum(def))) => def
            .labels()
            .get(*i)
            .map(|label| Cell::Text(label.clone()))
            .ok_or_else(|| scan_bug("group key ENUM ordinal out of range")),
        _ => Err(scan_bug("group key value/column type mismatch")),
    }
}
