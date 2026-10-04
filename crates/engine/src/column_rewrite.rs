//! 列型の拡大変換（`INTEGER`→`BIGINT`・`REAL`→`DOUBLE PRECISION`）に伴う
//! 既存行の一括書き換え（TABLE-19・Issue #1361。ポインタ表記のみ）。
//!
//! スカラー行ペイロード（`row_codec::encode_scalar_columns`）は列位置に依存する
//! 固定形式で、この 2 変換は列のフレーム幅（presence 1 + 4 バイト → presence 1 +
//! 8 バイト）を変える。カタログだけを書き換えると既存行を誤読するため、
//! `catalog::Storage::alter_table_alter_column_type` が呼び出し元の write txn 内で
//! 本モジュールを呼び、全テナントの既存行を新スキーマで再エンコードする。
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

/// 行の書き換えを要する拡大変換の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WideningKind {
    /// `INTEGER`（i32）→ `BIGINT`（i64）。
    IntegerToBigInt,
    /// `REAL`（f32）→ `DOUBLE PRECISION`（f64）。
    RealToDouble,
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
) -> Result<(), CatalogError> {
    let row_table_name = user_rows_table_name(table_name);
    let mut row_table = match write_txn.open_table(user_rows_table_def(&row_table_name)) {
        Ok(t) => t,
        // 行ストア未作成（既存行 0 件）。
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
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
            return Ok(());
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
            *slot = match (kind, std::mem::replace(slot, Value::Null)) {
                (_, Value::Null) => Value::Null,
                (WideningKind::IntegerToBigInt, Value::Integer(i)) => Value::BigInt(i64::from(i)),
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
            return Ok(());
        }
        resume = Some(last);
    }
}
