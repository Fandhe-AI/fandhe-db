//! 永続一意索引（TABLE-16・TASK-204、Issue #1070）。
//!
//! 親モジュール [`super`]（`constraint.rs`）の [`super::enforce_unique_keys_in_txn`]
//! から呼ばれる実装本体。旧実装（テナント全行の線形走査）を、`user_uniq/{table}`
//! （[`crate::catalog::user_uniq_table_def`]）という二次テーブルへの点照会に
//! 置き換え、1 文あたりの検査コストを書き込み行数・キー数に比例させ、テナントの
//! 保有行数には比例させない（1 行あたり O(k・log n)。k は宣言済み一意キー数）。
//!
//! # 索引テーブルの物理レイアウト
//!
//! キーは `(tenant_id, subkey)`（第 1 要素は常にサーバー側導出テナント。
//! RLS-9・TABLE-12 と同じ名前空間化）。`subkey` の先頭 1 バイトで名前空間を
//! 分ける:
//! - `0x00`（[`MARKER_SUBKEY`]）: このテナントの索引が完全に構築済みである
//!   ことを示すマーカー。値は `[format_version: u32 BE][signature]`。
//!   `signature`（[`schema_signature`]）は宣言済み一意キーの構成（主キー →
//!   UNIQUE 制約の宣言順・列型タグ）を正準エンコードしたもので、`ALTER TABLE
//!   ... ADD UNIQUE` 等でキー構成が変わるとマーカーが不一致になり、次回の
//!   検査で自動的に再構築される。
//! - `0x01`（正引き。[`forward_subkey`]）: `[0x01][ordinal: u16 BE][正準キー
//!   バイト列]`。`ordinal` は [`super::key_specs`] が返す一意キーの宣言順
//!   インデックス（主キーが `Some` なら常に 0、UNIQUE 制約は 1 番から）。
//!   値は当該キー値を保持する行の `id`（u64 BE）。
//! - `0x02`（逆引き。[`reverse_subkey`]）: `[0x02][id: u64 BE]`。値はその行が
//!   現在所有する正引きサブキー列（[`encode_reverse`]）で、行のキー値が
//!   変わった・行が削除された際に、古い正引きエントリを漏れなく後片付け
//!   するために使う。
//!
//! # 正しさの不変条件（遅延検証）
//!
//! マーカーが立っているテナントでは、生存行 R が持つ NULL でない各一意キー
//! K について、正引き `(tenant, K) → R.id` が必ず存在する（漏れは許さない）。
//! 一方、削除済みの行やキーが変わった行を指す stale な正引きエントリが残る
//! ことは許す——[`check_and_update`] は正引きが指す行 `X` を書き込み対象と
//! 突き合わせ、`X` が対象になければ現在の行データを読み戻してキーを
//! 再計算し、一致すれば違反、不一致または行が既に存在しなければ stale と
//! みなして上書きする（読み戻しは 1 件あたり高々 1 行）。この設計により、
//! 削除経路（[`forget_rows_in_txn`]）の後片付けが漏れても偽陽性・偽陰性は
//! 起きない——後片付けは索引の肥大化を防ぐための衛生措置に留まり、判定の
//! 正しさ自体には関与しない。
//!
//! # テナント境界（RLS-9・RLS-10 (c)）
//!
//! 索引キーの第 1 要素は常にサーバー側導出テナントであり、照会・範囲走査・
//! 読み戻し・バックフィル・TRUNCATE の掃除はすべて自テナントの物理キー範囲
//! （`(tenant, ..)` の範囲）に閉じる。他テナントの索引エントリ・行データには
//! 一切触れない。
//!
//! # 後方互換
//!
//! 索引テーブルが存在しない既存 DB では、各テナントの当該テーブルへの
//! 最初の書き込みでマーカー不在を検出し、自テナントの既存行だけを対象に
//! 1 回だけバックフィルする（その回のみ O(n)）。索引導入前のバイナリへ
//! ダウングレードして書き込む運用はサポートしない（索引と行データの不整合
//! を検出する機構を持たないため）。

use super::{decode_key_columns, key_bytes, KeySpec, NullPolicy};
use crate::catalog::{CatalogError, TableSchema};
use crate::tenant::TenantWriteError;
use redb::{ReadableTable, TableHandle};
use std::collections::{HashMap, HashSet};

/// 索引エントリの永続フォーマットバージョン。列宣言のシグネチャと合わせて
/// マーカーに書き込み、いずれかが変わった場合に自動再構築させる。
///
/// 1 → 2: [`schema_signature`] へ構成列の列名を追加した際に明示的に bump した
/// （codex P1 指摘・PR #1123）。`schema_signature` の変更自体でバイト列の長さ・
/// 内容が変わるため version 据え置きでも自然に不一致になるはずだが、旧
/// フォーマットのマーカーを取りこぼしなく fail-closed に無効化することを
/// バイト列の偶然の一致に頼らず保証するため、フォーマット変更のたびに
/// version も bump する運用とする。
const FORMAT_VERSION: u32 = 2;

// [`check_and_update`] が行ストアを読み戻した回数（単体テスト専用の計測
// フック。Issue #1070 受け入れ条件 1「検査コストがテナントの保有行数に
// 比例しない」ことを、テナントの既存行数に依存しない環境非依存の証拠として
// 示すために使う。本番ビルドには含まれない）。`cargo test` はテストごとに
// 別スレッドで並列実行するため、プロセス全体で共有する `static` ではなく
// スレッドローカルにする（他のテストスレッドの呼び出しが計測値へ混入する
// 事故を構造的に防ぐ）。
#[cfg(test)]
thread_local! {
    pub(super) static ROW_TABLE_GET_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `row_table.get` を呼ぶ直前に計測カウンタを増やす（`#[cfg(test)]` 限定）。
#[cfg(test)]
fn count_row_get() {
    ROW_TABLE_GET_COUNT.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn count_row_get() {}

/// マーカーエントリのサブキー（本モジュール固有の名前空間バイト。§モジュール
/// ドキュメント「索引テーブルの物理レイアウト」参照）。
const MARKER_SUBKEY: [u8; 1] = [0x00];

/// 正引きエントリのサブキー先頭バイト。
const FORWARD_TAG: u8 = 0x01;

/// 逆引きエントリのサブキー先頭バイト。
const REVERSE_TAG: u8 = 0x02;

/// TRUNCATE のテナント範囲掃除・マーカー再構築前の索引無効化を 1 回の
/// `write_txn` 内で無制限に確保しないための、1 バッチあたりの収集件数上限
/// （coding-rust.md「長さフィールドは上限を検証してからアロケーションに使う」
/// 対応。上限に達したら一旦適用してから続きを収集する）。
const CLEAR_CHUNK_LIMIT: usize = 4096;

/// 内部矛盾（索引が破損している・呼び出し契約が破られている等、正常系では
/// 到達しないはずの状態）を fail-closed に拒否する固定文言のエラーへ包む。
/// 値・id・テナント名は含めない（他テナントの存在情報を漏らさない。
/// security.md P0）。
fn internal(message: &'static str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(message.to_string()))
}

fn table_error(e: redb::TableError) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::from(e))
}

