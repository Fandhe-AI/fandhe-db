//! GIN 相当索引（INDEX-8、Issue #2207）の永続ポスティング索引の維持。
//!
//! `CREATE INDEX ... USING gin`（Issue #2206 が宣言とカタログを担当）が宣言した jsonb 列の索引エントリを、
//! 物理テーブル `user_gin/{索引名}`（[`crate::catalog::user_gin_table_def`]）へ行の書き込みと
//! **同一 write txn** で維持する。呼び出し元は次のとおり（いずれも `constraint.rs` の窓口経由）。
//!
//! - 書き込み: `constraint::enforce_row_constraints_in_txn`（INSERT・UPSERT・UPDATE・COPY・分割実行 DML・
//!   NoSQL・ビュー経由・FK 参照アクションが書き換えた子行）→ [`maintain_rows_in_txn`]
//! - 削除の後片付け: `tenant.rs` の削除系 3 経路と FK `ON DELETE CASCADE` → [`forget_rows_in_txn`]
//! - TRUNCATE: `tenant.rs::truncate_tables_unchecked` → [`clear_tenant_in_txn`]
//! - `CREATE INDEX` の初回構築: `catalog.rs::create_index_bound` → [`build_all_tenants_in_txn`]
//! - 診断: `EngineCore::verify_gin_index_read_only` → [`audit_tenant_read_only`]
//! - 読み取り（Issue #2208）: `sql::gin_scan`（広域取得 `sql::scan` の候補探査）→ [`GinIndexProbe`]・
//!   [`posting_range_bounds`]・[`decode_posting_rowid`]。**読み取り専用**で、索引・マーカーを一切書き換えない
//!   （マーカーが無効なテナントは呼び出し側が行範囲の走査へ縮退する）。
//!
//! 抽出規則と上限は [`extract`]、設計判断は `docs/design/gin-jsonb-index.md`（D3〜D8・§8）を参照。
//!
//! # 索引テーブルの物理レイアウト（永続形式。変更時は [`FORMAT_VERSION`] を上げる）
//!
//! キーは `(tenant_id, subkey)`（第 1 要素は常にサーバー側導出テナント。RLS-9）。`subkey` の先頭 1 バイトで
//! 名前空間を分ける。
//! - `0x00`（[`MARKER_SUBKEY`]）: このテナントの索引が現在の宣言で完全に構築済みであることを示すマーカー。
//!   値は `[format_version: u32 BE][signature]`。`signature` は `[演算子クラス: u8][列名長: u8][列名]
//!   [列型タグ: u8]`。列名を含めるため `RENAME COLUMN` の後はマーカーが不一致になり、各テナントの次の
//!   書き込みで遅延再構築される。
//! - `0x01`（ポスティング）: `[0x01][entry_len: u16 BE][entry][rowid: u64 BE]`、値は空。エントリ長の
//!   接頭辞はキー `"a"` の範囲走査にキー `"ab"` のポスティングが混ざらないようにするための必須要素
//!   （プレフィックス自由。末尾の rowid は昇順で、読み取り側の k-way マージ交差に使える）。
//! - `0x02`（逆引き）: `[0x02][rowid: u64 BE]`、値は `[count: u32 BE]` に続く `count` 個の
//!   `[len: u16 BE][entry]`。行の旧エントリ集合との差分更新と、削除時の後片付けに使う。
//!
//! # 正しさの不変条件（D4）
//!
//! マーカーが有効なテナントでは、生存行の抽出される全エントリにポスティングが**必ず**存在する（欠落不可）。
//! 削除済み・値変更済みの行を指す stale なポスティングは許容され、読み取り側の再検査で除かれる。
//! 後片付けは衛生措置で、正しさの依存先にしない。
//!
//! # テナント境界（RLS-9・RLS-10）
//!
//! 維持・遅延バックフィル・後片付け・TRUNCATE・診断はすべて自テナントの物理キー範囲 `(tenant, ..)` に
//! 閉じる。`CREATE INDEX` の全テナント走査だけは DDL（共有カタログ）として全テナントを対象にし、エラーは
//! 固定文言でテナント・値・件数を含めない。

pub(crate) mod extract;

use crate::catalog::{
    user_gin_table_def, user_gin_table_name, user_rows_table_def, user_rows_table_name,
    CatalogError, ColumnType, DeclaredGinIndex, GinIndexAudit, GinOpClass, TableSchema,
};
use crate::storage::rowid::{row_key, split_row_key, RowId};
use crate::tenant::TenantWriteError;
use redb::{ReadableTable, TableHandle};
use std::collections::{BTreeSet, HashMap, HashSet};

