//! `INSERT`／`DELETE`／`UPDATE`／UPSERT の `RETURNING` 句（Issue #873・#1182・SQL-21）の投影・
//! 行組み立てを担う。`RETURNING` が返す行は「その文が書き込んだ（削除前の）
//! 値」そのものであり、`sql::scan`／`sql::exec` の SELECT 経路が行う
//! redb 走査・RLS 述語評価は経由しない——呼び出し元（`sql::exec::
//! execute_insert_returning`／`execute_delete_returning` ほか、各 DML の
//! `*_returning` 実行入口）が既に確定させた
//! `(id, values)` を、`SELECT` の投影束縛（`sql::parser::bind_projection`）と
//! 同じ列解決規則（実カラム優先・疑似列 `id`）で `sql::exec::QueryResult` へ
//! 写像するだけの薄い層とする（第 2 の投影実装を作らない）。

use crate::catalog::TableSchema;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta, ResultRow};
use crate::sql::parser::ProjectedColumn;

/// 結果セット全体（テキスト・ベクトル各セルの複製バイト量の合計）の累計上限。
/// `sql::scan::MAX_SCAN_RESULT_BYTES`・`sql::exec::MAX_CANDIDATE_SCALAR_BYTES` と
/// 同じ定数（[`crate::arena::MAX_ARENA_TOTAL_BYTES`]）を流用し、確保前に検証する
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
pub(crate) const MAX_RETURNING_RESULT_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// `RETURNING` を持ちうる DML の種別。`wire-server::simple_query` が
/// `CommandComplete` タグ（`INSERT 0 <n>`／`DELETE <n>`／`UPDATE <n>`）を
/// 組み立てる際の接頭辞選択に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmlCommand {
    Insert,
    Update,
    Delete,
}

impl DmlCommand {
    /// pg 互換の `CommandComplete` タグ接頭辞。
    pub fn tag_prefix(self) -> &'static str {
        match self {
            DmlCommand::Insert => "INSERT",
            DmlCommand::Update => "UPDATE",
            DmlCommand::Delete => "DELETE",
        }
    }
}

/// `current` に `add` を加えた累計が `cap` を超えないことを確保前に検証する
/// （`sql::scan`・`sql::exec` の同名ヘルパーと同方針）。
fn try_accumulate_budget(current: usize, add: usize, cap: usize) -> Result<usize, SqlSurfaceError> {
    let next = current.saturating_add(add);
    if next > cap {
        return Err(SqlSurfaceError::payload_too_large(
            "RETURNING result exceeds capacity",
        ));
    }
    Ok(next)
}

/// テキストセルの選択的複製（累計バイト量を確保前に検証。`try_reserve_exact`
/// によりホスト側メモリ不足時も abort ではなく `Err` を返す）。
fn try_alloc_text_for_budget(
    text: &str,
    budget: &mut usize,
    cap: usize,
) -> Result<String, SqlSurfaceError> {
    *budget = try_accumulate_budget(*budget, text.len(), cap)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve RETURNING text field: {e}"),
        })?;
    owned.push_str(text);
    Ok(owned)
}

/// ベクトルセルの選択的複製（累計バイト量を確保前に検証。上記テキスト版と同方針）。
fn try_clone_vector_for_budget(
    vector: &[f32],
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<f32>, SqlSurfaceError> {
    let bytes = vector.len().saturating_mul(std::mem::size_of::<f32>());
    *budget = try_accumulate_budget(*budget, bytes, cap)?;
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(vector.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve RETURNING vector field: {e}"),
        })?;
    owned.extend_from_slice(vector);
    Ok(owned)
}

/// 配列セルの選択的複製（累計バイト量を確保前に検証。上記テキスト・ベクトル版と
/// 同方針。Issue #888）。
fn try_clone_array_for_budget(
    array_value: &crate::row_codec::ArrayValue,
    budget: &mut usize,
    cap: usize,
) -> Result<crate::row_codec::ArrayValue, SqlSurfaceError> {
    // 要素本文に加え要素ごとの構造体分（`Option<String>` 等）も計上する。本文長のみ
    // では空文字列を大量に含む配列で予算消費がほぼ 0 のまま管理領域を無制限に
    // 積み上げられてしまう（`sql::exec::try_alloc_array_for_budget`・`sql::scan` と
    // 同方針。Issue #888 レビュー指摘・PR #1011。Issue #1193 で全要素型へ拡張）。
    // 予算検証を複製より前に行うので、確保量は `cap` で上限される。
    *budget = try_accumulate_budget(*budget, array_value.approx_heap_bytes(), cap)?;
    Ok(array_value.clone())
}

