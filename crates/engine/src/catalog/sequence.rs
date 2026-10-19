//! SEQUENCE のカタログ定義と採番状態の格納土台（TABLE-24・TABLE-27 のポインタ・
//! Issue #1688。親: #1686。設計記録: `docs/design/sequence-namespace-and-ledger.md`）。
//!
//! 役割: SEQUENCE の定義を専用カタログ表 [`SEQUENCES_TABLE`] へ永続化し、`CREATE SEQUENCE`／
//! `DROP SEQUENCE`（`sql::ddl` が呼ぶ）の Storage 水準の実体を提供する。SEQUENCE は表・ビュー・
//! 索引・matview と同じ relation 名前空間に属するため、衝突・種別の判定は呼び出し元
//! （`catalog.rs` の各 DDL・[`super::materialized`]）が [`sequence_exists_in_txn`] を併記する。
//!
//! テナント境界（P0）: 定義は表・ビュー・ENUM 型と同じく全テナント共有カタログに置く
//! （#1711 の SCHEMA 導入までは単一の `public`。設計記録 D3）。一方、採番状態は
//! [`SEQUENCE_STATE_TABLE`] のキー先頭を `tenant_id` にして構造的にテナント単位へ分ける
//! （値の書き込み経路は #1690 が追加する。本 Issue では DROP の全テナント分削除と
//! テスト用の種まき・読み取りに限る）。内部 ID は単調増加で再利用しないため、同名で作り直した
//! SEQUENCE が過去の状態行を拾うことはない。
//!
//! fail-closed: 定義 blob の未知の版・未知のタグ・長さ不一致・範囲不変条件の違反は
//! `CorruptSchema` とし、decode で黙って補正しない。

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::{
    get_table_schema_in_txn, list_tables_in_txn, validate_identifier, CatalogError, ColumnDefault,
    Result, Storage, MAX_LIST_TABLES,
};

/// SEQUENCE 定義の専用カタログ表。キーは `(schema, name)`。#1711 より前の schema は
/// 常に [`DEFAULT_SEQUENCE_SCHEMA`]（`sequence_key` が唯一の組み立て点）。
pub(crate) const SEQUENCES_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("sequences");

/// 採番状態の表。キーは `(tenant_id, sequence_id)`。値の形式は #1690 が決める。
pub(crate) const SEQUENCE_STATE_TABLE: TableDefinition<(&str, u64), &[u8]> =
    TableDefinition::new("sequence_state");

/// 内部 ID カウンタ（固定キー 1 行の単調カウンタ。再利用しない）。
const SEQUENCE_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("sequence_id_counter");

/// [`SEQUENCE_ID_COUNTER_TABLE`] の唯一のキー。
const SEQUENCE_ID_COUNTER_KEY: &str = "next_id";

/// #1711（SCHEMA）より前の SEQUENCE が属する schema 名。
const DEFAULT_SEQUENCE_SCHEMA: &str = "public";

/// 定義 blob の版。
const SEQUENCE_DEF_VERSION: u8 = 1;

/// 型タグ: bigint（本 Issue ではこれのみ）。
const TYPE_TAG_BIGINT: u8 = 1;

/// 定義 blob の固定長（版 1 + id 8 + 型 1 + i64 × 5 + cycle 1 + owned_by 1）。
const SEQUENCE_DEF_LEN: usize = 1 + 8 + 1 + 8 * 5 + 1 + 1;

/// 登録可能な SEQUENCE の総数上限（[`MAX_LIST_TABLES`] と同じ既定値。DoS 対策）。
const MAX_SEQUENCES: usize = MAX_LIST_TABLES;

/// SEQUENCE の定義値（bigint 前提。`CACHE`・`OWNED BY` は後続 Issue のため持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceParams {
    pub(crate) increment: i64,
    pub(crate) min_value: i64,
    pub(crate) max_value: i64,
    pub(crate) start: i64,
    pub(crate) cycle: bool,
}

impl SequenceParams {
    /// 範囲不変条件（`increment != 0`・`min < max`・`min <= start <= max`）を検証する。
    /// SQL 表層（`sql::ddl`）が先に `22023` で拒否するため、ここへ到達するのは
    /// Rust API の直接呼び出しと破損 blob のみ（多層防御）。
    fn validate(&self) -> Result<()> {
        if self.increment == 0
            || self.min_value >= self.max_value
            || self.start < self.min_value
            || self.start > self.max_value
        {
            return Err(CatalogError::Invalid(
                "invalid sequence parameters".to_string(),
            ));
        }
        Ok(())
    }
}

/// 定義キー `(schema, name)` の唯一の組み立て点（#1711・#1717 での差し替え点）。
fn sequence_key(name: &str) -> (&str, &str) {
    (DEFAULT_SEQUENCE_SCHEMA, name)
}