pub(crate) use extract::GinLimits;
use extract::{extract_entries_from_text, GinExtractError};

/// 索引エントリの永続フォーマットバージョン。エントリ形式・ハッシュ・サブキー形式を変えるときは必ず
/// 上げる（旧版のマーカーが無効になり、各テナントは次回の書き込みで 1 回だけ再構築される）。
const FORMAT_VERSION: u32 = 1;

/// マーカーのサブキー。
const MARKER_SUBKEY: [u8; 1] = [0x00];
/// ポスティングのサブキー先頭バイト。
const POSTING_TAG: u8 = 0x01;
/// 逆引きのサブキー先頭バイト。
const REVERSE_TAG: u8 = 0x02;

/// 列型タグ（シグネチャ用。GIN は jsonb 列だけに宣言できる）。
const COLUMN_TYPE_TAG_JSONB: u8 = 0x01;

/// テナント範囲掃除の 1 バッチあたりの収集件数上限（無制限に確保しない。
/// 上限に達したら一旦適用してから続きを収集する）。
const CLEAR_CHUNK_LIMIT: usize = 4096;

/// 索引テーブルの型エイリアス（可変ハンドル）。
type IndexTable<'a> = redb::Table<'a, (&'static str, &'static [u8]), &'static [u8]>;

/// 内部矛盾（索引破損・宣言と実スキーマの不整合等）を fail-closed に拒否する固定文言のエラーへ包む。
/// 値・id・テナント名は含めない（他テナントの存在情報を漏らさない。security.md P0）。
fn internal(message: &'static str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(message.to_string()))
}

fn table_error(e: redb::TableError) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::from(e))
}

fn storage_error(e: redb::StorageError) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::from(e))
}

fn map_extract_error(e: GinExtractError) -> TenantWriteError {
    match e {
        GinExtractError::LimitExceeded => TenantWriteError::GinIndexEntryLimitExceeded,
        GinExtractError::Invalid => internal("gin index source value is not valid jsonb"),
    }
}

/// 解決済みの索引対象（宣言と実スキーマを突き合わせた結果）。
struct Target {
    column_index: usize,
    column_name: String,
    opclass: GinOpClass,
}

/// 宣言 `decl` を `schema` の実列へ解決する。列が無い・jsonb でないのは内部矛盾（`DROP COLUMN` は
/// 宣言を同一 txn で消し、`ALTER COLUMN TYPE` は静的検査で拒否するため通常は起きない）で、fail-closed。
fn resolve_target(
    schema: &TableSchema,
    decl: &DeclaredGinIndex,
) -> Result<Target, TenantWriteError> {
    let column_index = schema
        .columns
        .iter()
        .position(|c| c.name == decl.column)
        .ok_or_else(|| internal("gin index column not found in live schema columns"))?;
    let is_jsonb = schema
        .columns
        .get(column_index)
        .is_some_and(|c| matches!(c.ty, ColumnType::Jsonb));
    if !is_jsonb {
        return Err(internal("gin index column is not jsonb"));
    }
    Ok(Target {
        column_index,
        column_name: decl.column.clone(),
        opclass: decl.opclass,
    })
}

/// マーカー値の一部となるシグネチャ（演算子クラス・列名・列型タグ）を組み立てる。
fn signature(target: &Target) -> Result<Vec<u8>, TenantWriteError> {
    let opclass_byte: u8 = match target.opclass {
        GinOpClass::JsonbOps => 1,
        GinOpClass::JsonbPathOps => 2,
    };
    let name_len = u8::try_from(target.column_name.len())
        .map_err(|_| internal("gin index column name is too long"))?;
    let mut out = Vec::with_capacity(3 + target.column_name.len());
    out.push(opclass_byte);
    out.push(name_len);
    out.extend_from_slice(target.column_name.as_bytes());
    out.push(COLUMN_TYPE_TAG_JSONB);
    Ok(out)
}

/// マーカーの値が現在のフォーマットバージョンと宣言シグネチャに一致するか。
fn marker_matches(value: &[u8], signature: &[u8]) -> bool {
    value.len() == 4 + signature.len()
        && value.get(0..4) == Some(FORMAT_VERSION.to_be_bytes().as_slice())
        && value.get(4..) == Some(signature)
}

fn marker_value(signature: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + signature.len());
    v.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    v.extend_from_slice(signature);
    v
}

/// ポスティングのサブキー `[0x01][entry_len: u16 BE][entry][rowid: u64 BE]`。
fn posting_subkey(entry: &[u8], id: RowId) -> Result<Vec<u8>, TenantWriteError> {
    let len = u16::try_from(entry.len()).map_err(|_| internal("gin index entry is too large"))?;
    let mut out = Vec::with_capacity(1 + 2 + entry.len() + 8);
    out.push(POSTING_TAG);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(entry);
    out.extend_from_slice(&id.to_index_raw().to_be_bytes());
    Ok(out)
}

