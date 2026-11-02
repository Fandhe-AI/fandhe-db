//! `catalog::privilege::owner_change` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2236）。
//!
//! 所有者変更時の ACL 書き換え（O10）を固定する。新所有者の自己付与だけを削除し、無関係なロールの自己付与は
//! 保持すること。write txn は commit せず（破棄して）同じ txn 内の読み取りで結果を確認する。

use super::*;
use crate::catalog::privilege::{upsert_acl_in_txn, PRIV_USAGE};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const OLD_OWNER: u64 = 1;
const BOB: u64 = 2;
const NEW_OWNER: u64 = 3;

#[test]
fn rewrite_keeps_unrelated_self_grants_and_drops_new_owner_self_grants() {
    let path = unique_db_path("owner_change_self_grant");
    let _g = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    let txn = s.begin_write_txn().expect("begin");
    let obj = PrivilegeObject::Schema(5);
    // bob の自己付与（無関係なロール）・旧所有者から bob への付与・旧所有者の自己付与。
    upsert_acl_in_txn(&txn, "t1", &obj, BOB, BOB, PRIV_USAGE, 0).expect("upsert");
    upsert_acl_in_txn(&txn, "t1", &obj, BOB, OLD_OWNER, PRIV_USAGE, 0).expect("upsert");
    upsert_acl_in_txn(&txn, "t1", &obj, OLD_OWNER, OLD_OWNER, PRIV_USAGE, 0).expect("upsert");
    rewrite_acl_for_object(&txn, "t1", &obj, OLD_OWNER, NEW_OWNER).expect("rewrite");
    let rows: Vec<(u64, u64)> = list_acl_for_object_in_txn(&*txn, "t1", &obj)
        .expect("list")
        .iter()
        .map(|e| (e.grantee_id, e.grantor_id))
        .collect();
    assert!(rows.contains(&(BOB, BOB)), "{rows:?}");
    assert!(rows.contains(&(BOB, NEW_OWNER)), "{rows:?}");
    assert!(!rows.contains(&(NEW_OWNER, NEW_OWNER)), "{rows:?}");
    assert_eq!(rows.len(), 2, "{rows:?}");
}