/// BYTEA セルの選択的複製（累計バイト量を確保前に検証。上記テキスト版と同方針。
/// UTF-8 検証を行わない点のみ異なる。Issue #886）。
fn try_alloc_bytes_for_budget(
    bytes: &[u8],
    budget: &mut usize,
    cap: usize,
) -> Result<Vec<u8>, SqlSurfaceError> {
    *budget = try_accumulate_budget(*budget, bytes.len(), cap)?;
    let mut owned: Vec<u8> = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|e| SqlSurfaceError::Internal {
            detail: format!("failed to reserve RETURNING bytea field: {e}"),
        })?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}

/// 型不整合・実装バグの検出用（untrusted 入力起因ではないため `wire_code` は
/// `XX000`。`sql::scan::scan_bug` と同方針）。`RETURNING` の投影は
/// `sql::parser::bind_returning` が `Computed`（式項目）を構造的に排除した
/// うえで返すため、通常はここへ到達しない契約だが、多層防御として
/// fail-closed に扱う。
fn returning_bug(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: format!("RETURNING projection mismatch: {detail}"),
    }
}

/// 投影列メタデータ（`RowDescription` 相当）を組み立てる（`sql::scan` の同名
/// 処理と同じ規則）。
pub(crate) fn column_meta(
    projection: &[ProjectedColumn],
    schema: &TableSchema,
) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
    projection
        .iter()
        .map(|col| match col {
            ProjectedColumn::Id => Ok(ColumnMeta::Id),
            ProjectedColumn::Column { index, name } => {
                let ty = schema
                    .columns
                    .get(*index)
                    .map(|c| c.ty.clone())
                    .ok_or_else(|| returning_bug("projected column index out of range"))?;
                Ok(ColumnMeta::Scalar {
                    name: name.clone(),
                    ty,
                })
            }
            ProjectedColumn::Computed { .. } => {
                Err(returning_bug("computed projection items are not supported"))
            }
        })
        .collect()
}