fn storage_error(e: redb::StorageError) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::from(e))
}

/// 索引テーブル `index_table_name` が write トランザクション内で物理的に
/// 既に存在するかを、**作成を伴わずに**確認する（`clear_tenant_in_txn`・
/// `forget_rows_in_txn` 共用。codex P2 指摘・PR #1123）。
///
/// `redb::WriteTransaction::open_table` は存在しないテーブルを暗黙に作成
/// してしまう（`TableDoesNotExist` を返さない）ため、削除経路で開いて
/// 存在確認する方式では索引未作成テナントに対する DELETE／TRUNCATE のたびに
/// 空の `user_uniq/{table}` が永続化されてしまう。これは「索引テーブルは
/// 初回の一意キー検査（[`ensure_tenant_index`]）まで物理的に未作成」という
/// 契約（モジュールドキュメント「索引テーブルの物理レイアウト」参照）に
/// 反する。`WriteTransaction::list_tables`（作成しない列挙 API）で既存
/// テーブル名と突き合わせることで、作成せずに存在を判定する。
fn index_table_exists(
    write_txn: &redb::WriteTransaction,
    index_table_name: &str,
) -> Result<bool, TenantWriteError> {
    let mut tables = write_txn.list_tables().map_err(storage_error)?;
    Ok(tables.any(|handle| handle.name() == index_table_name))
}

/// [`super::key_specs`] が返す一意キー宣言（主キー・UNIQUE 制約の宣言順）から
/// マーカー値の一部となるシグネチャを組み立てる。列宣言の構成（キー数・
/// NULL の扱い・構成列数・構成列の**列名**（[`crate::catalog::validate_identifier`]
/// でテーブル内一意性が保証される識別子）・構成列の型タグ）が変わると異なる
/// バイト列になり、[`ensure_tenant_index`] がマーカー不一致として自動的に
/// 再構築する。
///
/// 列名を含めるのは、型タグのみでは「同じ型の別列」への一意制約の付け替え
/// （例: `UNIQUE (a)` → `UNIQUE (b)`、`a`・`b` が同じ列型）を区別できず、
/// 旧列の正引きエントリを新しい制約列のものとして誤って流用してしまい、
/// 新しい列での重複が検出できなくなる fail-open な穴があったため
/// （codex P1 指摘・PR #1123）。列名の長さは [`crate::catalog::validate_identifier`]
/// が課す上限（63 バイト）以下であることが保証されているため 1 バイトの
/// 長さプレフィックスで足りる。
fn schema_signature(schema: &TableSchema, specs: &[KeySpec]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(specs.len() as u32).to_be_bytes());
    for spec in specs {
        out.push(match spec.null_policy {
            NullPolicy::Reject => 0,
            NullPolicy::Skip => 1,
        });
        out.extend_from_slice(&(spec.indices.len() as u32).to_be_bytes());
        for &idx in &spec.indices {
            let column = schema.columns.get(idx);
            out.push(column.map(|c| c.ty.unique_key_tag()).unwrap_or(0));
            let name_bytes = column.map(|c| c.name.as_bytes()).unwrap_or(&[]);
            // `validate_identifier` が列名を 63 バイト以下に制限しているため
            // u8 の長さプレフィックスで表現できる（超過はここでは起こり得ない
            // 内部不変条件だが、念のため `u8::MAX` で飽和させ panic を避ける）。
            let len = u8::try_from(name_bytes.len()).unwrap_or(u8::MAX);
            out.push(len);
            out.extend_from_slice(&name_bytes[..usize::from(len)]);
        }
    }
    out
}

/// 正引きサブキー（`[0x01][ordinal: u16 BE][正準キーバイト列]`）を組み立てる。
fn forward_subkey(ordinal: u16, canonical_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + canonical_key.len());
    out.push(FORWARD_TAG);
    out.extend_from_slice(&ordinal.to_be_bytes());
    out.extend_from_slice(canonical_key);
    out
}

/// 逆引きサブキー（`[0x02][id: u64 BE]`）を組み立てる。
fn reverse_subkey(id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(REVERSE_TAG);
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// 正引きエントリの値（行 id、u64 BE 固定 8 バイト）をデコードする。破損値
/// （長さ不足）は fail-closed に内部エラーとして拒否する（untrusted な
/// wire 入力経路ではないが、ディスク破損を黙って無視しない。coding-rust.md）。
fn decode_row_id(buf: &[u8]) -> Result<u64, TenantWriteError> {
    let arr: [u8; 8] = buf
        .try_into()
        .map_err(|_| internal("unique index forward entry value has unexpected length"))?;
    Ok(u64::from_be_bytes(arr))
}

/// 逆引きエントリの値（`[count: u16 BE]` + `count` 個の `[len: u32 BE][正引き
/// サブキー]`）をデコードする。件数・長さはすべて読み取り前に境界検査する
/// （`get()`／`try_into`／checked 演算のみを使い、添字・`unwrap` は使わない）。
fn decode_reverse(buf: &[u8]) -> Result<Vec<Vec<u8>>, TenantWriteError> {
    let count_bytes: [u8; 2] = buf
        .get(0..2)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| internal("unique index reverse entry is truncated"))?;
    let count = u16::from_be_bytes(count_bytes) as usize;
    let mut out = Vec::new();
    let mut offset = 2usize;
    for _ in 0..count {
        let len_bytes: [u8; 4] = buf
            .get(
                offset
                    ..offset
                        .checked_add(4)
                        .ok_or_else(|| internal("unique index reverse entry length overflowed"))?,
            )
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| internal("unique index reverse entry is truncated"))?;
        offset = offset
            .checked_add(4)
            .ok_or_else(|| internal("unique index reverse entry length overflowed"))?;
        let len = u32::from_be_bytes(len_bytes) as usize;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| internal("unique index reverse entry length overflowed"))?;
        let entry = buf
            .get(offset..end)
            .ok_or_else(|| internal("unique index reverse entry is truncated"))?;
        out.push(entry.to_vec());
        offset = end;
    }
    Ok(out)
}

