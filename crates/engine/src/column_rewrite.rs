//! 列型変換（`INTEGER`／`SMALLINT`→`BIGINT`・`REAL`→`DOUBLE PRECISION`・
//! `BIGINT`→`SMALLINT`）に伴う既存行の一括書き換えと、`INTEGER`→`SMALLINT` の
//! 範囲検証のみの走査（TABLE-19・TABLE-30・TABLE-40・Issue #1361・#1560。ポインタ表記のみ）。
//!
//! スカラー行ペイロード（`row_codec::encode_scalar_columns`）は列位置に依存する
//! 固定形式で、拡大 2 変換と `BIGINT`→`SMALLINT` は列のフレーム幅（presence 1 + 4 バイト → presence 1 +
//! 8 バイト）を変える。カタログだけを書き換えると既存行を誤読するため、
//! `catalog::Storage::alter_table_alter_column_type` が呼び出し元の write txn 内で
//! 本モジュールを呼び、全テナントの既存行を新スキーマで再エンコードする。
//!
//! PK／UNIQUE／FOREIGN KEY の構成列も対象になる（Issue #1402）。型タグ付きの索引
//! （`user_uniq`・`key_index`）の失効は本モジュールの責務ではなく、呼び出し元の
//! `alter_column_type_with` が同一 txn 内で行う。
//!
//! 契約（fail-closed）:
//! - 書き換えるのは対象テーブルの行だけで、行キー `(tenant_id, id)`・tenant・
//!   visibility・embedding は保存する（行の追加・削除・テナント移動は行わない）
//! - 行キーとヘッダのテナントが不整合な行・デコード／エンコード失敗は
//!   `CatalogError::CorruptSchema`（`XX000`）の固定文言へ丸め、tenant・id・値を
//!   エラーへ含めない。呼び出し元は commit せず txn を破棄するため痕跡は残らない

use std::ops::Bound;

use redb::ReadableTable;

use crate::catalog::{
    map_row_table_error, user_rows_table_def, user_rows_table_name, CatalogError, TableSchema,
};
use crate::row_codec::{decode_scalar_columns, encode_scalar_columns, Value};
use crate::storage::{decode_row_for_key, encode_row, RowInput};

/// 1 バッチで集めるキー数の上限。redb は走査中の insert を許さないため、
/// キーだけを集めて走査を閉じてから書き換える（行サイズによらずメモリを抑える）。
const REWRITE_BATCH_KEYS: usize = 1024;

/// 行の書き換えを要する型変換の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WideningKind {
    /// `INTEGER`（i32）または `SMALLINT`（i16。値表現は `Value::Integer` で共通）
    /// → `BIGINT`（i64）。
    IntegerToBigInt,
    /// `BIGINT`（i64）→ `SMALLINT`（i16）。全行が範囲内であることを検証しつつ書き換える
    /// （範囲外が 1 行でもあれば [`RewriteOutcome::OutOfRange`]。Issue #1560）。
    BigIntToSmallInt,
    /// `REAL`（f32）→ `DOUBLE PRECISION`（f64）。
    RealToDouble,
}

/// 走査・書き換えの結果。範囲外は破損（`CorruptSchema`・`XX000`）と区別するため
/// エラーではなく別の結果として返し、呼び出し元が固定文言の `22003` へ写像する
/// （値・テナント・行 id は載せない。呼び出し元は commit せず txn を破棄する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewriteOutcome {
    Done,
    OutOfRange,
}

/// 走査の動作。
#[derive(Clone, Copy)]
enum Mode {
    Rewrite(WideningKind),
    /// 行を書き換えず `SMALLINT` の範囲だけを検査する（`INTEGER`→`SMALLINT`。
    /// 行のレイアウトと正準キーのタグが同一のためバイト列は不変）。
    ValidateSmallInt,
}

fn corrupt(message: &'static str) -> CatalogError {
    CatalogError::CorruptSchema(message.to_string())
}

/// `logical_index` 列の値を拡大して、全テナントの既存行を `new_schema` で
/// 再エンコードする。`old_schema` は行を読むための変更前スキーマ。
pub(crate) fn rewrite_rows_for_widening_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    old_schema: &TableSchema,
    new_schema: &TableSchema,
    logical_index: usize,
    kind: WideningKind,
) -> Result<RewriteOutcome, CatalogError> {
    scan_rows_in_txn(
        write_txn,
        table_name,
        old_schema,
        new_schema,
        logical_index,
        Mode::Rewrite(kind),
    )
}