/// 逆引きのサブキー `[0x02][rowid: u64 BE]`。
fn reverse_subkey(id: RowId) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(REVERSE_TAG);
    out.extend_from_slice(&id.to_index_raw().to_be_bytes());
    out
}

/// 逆引きの値 `[count: u32 BE]` + `count` 個の `[len: u16 BE][entry]` を組み立てる。
fn encode_reverse(entries: &BTreeSet<Vec<u8>>) -> Result<Vec<u8>, TenantWriteError> {
    let count =
        u32::try_from(entries.len()).map_err(|_| internal("gin index row has too many entries"))?;
    let mut out = Vec::new();
    out.extend_from_slice(&count.to_be_bytes());
    for entry in entries {
        let len =
            u16::try_from(entry.len()).map_err(|_| internal("gin index entry is too large"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(entry);
    }
    Ok(out)
}

/// [`encode_reverse`] の逆変換。件数・長さはすべて読み取り前に境界検査する（`get()`・`try_into`・
/// checked 演算のみ。破損は固定文言の内部エラー）。
fn decode_reverse(buf: &[u8]) -> Result<BTreeSet<Vec<u8>>, TenantWriteError> {
    let truncated = || internal("gin index reverse entry is truncated");
    let count_bytes: [u8; 4] = buf
        .get(0..4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(truncated)?;
    let count = u32::from_be_bytes(count_bytes);
    let mut out = BTreeSet::new();
    let mut offset = 4usize;
    for _ in 0..count {
        let len_end = offset.checked_add(2).ok_or_else(truncated)?;
        let len_bytes: [u8; 2] = buf
            .get(offset..len_end)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(truncated)?;
        let len = usize::from(u16::from_be_bytes(len_bytes));
        let end = len_end.checked_add(len).ok_or_else(truncated)?;
        let entry = buf.get(len_end..end).ok_or_else(truncated)?;
        out.insert(entry.to_vec());
        offset = end;
    }
    if offset != buf.len() {
        return Err(internal("gin index reverse entry has trailing bytes"));
    }
    Ok(out)
}

/// 索引テーブル `index_table_name` が write txn 内で既に存在するかを、**作成を伴わずに**確認する
/// （`open_table` は存在しない表を暗黙に作るため、削除系・TRUNCATE は先にこれで確かめる）。
fn index_table_exists(
    write_txn: &redb::WriteTransaction,
    index_table_name: &str,
) -> Result<bool, TenantWriteError> {
    let mut tables = write_txn.list_tables().map_err(storage_error)?;
    Ok(tables.any(|handle| handle.name() == index_table_name))
}

/// 行の生バイト列から GIN 列の jsonb テキストを読み、索引エントリ集合を抽出する。列が NULL なら空集合。
fn row_entries(
    schema: &TableSchema,
    target: &Target,
    buf: &[u8],
    limits: &GinLimits,
) -> Result<BTreeSet<Vec<u8>>, TenantWriteError> {
    let (_dim, metadata) = crate::storage::decode_row_dim_and_metadata_borrowed(buf)?;
    let mut mask = vec![false; schema.columns.len()];
    match mask.get_mut(target.column_index) {
        Some(slot) => *slot = true,
        None => return Err(internal("gin index column index out of range")),
    }
    let values = crate::row_codec::scan_scalar_columns_masked(schema, metadata, Some(&mask))?;
    match values.get(target.column_index) {
        Some(Some(crate::row_codec::ScalarRef::Json(text))) => {
            extract_entries_from_text(target.opclass, text, limits).map_err(map_extract_error)
        }
        Some(None) => Ok(BTreeSet::new()),
        _ => Err(internal("gin index column value is not jsonb")),
    }
}

/// テナント範囲 `(tenant_id, ..)` の索引エントリを最大 [`CLEAR_CHUNK_LIMIT`] 件だけ収集して削除し、
/// まだ残っている可能性があれば `true` を返す（呼び出し元は `false` になるまでループする）。
fn clear_tenant_chunk(
    index_table: &mut IndexTable<'_>,
    tenant_id: &str,
) -> Result<bool, TenantWriteError> {
    let mut victims: Vec<Vec<u8>> = Vec::new();
    {
        let start = std::ops::Bound::Included((tenant_id, [].as_slice()));
        let end = std::ops::Bound::Excluded((tenant_id, [0xffu8].as_slice()));
        let mut iter = index_table
            .range::<(&str, &[u8])>((start, end))
            .map_err(storage_error)?;
        for entry in &mut iter {
            let (k, _v) = entry.map_err(storage_error)?;
            let (key_tenant, subkey) = k.value();
            if key_tenant != tenant_id {
                // 範囲の構築上到達しないはずだが defense-in-depth で維持する。
                break;
            }
            victims.push(subkey.to_vec());
            if victims.len() >= CLEAR_CHUNK_LIMIT {
                break;
            }
        }
    }
    let more = victims.len() >= CLEAR_CHUNK_LIMIT;
    for subkey in &victims {
        index_table
            .remove((tenant_id, subkey.as_slice()))
            .map_err(storage_error)?;
    }
    Ok(more)
}

/// 行 `id` のエントリ集合 `new` を索引へ反映する（逆引きとの差分だけを書く）。`new` が空なら逆引きも
/// 消す。
fn apply_row(
    index_table: &mut IndexTable<'_>,
    tenant_id: &str,
    id: RowId,
    new: &BTreeSet<Vec<u8>>,
) -> Result<(), TenantWriteError> {
    let rev_sub = reverse_subkey(id);
    let old = match index_table
        .get((tenant_id, rev_sub.as_slice()))
        .map_err(storage_error)?
    {
        Some(guard) => Some(decode_reverse(guard.value())?),
        None => None,
    };
    if old.as_ref() == Some(new) || (old.is_none() && new.is_empty()) {
        return Ok(());
    }
    let empty = BTreeSet::new();
    let old_set = old.as_ref().unwrap_or(&empty);
    for entry in old_set.difference(new) {
        let sub = posting_subkey(entry, id)?;
        index_table
            .remove((tenant_id, sub.as_slice()))
            .map_err(storage_error)?;
    }
    for entry in new.difference(old_set) {
        let sub = posting_subkey(entry, id)?;
        index_table
            .insert((tenant_id, sub.as_slice()), [].as_slice())
            .map_err(storage_error)?;
    }
    if new.is_empty() {
        index_table
            .remove((tenant_id, rev_sub.as_slice()))
            .map_err(storage_error)?;
    } else {
        let rev_value = encode_reverse(new)?;
        index_table
            .insert((tenant_id, rev_sub.as_slice()), rev_value.as_slice())
            .map_err(storage_error)?;
    }
    Ok(())
}

/// 行 `id` が所有していたポスティングと逆引きを後片付けする。
fn forget_row(
    index_table: &mut IndexTable<'_>,
    tenant_id: &str,
    id: RowId,
) -> Result<(), TenantWriteError> {
    let rev_sub = reverse_subkey(id);
    let old = match index_table
        .get((tenant_id, rev_sub.as_slice()))
        .map_err(storage_error)?
    {
        Some(guard) => decode_reverse(guard.value())?,
        None => return Ok(()),
    };
    for entry in &old {
        let sub = posting_subkey(entry, id)?;
        index_table
            .remove((tenant_id, sub.as_slice()))
            .map_err(storage_error)?;
    }
    index_table
        .remove((tenant_id, rev_sub.as_slice()))
        .map_err(storage_error)?;
    Ok(())
}

/// 自テナントの行を逐次走査して索引へ新規に書く（索引が空であることが前提。重複判定は不要）。
fn write_fresh(
    index_table: &mut IndexTable<'_>,
    tenant_id: &str,
    id: RowId,
    entries: &BTreeSet<Vec<u8>>,
) -> Result<(), TenantWriteError> {
    if entries.is_empty() {
        return Ok(());
    }
    for entry in entries {
        let sub = posting_subkey(entry, id)?;
        index_table
            .insert((tenant_id, sub.as_slice()), [].as_slice())
            .map_err(storage_error)?;
    }
    let rev_value = encode_reverse(entries)?;
    index_table
        .insert(
            (tenant_id, reverse_subkey(id).as_slice()),
            rev_value.as_slice(),
        )
        .map_err(storage_error)?;
    Ok(())
}

/// テナントの索引が現在の宣言で構築済みであることを確認し、未構築（マーカー不在・不一致）なら
/// 自テナントの既存行だけを対象に再構築する。再構築したら `true`（書き込み済みの行も含めて反映済み）。
/// 再構築中に上限を超える行があれば [`TenantWriteError::GinIndexEntryLimitExceeded`]。
fn ensure_tenant_index<R>(
    row_table: &R,
    index_table: &mut IndexTable<'_>,
    schema: &TableSchema,
    tenant_id: &str,
    target: &Target,
    signature: &[u8],
    limits: &GinLimits,
) -> Result<bool, TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let up_to_date = match index_table
        .get((tenant_id, MARKER_SUBKEY.as_slice()))
        .map_err(storage_error)?
    {
        Some(guard) => marker_matches(guard.value(), signature),
        None => false,
    };
    if up_to_date {
        return Ok(false);
    }
    while clear_tenant_chunk(index_table, tenant_id)? {}

    let start = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(0)));
    let end = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(u64::MAX)));
    {
        let iter = row_table
            .range::<(&str, u64)>((start, end))
            .map_err(storage_error)?;
        for entry in iter {
            let (k, v) = entry.map_err(storage_error)?;
            let (key_tenant, id) = split_row_key(k.value());
            if key_tenant != tenant_id {
                break;
            }
            let entries = row_entries(schema, target, v.value(), limits)?;
            write_fresh(index_table, tenant_id, id, &entries)?;
        }
    }
    index_table
        .insert(
            (tenant_id, MARKER_SUBKEY.as_slice()),
            marker_value(signature).as_slice(),
        )
        .map_err(storage_error)?;
    Ok(true)
}