/// 定義 blob を符号化する（固定長・版バイト先頭）。
fn encode_sequence_def(id: u64, p: &SequenceParams) -> Vec<u8> {
    let mut out = Vec::with_capacity(SEQUENCE_DEF_LEN);
    out.push(SEQUENCE_DEF_VERSION);
    out.extend_from_slice(&id.to_le_bytes());
    out.push(TYPE_TAG_BIGINT);
    for v in [p.increment, p.min_value, p.max_value, p.start, 1_i64] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.push(u8::from(p.cycle));
    out.push(0); // owned_by: なし
    out
}

/// 定義 blob を復号する。未知の版・タグ・長さ不一致・不変条件違反は `CorruptSchema`
/// （fail-closed。detail は固定文言で格納バイト列の断片を含めない）。
fn decode_sequence_def(bytes: &[u8]) -> Result<(u64, SequenceParams)> {
    let corrupt = || CatalogError::CorruptSchema("corrupt sequence definition".to_string());
    if bytes.len() != SEQUENCE_DEF_LEN || bytes.first() != Some(&SEQUENCE_DEF_VERSION) {
        return Err(corrupt());
    }
    let read_i64 = |off: usize| -> Result<i64> {
        let raw: [u8; 8] = bytes
            .get(off..off + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(corrupt)?;
        Ok(i64::from_le_bytes(raw))
    };
    let id_raw: [u8; 8] = bytes
        .get(1..9)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(corrupt)?;
    let id = u64::from_le_bytes(id_raw);
    if bytes.get(9) != Some(&TYPE_TAG_BIGINT) {
        return Err(corrupt());
    }
    let params = SequenceParams {
        increment: read_i64(10)?,
        min_value: read_i64(18)?,
        max_value: read_i64(26)?,
        start: read_i64(34)?,
        cycle: match bytes.get(CYCLE_OFFSET) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(corrupt()),
        },
    };
    // cache は 1 固定、owned_by はなし固定（本 Issue では他の値を書かない）。
    if read_i64(42)? != 1 || bytes.get(CYCLE_OFFSET + 1) != Some(&0) {
        return Err(corrupt());
    }
    params.validate().map_err(|_| corrupt())?;
    Ok((id, params))
}

/// cycle バイトの位置（`10 + 8 * 5` = cache の直後。owned_by はその次の 1 バイト）。
const CYCLE_OFFSET: usize = 50;

