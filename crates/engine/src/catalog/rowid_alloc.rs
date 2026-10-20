//! 行ストア rowid の採番器（TABLE-23・Issue #2193。設計記録:
//! `docs/design/rowid-primary-key-separation.md` D2）。
//!
//! 役割: 行識別方式が採番方式（[`super::RowIdentity::Allocated`]）の表について、表単位・
//! テナント単位の rowid（`storage::rowid::RowId`）を払い出し、状態を専用の状態表
//! [`ROWID_STATE_TABLE`] へ保持する。払い出しは行の挿入と同一の write トランザクションで行う
//! （[`allocate_in_txn`] の呼び出し元の責務）。契約の詳細は spec のポインタ（TABLE-23）を正とし、
//! 本コメントには記載しない。
//!
//! 不変条件:
//! - テナント境界（P0）: 状態表のキーはテナントを含み、他テナントの状態・行は読まない
//!   （RLS-9 のポインタ）。
//! - 非露出: 払い出した値・枯渇をエラー文面へ出さない（固定文言）。
//! - DoS: 状態表は初回払い出し時に遅延生成する。
//!
//! 呼び出し元: `delete_table_in_txn`・`rename_table_in_txn` は
//! `Storage::drop_tables`／`Storage::alter_table_rename` が同一 txn で呼ぶ。

use std::ops::RangeInclusive;

use redb::{ReadableTable, TableDefinition, TableHandle};

use super::{user_rows_table_def, user_rows_table_name, CatalogError, Result};
use crate::storage::rowid::RowId;

/// 採番状態の表。キーは `(table_name, tenant_id)`（表単位の一括削除・改名を範囲操作に
/// するため表名を先頭にする。テナントは必ずキーに含まれる）。値は最後に払い出した rowid。
pub(crate) const ROWID_STATE_TABLE: TableDefinition<(&str, &str), u64> =
    TableDefinition::new("rowid_state");

/// 枯渇（`u64` の上限超過）時の固定文言。値・テナント名を含めない。
const EXHAUSTED_MESSAGE: &str = "row id space exhausted";

/// 1 回の削除・移送で扱うキー数（ロック中の一時確保を有界にする）。
const BATCH: usize = 1024;

/// 状態表が作成済みかを返す（未作成で表を作らないために使う）。
fn state_table_exists(write_txn: &redb::WriteTransaction) -> Result<bool> {
    let exists = write_txn
        .list_tables()?
        .any(|handle| handle.name() == ROWID_STATE_TABLE.name());
    Ok(exists)
}

/// `table` の `tenant` に対して `n` 個の連続 rowid を払い出し、状態行を同一 txn へ書く。
///
/// 戻り値は払い出した閉区間（`n == 0` は `Invalid`）。呼び出し元は行ストア
/// （`user_rows/{table}`）のテーブルハンドルを開く**前**に呼ぶこと（同一 txn で二重オープンに
/// なる）。枯渇は固定文言の `Invalid` で fail-closed に拒否する。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn allocate_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    n: u64,
) -> Result<RangeInclusive<RowId>> {
    if n == 0 {
        return Err(CatalogError::Invalid(
            "row id allocation count must be positive".to_string(),
        ));
    }
    let last = {
        let state = write_txn.open_table(ROWID_STATE_TABLE)?;
        let stored = state.get((table, tenant))?.map(|g| g.value());
        stored
    };
    let last = match last {
        Some(v) => v,
        None => max_existing_rowid(write_txn, tenant, table)?,
    };
    let first = last
        .checked_add(1)
        .ok_or_else(|| CatalogError::Invalid(EXHAUSTED_MESSAGE.to_string()))?;
    let end = last
        .checked_add(n)
        .ok_or_else(|| CatalogError::Invalid(EXHAUSTED_MESSAGE.to_string()))?;
    let mut state = write_txn.open_table(ROWID_STATE_TABLE)?;
    state.insert((table, tenant), end)?;
    Ok(RowId::from_allocated(first)..=RowId::from_allocated(end))
}