/// 読み取り側（`sql::gin_scan`）の窓口。宣言と実スキーマから書き込み側と**同じ**シグネチャを組み立て、
/// テナントのマーカーを読み取り専用で照合する（形式版とシグネチャの完全一致。D9）。書き込み側の
/// [`signature`]・[`marker_matches`] を共有するため、読み取り側が独自のマーカー規則を持たない。
pub(crate) struct GinIndexProbe {
    signature: Vec<u8>,
}

impl GinIndexProbe {
    /// 宣言 `decl` を `schema` の実列へ解決してシグネチャを作る。列が無い・jsonb でない場合は内部矛盾
    /// （通常は起きない）で、呼び出し側は固定文言の内部エラーとして fail-closed に拒否する。
    pub(crate) fn for_read(
        schema: &TableSchema,
        decl: &DeclaredGinIndex,
    ) -> Result<Self, TenantWriteError> {
        let target = resolve_target(schema, decl)?;
        Ok(Self {
            signature: signature(&target)?,
        })
    }

    /// テナント `tenant` の索引が現在の宣言で構築済みか。マーカー不在・不一致は `false`
    /// （遅延再構築前の状態。呼び出し側は行範囲の走査へ縮退する）。
    pub(crate) fn marker_valid<I>(
        &self,
        index: &I,
        tenant: &str,
    ) -> Result<bool, redb::StorageError>
    where
        I: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>,
    {
        Ok(match index.get((tenant, MARKER_SUBKEY.as_slice()))? {
            Some(guard) => marker_matches(guard.value(), &self.signature),
            None => false,
        })
    }
}