/// [`decode_reverse`] の逆変換。`entries` は正引きサブキー（[`forward_subkey`]
/// の出力）の集合。
fn encode_reverse<'a>(
    entries: impl ExactSizeIterator<Item = &'a Vec<u8>>,
) -> Result<Vec<u8>, TenantWriteError> {
    let count = u16::try_from(entries.len())
        .map_err(|_| internal("unique index row has too many unique key entries"))?;
    let mut out = Vec::new();
    out.extend_from_slice(&count.to_be_bytes());
    for entry in entries {
        let len = u32::try_from(entry.len())
            .map_err(|_| internal("unique index forward subkey is too large"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(entry);
    }
    Ok(out)
}

/// 索引テーブルの型エイリアス（可変ハンドル）。
type IndexTable<'a> = redb::Table<'a, (&'static str, &'static [u8]), &'static [u8]>;

/// テナント範囲 `(tenant_id, ..)` の索引エントリを最大 [`CLEAR_CHUNK_LIMIT`]
/// 件だけ収集して削除し、まだ残っている可能性があれば `true` を返す
/// （呼び出し元は `false` になるまでループする）。範囲走査中は
/// `index_table` への書き込みができない（借用が競合する）ため、収集と
/// 削除を分離する。
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
                // 閉区間の構築上到達しないはずだが defense-in-depth で維持する
                // （`enumerate_dml_candidates` と同じ判断）。
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

/// テナント `tenant_id` の索引エントリ（マーカー含む）をすべて削除する。
/// [`crate::tenant::truncate_table_unchecked`]（TRUNCATE）から、行ストアの
/// `retain_in` 後・`row_table` を drop した後の同一 write トランザクション
/// 内で呼ぶ。`PRIMARY KEY`・`UNIQUE` のいずれも宣言しないテーブル、または
/// 索引テーブルがまだ物理的に作成されていない（[`ensure_tenant_index`] に
/// よる初回検査を経ていない）テナントでは索引テーブルを開かず何もしない
/// ——`redb::WriteTransaction::open_table` は存在しないテーブルを作成して
/// しまう（`TableDoesNotExist` を返さない）ため、[`index_table_exists`]
/// （列挙のみで作成しない API）で先に存在を確認する（PK/UNIQUE 未宣言
/// テーブル・索引未作成テナントへの TRUNCATE で空の索引テーブルが永続化
/// される副作用を防ぐ。codex P2 指摘・PR #1123）。他テナントの範囲には
/// 一切触れない（範囲は `(tenant_id, ..)` に閉じる。RLS-9）。
pub(super) fn clear_tenant_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
) -> Result<(), TenantWriteError> {
    if schema.primary_key().is_none() && schema.unique_constraints().is_empty() {
        return Ok(());
    }
    let index_table_name = crate::catalog::user_uniq_table_name(table_name);
    if !index_table_exists(write_txn, &index_table_name)? {
        return Ok(());
    }
    let mut index_table = write_txn
        .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
        .map_err(table_error)?;
    while clear_tenant_chunk(&mut index_table, tenant_id)? {}
    Ok(())
}

/// 削除された行 `ids`（同一テナント）が所有していた正引きエントリを後片付け
/// する（衛生措置。§モジュールドキュメント「正しさの不変条件」参照——本関数
/// の呼び出し漏れは偽陽性・偽陰性を起こさない）。呼び出し元
/// （[`crate::tenant`] の削除系経路）は行ストアの可変ハンドルを保持したまま
/// 呼んでよい契約とする——本関数は索引テーブルのみを開き、行ストアには
/// 一切触れない（`TableAlreadyOpen` を避けるための構造的な保証）。
///
/// 索引テーブルが物理的に未作成（[`index_table_exists`] で作成を伴わずに
/// 確認する。codex P2 指摘・PR #1123——`open_table` で存在確認すると
/// 索引未作成テナントへの DELETE のたびに空の索引テーブルが永続化されて
/// しまう）、または `PRIMARY KEY`・`UNIQUE` のいずれも宣言しないテーブルの
/// 場合は何もしない。
pub(super) fn forget_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    tenant_id: &str,
    ids: &[u64],
) -> Result<(), TenantWriteError> {
    if ids.is_empty() {
        return Ok(());
    }
    if schema.primary_key().is_none() && schema.unique_constraints().is_empty() {
        return Ok(());
    }
    let index_table_name = crate::catalog::user_uniq_table_name(table_name);
    if !index_table_exists(write_txn, &index_table_name)? {
        return Ok(());
    }
    let mut index_table = write_txn
        .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
        .map_err(table_error)?;
    for &id in ids {
        let rev_sub = reverse_subkey(id);
        let old_entries = match index_table
            .get((tenant_id, rev_sub.as_slice()))
            .map_err(storage_error)?
        {
            Some(guard) => decode_reverse(guard.value())?,
            None => continue,
        };
        for forward_sub in &old_entries {
            let owner = match index_table
                .get((tenant_id, forward_sub.as_slice()))
                .map_err(storage_error)?
            {
                Some(guard) => Some(decode_row_id(guard.value())?),
                None => None,
            };
            if owner == Some(id) {
                index_table
                    .remove((tenant_id, forward_sub.as_slice()))
                    .map_err(storage_error)?;
            }
        }
        index_table
            .remove((tenant_id, rev_sub.as_slice()))
            .map_err(storage_error)?;
    }
    Ok(())
}