/// 状態行が無いときの初期値: 行ストアの `(tenant, ..)` 範囲の最大キー（無ければ 0）。
fn max_existing_rowid(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
) -> Result<u64> {
    let row_table_name = user_rows_table_name(table);
    let rows = match write_txn.open_table(user_rows_table_def(&row_table_name)) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(super::map_row_table_error(e)),
    };
    let mut range = rows.range((tenant, 0u64)..=(tenant, u64::MAX))?;
    let last = range.next_back();
    let max = match last {
        Some(entry) => {
            let (k, _v) = entry?;
            k.value().1
        }
        None => 0,
    };
    Ok(max)
}

/// `table` の全テナント分の採番状態を削除する（`DROP TABLE` と同一 txn。同名再作成が古い
/// 状態を引き継ぐ fail-open を防ぐ）。状態表が未作成なら何もしない（表を作らない）。
pub(crate) fn delete_table_in_txn(write_txn: &redb::WriteTransaction, table: &str) -> Result<()> {
    if !state_table_exists(write_txn)? {
        return Ok(());
    }
    let mut state = write_txn.open_table(ROWID_STATE_TABLE)?;
    loop {
        let keys = collect_tenants(&state, table)?;
        if keys.is_empty() {
            return Ok(());
        }
        for tenant in &keys {
            state.remove((table, tenant.as_str()))?;
        }
    }
}

/// `old` の採番状態を `new` へ移す（`ALTER TABLE ... RENAME TO` と同一 txn）。新名側の孤児
/// 状態行を先に削除してから移す。状態表が未作成なら何もしない。
pub(crate) fn rename_table_in_txn(
    write_txn: &redb::WriteTransaction,
    old: &str,
    new: &str,
) -> Result<()> {
    if !state_table_exists(write_txn)? {
        return Ok(());
    }
    delete_table_in_txn(write_txn, new)?;
    let mut state = write_txn.open_table(ROWID_STATE_TABLE)?;
    loop {
        let keys = collect_tenants(&state, old)?;
        if keys.is_empty() {
            return Ok(());
        }
        for tenant in &keys {
            let moved = state.remove((old, tenant.as_str()))?.map(|g| g.value());
            if let Some(v) = moved {
                state.insert((new, tenant.as_str()), v)?;
            }
        }
    }
}

