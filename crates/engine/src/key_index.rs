//! 永続キー索引（Issue #1071・perf(engine)）。`constraint.rs` の `FOREIGN KEY`
//! 参照整合性検査（TABLE-17・TASK-205）が全行走査に頼っていた 2 箇所——
//! 参照元側の列参照存在確認・参照先側の被参照確認——を、テナントの保有行数に
//! 比例しない索引照会へ置き換えるための下位モジュール。`constraint.rs` の
//! 単一検査点からのみ呼ばれる `pub(crate)`（クレート外・wire-server へは
//! 公開しない）。
//!
//! # 索引の形
//!
//! 索引は `(テーブル, 索引名)` で識別し、索引名は `k:{col1},{col2},...}` 形式
//! （構成列名を宣言順にカンマ区切り。列名自体は `validate_identifier` 済みの
//! `[A-Za-z0-9_]` のみのため区切り文字と衝突しない）。同じテーブル・同じ列集合を
//! 指す索引は自然に 1 本へ集約される（例: 自己参照 FK で参照元列・参照先列が
//! 一致する場合）。
//!
//! 各索引は 2 つの redb テーブルを持つ:
//! - 順引き `key_index/{table}/{name}`: `(tenant, key_bytes, row_id) -> ()`。
//!   「このテナントにこのキー値を持つ行が存在するか」を前方一致 range で判定する。
//! - 逆引き `key_index_rev/{table}/{name}`: `(tenant, row_id) -> key_bytes`。
//!   同期時に行の直前のキー値（pre-image）を求め、順引き側の旧エントリを
//!   O(1) で特定するために使う。
//!
//! どのテーブルのどのテナントがどの索引を構築済みかは `key_index_registry`
//! （`(table, name, tenant) -> ()`）に記録する。**登録はテナント単位**
//! （Issue #1071 レビュー指摘 P0: `ensure_index_in_txn` が旧実装のまま
//! `(table, name)` 単位で登録すると、初回構築した 1 テナントの backfill
//! 走査が他の全テナントの索引済み状態まで確定させてしまい、以後その
//! テナントは未構築のまま索引経路〔`Ok(Some(()))`〕を誤って通過し得る）。
//! 未登録（そのテナントでは未構築）の索引は「まだ構築していない」ことを
//! 意味し、呼び出し元（`constraint.rs`）は全行走査へフォールバックした上で
//! [`ensure_index_in_txn`] を呼んで以後の文から索引経路に切り替える
//! （テナントごとに初回のみ**そのテナントの行だけ**を 1 回走査する。DDL 級の
//! 保守操作として扱う）。fwd／rev の実テーブルは `(table, name)` 単位で共有し
//! （テナントは既にキーの第 1 成分のため、テーブル自体を分ける必要はない）、
//! 複数テナントが同じ索引を順次 backfill しても互いの既存エントリを破壊しない
//! （`redb::WriteTransaction::open_table` は get-or-create）。
//!
//! キー値の正準エンコードは一意性制約の検査点（`constraint::key_bytes`）と
//! 共有する。`FOREIGN KEY` が `id` 疑似列を参照する場合も、子列（`INTEGER`／
//! `BIGINT`）の値をそのまま同じ正準エンコードで索引化する——参照先の `id` は
//! 常に子列と同じ数値としてしか比較され得ないため、`id` 専用の別エンコードは
//! 不要（[`parent_id_key_bytes`] が `u64` の物理 id を子列の型へ変換してから
//! [`crate::constraint::push_canonical_component`] へ渡す）。
//!
//! # テナント境界（RLS-9・RLS-10 (c)）
//!
//! 索引キーの第 1 成分は常にサーバー側で導出したテナント（呼び出し元が渡す
//! `tenant_id`）であり、照会・同期・消去のいずれもそのテナントの前方一致に
//! 閉じる。判定母集合は可視性を問わない全行のまま（一意性検査・旧実装と同じ。
//! `constraint.rs` モジュールドキュメント参照）。他テナントの索引エントリには
//! 一切触れない。
//!
//! [`ensure_index_in_txn`] による初回構築（backfill）も**要求元テナントの行
//! だけ**を読む（登録簿がテナント単位のため、他テナントの行数・破損状態には
//! 一切触れない。`docs/design/foreign-key.md` 参照）。
//!
//! # 呼び出し契約
//!
//! [`sync_rows_in_txn`] は `constraint::enforce_row_constraints_in_txn` の
//! 先頭・CHECK/UNIQUE/FOREIGN KEY のいずれの検査より前に、書き込み対象行の id
//! 集合に対して呼ぶ（redb の write トランザクションは自身が書いた未 commit の
//! 値を読めるため、行は既に書き込み済みである前提）。削除・TRUNCATE には行の
//! 書き込みが伴わないため、`constraint::enforce_referencing_rows_in_txn` が
//! [`crate::constraint::ReferencedRowsChange::Removed`]／
//! [`crate::constraint::ReferencedRowsChange::TenantCleared`] の場合に限り
//! ここから直接 [`sync_rows_in_txn`]／[`clear_tenant_in_txn`] を呼ぶ。
//! `ColumnsUpdated`／`AllColumnsReplaced` は `enforce_row_constraints_in_txn` が
//! 既に計算した [`KeyIndexDelta`] を再利用する契約（同じ id を二重に同期すると
//! 直前の同期結果と現在値が一致してしまい、変化を検出できなくなるため）。

use crate::catalog::{CatalogError, TableSchema};
use crate::constraint::{
    decode_key_columns, key_bytes, push_canonical_component, KeySpec, NullPolicy,
};
use crate::tenant::TenantWriteError;
use redb::{ReadableTable, TableDefinition};
use std::collections::{BTreeMap, BTreeSet};

/// 索引登録簿の総エントリ数の上限（fail-closed。破損 DB での無限走査を防ぐ。
/// `catalog.rs::MAX_INDEX_COUNT` と同じ判断）。
const MAX_KEY_INDEXES: usize = 4096;

/// 索引登録簿: `(table, name, tenant) -> ()`。エントリの存在が「そのテナントで
/// 索引構築済み」を表す（テナント単位。モジュール doc 参照）。
const REGISTRY_TABLE: TableDefinition<(&str, &str, &str), ()> =
    TableDefinition::new("key_index_registry");

type FwdTableDef<'a> = TableDefinition<'a, (&'static str, &'static [u8], u64), ()>;
type RevTableDef<'a> = TableDefinition<'a, (&'static str, u64), &'static [u8]>;
/// 順引き索引の読み書きハンドル（clippy::type_complexity 対応の唯一の別名）。
type FwdTable<'a> = redb::Table<'a, (&'static str, &'static [u8], u64), ()>;

fn fwd_table_def(name: &str) -> FwdTableDef<'_> {
    TableDefinition::new(name)
}

fn rev_table_def(name: &str) -> RevTableDef<'_> {
    TableDefinition::new(name)
}

fn fwd_table_name(table: &str, name: &str) -> String {
    format!("key_index/{table}/{name}")
}

fn rev_table_name(table: &str, name: &str) -> String {
    format!("key_index_rev/{table}/{name}")
}