/// エントリ `entry` のポスティング範囲 `[0x01][len: u16 BE][entry][rowid 0..=MAX]` の下端・上端
/// （閉区間のサブキー）。エントリ長が `u16` に収まらなければ `None`（索引不可。抽出側の上限で
/// 通常は起きない）。長さ接頭辞により、`a` の範囲に `ab` のポスティングは入らない。
pub(crate) fn posting_range_bounds(entry: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let len = u16::try_from(entry.len()).ok()?;
    let mut prefix = Vec::with_capacity(3 + entry.len() + 8);
    prefix.push(POSTING_TAG);
    prefix.extend_from_slice(&len.to_be_bytes());
    prefix.extend_from_slice(entry);
    let mut lo = prefix.clone();
    lo.extend_from_slice(&0u64.to_be_bytes());
    let mut hi = prefix;
    hi.extend_from_slice(&u64::MAX.to_be_bytes());
    Some((lo, hi))
}

/// ポスティングのサブキーから rowid を復号する。サブキーが `entry` の範囲の形（長さ・接頭辞）に
/// 完全一致しない場合は `None`（破損。呼び出し側は固定文言の内部エラーで fail-closed にする）。
pub(crate) fn decode_posting_rowid(subkey: &[u8], entry: &[u8]) -> Option<RowId> {
    let len = u16::try_from(entry.len()).ok()?;
    let prefix_len = 3usize.checked_add(entry.len())?;
    if subkey.len() != prefix_len.checked_add(8)?
        || subkey.first() != Some(&POSTING_TAG)
        || subkey.get(1..3) != Some(len.to_be_bytes().as_slice())
        || subkey.get(3..prefix_len) != Some(entry)
    {
        return None;
    }
    let raw: [u8; 8] = subkey.get(prefix_len..)?.try_into().ok()?;
    Some(RowId::from_index_raw(u64::from_be_bytes(raw)))
}