/// `table` に属する状態行のテナント名を最大 [`BATCH`] 件集める。
fn collect_tenants(
    state: &redb::Table<'_, (&'static str, &'static str), u64>,
    table: &str,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in state.range((table, "")..)? {
        let (k, _v) = entry?;
        let (t, tenant) = k.value();
        if t != table || out.len() >= BATCH {
            break;
        }
        out.push(tenant.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::storage::Storage;
    use redb::ReadableTableMetadata;

    fn storage_with_table() -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path("rowid-alloc");
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("body", ColumnType::Text, true)],
            ))
            .expect("create");
        (storage, guard)
    }

    fn alloc(storage: &Storage, tenant: &str, table: &str, n: u64) -> (u64, u64) {
        let txn = storage.begin_write_txn().expect("txn");
        let r = allocate_in_txn(&txn, tenant, table, n).expect("alloc");
        txn.commit_raw_for_test().expect("commit");
        (r.start().raw_for_state(), r.end().raw_for_state())
    }

    fn state_len(storage: &Storage) -> u64 {
        let txn = storage.begin_write_txn().expect("txn");
        let t = txn.open_table(ROWID_STATE_TABLE).expect("open");
        t.len().expect("len")
    }

    #[test]
    fn monotonic_and_range_allocation() {
        let (s, _guard) = storage_with_table();
        assert_eq!(alloc(&s, "t1", "docs", 1), (1, 1));
        assert_eq!(alloc(&s, "t1", "docs", 3), (2, 4));
        assert_eq!(alloc(&s, "t1", "docs", 1), (5, 5));
    }

    #[test]
    fn tenants_are_independent() {
        let (s, _guard) = storage_with_table();
        assert_eq!(alloc(&s, "t1", "docs", 5), (1, 5));
        assert_eq!(alloc(&s, "t2", "docs", 2), (1, 2));
        assert_eq!(alloc(&s, "t1", "docs", 1), (6, 6));
    }

    #[test]
    fn abort_rolls_back_state() {
        let (s, _guard) = storage_with_table();
        assert_eq!(alloc(&s, "t1", "docs", 2), (1, 2));
        let txn = s.begin_write_txn().expect("txn");
        let r = allocate_in_txn(&txn, "t1", "docs", 2).expect("alloc");
        assert_eq!(r.start().raw_for_state(), 3);
        txn.abort().expect("abort");
        // abort した払い出しはコミット済みの行を持たないので、同じ値が再び出てよい。
        assert_eq!(alloc(&s, "t1", "docs", 1), (3, 3));
    }

    #[test]
    fn initial_value_follows_existing_rows_of_the_same_tenant_only() {
        let (s, _guard) = storage_with_table();
        // 従来方式で書いた行（id = 7）を持つ表に採番器を初めて使う。
        s.insert_typed_row(
            "docs",
            7,
            "t1",
            crate::storage::Visibility::Private,
            &[crate::row_codec::Value::Text("x".to_string())],
        )
        .expect("insert");
        s.insert_typed_row(
            "docs",
            100,
            "t2",
            crate::storage::Visibility::Private,
            &[crate::row_codec::Value::Text("y".to_string())],
        )
        .expect("insert");
        assert_eq!(alloc(&s, "t1", "docs", 1), (8, 8));
        // 他テナントの最大 id には影響されない。
        assert_eq!(alloc(&s, "t3", "docs", 1), (1, 1));
    }

    #[test]
    fn exhaustion_is_rejected_with_fixed_message() {
        let (s, _guard) = storage_with_table();
        {
            let txn = s.begin_write_txn().expect("txn");
            {
                let mut t = txn.open_table(ROWID_STATE_TABLE).expect("open");
                t.insert(("docs", "t1"), u64::MAX - 1).expect("insert");
            }
            txn.commit_raw_for_test().expect("commit");
        }
        let txn = s.begin_write_txn().expect("txn");
        let err = allocate_in_txn(&txn, "t1", "docs", 2).expect_err("exhausted");
        assert!(matches!(&err, CatalogError::Invalid(m) if m == EXHAUSTED_MESSAGE));
        assert!(!err.to_string().contains("t1"));
        let ok = allocate_in_txn(&txn, "t1", "docs", 1).expect("last value");
        assert_eq!(ok.end().raw_for_state(), u64::MAX);
    }

    #[test]
    fn zero_count_is_rejected() {
        let (s, _guard) = storage_with_table();
        let txn = s.begin_write_txn().expect("txn");
        assert!(allocate_in_txn(&txn, "t1", "docs", 0).is_err());
    }

    #[test]
    fn delete_and_rename_are_noops_without_state_table() {
        let (s, _guard) = storage_with_table();
        let txn = s.begin_write_txn().expect("txn");
        delete_table_in_txn(&txn, "docs").expect("delete");
        rename_table_in_txn(&txn, "docs", "docs2").expect("rename");
        assert!(!state_table_exists(&txn).expect("exists"));
    }

    #[test]
    fn delete_removes_all_tenants_of_the_table_only() {
        let (s, _guard) = storage_with_table();
        alloc(&s, "t1", "docs", 1);
        alloc(&s, "t2", "docs", 1);
        alloc(&s, "t1", "other", 1);
        let txn = s.begin_write_txn().expect("txn");
        delete_table_in_txn(&txn, "docs").expect("delete");
        txn.commit_raw_for_test().expect("commit");
        assert_eq!(state_len(&s), 1);
        // 削除後の再払い出しは 1 から（同名再作成が旧状態を引き継がない）。
        assert_eq!(alloc(&s, "t1", "docs", 1), (1, 1));
    }

    #[test]
    fn rename_moves_state_and_clears_orphans() {
        let (s, _guard) = storage_with_table();
        alloc(&s, "t1", "docs", 4);
        alloc(&s, "t9", "new_name", 9);
        let txn = s.begin_write_txn().expect("txn");
        rename_table_in_txn(&txn, "docs", "new_name").expect("rename");
        txn.commit_raw_for_test().expect("commit");
        assert_eq!(alloc(&s, "t1", "new_name", 1), (5, 5));
        // 新名側の孤児（t9）は消えている。
        assert_eq!(alloc(&s, "t9", "new_name", 1), (1, 1));
        // 旧名側は空。
        assert_eq!(alloc(&s, "t1", "docs", 1), (1, 1));
    }
}