/// テナントの索引が完全に構築済みであることを確認し、未構築（マーカー不在・
/// 列宣言シグネチャ不一致）なら自テナントの既存行だけを対象に再構築する。
/// `written_ids`（今回の書き込みで書いた・上書きした id）は対象から除外
/// する——それらは [`check_and_update`] が別途処理するため、ここで扱うと
/// 二重処理になる。
///
/// バックフィル中に既存行どうしの一意キー重複を検出した場合は、内部矛盾
/// として fail-closed に拒否する（旧実装の全走査判定で既に保証されて
/// いたはずの不変条件のため、通常は到達しない）。
pub(super) fn ensure_tenant_index<R>(
    row_table: &R,
    index_table: &mut IndexTable<'_>,
    schema: &TableSchema,
    specs: &[KeySpec],
    mask: &[bool],
    tenant_id: &str,
    written_ids: &HashSet<u64>,
) -> Result<(), TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let signature = schema_signature(schema, specs);
    let marker_key = (tenant_id, MARKER_SUBKEY.as_slice());
    let up_to_date = match index_table.get(marker_key).map_err(storage_error)? {
        Some(guard) => {
            let v = guard.value();
            v.len() == 4 + signature.len()
                && v.get(0..4) == Some(FORMAT_VERSION.to_be_bytes().as_slice())
                && v.get(4..) == Some(signature.as_slice())
        }
        None => false,
    };
    if up_to_date {
        return Ok(());
    }

    // マーカー不在・不一致: 自テナントの索引エントリを丸ごと無効化してから
    // 既存行（`written_ids` を除く）を再構築する。
    while clear_tenant_chunk(index_table, tenant_id)? {}

    // 行ストアを走査しながら索引テーブルへ逐次書き込む（Issue #1123 レビュー
    // 対応）。旧実装は全行の正引きキーを `entries` に、重複判定用の全キーを
    // `seen` にそれぞれ蓄積してから一括書き込みしており、大きなテナントでは
    // 保持量が行数・キー長に比例して増大し OOM の要因になっていた
    // （行ストア自体の O(n) 走査は変わらないが、追加で保持する量は 1 行分
    // （`values`・`forward_subs`）に定数化する）。重複判定は「直前に
    // `clear_tenant_chunk` で丸ごと消した索引テーブル」への点照会で行う——
    // 既に挿入済みの正引きキーだけがヒットしうるため、`HashMap` での全件
    // 保持と同じ判定結果になる。
    let start = std::ops::Bound::Included((tenant_id, 0u64));
    let end = std::ops::Bound::Included((tenant_id, u64::MAX));
    {
        let iter = row_table
            .range::<(&str, u64)>((start, end))
            .map_err(storage_error)?;
        for entry in iter {
            let (k, v) = entry.map_err(storage_error)?;
            let (key_tenant, id) = k.value();
            if key_tenant != tenant_id {
                break;
            }
            if written_ids.contains(&id) {
                // 今回の書き込み対象は `check_and_update` 側で処理する。
                continue;
            }
            let values = decode_key_columns(schema, mask, v.value())?;
            let mut forward_subs: Vec<Vec<u8>> = Vec::new();
            for (ordinal, spec) in specs.iter().enumerate() {
                let Some(canonical) = key_bytes(spec, &values).map_err(internal)? else {
                    continue;
                };
                let ordinal_u16 = u16::try_from(ordinal)
                    .map_err(|_| internal("unique key ordinal exceeds u16 range"))?;
                let sub = forward_subkey(ordinal_u16, &canonical);
                let owner = match index_table
                    .get((tenant_id, sub.as_slice()))
                    .map_err(storage_error)?
                {
                    Some(guard) => Some(decode_row_id(guard.value())?),
                    None => None,
                };
                if let Some(owner) = owner {
                    if owner != id {
                        return Err(internal(
                            "internal: duplicate unique key found while backfilling persistent index",
                        ));
                    }
                }
                index_table
                    .insert((tenant_id, sub.as_slice()), id.to_be_bytes().as_slice())
                    .map_err(storage_error)?;
                forward_subs.push(sub);
            }
            if !forward_subs.is_empty() {
                let rev_value = encode_reverse(forward_subs.iter())?;
                index_table
                    .insert(
                        (tenant_id, reverse_subkey(id).as_slice()),
                        rev_value.as_slice(),
                    )
                    .map_err(storage_error)?;
            }
        }
    }

    let mut marker_value = Vec::with_capacity(4 + signature.len());
    marker_value.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    marker_value.extend_from_slice(&signature);
    index_table
        .insert(marker_key, marker_value.as_slice())
        .map_err(storage_error)?;
    Ok(())
}

/// テナントの索引が「完全に構築済み」という不変条件を**読み取り専用**で検証する
/// （クラッシュ耐性検証専用の診断 API。§モジュールドキュメント「正しさの不変
/// 条件」参照。Issue #1070・PR #1123 レビュー対応）。[`ensure_tenant_index`] と
/// 異なり、マーカー不在・不一致・正引きエントリ欠落を検出しても**絶対に
/// 再構築・書き込みをしない**——`examples/crash_tool_unique_index.rs` の
/// `verify` は重複 INSERT が `23505` で拒否されることを索引永続性の証拠として
/// いたが、その INSERT プローブ自体が `enforce_unique_keys_in_txn` 経由で
/// `ensure_tenant_index` を呼び、索引テーブル・マーカーの欠落を検出次第
/// 静かに再構築してから判定してしまうため、クラッシュ後の索引欠落そのものを
/// 検出できない（プローブは常に拒否される）。本関数はプローブより前に、
/// マーカーと各生存行の正引きエントリの存在を読み取るだけで検証し、1 件でも
/// 欠落・不一致があれば `Err` を返す。
pub(super) fn verify_tenant_index_read_only<R, I>(
    row_table: &R,
    index_table: &I,
    schema: &TableSchema,
    specs: &[KeySpec],
    mask: &[bool],
    tenant_id: &str,
) -> Result<(), TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
    I: ReadableTable<(&'static str, &'static [u8]), &'static [u8]>,
{
    let signature = schema_signature(schema, specs);
    let marker_key = (tenant_id, MARKER_SUBKEY.as_slice());
    let up_to_date = match index_table.get(marker_key).map_err(storage_error)? {
        Some(guard) => {
            let v = guard.value();
            v.len() == 4 + signature.len()
                && v.get(0..4) == Some(FORMAT_VERSION.to_be_bytes().as_slice())
                && v.get(4..) == Some(signature.as_slice())
        }
        None => false,
    };
    if !up_to_date {
        return Err(internal(
            "unique index marker is missing or stale (read-only verification)",
        ));
    }

    let start = std::ops::Bound::Included((tenant_id, 0u64));
    let end = std::ops::Bound::Included((tenant_id, u64::MAX));
    let iter = row_table
        .range::<(&str, u64)>((start, end))
        .map_err(storage_error)?;
    for entry in iter {
        let (k, v) = entry.map_err(storage_error)?;
        let (key_tenant, id) = k.value();
        if key_tenant != tenant_id {
            // 閉区間の構築上到達しないはずだが defense-in-depth で維持する
            // （`ensure_tenant_index` と同じ判断）。
            break;
        }
        let values = decode_key_columns(schema, mask, v.value())?;
        for (ordinal, spec) in specs.iter().enumerate() {
            let Some(canonical) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            let ordinal_u16 = u16::try_from(ordinal)
                .map_err(|_| internal("unique key ordinal exceeds u16 range"))?;
            let sub = forward_subkey(ordinal_u16, &canonical);
            let owner = index_table
                .get((tenant_id, sub.as_slice()))
                .map_err(storage_error)?
                .map(|guard| decode_row_id(guard.value()))
                .transpose()?;
            if owner != Some(id) {
                return Err(internal(
                    "unique index forward entry missing for a live row (read-only verification)",
                ));
            }
        }
    }
    Ok(())
}