/// 書き込んだ行 `written_ids`（同一テナント）のエントリを、宣言済みの全 GIN 索引へ反映する
/// （`constraint::enforce_row_constraints_in_txn` から呼ばれる維持の唯一の入口）。マーカーが無効な
/// テナントは先に自テナントの既存行から再構築する。読み戻せない id は削除扱いで後片付けする。
/// 上限超過は [`TenantWriteError::GinIndexEntryLimitExceeded`]（呼び出し元は commit しない）。
pub(crate) fn maintain_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    written_ids: &[RowId],
    decls: &[DeclaredGinIndex],
    limits: &GinLimits,
) -> Result<(), TenantWriteError> {
    if written_ids.is_empty() || decls.is_empty() {
        return Ok(());
    }
    let row_table_name = user_rows_table_name(table_name);
    let row_table = write_txn
        .open_table(user_rows_table_def(&row_table_name))
        .map_err(crate::catalog::map_row_table_error)?;
    for decl in decls {
        let target = resolve_target(schema, decl)?;
        let sig = signature(&target)?;
        let gin_table_name = user_gin_table_name(&decl.index_name);
        let mut index_table = write_txn
            .open_table(user_gin_table_def(&gin_table_name))
            .map_err(table_error)?;
        let rebuilt = ensure_tenant_index(
            &row_table,
            &mut index_table,
            schema,
            tenant_id,
            &target,
            &sig,
            limits,
        )?;
        if rebuilt {
            continue;
        }
        for &id in written_ids {
            let entries = {
                match row_table
                    .get(row_key(tenant_id, id))
                    .map_err(crate::catalog::CatalogError::from)?
                {
                    Some(guard) => Some(row_entries(schema, &target, guard.value(), limits)?),
                    None => None,
                }
            };
            match entries {
                Some(entries) => apply_row(&mut index_table, tenant_id, id, &entries)?,
                None => forget_row(&mut index_table, tenant_id, id)?,
            }
        }
    }
    Ok(())
}

/// 削除された行 `ids`（同一テナント）が所有していたポスティングを後片付けする（衛生措置。呼び出し漏れは
/// 正しさに影響しない）。索引テーブルのみを開き行ストアには触れないため、行ストアの可変ハンドルを
/// 保持したまま呼べる。物理テーブルが未作成なら何もしない（作成しない）。
pub(crate) fn forget_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    ids: &[RowId],
    decls: &[DeclaredGinIndex],
) -> Result<(), TenantWriteError> {
    if ids.is_empty() || decls.is_empty() {
        return Ok(());
    }
    for decl in decls {
        let gin_table_name = user_gin_table_name(&decl.index_name);
        if !index_table_exists(write_txn, &gin_table_name)? {
            continue;
        }
        let mut index_table = write_txn
            .open_table(user_gin_table_def(&gin_table_name))
            .map_err(table_error)?;
        for &id in ids {
            forget_row(&mut index_table, tenant_id, id)?;
        }
    }
    Ok(())
}

/// TRUNCATE で行を全削除したテナントの索引を空にし、現在のシグネチャでマーカーを書き直す（空の索引として
/// 完全な状態にする）。他テナントの範囲には触れない。物理テーブルが未作成なら何もしない（作成しない）。
pub(crate) fn clear_tenant_in_txn(
    write_txn: &redb::WriteTransaction,
    schema: &TableSchema,
    tenant_id: &str,
    decls: &[DeclaredGinIndex],
) -> Result<(), TenantWriteError> {
    for decl in decls {
        let gin_table_name = user_gin_table_name(&decl.index_name);
        if !index_table_exists(write_txn, &gin_table_name)? {
            continue;
        }
        let target = resolve_target(schema, decl)?;
        let sig = signature(&target)?;
        let mut index_table = write_txn
            .open_table(user_gin_table_def(&gin_table_name))
            .map_err(table_error)?;
        while clear_tenant_chunk(&mut index_table, tenant_id)? {}
        index_table
            .insert(
                (tenant_id, MARKER_SUBKEY.as_slice()),
                marker_value(&sig).as_slice(),
            )
            .map_err(storage_error)?;
    }
    Ok(())
}