/// `write_txn.open_table` の `redb::TableError` を [`TenantWriteError`] へ
/// 変換する（`redb::TableError` から `TenantWriteError` への直接の `From` 実装は
/// 無いため、索引テーブル固有のエラー写像として一元化する。索引未作成
/// 〔`TableDoesNotExist`〕を区別したい呼び出し元は `write_txn.open_table` を
/// 直接使う）。
fn open_kv_table<'a, 'n, K, V>(
    write_txn: &'a redb::WriteTransaction,
    def: TableDefinition<'n, K, V>,
) -> Result<redb::Table<'a, K, V>, TenantWriteError>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    write_txn
        .open_table(def)
        .map_err(|e| TenantWriteError::from(CatalogError::from(e)))
}

/// [`crate::constraint`] の内部矛盾ヘルパーと同じ固定文言ラッパー
/// （`validate_schema` を通過したスキーマからは到達しないはずの状態を
/// fail-closed に拒否する）。
fn internal(message: &'static str) -> TenantWriteError {
    TenantWriteError::Catalog(CatalogError::Invalid(message.to_string()))
}

/// 索引名を構成列名（宣言順）から組み立てる（唯一の生成点）。
pub(crate) fn index_name_for_columns(columns: &[String]) -> String {
    format!("k:{}", columns.join(","))
}

fn columns_from_index_name(name: &str) -> Option<Vec<String>> {
    let rest = name.strip_prefix("k:")?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    Some(rest.split(',').map(|s| s.to_string()).collect())
}