/// キー列が復元できない（破損した）不可視行の一意キーを、永続索引の逆引き
/// エントリから復元する（Issue #1254・RLS-10・RLS-11。`tenant::upsert_typed_rows_impl`
/// の UNIQUE 対象事前走査が [`super::scan_tenant_rows_by_unique_key`] で除外した
/// 行向け）。`indices` に一致する一意キー宣言の正準キー（`(id, 正準キー)` の組）
/// だけを返す。行本体には一切触れない。索引テーブルが未作成、または該当行の
/// 逆引きエントリが無い場合は何も返さない（索引を作成・更新しない読み取り専用）。
pub(super) fn recover_forward_keys_for_rows(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    schema: &TableSchema,
    specs: &[KeySpec],
    indices: &[usize],
    tenant_id: &str,
    ids: &[u64],
) -> Result<Vec<(u64, Vec<u8>)>, TenantWriteError> {
    let index_table_name = crate::catalog::user_uniq_table_name(table_name);
    if !index_table_exists(write_txn, &index_table_name)? {
        return Ok(Vec::new());
    }
    let Some(ordinal) = specs.iter().position(|s| s.indices == indices) else {
        return Ok(Vec::new());
    };
    let ordinal_u16 =
        u16::try_from(ordinal).map_err(|_| internal("unique key ordinal exceeds u16 range"))?;
    let index_table = write_txn
        .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
        .map_err(table_error)?;
    // 索引が現在のスキーマ・形式のものでなければ、逆引きは旧キーを指しうるため
    // 全行を復元不能として扱う（呼び出し側が一様に fail-closed に倒す）。
    let signature = schema_signature(schema, specs);
    let marker_ok = match index_table
        .get((tenant_id, MARKER_SUBKEY.as_slice()))
        .map_err(storage_error)?
    {
        Some(guard) => {
            let v = guard.value();
            v.len() == 4 + signature.len()
                && v.get(0..4) == Some(FORMAT_VERSION.to_be_bytes().as_slice())
                && v.get(4..) == Some(signature.as_slice())
        }
        None => false,
    };
    if !marker_ok {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for &id in ids {
        let rev_sub = reverse_subkey(id);
        let Some(guard) = index_table
            .get((tenant_id, rev_sub.as_slice()))
            .map_err(storage_error)?
        else {
            continue;
        };
        for sub in decode_reverse(guard.value())? {
            let is_target = sub.first() == Some(&FORWARD_TAG)
                && sub.get(1..3) == Some(ordinal_u16.to_be_bytes().as_slice());
            if !is_target {
                continue;
            }
            let Some(key) = sub.get(3..) else {
                continue;
            };
            // 正引きが同じ行 id を指すことを確認する（古い索引の旧キーを採用しない）。
            let owner = index_table
                .get((tenant_id, sub.as_slice()))
                .map_err(storage_error)?
                .map(|g| decode_row_id(g.value()))
                .transpose()?;
            if owner == Some(id) {
                out.push((id, key.to_vec()));
            }
        }
    }
    Ok(out)
}

/// [`super::enforce_unique_keys_in_txn`] の実処理本体（[`ensure_tenant_index`]
/// の後に呼ぶ）。`written_ids` が今回書き込んだ・上書きした行の一意キー値を
/// 索引に反映しつつ、他の生存行との衝突を検出する。手順は本モジュールの
/// ドキュメント「正しさの不変条件」参照。
pub(super) fn check_and_update<R>(
    row_table: &R,
    index_table: &mut IndexTable<'_>,
    schema: &TableSchema,
    specs: &[KeySpec],
    tenant_id: &str,
    written_ids: &HashSet<u64>,
    mask: &[bool],
) -> Result<(), TenantWriteError>
where
    R: ReadableTable<(&'static str, u64), &'static [u8]>,
{
    // 1. 今回書き込んだ行同士のキー値衝突を検出しつつ、各行が持つ新しい
    //    正引きエントリ集合を組み立てる（旧実装の段 1 と同じ判定）。
    let mut written_keys: Vec<HashMap<Vec<u8>, u64>> =
        specs.iter().map(|_| HashMap::new()).collect();
    let mut new_entries: HashMap<u64, Vec<(u16, Vec<u8>)>> = HashMap::new();
    for &id in written_ids {
        count_row_get();
        let Some(guard) = row_table.get((tenant_id, id)).map_err(storage_error)? else {
            // 呼び出し元は必ず同一 txn 内で先に書き込み済みのはずだが、内部
            // 不変条件の欠落があっても判定対象から除外するに留める
            // （旧実装と同じ扱い）。
            continue;
        };
        let values = decode_key_columns(schema, mask, guard.value())?;
        let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
        for (ordinal, spec) in specs.iter().enumerate() {
            let Some(key) = key_bytes(spec, &values).map_err(internal)? else {
                continue;
            };
            let ordinal_u16 = u16::try_from(ordinal)
                .map_err(|_| internal("unique key ordinal exceeds u16 range"))?;
            let bucket = written_keys
                .get_mut(ordinal)
                .ok_or_else(|| internal("unique key ordinal out of range"))?;
            if let Some(existing_id) = bucket.insert(key.clone(), id) {
                if existing_id != id {
                    return Err(TenantWriteError::UniqueViolation);
                }
            }
            entries.push((ordinal_u16, key));
        }
        new_entries.insert(id, entries);
    }
    // 全書き込み対象行のキーが空（宣言済み一意キーが 1 個も NULL でない値を
    // 持たない）場合でも早期 return しない——更新により一意キーが全て NULL に
    // 変化した行は `new_entries` の該当行が空 Vec を持つが、その行が更新前に
    // 保持していた索引エントリ（旧キー）は段 3 の逆引き差分削除でしか
    // 後片付けされない。ここで早期 return すると段 3 に到達せず、stale な
    // 正引き・逆引きエントリが索引に残り続ける（Codex レビュー指摘・PR #1123）。

    // 2. 各新エントリを索引に照会し、書き込み対象でない別行が同じキーを
    //    現に保持していないか確認する（stale なエントリは読み戻しで判別）。
    for (&id, entries) in &new_entries {
        for (ordinal, key) in entries {
            let subkey = forward_subkey(*ordinal, key);
            let owner = match index_table
                .get((tenant_id, subkey.as_slice()))
                .map_err(storage_error)?
            {
                Some(guard) => decode_row_id(guard.value())?,
                None => continue,
            };
            if owner == id || written_ids.contains(&owner) {
                // 自分自身、または今回の書き込み対象どうしの衝突は段 1 で
                // 既に判定済み（同じ (ordinal, key) を持つ別の書き込み対象が
                // あれば `written_keys` の `insert` が既に検出している）。
                continue;
            }
            count_row_get();
            match row_table.get((tenant_id, owner)).map_err(storage_error)? {
                Some(guard) => {
                    let owner_values = decode_key_columns(schema, mask, guard.value())?;
                    let spec = specs
                        .get(usize::from(*ordinal))
                        .ok_or_else(|| internal("unique key ordinal out of range"))?;
                    let recomputed = key_bytes(spec, &owner_values).map_err(internal)?;
                    if recomputed.as_deref() == Some(key.as_slice()) {
                        return Err(TenantWriteError::UniqueViolation);
                    }
                    // 別テナントへの再割当て・別キーへの更新等でキーが変わって
                    // いた場合は stale。段 3 で上書きする。
                }
                None => {
                    // 参照先行が既に存在しない stale エントリ。段 3 で上書きする。
                }
            }
        }
    }

    // 3. 逆引きの旧エントリのうち新集合に含まれないものを後片付けしてから、
    //    新しい正引き・逆引きエントリを書き込む。
    for (&id, entries) in &new_entries {
        let rev_sub = reverse_subkey(id);
        let new_forward_subs: Vec<Vec<u8>> = entries
            .iter()
            .map(|(ordinal, key)| forward_subkey(*ordinal, key))
            .collect();
        let new_set: HashSet<&[u8]> = new_forward_subs.iter().map(|v| v.as_slice()).collect();

        let old_entries = match index_table
            .get((tenant_id, rev_sub.as_slice()))
            .map_err(storage_error)?
        {
            Some(guard) => decode_reverse(guard.value())?,
            None => Vec::new(),
        };
        for old_sub in &old_entries {
            if new_set.contains(old_sub.as_slice()) {
                continue;
            }
            let still_ours = match index_table
                .get((tenant_id, old_sub.as_slice()))
                .map_err(storage_error)?
            {
                Some(guard) => decode_row_id(guard.value())? == id,
                None => false,
            };
            if still_ours {
                index_table
                    .remove((tenant_id, old_sub.as_slice()))
                    .map_err(storage_error)?;
            }
        }

        for sub in &new_forward_subs {
            index_table
                .insert((tenant_id, sub.as_slice()), id.to_be_bytes().as_slice())
                .map_err(storage_error)?;
        }
        if new_forward_subs.is_empty() {
            index_table
                .remove((tenant_id, rev_sub.as_slice()))
                .map_err(storage_error)?;
        } else {
            let rev_value = encode_reverse(new_forward_subs.iter())?;
            index_table
                .insert((tenant_id, rev_sub.as_slice()), rev_value.as_slice())
                .map_err(storage_error)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_and_reverse_subkey_roundtrip() {
        let key = vec![1u8, 2, 3];
        let sub = forward_subkey(7, &key);
        assert_eq!(sub[0], FORWARD_TAG);
        assert_eq!(&sub[1..3], &7u16.to_be_bytes());
        assert_eq!(&sub[3..], key.as_slice());

        let rsub = reverse_subkey(42);
        assert_eq!(rsub[0], REVERSE_TAG);
        assert_eq!(&rsub[1..], &42u64.to_be_bytes());
    }

    #[test]
    fn decode_row_id_rejects_short_buffer() {
        assert!(decode_row_id(&[1, 2, 3]).is_err());
        assert!(decode_row_id(&7u64.to_be_bytes()).is_ok());
    }

    #[test]
    fn reverse_entry_roundtrip() {
        let subs = vec![forward_subkey(0, b"a"), forward_subkey(1, b"bb")];
        let encoded = encode_reverse(subs.iter()).unwrap();
        let decoded = decode_reverse(&encoded).unwrap();
        assert_eq!(decoded, subs);
    }

    #[test]
    fn decode_reverse_rejects_truncated_buffer() {
        // count = 1 だが本体が無い。
        let bad = 1u16.to_be_bytes().to_vec();
        assert!(decode_reverse(&bad).is_err());
    }

    #[test]
    fn decode_reverse_rejects_length_overflow() {
        let mut bad = 1u16.to_be_bytes().to_vec();
        bad.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_reverse(&bad).is_err());
    }

    /// 受け入れ条件 1（Issue #1070）の環境非依存の構造的証拠: 1 行 INSERT の
    /// 検査コスト（行ストアの読み戻し回数）が、テナントの既存行数 N に対して
    /// 線形に増えないことを、共有 CI 環境のノイズに左右されない形で示す。
    /// N=100 と N=3,000 のそれぞれで、既存行が索引済みの状態を作った上で
    /// 1 行追加した際の [`ROW_TABLE_GET_COUNT`] の増分を比較し、両者が同じ
    /// 小さな定数に収まる（N に依存して増えない）ことを確認する。
    #[test]
    fn single_insert_read_cost_does_not_scale_with_existing_row_count() {
        let small = measure_single_insert_read_cost(100);
        let large = measure_single_insert_read_cost(3_000);
        assert!(
            small <= 4,
            "read cost for a small tenant must stay tiny (bounded by key count), got {small}"
        );
        assert_eq!(
            small, large,
            "read cost for a single 1-key INSERT must not scale with the tenant's existing row count (N=100 got {small}, N=3000 got {large})"
        );
    }

    /// `measure_single_insert_read_cost` 用のヘルパ: 主キー 1 列のテーブルを
    /// 作り、`existing_rows` 件を先に committed で挿入して索引を構築した後、
    /// 1 行だけ追加で INSERT し、その 1 文が [`enforce_unique_keys_in_txn`]
    /// （実際には [`check_and_update`]）で行った行ストア読み戻し回数を返す。
    fn measure_single_insert_read_cost(existing_rows: u64) -> usize {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::policy::PolicyContext;
        use crate::row_codec::Value;
        use crate::storage::{Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path(&format!("unique-index-read-cost-{existing_rows}"));
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("code", ColumnType::Text, false)],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

        for id in 0..existing_rows {
            crate::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                id,
                Visibility::Public,
                &[Value::Text(format!("code-{id}"))],
                &crate::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("op id"),
            )
            .expect("seed insert must succeed");
        }

        ROW_TABLE_GET_COUNT.with(|c| c.set(0));
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            existing_rows,
            Visibility::Public,
            &[Value::Text(format!("code-{existing_rows}"))],
            &crate::recovery::required_op_id::OperationId::parse("op-final").expect("op id"),
        )
        .expect("final insert must succeed");
        ROW_TABLE_GET_COUNT.with(|c| c.get())
    }

    /// [`check_and_update`] は、書き込み対象行の一意キーが（更新により）
    /// すべて NULL になった場合でも段 3（逆引き差分削除）まで到達し、旧
    /// 索引エントリ（正引き・逆引きの両方）を後片付けする——`written_keys`
    /// が空だからといって早期 return してはならない（Codex レビュー
    /// 指摘・PR #1123）。後片付けが漏れると、参照先を失った正引きエントリが
    /// 索引に残り続け、後続の別行が読み戻し（stale 判定）でしか同じ値を
    /// 再利用できなくなる（索引照会だけで O(1) 相当に判定できるという
    /// 本モジュールの設計目標が損なわれる）。
    #[test]
    fn update_that_nulls_out_the_only_key_removes_the_stale_reverse_entry() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema, UniqueConstraint};
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::{LedgerMode, OperationId};
        use crate::row_codec::Value;
        use crate::storage::{Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("unique-index-null-update-cleanup");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new("docs", vec![ColumnDef::new("a", ColumnType::Text, true)])
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["a".to_string()])]);
        storage.create_table(&schema).expect("create table");
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[Value::Text("x".to_string())],
            &OperationId::parse("op-1").expect("op id"),
        )
        .expect("seed insert must succeed");

        let null_op_id = OperationId::parse("op-null").expect("op id");
        let ledger_write = LedgerMode::Ledgered
            .resolve(Some(&null_op_id))
            .expect("ledger resolve");
        crate::tenant::update_row_columns_unchecked(
            crate::tenant::WriteTarget::Autocommit(&storage),
            "docs",
            &ctx,
            1,
            &[(0, Value::Null)],
            ledger_write,
            None,
        )
        .expect("update that nulls out the unique key must succeed");

        // 索引テーブルを直接開いて、id=1 の逆引きエントリが残っていないことを
        // 確認する（段 3 まで到達していれば `remove` 済みのはず）。
        let index_table_name = crate::catalog::user_uniq_table_name("docs");
        let write_txn = storage.begin_write_txn().expect("open txn for inspection");
        let index_table = write_txn
            .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
            .expect("open index table");
        let rev_sub = reverse_subkey(1);
        let leftover = index_table
            .get(("tenant-a", rev_sub.as_slice()))
            .expect("index table get")
            .is_some();
        assert!(
            !leftover,
            "reverse entry for id=1 must be cleaned up once its unique key became NULL"
        );
    }

    /// [`schema_signature`] は、同じ型・同じ制約数・同じ NULL ポリシーであっても
    /// 参照する列が異なれば異なるバイト列を返さなければならない（codex P1
    /// 指摘・PR #1123）。列名を含めない旧実装では、`UNIQUE (a)` から
    /// `UNIQUE (b)`（`a`・`b` が同じ型）へ制約の対象列が変わっても signature が
    /// 一致してしまい、[`ensure_tenant_index`] が「索引は構築済み」と誤判定して
    /// 旧列（`a`）の正引きエントリを新しい制約列（`b`）のものとして流用し続け、
    /// `b` の重複を検出できなくなる fail-open な穴があった。
    #[test]
    fn schema_signature_differs_for_same_type_different_columns() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema, UniqueConstraint};

        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        );

        let schema_unique_on_a = schema
            .clone()
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["a".to_string()])]);
        let schema_unique_on_b =
            schema.with_unique_constraints(vec![UniqueConstraint::new(vec!["b".to_string()])]);

        let (specs_a, _) = super::super::key_specs(&schema_unique_on_a).expect("key specs for a");
        let (specs_b, _) = super::super::key_specs(&schema_unique_on_b).expect("key specs for b");

        let sig_a = schema_signature(&schema_unique_on_a, &specs_a);
        let sig_b = schema_signature(&schema_unique_on_b, &specs_b);
        assert_ne!(
            sig_a, sig_b,
            "signatures for UNIQUE(a) and UNIQUE(b) must differ even though \
             a and b share the same column type (same tag, same spec shape)"
        );
    }

    /// 上記 `schema_signature` の差分が、実際の索引維持ロジック
    /// （[`crate::constraint::enforce_unique_keys_in_txn`]）で観測可能な違いに
    /// つながることを確認する end-to-end 回帰（codex P1 指摘・PR #1123）。
    ///
    /// 手順: `UNIQUE (a)` のテーブルで行 1（a='x', b='dup'）を通常の INSERT で
    /// 挿入し、列 `a` を対象にした索引を正しく構築させる。その後、**同じ物理
    /// テーブル・索引テーブルに対して**「制約の対象列が `a` → `b`（同じ TEXT
    /// 型）へ変わった」状況を模した `TableSchema`（`schema_unique_on_b`）で
    /// 行 2（a='y', b='dup'。列 `b` の値が行 1 と重複）を書き込む。
    ///
    /// 修正前は、`schema_unique_on_b` に対しても「索引は構築済み」と誤判定
    /// され、列 `b` の値は一度も索引化されていないため重複が検出されず
    /// INSERT が誤って成功してしまう。修正後は signature 不一致で自テナントの
    /// 索引が `b` を対象に再構築され、行 1 の `b='dup'` が正しく索引化された
    /// 結果、行 2 の重複が `TenantWriteError::UniqueViolation`（wire_code
    /// 23505 に写像される）として拒否される。
    #[test]
    fn unique_constraint_column_swap_to_a_same_typed_column_is_detected_as_a_duplicate() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema, UniqueConstraint};
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::OperationId;
        use crate::row_codec::Value;
        use crate::storage::{RowInput, Storage, Visibility};
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("unique-index-schema-signature-column-swap");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let schema_unique_on_a = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        )
        .with_unique_constraints(vec![UniqueConstraint::new(vec!["a".to_string()])]);
        storage
            .create_table(&schema_unique_on_a)
            .expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");

        // 行 1: a='x'（現行制約の一意キー値）、b='dup'（この時点では制約対象外）。
        // 通常の INSERT 経路を通すため、列 `a` を対象にした索引が正しく構築
        // される。
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            1,
            Visibility::Public,
            &[Value::Text("x".to_string()), Value::Text("dup".to_string())],
            &OperationId::parse("op-1").expect("op id"),
        )
        .expect("seed insert must succeed and build the index for column a");

        // 「UNIQUE 制約の対象列が a → b（同じ TEXT 型）へ変わった」状況を
        // 模した仮のスキーマ（本 DB のカタログ自体は更新しない——`unique_index`
        // モジュール単体の索引維持ロジックを検証する white-box テスト）。
        let schema_unique_on_b = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        )
        .with_unique_constraints(vec![UniqueConstraint::new(vec!["b".to_string()])]);

        // 行 2 を行ストアへ直接書き込む（a='y', b='dup'。列 b の値が行 1 と
        // 重複）。カタログの `docs` は引き続き `schema_unique_on_a` を宣言した
        // ままなので、通常の `insert_typed_row` は列 `a` の重複しか検査
        // できない——この後 `schema_unique_on_b` を渡して
        // `enforce_unique_keys_in_txn` を直接呼び、「列 b を対象にした検査」を
        // 明示的に再現する。
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let row_table_name = crate::catalog::user_rows_table_name("docs");
            let mut row_table = write_txn
                .open_table(crate::catalog::user_rows_table_def(&row_table_name))
                .expect("open row table");
            let metadata = crate::row_codec::encode_scalar_columns(
                &schema_unique_on_b,
                &[Value::Text("y".to_string()), Value::Text("dup".to_string())],
            )
            .expect("encode row 2 columns");
            let encoded = crate::storage::encode_row(&RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &metadata,
            })
            .expect("encode row 2 for raw insert");
            row_table
                .insert(("tenant-a", 2u64), encoded.as_slice())
                .expect("insert row 2 raw");
        }

        let err = crate::constraint::enforce_unique_keys_in_txn(
            &write_txn,
            "docs",
            &schema_unique_on_b,
            "tenant-a",
            &[2],
        )
        .expect_err(
            "row 2's value in column b duplicates row 1's column-b value; this must be \
             rejected once the index rebuilds for the new constraint column instead of \
             reusing the stale column-a index (Codex P1 regression, PR #1123)",
        );
        assert!(
            matches!(err, TenantWriteError::UniqueViolation),
            "expected UniqueViolation (wire_code 23505), got {err:?}"
        );
        // 重複と判定されたため write_txn は commit せず破棄する（行 2 を
        // 反映しない）。
        drop(write_txn);
    }

    /// [`ensure_tenant_index`] は、既存マーカーの `format_version` が現行の
    /// [`FORMAT_VERSION`] と異なれば、シグネチャのバイト列を精査するまでもなく
    /// 「未構築」として扱い、テナントの索引を再構築しなければならない
    /// （codex P1 指摘・PR #1123 のレビュー対応。列名を signature へ加えた
    /// フォーマット変更自体に対して、偶然のバイト列一致に頼らず fail-closed に
    /// 無効化することを保証する）。再構築後は、マーカーが現行の
    /// `FORMAT_VERSION` で書き直されていることを直接確認する。
    #[test]
    fn ensure_tenant_index_rebuilds_when_marker_format_version_is_outdated() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema, UniqueConstraint};
        use crate::policy::PolicyContext;
        use crate::recovery::required_op_id::OperationId;
        use crate::row_codec::Value;
        use crate::storage::Storage;
        use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

        let path = unique_db_path("unique-index-outdated-format-version");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let schema = TableSchema::new("docs", vec![ColumnDef::new("a", ColumnType::Text, true)])
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["a".to_string()])]);
        storage.create_table(&schema).expect("create table");

        let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            1,
            crate::storage::Visibility::Public,
            &[Value::Text("x".to_string())],
            &OperationId::parse("op-1").expect("op id"),
        )
        .expect("seed insert must succeed");

        // マーカーを「旧フォーマット版（format_version=1）」の値へ直接書き換える
        // （signature 本体のバイト列は現行のものを流用しても構わない——
        // version 自体の不一致だけで再構築されることを確認したいため）。
        let index_table_name = crate::catalog::user_uniq_table_name("docs");
        {
            let write_txn = storage.begin_write_txn().expect("begin write txn");
            {
                let mut index_table = write_txn
                    .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
                    .expect("open index table");
                let (specs, _) = super::super::key_specs(&schema).expect("key specs");
                let signature = schema_signature(&schema, &specs);
                let mut stale_marker = Vec::with_capacity(4 + signature.len());
                stale_marker.extend_from_slice(&1u32.to_be_bytes()); // 旧 format_version
                stale_marker.extend_from_slice(&signature);
                index_table
                    .insert(
                        ("tenant-a", MARKER_SUBKEY.as_slice()),
                        stale_marker.as_slice(),
                    )
                    .expect("overwrite marker with outdated format_version");
            }
            write_txn
                .commit_raw_for_test()
                .expect("commit stale marker");
        }

        // 重複しない新規行の INSERT（マーカーが「構築済み」と誤って信頼されて
        // いても成功してしまうため、この呼び出し自体は再構築の有無を直接
        // 証明しない。マーカーが実際に書き直されたかを別途確認する）。
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            2,
            crate::storage::Visibility::Public,
            &[Value::Text("y".to_string())],
            &OperationId::parse("op-2").expect("op id"),
        )
        .expect("a genuinely distinct value must still be accepted");

        let write_txn = storage.begin_write_txn().expect("open txn for inspection");
        let index_table = write_txn
            .open_table(crate::catalog::user_uniq_table_def(&index_table_name))
            .expect("open index table");
        let marker = index_table
            .get(("tenant-a", MARKER_SUBKEY.as_slice()))
            .expect("marker get")
            .expect("marker must exist after ensure_tenant_index ran");
        assert_eq!(
            marker.value().get(0..4),
            Some(FORMAT_VERSION.to_be_bytes().as_slice()),
            "the outdated format_version marker must have been rebuilt and rewritten \
             with the current FORMAT_VERSION, not left as-is"
        );
    }
}