/// `name` が [`SEQUENCES_TABLE`] に登録済みかを write txn 内で判定する（relation 名前空間の
/// 衝突・種別判定用。表が未作成なら `false` で、表を作らない）。表・ビュー・索引・matview の
/// DDL は `index_name_exists_in_txn` を呼ぶ箇所に必ず本関数を併記する。
pub(crate) fn sequence_exists_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<bool> {
    // 読み取り専用オープン（write txn の `open_table` は欠落表を作ってしまうため）。
    match ReadSource::open_table(write_txn, SEQUENCES_TABLE) {
        Ok(t) => Ok(t.get(sequence_key(name))?.is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_sequence_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(SEQUENCE_ID_COUNTER_TABLE)?;
    let current = counter
        .get(SEQUENCE_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(SEQUENCE_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// `ColumnDefault` が SEQUENCE `name` を参照するか。現状 SEQUENCE を参照する variant は
/// 無いので常に `false`。ワイルドカードを置かない網羅 match にして、#1690 で variant を足すと
/// ここがコンパイルエラーになり、`DROP SEQUENCE` の依存判定の書き漏れを防ぐ。
fn column_default_references_sequence(default: &ColumnDefault, _name: &str) -> bool {
    match default {
        ColumnDefault::Text(_)
        | ColumnDefault::Number(_)
        | ColumnDefault::Bool(_)
        | ColumnDefault::CurrentTime(_) => false,
    }
}

/// SEQUENCE `name` を DEFAULT に使う列が 1 つでもあるか（2BP01 判定）。表数は
/// [`MAX_LIST_TABLES`] で打ち切られる（`list_tables_in_txn` の上限）。
fn sequence_has_dependents_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<bool> {
    for table_name in list_tables_in_txn(write_txn)? {
        let schema = get_table_schema_in_txn(write_txn, &table_name)?;
        if schema.columns.iter().any(|c| {
            c.default_expr
                .as_ref()
                .is_some_and(|d| column_default_references_sequence(d, name))
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `(tenant_id, sequence_id)` の状態行から `sequence_id` が一致する行を全テナント分削除する。
/// `retain` で走査削除しキーを無制限に集めない。状態表が未作成なら何もしない（表を作らない）。
fn delete_sequence_state_in_txn(write_txn: &redb::WriteTransaction, id: u64) -> Result<()> {
    match ReadSource::open_table(write_txn, SEQUENCE_STATE_TABLE) {
        // 存在確認のハンドルは書き込みオープンの前に解放する（`TableAlreadyOpen` 回避）。
        Ok(t) => drop(t),
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let mut t = write_txn.open_table(SEQUENCE_STATE_TABLE)?;
    t.retain(|(_, sid), _| sid != id)?;
    Ok(())
}

impl Storage {
    /// SEQUENCE を作成する。判定順は同一 write txn 内で名前の衝突（`TableAlreadyExists`。
    /// 表・ビュー・索引・matview・SEQUENCE）→ 件数上限（`ViewLimitExceeded`）→ 保存。
    /// `sql::ddl::execute_create_sequence` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn create_sequence(&self, name: &str, params: &SequenceParams) -> Result<()> {
        validate_identifier(name)?;
        params.validate()?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        if super::table_exists_in_txn(&write_txn, name)?
            || super::view_exists_in_txn(&write_txn, name)?
            || super::index_name_exists_in_txn(&write_txn, name)?
            || super::materialized_view_exists_in_txn(&write_txn, name)?
            || sequence_exists_in_txn(&write_txn, name)?
        {
            return Err(CatalogError::TableAlreadyExists(name.to_string()));
        }
        let id = allocate_sequence_id_in_txn(&write_txn)?;
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            if table.len()? >= MAX_SEQUENCES as u64 {
                return Err(CatalogError::ViewLimitExceeded(
                    "too many sequences".to_string(),
                ));
            }
            table.insert(
                sequence_key(name),
                encode_sequence_def(id, params).as_slice(),
            )?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// SEQUENCE を削除する。名前が他種別なら `WrongObjectKind`、無ければ `TableNotFound`。
    /// 同一 write txn 内で依存 DEFAULT の検査（`DependentObjectsStillExist`）→ 定義の削除 →
    /// 全テナント分の状態行の削除を行う。
    pub(crate) fn drop_sequence(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let existing = match ReadSource::open_table(&*write_txn, SEQUENCES_TABLE) {
            Ok(t) => t.get(sequence_key(name))?.map(|g| g.value().to_vec()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let Some(bytes) = existing else {
            if super::table_exists_in_txn(&write_txn, name)?
                || super::view_exists_in_txn(&write_txn, name)?
                || super::index_name_exists_in_txn(&write_txn, name)?
                || super::materialized_view_exists_in_txn(&write_txn, name)?
            {
                return Err(CatalogError::WrongObjectKind(name.to_string()));
            }
            return Err(CatalogError::TableNotFound(name.to_string()));
        };
        let (id, _) = decode_sequence_def(&bytes)?;
        if sequence_has_dependents_in_txn(&write_txn, name)? {
            return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
        }
        {
            let mut table = write_txn.open_table(SEQUENCES_TABLE)?;
            table.remove(sequence_key(name))?;
        }
        delete_sequence_state_in_txn(&write_txn, id)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// 定義をスナップショット読み取りで取得する（`(id, params)`。無ければ `None`）。
    #[cfg_attr(not(test), allow(dead_code))] // #1689・#1690 が使う。
    pub(crate) fn sequence_definition(&self, name: &str) -> Result<Option<(u64, SequenceParams)>> {
        validate_identifier(name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(SEQUENCES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(sequence_key(name))? {
            Some(g) => Ok(Some(decode_sequence_def(g.value())?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    fn params() -> SequenceParams {
        SequenceParams {
            increment: 1,
            min_value: 1,
            max_value: i64::MAX,
            start: 1,
            cycle: false,
        }
    }

    /// 状態行をテスト用に直接書く（値の書き込み経路は #1690 が追加する）。
    fn put_state(storage: &Storage, tenant: &str, id: u64) {
        let txn = storage.begin_write_txn().expect("txn");
        {
            let mut t = txn.open_table(SEQUENCE_STATE_TABLE).expect("open");
            t.insert((tenant, id), [0u8].as_slice()).expect("insert");
        }
        crate::recovery::commit_boundary::commit(txn).expect("commit");
    }

    fn state_exists(storage: &Storage, tenant: &str, id: u64) -> bool {
        let rt = storage.db().begin_read().expect("read");
        match rt.open_table(SEQUENCE_STATE_TABLE) {
            Ok(t) => t.get((tenant, id)).expect("get").is_some(),
            Err(_) => false,
        }
    }

    #[test]
    fn blob_roundtrip() {
        let p = SequenceParams {
            increment: -2,
            min_value: i64::MIN,
            max_value: 10,
            start: 10,
            cycle: true,
        };
        let (id, back) = decode_sequence_def(&encode_sequence_def(7, &p)).expect("decode");
        assert_eq!((id, back), (7, p));
    }

    #[test]
    fn blob_corruption_is_rejected() {
        let good = encode_sequence_def(1, &params());
        let mut bad_version = good.clone();
        bad_version[0] = 9;
        let mut bad_type = good.clone();
        bad_type[9] = 9;
        let mut bad_cycle = good.clone();
        bad_cycle[50] = 2;
        let mut bad_owned = good.clone();
        bad_owned[51] = 1;
        for b in [
            bad_version,
            bad_type,
            bad_cycle,
            bad_owned,
            good[..good.len() - 1].to_vec(),
            Vec::new(),
        ] {
            assert!(matches!(
                decode_sequence_def(&b),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
        // 範囲不変条件違反（increment 0）も破損扱い。
        let mut zero_inc = good;
        zero_inc[10..18].copy_from_slice(&0_i64.to_le_bytes());
        assert!(matches!(
            decode_sequence_def(&zero_inc),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn ids_are_monotonic_and_not_reused() {
        let (s, _g) = tmp_storage("seq_ids");
        s.create_sequence("a", &params()).expect("create");
        let (id1, _) = s.sequence_definition("a").expect("def").expect("some");
        s.drop_sequence("a").expect("drop");
        s.create_sequence("a", &params()).expect("recreate");
        let (id2, _) = s.sequence_definition("a").expect("def").expect("some");
        assert!(id2 > id1);
    }

    #[test]
    fn duplicate_and_missing_names() {
        let (s, _g) = tmp_storage("seq_dup");
        s.create_sequence("a", &params()).expect("create");
        assert!(matches!(
            s.create_sequence("a", &params()),
            Err(CatalogError::TableAlreadyExists(_))
        ));
        assert!(matches!(
            s.drop_sequence("nope"),
            Err(CatalogError::TableNotFound(_))
        ));
    }

    #[test]
    fn drop_removes_state_for_all_tenants_only_for_that_sequence() {
        let (s, _g) = tmp_storage("seq_state");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let (ida, _) = s.sequence_definition("a").unwrap().unwrap();
        let (idb, _) = s.sequence_definition("b").unwrap().unwrap();
        put_state(&s, "t1", ida);
        put_state(&s, "t2", ida);
        put_state(&s, "t1", idb);
        // テナントのキーが違えば別の行（他テナントの状態は読めない）。
        assert!(!state_exists(&s, "t3", ida));
        s.drop_sequence("a").expect("drop");
        assert!(!state_exists(&s, "t1", ida));
        assert!(!state_exists(&s, "t2", ida));
        assert!(state_exists(&s, "t1", idb));
    }

    #[test]
    fn drop_without_state_table_does_not_create_it() {
        let (s, _g) = tmp_storage("seq_nostate");
        s.create_sequence("a", &params()).expect("a");
        s.drop_sequence("a").expect("drop");
        let rt = s.db().begin_read().expect("read");
        assert!(matches!(
            rt.open_table(SEQUENCE_STATE_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
    }

    #[test]
    fn invalid_params_are_rejected() {
        let (s, _g) = tmp_storage("seq_invalid");
        let mut p = params();
        p.increment = 0;
        assert!(matches!(
            s.create_sequence("a", &p),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// DROP の全走査コストの実測（設計記録が #1688 に委ねた判断。結果は設計記録へ数値で残す）。
    /// 閾値アサートは置かず、走査が完走し対象行だけが消えることのみ検証する。
    #[test]
    fn drop_scan_cost_measurement() {
        let (s, _g) = tmp_storage("seq_scan");
        s.create_sequence("a", &params()).expect("a");
        s.create_sequence("b", &params()).expect("b");
        let (ida, _) = s.sequence_definition("a").unwrap().unwrap();
        let (idb, _) = s.sequence_definition("b").unwrap().unwrap();
        let txn = s.begin_write_txn().expect("txn");
        {
            let mut t = txn.open_table(SEQUENCE_STATE_TABLE).expect("open");
            for i in 0..10_000 {
                let tenant = format!("tenant{i}");
                t.insert((tenant.as_str(), ida), [0u8].as_slice()).unwrap();
                t.insert((tenant.as_str(), idb), [0u8].as_slice()).unwrap();
            }
        }
        crate::recovery::commit_boundary::commit(txn).expect("commit");
        let start = std::time::Instant::now();
        s.drop_sequence("a").expect("drop");
        let elapsed = start.elapsed();
        eprintln!("sequence drop scan (10000 tenants x 2 sequences): {elapsed:?}");
        assert!(!state_exists(&s, "tenant0", ida));
        assert!(state_exists(&s, "tenant9999", idb));
    }
}