/// `CREATE INDEX ... USING gin` の初回構築。同じ write txn の中で全テナント・全可視性の行を 1 回だけ
/// ストリーミング走査し、行ごとの抽出・上限検証・ポスティング書き込みを行い、テナントごとに走査を終えた
/// 時点でマーカーを書く。行テーブルが未作成なら 0 行として扱う。構築の前に同名の残骸（削除済み索引の
/// 物理テーブル）を捨てる。上限超過は [`TenantWriteError::GinIndexEntryLimitExceeded`]（呼び出し元は
/// commit しない。副作用ゼロ）。
pub(crate) fn build_all_tenants_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    decl: &DeclaredGinIndex,
    limits: &GinLimits,
) -> Result<(), TenantWriteError> {
    let gin_table_name = user_gin_table_name(&decl.index_name);
    write_txn
        .delete_table(user_gin_table_def(&gin_table_name))
        .map_err(table_error)?;
    let target = resolve_target(schema, decl)?;
    let sig = signature(&target)?;
    let row_table_name = user_rows_table_name(table_name);
    let row_table = match write_txn.open_table(user_rows_table_def(&row_table_name)) {
        Ok(t) => t,
        // 行テーブル未作成は行 0 件として扱う（マーカーは各テナントの初回書き込みで遅延構築される）。
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => {
            return Err(TenantWriteError::Catalog(
                crate::catalog::map_row_table_error(e),
            ))
        }
    };
    let mut index_table = write_txn
        .open_table(user_gin_table_def(&gin_table_name))
        .map_err(table_error)?;
    let marker = marker_value(&sig);
    let mut current_tenant: Option<String> = None;
    for entry in row_table.iter().map_err(storage_error)? {
        let (k, v) = entry.map_err(storage_error)?;
        let (tenant, id) = split_row_key(k.value());
        if current_tenant.as_deref() != Some(tenant) {
            if let Some(prev) = current_tenant.take() {
                index_table
                    .insert((prev.as_str(), MARKER_SUBKEY.as_slice()), marker.as_slice())
                    .map_err(storage_error)?;
            }
            current_tenant = Some(tenant.to_string());
        }
        let entries = row_entries(schema, &target, v.value(), limits)?;
        write_fresh(&mut index_table, tenant, id, &entries)?;
    }
    if let Some(last) = current_tenant {
        index_table
            .insert((last.as_str(), MARKER_SUBKEY.as_slice()), marker.as_slice())
            .map_err(storage_error)?;
    }
    Ok(())
}