/// `columns`（`schema` の生存列名）を現在のスキーマの論理インデックスへ解決し、
/// 索引照会・同期に使う [`KeySpec`]（NULLS DISTINCT: いずれかの列が NULL の行は
/// 索引の対象外）を組み立てる。
fn resolve_key_spec(schema: &TableSchema, columns: &[String]) -> Result<KeySpec, CatalogError> {
    let indices = columns
        .iter()
        .map(|name| {
            schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(
                        "key index column not found in live schema columns".to_string(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(KeySpec::new(indices, NullPolicy::Skip))
}

fn mask_for_spec(schema: &TableSchema, spec: &KeySpec) -> Vec<bool> {
    let mut mask = vec![false; schema.columns.len()];
    for &idx in spec.indices() {
        if let Some(slot) = mask.get_mut(idx) {
            *slot = true;
        }
    }
    mask
}

/// 参照元列（`INTEGER`／`BIGINT`）の物理 `id`（`u64`）値を、その列の正準エンコード
/// （[`push_canonical_component`]）へ変換する。`id` が列の値域に収まらない場合は
/// `Ok(None)`（その列にその値を持つ子行は構造的に存在し得ないため、索引照会上は
/// 「一致し得ない」として扱ってよい——値域外の値を書き込んだ行は存在しない）。
pub(crate) fn parent_id_key_bytes(
    column_ty: &crate::catalog::ColumnType,
    id: u64,
) -> Result<Option<Vec<u8>>, TenantWriteError> {
    use crate::catalog::ColumnType;
    use crate::row_codec::ScalarRef;
    let scalar = match column_ty {
        ColumnType::Integer => i32::try_from(id).ok().map(ScalarRef::Integer),
        ColumnType::BigInt => i64::try_from(id).ok().map(ScalarRef::BigInt),
        // `validate_foreign_keys` が `id` 参照の子列を `INTEGER`／`BIGINT` に
        // 限定するため到達しない内部矛盾。
        _ => {
            return Err(internal(
                "id-referencing foreign key column has an unexpected type",
            ))
        }
    };
    match scalar {
        None => Ok(None),
        Some(value) => {
            let mut out = Vec::new();
            push_canonical_component(&mut out, value).map_err(internal)?;
            Ok(Some(out))
        }
    }
}

/// 書き込み対象行の各索引での旧値・新値の差分（[`sync_rows_in_txn`]／
/// [`clear_tenant_in_txn`] の戻り値）。
///
/// `enforce_referencing_rows_in_txn` はこの差分を使って「失われたキーを今も
/// 参照している子行がないか」だけを確認すればよく、変更に無関係な子行を
/// 走査する必要がない。
#[derive(Debug, Default, Clone)]
pub(crate) struct KeyIndexDelta {
    /// 索引名 → このテナント内でこの索引から失われた（他のどの行も持たなく
    /// なった）キー値の集合。
    lost: BTreeMap<String, BTreeSet<Vec<u8>>>,
    /// 同期時点で登録済みだった索引名の集合。
    registered: BTreeSet<String>,
}

impl KeyIndexDelta {
    /// `columns` に対応する索引について、失われたキー値を返す
    /// （未登録／変化なしなら空）。
    pub(crate) fn lost_keys(&self, columns: &[String]) -> BTreeSet<Vec<u8>> {
        self.lost
            .get(&index_name_for_columns(columns))
            .cloned()
            .unwrap_or_default()
    }

    /// `columns` に対応する索引が、この差分の計算時点で登録済みだったか。
    ///
    /// `lost_keys` が空集合を返すケースは「登録済みで実際に変化なし」と
    /// 「未登録で追跡対象外」の両方があり得るため区別できない
    /// （Issue #1071 レビュー指摘）。呼び出し元はこのフラグで未登録を
    /// 判別し、未登録なら [`none_referenced_in_txn`] 等と同じ
    /// フォールバック（全行走査 + 索引構築）へ回す。
    pub(crate) fn is_registered(&self, columns: &[String]) -> bool {
        self.registered.contains(&index_name_for_columns(columns))
    }
}

/// `table` の登録簿に記録済みの `(索引名, テナント)` を列挙する（`table` に
/// 一致するものだけ。全テナント分）。登録簿未作成なら空を返す。走査件数は
/// [`MAX_KEY_INDEXES`] で打ち切る（fail-closed。
/// `catalog.rs::retain_index_defs_in_txn` と同じ判断。テナント単位登録に
/// なった分、同じ索引名でもテナント数だけエントリが増えるが、上限はテーブル
/// 全体の総登録数のまま据え置く——無制限のテナント数 × 索引数を許すと登録簿
/// 自体が無制限に肥大するため）。
fn registry_entries_for_table(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<Vec<(String, String)>, CatalogError> {
    let reg = match write_txn.open_table(REGISTRY_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let start = std::ops::Bound::Included((table, "", ""));
    let mut entries = Vec::new();
    let mut iter = reg
        .range::<(&str, &str, &str)>((start, std::ops::Bound::Unbounded))
        .map_err(CatalogError::from)?;
    for (scanned, entry) in (&mut iter).enumerate() {
        if scanned >= MAX_KEY_INDEXES {
            return Err(CatalogError::CorruptSchema(format!(
                "key index registry exceeds {MAX_KEY_INDEXES} entries"
            )));
        }
        let (k, _v) = entry?;
        let (entry_table, name, tenant) = k.value();
        if entry_table != table {
            break;
        }
        entries.push((name.to_string(), tenant.to_string()));
    }
    Ok(entries)
}

/// `table` の登録簿から、テナント `tenant` に一致するものだけの索引名を返す
/// （[`sync_rows_in_txn`]／[`clear_tenant_in_txn`] が同期対象を絞るために使う）。
fn registered_names_for_table(
    write_txn: &redb::WriteTransaction,
    table: &str,
    tenant: &str,
) -> Result<Vec<String>, CatalogError> {
    Ok(registry_entries_for_table(write_txn, table)?
        .into_iter()
        .filter(|(_, entry_tenant)| entry_tenant == tenant)
        .map(|(name, _)| name)
        .collect())
}

fn is_registered(
    write_txn: &redb::WriteTransaction,
    table: &str,
    name: &str,
    tenant: &str,
) -> Result<bool, CatalogError> {
    match write_txn.open_table(REGISTRY_TABLE) {
        Ok(t) => Ok(t.get((table, name, tenant))?.is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// `table` が索引化の対象となり得るか（`FOREIGN KEY`・主キー・UNIQUE 制約の
/// いずれも宣言しないテーブルは登録簿を一切照会せずコストゼロで抜ける）。
fn table_may_need_index(schema: &TableSchema) -> bool {
    !schema.foreign_keys().is_empty()
        || schema.primary_key().is_some()
        || !schema.unique_constraints().is_empty()
}

/// `tenant.rs` の各書き込み関数が行を書き込んだ**直後**（`constraint.rs` の
/// CHECK／UNIQUE／FOREIGN KEY 検査より前）に呼ぶ、登録済み索引の同期点。
/// `written_ids` の各行について、登録済みの各索引の旧キー（逆引き）と現在の
/// 行値から求めた新キーを比較し、順引き・逆引きの双方を更新する。
///
/// 索引が 1 つも登録されていないテーブル（大多数）はコストゼロ
/// （[`table_may_need_index`] の判定＋登録簿の空チェックのみ）。
pub(crate) fn sync_rows_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    schema: &TableSchema,
    tenant_id: &str,
    ids: &[u64],
) -> Result<KeyIndexDelta, TenantWriteError> {
    let mut delta = KeyIndexDelta::default();
    if ids.is_empty() || !table_may_need_index(schema) {
        return Ok(delta);
    }
    let names = registered_names_for_table(write_txn, table, tenant_id)?;
    if names.is_empty() {
        return Ok(delta);
    }
    let row_table_name = crate::catalog::user_rows_table_name(table);
    let row_table = match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name))
    {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    };
    for name in &names {
        delta.registered.insert(name.clone());
        let Some(columns) = columns_from_index_name(name) else {
            continue;
        };
        let spec = resolve_key_spec(schema, &columns)?;
        let mask = mask_for_spec(schema, &spec);

        let fwd_name = fwd_table_name(table, name);
        let rev_name = rev_table_name(table, name);
        let mut fwd = open_kv_table(write_txn, fwd_table_def(&fwd_name))?;
        let mut rev = open_kv_table(write_txn, rev_table_def(&rev_name))?;

        for &id in ids {
            let old_key: Option<Vec<u8>> = rev
                .get((tenant_id, id))
                .map_err(CatalogError::from)?
                .map(|g| g.value().to_vec());
            let new_key: Option<Vec<u8>> = match &row_table {
                Some(rt) => match rt.get((tenant_id, id)).map_err(CatalogError::from)? {
                    Some(guard) => {
                        let values = decode_key_columns(schema, &mask, guard.value())?;
                        key_bytes(&spec, &values).map_err(internal)?
                    }
                    None => None,
                },
                None => None,
            };
            if old_key == new_key {
                continue;
            }
            if let Some(old) = &old_key {
                fwd.remove((tenant_id, old.as_slice(), id))
                    .map_err(CatalogError::from)?;
                delta
                    .lost
                    .entry(name.clone())
                    .or_default()
                    .insert(old.clone());
            }
            match &new_key {
                Some(new) => {
                    fwd.insert((tenant_id, new.as_slice(), id), ())
                        .map_err(CatalogError::from)?;
                    rev.insert((tenant_id, id), new.as_slice())
                        .map_err(CatalogError::from)?;
                }
                None => {
                    if old_key.is_some() {
                        rev.remove((tenant_id, id)).map_err(CatalogError::from)?;
                    }
                }
            }
        }

        // 同一バッチ内で「行 A がキー K を失い、行 B が新たに K を得る」
        // （親キーの UPDATE による入れ替え・削除→同一値の再挿入等）場合、
        // 上のループは行 A の処理時点で K を無条件に `delta.lost` へ積む。
        // 全 id の同期が終わった時点の索引を読み直し、他行がまだ保持している
        // キーは `lost` から取り除く（レビュー指摘: `sync_rows_in_txn` が
        // 旧キー削除の都度 `lost` へ追加し、バッチ完了後の再確認をしていな
        // かったため、参照先が実在するのに `ForeignKeyViolation` を誤って
        // 返していた）。
        if let Some(lost_for_name) = delta.lost.get_mut(name) {
            let mut still_lost = BTreeSet::new();
            for key in lost_for_name.iter() {
                if !key_present(&fwd, tenant_id, key).map_err(TenantWriteError::from)? {
                    still_lost.insert(key.clone());
                }
            }
            *lost_for_name = still_lost;
        }
    }
    Ok(delta)
}

/// `TRUNCATE`（[`crate::constraint::ReferencedRowsChange::TenantCleared`]）が
/// 呼ぶ、テナント単位の索引消去。登録済みの各索引について、このテナントの
/// 逆引きエントリをすべて削除し、対応する順引きエントリも削除する。戻り値の
/// [`KeyIndexDelta`] は「このテナントでこの索引から失われたキー」の集合だが、
/// `TenantCleared` の呼び出し元は個々のキーではなく「登録済みかどうか」だけを
/// 見て `any_entry_for_tenant_in_txn` によるテナント単位の存在確認へ倒す
/// （個々のキー値は使わない。行数に比例しないテナント単位の判定で十分なため）。
pub(crate) fn clear_tenant_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    tenant_id: &str,
) -> Result<KeyIndexDelta, TenantWriteError> {
    let mut delta = KeyIndexDelta::default();
    let names = registered_names_for_table(write_txn, table, tenant_id)?;
    for name in &names {
        delta.registered.insert(name.clone());
        let rev_name = rev_table_name(table, name);
        let mut ids_and_keys: Vec<(u64, Vec<u8>)> = Vec::new();
        {
            let mut rev = match write_txn.open_table(rev_table_def(&rev_name)) {
                Ok(t) => t,
                Err(redb::TableError::TableDoesNotExist(_)) => continue,
                Err(e) => return Err(TenantWriteError::from(CatalogError::from(e))),
            };
            let start = std::ops::Bound::Included((tenant_id, 0u64));
            let end = std::ops::Bound::Included((tenant_id, u64::MAX));
            let mut iter = rev
                .range::<(&str, u64)>((start, end))
                .map_err(CatalogError::from)?;
            for entry in &mut iter {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (entry_tenant, id) = k.value();
                if entry_tenant != tenant_id {
                    break;
                }
                ids_and_keys.push((id, v.value().to_vec()));
            }
            drop(iter);
            for (id, _) in &ids_and_keys {
                rev.remove((tenant_id, *id)).map_err(CatalogError::from)?;
            }
        }
        if ids_and_keys.is_empty() {
            continue;
        }
        let fwd_name = fwd_table_name(table, name);
        let mut fwd = open_kv_table(write_txn, fwd_table_def(&fwd_name))?;
        for (id, key) in &ids_and_keys {
            fwd.remove((tenant_id, key.as_slice(), *id))
                .map_err(CatalogError::from)?;
            delta
                .lost
                .entry(name.clone())
                .or_default()
                .insert(key.clone());
        }
    }
    Ok(delta)
}

/// [`crate::catalog::Storage::drop_table`] が同一 write txn から呼ぶ。対象
/// テーブルが維持する索引（順引き・逆引き・登録簿エントリ）をすべて削除する
/// （`catalog.rs::delete_indexes_for_table_in_txn` と同じ「テーブルのライフ
/// サイクルに追随して掃除する」設計判断。残置すると同名テーブルの再作成後に
/// 旧テナント・旧スキーマ由来のキーが混入し、fail-open な誤判定を招く）。
pub(crate) fn drop_indexes_for_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<(), CatalogError> {
    // 登録簿はテナント単位（モジュール doc 参照）だが、fwd／rev の実テーブルは
    // `(table, name)` で共有するため、削除は一意な索引名ごとに 1 回で足りる。
    let entries = registry_entries_for_table(write_txn, table)?;
    if entries.is_empty() {
        return Ok(());
    }
    let unique_names: BTreeSet<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    for name in &unique_names {
        let fwd_name = fwd_table_name(table, name);
        let rev_name = rev_table_name(table, name);
        match write_txn.delete_table(fwd_table_def(&fwd_name)) {
            Ok(_) | Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(e.into()),
        }
        match write_txn.delete_table(rev_table_def(&rev_name)) {
            Ok(_) | Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut reg = write_txn.open_table(REGISTRY_TABLE)?;
    for (name, tenant) in &entries {
        reg.remove((table, name.as_str(), tenant.as_str()))?;
    }
    Ok(())
}

/// `columns`（`schema` の生存列名）に対応する索引を、テナント `tenant_id` の
/// 現在行から構築し、そのテナントに限り登録簿へ登録する（未登録の索引を
/// フォールバック走査で判定した直後に呼ぶ「初回だけそのテナント分を走査する」
/// 保守操作。DDL 級のコスト）。既に登録済みなら何もしない（複数の
/// `FOREIGN KEY` が同じ列集合を要求する場合、同一テナントに対する冪等性）。
///
/// **テナント境界（Issue #1071 レビュー指摘 P0）**: 走査は
/// `(tenant_id, 0)..=(tenant_id, u64::MAX)` の物理キー範囲に閉じ、他テナントの
/// 行数・破損状態には一切触れない（`clear_tenant_in_txn` と同じ範囲指定）。
/// fwd／rev の実テーブルは `(table, name)` で全テナント共有だが、この関数が
/// 書き込むのは `tenant_id` のエントリだけであり、他テナントの既存エントリは
/// 変更しない（`redb::WriteTransaction::open_table` は get-or-create のため、
/// 複数テナントが順に backfill しても互いの索引を破壊しない）。
///
/// 行ヘッダのテナントと物理キーのテナントの整合は
/// [`crate::storage::verify_row_key_tenant`] で検証し、不整合な DB は
/// `CorruptSchema` として fail-closed に拒否する（
/// `constraint::table_has_duplicate_unique_key` と同じ判断）。
pub(crate) fn ensure_index_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    schema: &TableSchema,
    columns: &[String],
    tenant_id: &str,
) -> Result<(), TenantWriteError> {
    let name = index_name_for_columns(columns);
    if is_registered(write_txn, table, &name, tenant_id)? {
        return Ok(());
    }
    let spec = resolve_key_spec(schema, columns)?;
    let mask = mask_for_spec(schema, &spec);

    let fwd_name = fwd_table_name(table, &name);
    let rev_name = rev_table_name(table, &name);
    let row_table_name = crate::catalog::user_rows_table_name(table);
    match write_txn.open_table(crate::catalog::user_rows_table_def(&row_table_name)) {
        Ok(row_table) => {
            let mut fwd = open_kv_table(write_txn, fwd_table_def(&fwd_name))?;
            let mut rev = open_kv_table(write_txn, rev_table_def(&rev_name))?;
            let start = std::ops::Bound::Included((tenant_id, 0u64));
            let end = std::ops::Bound::Included((tenant_id, u64::MAX));
            let mut iter = row_table
                .range::<(&str, u64)>((start, end))
                .map_err(CatalogError::from)?;
            for entry in &mut iter {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (key_tenant, id) = k.value();
                if key_tenant != tenant_id {
                    break;
                }
                let buf = v.value();
                let (row_tenant, _visibility, _offset) = crate::storage::decode_row_header(buf)
                    .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
                crate::storage::verify_row_key_tenant(key_tenant, row_tenant)
                    .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
                let values = decode_key_columns(schema, &mask, buf)?;
                if let Some(key) = key_bytes(&spec, &values).map_err(internal)? {
                    fwd.insert((key_tenant, key.as_slice(), id), ())
                        .map_err(CatalogError::from)?;
                    rev.insert((key_tenant, id), key.as_slice())
                        .map_err(CatalogError::from)?;
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {
            // 行が 1 件もない。索引テーブル自体は空のまま作成しておく
            // （後続の `open_table` が確実に成功するように）。
            let _ = open_kv_table(write_txn, fwd_table_def(&fwd_name))?;
            let _ = open_kv_table(write_txn, rev_table_def(&rev_name))?;
        }
        Err(e) => {
            return Err(TenantWriteError::from(crate::catalog::map_row_table_error(
                e,
            )))
        }
    }
    let mut reg = open_kv_table(write_txn, REGISTRY_TABLE)?;
    reg.insert((table, name.as_str(), tenant_id), ())
        .map_err(CatalogError::from)?;
    Ok(())
}

/// 順引き索引 1 本の中で、テナント `tenant_id` に完全一致するキー `key` を持つ
/// 行が 1 件でも存在するかを返す。
fn key_present<T>(fwd: &T, tenant_id: &str, key: &[u8]) -> Result<bool, CatalogError>
where
    T: ReadableTable<(&'static str, &'static [u8], u64), ()>,
{
    let start = std::ops::Bound::Included((tenant_id, key, 0u64));
    let end = std::ops::Bound::Included((tenant_id, key, u64::MAX));
    let mut iter = fwd
        .range::<(&str, &[u8], u64)>((start, end))
        .map_err(CatalogError::from)?;
    match iter.next() {
        None => Ok(false),
        Some(Ok(_)) => Ok(true),
        Some(Err(e)) => Err(CatalogError::from(e)),
    }
}

/// 順引き索引 1 本の中に、テナント `tenant_id` のエントリ（キー値を問わない）が
/// 1 件でも存在するかを返す（`TRUNCATE` 後の「このテナントの参照元行が
/// 1 件でも残っているか」判定に使う。個々のキー値の一致は見ない、粗い判定）。
fn any_entry_present<T>(fwd: &T, tenant_id: &str) -> Result<bool, CatalogError>
where
    T: ReadableTable<(&'static str, &'static [u8], u64), ()>,
{
    let start = std::ops::Bound::Included((tenant_id, &[][..], 0u64));
    let mut iter = fwd
        .range::<(&str, &[u8], u64)>((start, std::ops::Bound::Unbounded))
        .map_err(CatalogError::from)?;
    match iter.next() {
        None => Ok(false),
        Some(Ok((k, _))) => Ok(k.value().0 == tenant_id),
        Some(Err(e)) => Err(CatalogError::from(e)),
    }
}

/// `fwd_name` が `write_txn` の中に実在するかを、`redb::WriteTransaction`
/// の `open_table` に頼らず明示的に確認する（Issue #1071 レビュー指摘 P0:
/// `WriteTransaction::open_table` は get-or-create のため、レジストリに
/// エントリがあるのに実テーブルが存在しない破損状態でも `TableDoesNotExist`
/// を返さず黙って空テーブルを新規作成してしまい、[`key_present`]／
/// [`any_entry_present`] が「エントリ 0 件 = 参照なし」と区別できず
/// fail-open になる。`list_tables` によるテーブル名の存在確認だけがこの
/// 状態を検出できる）。
fn fwd_table_exists(
    write_txn: &redb::WriteTransaction,
    fwd_name: &str,
) -> Result<bool, CatalogError> {
    use redb::TableHandle;
    let mut tables = write_txn.list_tables().map_err(CatalogError::from)?;
    Ok(tables.any(|t| t.name() == fwd_name))
}

/// 登録済み（[`is_registered`] が `true`）の索引の順引きテーブルを読み取り用に
/// 開く。登録簿にエントリがあるのに実テーブルが存在しなければ、
/// `ensure_index_in_txn` が登録前に必ず作成する不変条件が破られた破損状態
/// として `CorruptSchema` で fail-closed に拒否する（黙って空テーブルとして
/// 扱うと「参照なし」の誤判定で `DELETE`／`TRUNCATE` を許してしまう）。
fn open_fwd_readable<'a>(
    write_txn: &'a redb::WriteTransaction,
    fwd_name: &str,
) -> Result<FwdTable<'a>, TenantWriteError> {
    if !fwd_table_exists(write_txn, fwd_name).map_err(TenantWriteError::from)? {
        return Err(TenantWriteError::from(CatalogError::CorruptSchema(
            format!("registered key index is missing its forward table {fwd_name:?}"),
        )));
    }
    write_txn
        .open_table(fwd_table_def(fwd_name))
        .map_err(|e| TenantWriteError::from(CatalogError::from(e)))
}

/// [`crate::constraint`] の参照元側検査（子行の INSERT／UPDATE）が呼ぶ。
/// `table`（親テーブル）が `columns` に対応する索引を登録済みなら、`keys` の
/// 全キーがテナント `tenant_id` に存在することを索引照会だけで確認する
/// （`Ok(Some(()))`）。1 つでも欠ければ即座に
/// [`TenantWriteError::ForeignKeyViolation`]。索引が未登録なら `Ok(None)`
/// （呼び出し元がフォールバック走査へ切り替える合図）。
pub(crate) fn all_keys_exist_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
    columns: &[String],
    tenant_id: &str,
    keys: &BTreeSet<Vec<u8>>,
) -> Result<Option<()>, TenantWriteError> {
    let name = index_name_for_columns(columns);
    if !is_registered(write_txn, table, &name, tenant_id)? {
        return Ok(None);
    }
    let fwd_name = fwd_table_name(table, &name);
    let fwd = open_fwd_readable(write_txn, &fwd_name)?;
    for key in keys {
        if !key_present(&fwd, tenant_id, key)? {
            return Err(TenantWriteError::ForeignKeyViolation);
        }
    }
    Ok(Some(()))
}

/// [`crate::constraint`] の参照先側検査（親行の DELETE／UPDATE／TRUNCATE）が
/// 呼ぶ。`child_table`（子テーブル）が `child_columns`（`FOREIGN KEY` の参照元
/// 列）に対応する索引を登録済みなら、`lost_keys` の**いずれか**をテナント
/// `tenant_id` の子行が今も参照していないかを索引照会だけで確認する。
/// 参照していれば即座に [`TenantWriteError::ForeignKeyViolation`]。索引が
/// 未登録なら `Ok(None)`。
pub(crate) fn none_referenced_in_txn(
    write_txn: &redb::WriteTransaction,
    child_table: &str,
    child_columns: &[String],
    tenant_id: &str,
    lost_keys: &BTreeSet<Vec<u8>>,
) -> Result<Option<()>, TenantWriteError> {
    if lost_keys.is_empty() {
        return Ok(Some(()));
    }
    let name = index_name_for_columns(child_columns);
    if !is_registered(write_txn, child_table, &name, tenant_id)? {
        return Ok(None);
    }
    let fwd_name = fwd_table_name(child_table, &name);
    let fwd = open_fwd_readable(write_txn, &fwd_name)?;
    for key in lost_keys {
        if key_present(&fwd, tenant_id, key)? {
            return Err(TenantWriteError::ForeignKeyViolation);
        }
    }
    Ok(Some(()))
}

/// `TRUNCATE`（`TenantCleared`）用: `child_table` の `child_columns` 索引に、
/// テナント `tenant_id` のエントリが 1 件でも残っていないかを確認する
/// （個々のキー値は見ない粗い判定。TRUNCATE 後は参照先テナントの行が
/// 0 件になるため、値を問わずエントリの有無だけで違反を判定できる）。
/// 索引が未登録なら `Ok(None)`。
pub(crate) fn none_referenced_for_tenant_in_txn(
    write_txn: &redb::WriteTransaction,
    child_table: &str,
    child_columns: &[String],
    tenant_id: &str,
) -> Result<Option<()>, TenantWriteError> {
    let name = index_name_for_columns(child_columns);
    if !is_registered(write_txn, child_table, &name, tenant_id)? {
        return Ok(None);
    }
    let fwd_name = fwd_table_name(child_table, &name);
    let fwd = open_fwd_readable(write_txn, &fwd_name)?;
    let present = any_entry_present(&fwd, tenant_id)?;
    if present {
        return Err(TenantWriteError::ForeignKeyViolation);
    }
    Ok(Some(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, ForeignKeyDef, TableSchema};
    use crate::policy::PolicyContext;
    use crate::recovery::ledger::LedgerWrite;
    use crate::row_codec::Value;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    fn tmp_storage(label: &str) -> (Storage, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn ctx(tenant: &'static str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id")
    }

    fn parent_schema() -> TableSchema {
        TableSchema::new(
            "parents",
            vec![ColumnDef::new("code", ColumnType::Text, false)],
        )
        .with_primary_key(vec!["code".to_string()])
    }

    fn child_schema() -> TableSchema {
        TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_code", ColumnType::Text, true)],
        )
        .with_foreign_keys(vec![ForeignKeyDef::new(
            vec!["parent_code".to_string()],
            "parents".to_string(),
            vec!["code".to_string()],
        )])
    }

    /// 索引を明示的に構築した状態で、INSERT・UPDATE（キー変更あり／なし）・
    /// DELETE の各操作後に「行から再計算した索引」と「永続索引（順引き・
    /// 逆引き）」が一致することを確認する（受入基準: 索引の一貫性）。
    #[test]
    fn sync_keeps_forward_and_reverse_index_consistent() {
        let (storage, _guard) = tmp_storage("key-index-consistency");
        let schema = parent_schema();
        storage.create_table(&schema).expect("create table");
        let columns = vec!["code".to_string()];

        // 索引を先に構築（フォールバック経路を経由せず、直接 ensure を呼ぶ）。
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            ensure_index_in_txn(&write_txn, "parents", &schema, &columns, "tenant-a")
                .expect("ensure index");
            write_txn.commit_raw_for_test().expect("commit");
        }
        assert!(index_has_entry(&storage, "parents", &columns, "tenant-a").is_ok());

        // INSERT: id=1, code="a"
        crate::tenant::insert_typed_row(
            &storage,
            "parents",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("a".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("insert must succeed");
        assert_key_present(&storage, "parents", &columns, "tenant-a", "a", true);

        // UPDATE: code を "a" -> "b" に変更（列指定 UPDATE。`update_row` は
        // VECTOR 列を持たないテーブルを無条件に拒否する既知のギャップ
        // （`TableSchema::validate_embedding_dim` ドキュメント参照。Issue #995
        // のスコープ外）があるため、列指定 UPDATE を使う）。旧キーは索引から
        // 消え、新キーが現れる。
        crate::tenant::update_row_columns_unchecked(
            &storage,
            "parents",
            &ctx("tenant-a"),
            1,
            &[(0, Value::Text("b".to_string()))],
            LedgerWrite::Disabled,
            None,
        )
        .expect("update must succeed");
        assert_key_present(&storage, "parents", &columns, "tenant-a", "a", false);
        assert_key_present(&storage, "parents", &columns, "tenant-a", "b", true);

        // DELETE: 索引からも消える。
        crate::tenant::delete_row(
            &storage,
            "parents",
            &ctx("tenant-a"),
            1,
            &crate::recovery::required_op_id::OperationId::parse("op-3").expect("op id"),
        )
        .expect("delete must succeed");
        assert_key_present(&storage, "parents", &columns, "tenant-a", "b", false);
    }

    /// 2 テナントが同じキー値を持っても索引上で衝突せず、片方の DELETE が
    /// もう片方の索引項目を消さない（受入基準: テナント分離）。索引の登録簿
    /// もテナント単位であることを確認する（Issue #1071 レビュー指摘 P0:
    /// `ensure_index_in_txn` が `tenant-a` の backfill を行っても `tenant-b`
    /// は未登録のままであり、`tenant-b` 自身の backfill を経て初めて索引経路
    /// に切り替わる）。
    #[test]
    fn tenant_isolation_in_index() {
        let (storage, _guard) = tmp_storage("key-index-tenant-isolation");
        let schema = parent_schema();
        storage.create_table(&schema).expect("create table");
        let columns = vec!["code".to_string()];
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            ensure_index_in_txn(&write_txn, "parents", &schema, &columns, "tenant-a")
                .expect("ensure index for tenant-a");
            write_txn.commit_raw_for_test().expect("commit");
        }

        for tenant in ["tenant-a", "tenant-b"] {
            crate::tenant::insert_typed_row(
                &storage,
                "parents",
                &ctx(tenant),
                1,
                Visibility::Public,
                &[Value::Text("shared".to_string())],
                &crate::recovery::required_op_id::OperationId::parse(&format!("op-{tenant}"))
                    .expect("op id"),
            )
            .expect("insert must succeed for each tenant independently");
        }

        // `tenant-b` はまだ backfill していないため、`tenant-a` の backfill の
        // 副作用を受けず未登録のまま（フォールバックの合図 `Ok(None)`）。
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            let mut keys = BTreeSet::new();
            keys.insert(encode_text_key("shared"));
            let result = all_keys_exist_in_txn(&write_txn, "parents", &columns, "tenant-b", &keys)
                .expect("query must not error");
            assert!(
                result.is_none(),
                "tenant-a's backfill must not register the index for tenant-b"
            );
            write_txn.abort().expect("abort");
        }
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            ensure_index_in_txn(&write_txn, "parents", &schema, &columns, "tenant-b")
                .expect("ensure index for tenant-b");
            write_txn.commit_raw_for_test().expect("commit");
        }

        assert_key_present(&storage, "parents", &columns, "tenant-a", "shared", true);
        assert_key_present(&storage, "parents", &columns, "tenant-b", "shared", true);

        crate::tenant::delete_row(
            &storage,
            "parents",
            &ctx("tenant-a"),
            1,
            &crate::recovery::required_op_id::OperationId::parse("op-del").expect("op id"),
        )
        .expect("delete must succeed");
        assert_key_present(&storage, "parents", &columns, "tenant-a", "shared", false);
        assert_key_present(&storage, "parents", &columns, "tenant-b", "shared", true);
    }

    /// 索引未登録の状態（旧 DB を模した状態）では `all_keys_exist_in_txn`／
    /// `none_referenced_in_txn` が `None`（フォールバックの合図）を返し、
    /// `ensure_index_in_txn` を挟むと以後は索引経路（`Some`）に切り替わる。
    #[test]
    fn falls_back_when_unregistered_then_switches_to_index() {
        let (storage, _guard) = tmp_storage("key-index-fallback");
        let schema = parent_schema();
        storage.create_table(&schema).expect("create table");
        let columns = vec!["code".to_string()];

        crate::tenant::insert_typed_row(
            &storage,
            "parents",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("a".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-1").expect("op id"),
        )
        .expect("insert must succeed");

        let write_txn = storage.begin_write_txn().expect("begin write");
        let mut keys = BTreeSet::new();
        keys.insert(encode_text_key("a"));
        let result = all_keys_exist_in_txn(&write_txn, "parents", &columns, "tenant-a", &keys)
            .expect("query must not error");
        assert!(result.is_none(), "unregistered index must report None");
        ensure_index_in_txn(&write_txn, "parents", &schema, &columns, "tenant-a")
            .expect("ensure index");
        let result = all_keys_exist_in_txn(&write_txn, "parents", &columns, "tenant-a", &keys)
            .expect("query must not error");
        assert_eq!(result, Some(()));
        write_txn.abort().expect("abort");
    }

    fn encode_text_key(text: &str) -> Vec<u8> {
        let spec = KeySpec::new(vec![0], NullPolicy::Skip);
        let values = vec![Some(crate::row_codec::ScalarRef::Text(text))];
        key_bytes(&spec, &values)
            .expect("encode must not fail")
            .expect("value must not be NULL")
    }

    fn index_has_entry(
        storage: &Storage,
        table: &str,
        columns: &[String],
        tenant: &str,
    ) -> Result<(), ()> {
        let write_txn = storage.begin_write_txn().map_err(|_| ())?;
        let name = index_name_for_columns(columns);
        let registered = is_registered(&write_txn, table, &name, tenant).map_err(|_| ())?;
        write_txn.abort().map_err(|_| ())?;
        if registered {
            Ok(())
        } else {
            Err(())
        }
    }

    fn assert_key_present(
        storage: &Storage,
        table: &str,
        columns: &[String],
        tenant: &str,
        text: &str,
        expected: bool,
    ) {
        let write_txn = storage.begin_write_txn().expect("begin write");
        let mut keys = BTreeSet::new();
        keys.insert(encode_text_key(text));
        // `all_keys_exist_in_txn` は不在キーを `Err(ForeignKeyViolation)`
        // （fail-fast の本番契約。`constraint::verify_required_parent_keys`
        // 参照）で表すため、テストのブール判定へ変換する。`Ok(None)` は
        // 索引未登録（テスト側の前提が崩れている）を意味し区別して失敗させる。
        let present = match all_keys_exist_in_txn(&write_txn, table, columns, tenant, &keys) {
            Ok(Some(())) => true,
            Ok(None) => panic!("index for {table}/{columns:?} must be registered in this test"),
            Err(TenantWriteError::ForeignKeyViolation) => false,
            Err(e) => panic!("unexpected query error: {e}"),
        };
        write_txn.abort().expect("abort");
        assert_eq!(
            present, expected,
            "key {text:?} presence mismatch for tenant {tenant}"
        );
    }

    /// 受入基準: FK 検査コストがテナントの保有行数に比例しないことを、行数の
    /// 異なる 2 条件（N=20・N=400）で「参照先の値の組を復元した回数」
    /// （`constraint::decode_key_columns` の呼び出し回数。索引未登録時の全行
    /// 走査ではテナントの行数分呼ばれ、索引経路では呼ばれない）のカウンタが
    /// 一致することにより確認する（壁時計時間ではなく決定的なカウンタで判定
    /// する。`docs/design/benchmark-judgement-policy.md` §5 参照。カウンタは
    /// スレッドローカルのため、cargo test の並列実行下でも他テストの影響を
    /// 受けない）。
    #[test]
    fn foreign_key_check_cost_does_not_scale_with_row_count() {
        use crate::constraint::test_counters;

        fn run(n: u64) -> u64 {
            let (storage, _guard) = tmp_storage(&format!("key-index-scale-{n}"));
            let parent = parent_schema();
            storage.create_table(&parent).expect("create parent");
            let child = child_schema();
            storage.create_table(&child).expect("create child");

            for i in 0..n {
                crate::tenant::insert_typed_row(
                    &storage,
                    "parents",
                    &ctx("tenant-a"),
                    i,
                    Visibility::Public,
                    &[Value::Text(format!("code-{i}"))],
                    &crate::recovery::required_op_id::OperationId::parse(&format!("op-p-{i}"))
                        .expect("op id"),
                )
                .expect("parent insert");
            }
            // 子行 1 件を最後の親キーへ向けて挿入し、索引を構築させる
            // （初回のみ全走査。以後は索引経路）。
            crate::tenant::insert_typed_row(
                &storage,
                "children",
                &ctx("tenant-a"),
                0,
                Visibility::Public,
                &[Value::Text(format!("code-{}", n - 1))],
                &crate::recovery::required_op_id::OperationId::parse("op-c-0").expect("op id"),
            )
            .expect("child insert must satisfy the foreign key");

            // 索引構築（backfill）済みの状態で、追加の子 INSERT のコストを測る。
            test_counters::reset();
            crate::tenant::insert_typed_row(
                &storage,
                "children",
                &ctx("tenant-a"),
                1,
                Visibility::Public,
                &[Value::Text(format!("code-{}", n - 1))],
                &crate::recovery::required_op_id::OperationId::parse("op-c-1").expect("op id"),
            )
            .expect("second child insert must satisfy the foreign key");
            test_counters::get()
        }

        let small = run(20);
        let large = run(400);
        assert_eq!(
            small, large,
            "post-backfill foreign key check must not visit a number of rows \
             proportional to the tenant's row count"
        );
    }

    /// 登録簿にエントリがあるのに順引きテーブルが実在しない破損状態
    /// （`ensure_index_in_txn` が両者を同一 write_txn で作成する不変条件が
    /// 破られた状態）を人為的に作り、`CorruptSchema` で fail-closed に
    /// 拒否されることを確認する（Issue #1071 レビュー指摘 P0:
    /// `redb::WriteTransaction::open_table` は get-or-create のため、素朴な
    /// 実装では黙って空テーブル扱いになり「参照なし」と誤判定して
    /// `DELETE`／`TRUNCATE` を許してしまう）。
    #[test]
    fn missing_forward_table_for_registered_index_is_corrupt_schema() {
        let (storage, _guard) = tmp_storage("key-index-corrupt-fwd");
        let schema = parent_schema();
        storage.create_table(&schema).expect("create table");
        let columns = vec!["code".to_string()];
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            ensure_index_in_txn(&write_txn, "parents", &schema, &columns, "tenant-a")
                .expect("ensure index");
            write_txn.commit_raw_for_test().expect("commit");
        }
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            let name = index_name_for_columns(&columns);
            let fwd_name = fwd_table_name("parents", &name);
            write_txn
                .delete_table(fwd_table_def(&fwd_name))
                .expect("delete forward table (simulate corruption)");
            write_txn.commit_raw_for_test().expect("commit");
        }

        let write_txn = storage.begin_write_txn().expect("begin write");
        let mut keys = BTreeSet::new();
        keys.insert(encode_text_key("a"));
        let result = all_keys_exist_in_txn(&write_txn, "parents", &columns, "tenant-a", &keys);
        assert!(
            matches!(
                result,
                Err(TenantWriteError::Catalog(CatalogError::CorruptSchema(_)))
            ),
            "registered-but-missing forward table must be fail-closed, got {result:?}"
        );
        let result = none_referenced_in_txn(&write_txn, "parents", &columns, "tenant-a", &keys);
        assert!(
            matches!(
                result,
                Err(TenantWriteError::Catalog(CatalogError::CorruptSchema(_)))
            ),
            "registered-but-missing forward table must be fail-closed, got {result:?}"
        );
        write_txn.abort().expect("abort");
    }

    fn self_ref_schema() -> TableSchema {
        TableSchema::new(
            "nodes",
            vec![
                ColumnDef::new("code", ColumnType::Text, false),
                ColumnDef::new("parent_code", ColumnType::Text, true),
            ],
        )
        .with_primary_key(vec!["code".to_string()])
        .with_foreign_keys(vec![ForeignKeyDef::new(
            vec!["parent_code".to_string()],
            "nodes".to_string(),
            vec!["code".to_string()],
        )])
    }

    /// 自己参照 `FOREIGN KEY` を持つテーブルで、`code`（親列）索引がこの文で
    /// 初めて構築される状況を人為的に再現し、`constraint::prepare_referenced_key_indexes_in_txn`
    /// を旧行の物理削除**前**に呼んでおけば、削除された旧キーへの残存参照
    /// （B が今も `parent_code="X"` を保持）を正しく検出できることを確認する
    /// （cursor bugbot 指摘・Issue #1071 レビュー指摘: この事前 backfill を
    /// 省くと、索引の初回構築が削除後の状態から行われ、削除された旧キーが
    /// 逆引き索引の pre-image に一度も現れず検査をすり抜けていた）。
    #[test]
    fn self_ref_replace_detects_dangling_reference_when_index_first_built_this_statement() {
        let (storage, _guard) = tmp_storage("key-index-self-ref-dangling");
        let schema = self_ref_schema();
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "nodes",
            &ctx("tenant-a"),
            0,
            Visibility::Public,
            &[Value::Text("X".to_string()), Value::Null],
            &crate::recovery::required_op_id::OperationId::parse("op-a").expect("op id"),
        )
        .expect("insert A");
        crate::tenant::insert_typed_row(
            &storage,
            "nodes",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("Y".to_string()), Value::Text("X".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-b").expect("op id"),
        )
        .expect("insert B referencing X");

        // ここまでの通常 INSERT で `code`（親列）索引は B の子側 FK 検査
        // （`verify_required_parent_keys`）経由で自然に backfill・登録済み
        // （「別の文で既に登録済み」の通常ケースは Issue #1071 導入後も安全）。
        // 登録簿・実テーブルを人為的に削除し、「このファイル形置換の文で
        // 初めて backfill される」旧 DB・初回参照の状態を再現する。
        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            let name = index_name_for_columns(&["code".to_string()]);
            {
                let mut reg = write_txn.open_table(REGISTRY_TABLE).expect("open registry");
                reg.remove(("nodes", name.as_str(), "tenant-a"))
                    .expect("remove registry entry");
            }
            let fwd_name = fwd_table_name("nodes", &name);
            let rev_name = rev_table_name("nodes", &name);
            write_txn
                .delete_table(fwd_table_def(&fwd_name))
                .expect("delete fwd");
            write_txn
                .delete_table(rev_table_def(&rev_name))
                .expect("delete rev");
            write_txn.commit_raw_for_test().expect("commit");
        }

        // `tenant::replace_typed_rows_by_text_key` と同じ検査シーケンス
        // （prepare → 物理削除 → 参照先側検査）をこのテストモジュールから
        // 直接構成する（`tenant.rs` の非公開ヘルパーへは届かないため）。A を
        // 削除し、X を再供給しない（B は今も X を参照したまま）。
        let write_txn = storage.begin_write_txn().expect("begin write");
        crate::constraint::prepare_referenced_key_indexes_in_txn(
            &write_txn, "nodes", &schema, "tenant-a",
        )
        .expect("prepare");
        {
            let row_table_name = crate::catalog::user_rows_table_name("nodes");
            let mut row_table = write_txn
                .open_table(crate::catalog::user_rows_table_def(&row_table_name))
                .expect("open row table");
            row_table.remove(("tenant-a", 0u64)).expect("remove A");
        }
        let result = crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            "nodes",
            &schema,
            "tenant-a",
            crate::constraint::ReferencedRowsChange::Removed { ids: &[0u64] },
            crate::constraint::FkCheckMode::All,
        );
        assert!(
            matches!(result, Err(TenantWriteError::ForeignKeyViolation)),
            "B still references the removed key X; must be rejected, got {result:?}"
        );
        write_txn.abort().expect("abort");
    }

    /// 上のテストと同じ再現手順で、削除された X が**新規行で再供給される**
    /// 場合は違反にならないことを確認する（P1 の「再供給時の誤検知」回帰と
    /// 組み合わせて自己参照ケースを固定する）。
    #[test]
    fn self_ref_replace_allows_resupplied_key_when_index_first_built_this_statement() {
        let (storage, _guard) = tmp_storage("key-index-self-ref-resupply");
        let schema = self_ref_schema();
        storage.create_table(&schema).expect("create table");

        crate::tenant::insert_typed_row(
            &storage,
            "nodes",
            &ctx("tenant-a"),
            0,
            Visibility::Public,
            &[Value::Text("X".to_string()), Value::Null],
            &crate::recovery::required_op_id::OperationId::parse("op-a").expect("op id"),
        )
        .expect("insert A");
        crate::tenant::insert_typed_row(
            &storage,
            "nodes",
            &ctx("tenant-a"),
            1,
            Visibility::Public,
            &[Value::Text("Y".to_string()), Value::Text("X".to_string())],
            &crate::recovery::required_op_id::OperationId::parse("op-b").expect("op id"),
        )
        .expect("insert B referencing X");

        {
            let write_txn = storage.begin_write_txn().expect("begin write");
            let name = index_name_for_columns(&["code".to_string()]);
            {
                let mut reg = write_txn.open_table(REGISTRY_TABLE).expect("open registry");
                reg.remove(("nodes", name.as_str(), "tenant-a"))
                    .expect("remove registry entry");
            }
            let fwd_name = fwd_table_name("nodes", &name);
            let rev_name = rev_table_name("nodes", &name);
            write_txn
                .delete_table(fwd_table_def(&fwd_name))
                .expect("delete fwd");
            write_txn
                .delete_table(rev_table_def(&rev_name))
                .expect("delete rev");
            write_txn.commit_raw_for_test().expect("commit");
        }

        // 今回は A（id=0）を削除した直後に、同じ write_txn 内で新規行
        // C（id=2, code="X"）を挿入して X を再供給する。子側検査
        // （`enforce_row_constraints_in_txn`）を先に走らせて索引へ再供給分を
        // 同期してから、参照先側検査（削除分の同期）を走らせる——
        // `tenant::replace_typed_rows_by_text_key` と同じ呼び出し順序。
        let write_txn = storage.begin_write_txn().expect("begin write");
        crate::constraint::prepare_referenced_key_indexes_in_txn(
            &write_txn, "nodes", &schema, "tenant-a",
        )
        .expect("prepare");
        {
            let row_table_name = crate::catalog::user_rows_table_name("nodes");
            let mut row_table = write_txn
                .open_table(crate::catalog::user_rows_table_def(&row_table_name))
                .expect("open row table");
            row_table.remove(("tenant-a", 0u64)).expect("remove A");
            let row = crate::storage::RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &crate::row_codec::encode_scalar_columns(
                    &schema,
                    &[Value::Text("X".to_string()), Value::Null],
                )
                .expect("encode C"),
            };
            let encoded = crate::storage::encode_row(&row).expect("encode row C");
            row_table
                .insert(("tenant-a", 2u64), encoded.as_slice())
                .expect("insert C resupplying X");
        }
        crate::constraint::enforce_row_constraints_in_txn(
            &write_txn,
            "nodes",
            &schema,
            "tenant-a",
            &[2u64],
            crate::constraint::FkCheckMode::All,
        )
        .expect("C's own FK check (parent_code=NULL) must pass");
        let result = crate::constraint::enforce_referencing_rows_in_txn(
            &write_txn,
            "nodes",
            &schema,
            "tenant-a",
            crate::constraint::ReferencedRowsChange::Removed { ids: &[0u64] },
            crate::constraint::FkCheckMode::All,
        );
        assert!(
            result.is_ok(),
            "X is resupplied by C; B's reference must remain satisfied, got {result:?}"
        );
        write_txn.abort().expect("abort");
    }
}