/// `INTEGER`→`SMALLINT` の縮小で、全テナント・全可視性の既存行が `i16` に収まるかを
/// 検証する（行は書き換えない）。`PolicyContext` で絞ると不可視行の範囲外値を見逃し、
/// 読み出し時の decode 失敗を招くため全行が対象。
pub(crate) fn validate_rows_fit_smallint_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    old_schema: &TableSchema,
    logical_index: usize,
) -> Result<RewriteOutcome, CatalogError> {
    scan_rows_in_txn(
        write_txn,
        table_name,
        old_schema,
        old_schema,
        logical_index,
        Mode::ValidateSmallInt,
    )
}

fn scan_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    old_schema: &TableSchema,
    new_schema: &TableSchema,
    logical_index: usize,
    mode: Mode,
) -> Result<RewriteOutcome, CatalogError> {
    let row_table_name = user_rows_table_name(table_name);
    let mut row_table = match write_txn.open_table(user_rows_table_def(&row_table_name)) {
        Ok(t) => t,
        // 行ストア未作成（既存行 0 件）。
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(RewriteOutcome::Done),
        Err(e) => return Err(map_row_table_error(e)),
    };

    let mut resume: Option<(String, u64)> = None;
    loop {
        let mut keys: Vec<(String, u64)> = Vec::new();
        keys.try_reserve_exact(REWRITE_BATCH_KEYS)
            .map_err(|_| corrupt("failed to reserve row rewrite batch"))?;
        {
            let start = match &resume {
                Some((t, id)) => Bound::Excluded((t.as_str(), *id)),
                None => Bound::Unbounded,
            };
            for entry in row_table.range::<(&str, u64)>((start, Bound::Unbounded))? {
                let (k, _v) = entry?;
                let (tenant, id) = k.value();
                keys.push((tenant.to_string(), id));
                if keys.len() >= REWRITE_BATCH_KEYS {
                    break;
                }
            }
        }
        let Some(last) = keys.last().cloned() else {
            return Ok(RewriteOutcome::Done);
        };
        let full = keys.len() >= REWRITE_BATCH_KEYS;

        for (tenant, id) in &keys {
            let raw: Vec<u8> = match row_table.get((tenant.as_str(), *id))? {
                Some(g) => g.value().to_vec(),
                None => return Err(corrupt("row vanished during column rewrite")),
            };
            let row = decode_row_for_key(tenant, *id, &raw)
                .map_err(|_| corrupt("failed to decode row during column rewrite"))?;
            let mut values = decode_scalar_columns(old_schema, &row.metadata)
                .map_err(|_| corrupt("failed to decode row payload during column rewrite"))?;
            let slot = values
                .get_mut(logical_index)
                .ok_or_else(|| corrupt("column index out of range during column rewrite"))?;
            let kind = match mode {
                Mode::ValidateSmallInt => {
                    match slot {
                        Value::Null => {}
                        Value::Integer(i) if i16::try_from(*i).is_ok() => {}
                        Value::Integer(_) => return Ok(RewriteOutcome::OutOfRange),
                        _ => return Err(corrupt("unexpected value type during column rewrite")),
                    }
                    continue;
                }
                Mode::Rewrite(kind) => kind,
            };
            *slot = match (kind, std::mem::replace(slot, Value::Null)) {
                (_, Value::Null) => Value::Null,
                (WideningKind::IntegerToBigInt, Value::Integer(i)) => Value::BigInt(i64::from(i)),
                (WideningKind::BigIntToSmallInt, Value::BigInt(i)) => {
                    match i32::try_from(i).ok().filter(|v| i16::try_from(*v).is_ok()) {
                        Some(v) => Value::Integer(v),
                        None => return Ok(RewriteOutcome::OutOfRange),
                    }
                }
                (WideningKind::RealToDouble, Value::Real(r)) => {
                    Value::Double(crate::scalar_float::canonicalize_double(f64::from(r)))
                }
                _ => return Err(corrupt("unexpected value type during column rewrite")),
            };
            let metadata = encode_scalar_columns(new_schema, &values)
                .map_err(|_| corrupt("failed to encode row payload during column rewrite"))?;
            let encoded = encode_row(&RowInput {
                tenant_id: &row.tenant_id,
                visibility: row.visibility,
                embedding: &row.embedding,
                metadata: &metadata,
            })
            .map_err(|_| corrupt("failed to encode row during column rewrite"))?;
            row_table.insert((tenant.as_str(), *id), encoded.as_slice())?;
        }

        if !full {
            return Ok(RewriteOutcome::Done);
        }
        resume = Some(last);
    }
}