/// 診断 API（`EngineCore::verify_gin_index_read_only`）の本体。自テナントの索引を**読み取り専用**で検証し、
/// マーカーの一致・欠落ポスティング数・stale 数を返す。再構築も書き込みもしない。`index_table` が `None`
/// （物理テーブル未作成）はマーカー無効・全エントリ欠落として扱う。
pub(crate) fn audit_tenant_read_only<R, I>(
    row_table: &R,
    index_table: Option<&I>,
    schema: &TableSchema,
    tenant_id: &str,
    decl: &DeclaredGinIndex,
    limits: &GinLimits,
) -> Result<GinIndexAudit, TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>,
{
    let target = resolve_target(schema, decl)?;
    let sig = signature(&target)?;

    // 生存行から「あるべき」ポスティングと逆引きを導出する。
    let mut expected_postings: HashSet<Vec<u8>> = HashSet::new();
    let mut expected_reverse: HashMap<RowId, Vec<u8>> = HashMap::new();
    let start = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(0)));
    let end = std::ops::Bound::Included(row_key(tenant_id, RowId::from_index_raw(u64::MAX)));
    for entry in row_table
        .range::<(&str, u64)>((start, end))
        .map_err(storage_error)?
    {
        let (k, v) = entry.map_err(storage_error)?;
        let (key_tenant, id) = split_row_key(k.value());
        if key_tenant != tenant_id {
            break;
        }
        let entries = row_entries(schema, &target, v.value(), limits)?;
        if entries.is_empty() {
            continue;
        }
        for e in &entries {
            expected_postings.insert(posting_subkey(e, id)?);
        }
        expected_reverse.insert(id, encode_reverse(&entries)?);
    }

    let mut marker_valid = false;
    let mut stale = 0usize;
    if let Some(index_table) = index_table {
        marker_valid = match index_table
            .get((tenant_id, MARKER_SUBKEY.as_slice()))
            .map_err(storage_error)?
        {
            Some(guard) => marker_matches(guard.value(), &sig),
            None => false,
        };
        let lo = std::ops::Bound::Included((tenant_id, [POSTING_TAG].as_slice()));
        let hi = std::ops::Bound::Excluded((tenant_id, [REVERSE_TAG + 1].as_slice()));
        for entry in index_table
            .range::<(&str, &[u8])>((lo, hi))
            .map_err(storage_error)?
        {
            let (k, v) = entry.map_err(storage_error)?;
            let (key_tenant, subkey) = k.value();
            if key_tenant != tenant_id {
                break;
            }
            match subkey.first() {
                Some(&POSTING_TAG) => {
                    if !expected_postings.remove(subkey) {
                        stale += 1;
                    }
                }
                Some(&REVERSE_TAG) => {
                    let raw: [u8; 8] = subkey
                        .get(1..)
                        .and_then(|s| s.try_into().ok())
                        .ok_or_else(|| internal("gin index reverse key has unexpected length"))?;
                    let id = RowId::from_index_raw(u64::from_be_bytes(raw));
                    match expected_reverse.remove(&id) {
                        Some(expected) if expected.as_slice() == v.value() => {}
                        _ => stale += 1,
                    }
                }
                _ => {}
            }
        }
    }
    Ok(GinIndexAudit {
        marker_valid,
        missing: expected_postings.len(),
        stale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_value_round_trips() {
        let mut set = BTreeSet::new();
        set.insert(vec![0x01, b'a']);
        set.insert(vec![0x02, 3, b'1']);
        let enc = encode_reverse(&set).unwrap();
        assert_eq!(decode_reverse(&enc).unwrap(), set);
        assert_eq!(
            decode_reverse(&encode_reverse(&BTreeSet::new()).unwrap()).unwrap(),
            BTreeSet::new()
        );
    }

    #[test]
    fn reverse_value_rejects_corruption() {
        let mut set = BTreeSet::new();
        set.insert(vec![0x01, b'a']);
        let enc = encode_reverse(&set).unwrap();
        // 切り詰め・余剰・件数の過大申告・長さの過大申告はすべて拒否する。
        assert!(decode_reverse(&enc[..enc.len() - 1]).is_err());
        let mut extra = enc.clone();
        extra.push(0);
        assert!(decode_reverse(&extra).is_err());
        assert!(decode_reverse(&[0, 0, 0xff, 0xff]).is_err());
        assert!(decode_reverse(&[0, 0, 0, 1, 0xff, 0xff]).is_err());
        assert!(decode_reverse(&[]).is_err());
    }

    #[test]
    fn posting_subkey_is_prefix_free_between_keys() {
        let id = RowId::from_index_raw(7);
        let a = posting_subkey(&[0x01, b'a'], id).unwrap();
        let ab = posting_subkey(&[0x01, b'a', b'b'], id).unwrap();
        // 長さ接頭辞により、`a` の範囲（接頭辞 `[0x01][len(a)][a]`）に `ab` は入らない。
        let a_prefix = &a[..a.len() - 8];
        assert!(!ab.starts_with(a_prefix));
        assert_eq!(&a[a.len() - 8..], &7u64.to_be_bytes());
    }

    #[test]
    fn marker_requires_exact_version_and_signature() {
        let sig = vec![1, 1, b'c', COLUMN_TYPE_TAG_JSONB];
        let value = marker_value(&sig);
        assert!(marker_matches(&value, &sig));
        assert!(!marker_matches(
            &value,
            &[2, 1, b'c', COLUMN_TYPE_TAG_JSONB]
        ));
        assert!(!marker_matches(&value[..value.len() - 1], &sig));
        let mut other_version = value.clone();
        other_version[3] ^= 1;
        assert!(!marker_matches(&other_version, &sig));
    }

    #[test]
    fn posting_range_is_prefix_free_and_rowid_round_trips() {
        let a = vec![0x01, b'a'];
        let ab = vec![0x01, b'a', b'b'];
        let (lo, hi) = posting_range_bounds(&a).unwrap();
        let in_range = |sub: &[u8]| lo.as_slice() <= sub && sub <= hi.as_slice();
        let id = RowId::from_index_raw(7);
        assert!(in_range(&posting_subkey(&a, id).unwrap()));
        assert!(!in_range(&posting_subkey(&ab, id).unwrap()));
        assert!(in_range(
            &posting_subkey(&a, RowId::from_index_raw(u64::MAX)).unwrap()
        ));
        assert_eq!(
            decode_posting_rowid(&posting_subkey(&a, id).unwrap(), &a),
            Some(id)
        );
        // 別エントリの形・切り詰め・余剰・別タグは拒否する。
        assert_eq!(
            decode_posting_rowid(&posting_subkey(&ab, id).unwrap(), &a),
            None
        );
        let good = posting_subkey(&a, id).unwrap();
        assert_eq!(decode_posting_rowid(&good[..good.len() - 1], &a), None);
        let mut extra = good.clone();
        extra.push(0);
        assert_eq!(decode_posting_rowid(&extra, &a), None);
        let mut other_tag = good.clone();
        other_tag[0] = REVERSE_TAG;
        assert_eq!(decode_posting_rowid(&other_tag, &a), None);
        assert_eq!(decode_posting_rowid(&[], &a), None);
    }
}