/// 1 行分の投影（`id`・スキーマ列順の `values`）を [`ResultRow`] へ写像する。
/// `values` は `schema.columns` の列順に対応し、`VECTOR` 列の位置には実際の
/// `Value::Vector` が入っている必要がある（`sql::parser::BoundInsert::values`
/// はこの契約を直接満たす。`tenant::CapturedRow::values` は
/// `row_codec::decode_scalar_columns` が返す `VECTOR` 列 `Value::Null` を
/// `Row::embedding` で明示的に差し替えたうえでこの契約を満たす——`tenant.rs`
/// の捕捉ロジック参照。`row_codec::decode_row`〔別バージョンの物理行フォーマット。
/// 本モジュールの通常の書き込み経路では使われない〕の `DecodedRow::values` は
/// この契約を満たさないため使わない）。`score` は検索結果ではないため常に
/// `0.0`（`sql::scan::execute_scan` と同じ「順序を持たない結果セット」の扱い）。
pub(crate) fn project_row(
    id: u64,
    values: &[Value],
    projection: &[ProjectedColumn],
    budget: &mut usize,
) -> Result<ResultRow, SqlSurfaceError> {
    let mut cells = Vec::with_capacity(projection.len());
    for col in projection {
        let cell = match col {
            ProjectedColumn::Id => Cell::Integer(id),
            ProjectedColumn::Column { index, .. } => match values.get(*index) {
                Some(Value::Null) => Cell::Null,
                Some(Value::Text(s)) => Cell::Text(try_alloc_text_for_budget(
                    s,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                Some(Value::Vector(v)) => Cell::Vector(try_clone_vector_for_budget(
                    v,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                Some(Value::Integer(v)) => Cell::SignedInteger(i64::from(*v)),
                Some(Value::BigInt(v)) => Cell::SignedInteger(*v),
                // F8（Issue #882 計画）: REAL は f64 への無損失拡大、DOUBLE は
                // そのまま `Cell::Float` へ投影する。
                Some(Value::Real(v)) => Cell::Float(f64::from(*v)),
                Some(Value::Double(v)) => Cell::Float(*v),
                Some(Value::Bool(b)) => Cell::Bool(*b),
                Some(Value::Date(d)) => Cell::Date(*d),
                Some(Value::Timestamp(t)) => Cell::Timestamp(*t),
                Some(Value::Array(array_value)) => Cell::Array(try_clone_array_for_budget(
                    array_value,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                Some(Value::Bytes(b)) => Cell::Bytes(try_alloc_bytes_for_budget(
                    b,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                Some(Value::Json(s)) => Cell::Json(try_alloc_text_for_budget(
                    s,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                // ENUM 列は既存の `Cell::Text` へ写像する（Issue #890 D7。
                // `sql::exec` の投影と同じ扱い）。
                Some(Value::Enum(label)) => Cell::Text(try_alloc_text_for_budget(
                    label,
                    budget,
                    MAX_RETURNING_RESULT_BYTES,
                )?),
                Some(Value::Numeric(d)) => Cell::Numeric(*d),
                Some(Value::Uuid(u)) => Cell::Uuid(*u),
                None => return Err(returning_bug("value index out of range")),
            },
            ProjectedColumn::Computed { .. } => {
                return Err(returning_bug("computed projection items are not supported"));
            }
        };
        cells.push(cell);
    }
    Ok(ResultRow {
        id,
        score: 0.0,
        cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 累計が上限を超える場合は確保前に `54000`（`PayloadTooLarge`）で拒否する
    /// （`try_accumulate_budget` は private のため本モジュール内でのみ検証可能）。
    #[test]
    fn try_accumulate_budget_rejects_when_cap_is_exceeded() {
        let cap = 100usize;
        let err = try_accumulate_budget(cap - 1, 2, cap).expect_err("must exceed cap");
        assert_eq!(err.wire_code(), "54000");
    }

    /// ちょうど上限に達する場合は許容する（境界値）。
    #[test]
    fn try_accumulate_budget_allows_exact_cap() {
        let cap = 100usize;
        let next = try_accumulate_budget(0, cap, cap).expect("must fit exactly at cap");
        assert_eq!(next, cap);
    }

    /// [`try_clone_array_for_budget`] は TEXT 要素の本文長だけでなく
    /// `Vec<String>` の構造体分（`String` 1 個あたり `size_of::<String>()`）も
    /// 予算に計上する（PR #1011 codex レビュー P1 指摘対応）。空文字列を大量に
    /// 含む配列は本文長の合計がほぼ 0 になるため、要素数を無視すると予算検証を
    /// 迂回して `String` の管理領域を無制限に確保できてしまう。
    #[test]
    fn try_clone_array_for_budget_counts_text_element_struct_overhead_for_empty_strings() {
        use crate::row_codec::ArrayValue;

        // 1,024 個の空文字列。本文バイト量は 0 だが、`size_of::<String>()`
        // （24 バイト程度）× 1,024 個分の構造体オーバーヘッドは無視できない。
        let items: Vec<Option<String>> = std::iter::repeat_n(Some(String::new()), 1024).collect();
        let array_value = ArrayValue::Text(items);
        let expected_overhead = 1024usize.saturating_mul(std::mem::size_of::<String>());

        let mut budget = 0usize;
        // cap をオーバーヘッド未満に設定すると、本文長のみを計上する実装では
        // 誤って受理してしまう境界。
        let cap = expected_overhead - 1;
        let err = try_clone_array_for_budget(&array_value, &mut budget, cap)
            .expect_err("struct overhead of many empty strings must trip the budget cap");
        assert_eq!(err.wire_code(), "54000");

        // 十分な cap を与えれば受理され、budget にオーバーヘッド分が反映される。
        let mut budget = 0usize;
        let cloned = try_clone_array_for_budget(&array_value, &mut budget, expected_overhead)
            .expect("must fit when cap covers struct overhead");
        assert_eq!(budget, expected_overhead);
        match cloned {
            ArrayValue::Text(items) => assert_eq!(items.len(), 1024),
            _ => panic!("expected ArrayValue::Text"),
        }
    }
}
