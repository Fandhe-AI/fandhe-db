//! `catalog::privilege` の単体テスト（SQL-40 のポインタ、Issue #1961）。
//!
//! 受入基準 (B) 破損した ACL を許可側へ倒さない、テナント境界、relation のライフサイクル（DROP・RENAME・
//! 列の DROP・RENAME）への追随を Storage 水準で固定する。

use super::*;
use crate::catalog::materialized::MaterializedViewDef;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, s)
}

fn write<T>(s: &Storage, f: impl FnOnce(&redb::WriteTransaction) -> Result<T>) -> Result<T> {
    let txn = s.begin_write_txn().expect("begin");
    let out = f(&txn)?;
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    Ok(out)
}

fn tbl(name: &str) -> PrivilegeObject {
    PrivilegeObject::Table(name.to_string())
}

fn col(table: &str, column: &str) -> PrivilegeObject {
    PrivilegeObject::Column {
        table: table.to_string(),
        column: column.to_string(),
    }
}

fn grant(s: &Storage, tenant: &str, obj: &PrivilegeObject, grantee: u64, privs: u16) {
    write(s, |t| {
        upsert_acl_in_txn(t, tenant, obj, grantee, 1, privs, 0)
    })
    .expect("grant");
}

fn list(s: &Storage, tenant: &str, obj: &PrivilegeObject) -> Vec<AclEntry> {
    s.list_acl_for_object(tenant, obj).expect("list")
}

fn mk_table(s: &Storage, name: &str) {
    s.create_table(&TableSchema::new(
        name,
        vec![
            ColumnDef::new("a", ColumnType::Text, true),
            ColumnDef::new("b", ColumnType::Text, true),
        ],
    ))
    .expect("table");
}

fn is_fixed_corrupt<T: std::fmt::Debug>(r: Result<T>) -> bool {
    matches!(r, Err(CatalogError::CorruptSchema(m)) if m == "invalid privilege catalog entry")
}

#[test]
fn acl_blob_roundtrip_and_golden_bytes() {
    let obj = tbl("t");
    let bytes = encode_acl_blob(&obj, PRIV_SELECT | PRIV_UPDATE, PRIV_SELECT).expect("encode");
    assert_eq!(bytes, vec![1, 0b101, 0, 0b001, 0]);
    assert_eq!(
        decode_acl_blob(&obj, &bytes).expect("decode"),
        (PRIV_SELECT | PRIV_UPDATE, PRIV_SELECT)
    );
}

#[test]
fn corrupt_acl_blobs_fail_closed_with_fixed_message() {
    let t = tbl("t");
    for bad in [
        vec![],
        vec![1, 1, 0, 0],
        vec![1, 1, 0, 0, 0, 0],
        vec![2, 1, 0, 0, 0],
        // privileges が 0
        vec![1, 0, 0, 0, 0],
        // 表の許可集合の外（USAGE）
        vec![1, 0, 1, 0, 0],
        // grant_options が privileges の部分集合でない
        vec![1, 1, 0, 2, 0],
        // 未定義ビット
        vec![1, 0, 0x80, 0, 0],
    ] {
        assert!(is_fixed_corrupt(decode_acl_blob(&t, &bad)), "{bad:?}");
    }
    // 種別ごとの許可集合。
    assert!(decode_acl_blob(&PrivilegeObject::Function(1), &[1, 0, 4, 0, 0]).is_ok());
    assert!(is_fixed_corrupt(decode_acl_blob(
        &PrivilegeObject::Function(1),
        &[1, 1, 0, 0, 0]
    )));
    assert!(is_fixed_corrupt(decode_acl_blob(
        &col("t", "a"),
        &[1, PRIV_DELETE as u8, 0, 0, 0]
    )));
    assert!(encode_acl_blob(&PrivilegeObject::Schema(1), PRIV_SELECT, 0).is_err());
}

#[test]
fn corrupt_key_components_are_rejected() {
    // 未知のタグ・column の不整合・ID の非正規形・0。
    assert!(PrivilegeObject::from_components(9, "t", "").is_err());
    assert!(PrivilegeObject::from_components(KIND_TABLE, "t", "c").is_err());
    assert!(PrivilegeObject::from_components(KIND_COLUMN, "t", "").is_err());
    assert!(PrivilegeObject::from_components(KIND_SCHEMA, "01", "").is_err());
    assert!(PrivilegeObject::from_components(KIND_SCHEMA, "0", "").is_err());
    assert!(PrivilegeObject::from_components(KIND_SEQUENCE, "x", "").is_err());
    assert_eq!(
        PrivilegeObject::from_components(KIND_FUNCTION, "12", "").expect("ok"),
        PrivilegeObject::Function(12)
    );
}

#[test]
fn corrupt_row_in_store_returns_err_not_grant_or_empty() {
    let (_g, s) = open("acl-corrupt");
    write(&s, |t| {
        t.open_table(ACL_TABLE)?.insert(
            ("t1", KIND_TABLE, "tbl", "", 5u64, 1u64),
            [9u8, 9].as_slice(),
        )?;
        Ok(())
    })
    .expect("raw");
    assert!(is_fixed_corrupt(s.list_acl_for_object("t1", &tbl("tbl"))));
    // grantor が 0 の行も破損。
    write(&s, |t| {
        let blob = encode_acl_blob(&tbl("tbl2"), PRIV_SELECT, 0)?;
        t.open_table(ACL_TABLE)?
            .insert(("t1", KIND_TABLE, "tbl2", "", 5u64, 0u64), blob.as_slice())?;
        Ok(())
    })
    .expect("raw");
    assert!(is_fixed_corrupt(s.list_acl_for_object("t1", &tbl("tbl2"))));
    // 他テナントは影響を受けない。
    assert!(list(&s, "t2", &tbl("tbl")).is_empty());
}

#[test]
fn upsert_validates_and_zero_privileges_removes() {
    let (_g, s) = open("acl-upsert");
    let t = tbl("tbl");
    grant(&s, "t1", &t, 5, PRIV_SELECT | PRIV_INSERT);
    let got = list(&s, "t1", &t);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].privileges, PRIV_SELECT | PRIV_INSERT);
    // 置き換え。
    write(&s, |tx| {
        upsert_acl_in_txn(tx, "t1", &t, 5, 1, PRIV_SELECT, PRIV_SELECT)
    })
    .expect("replace");
    assert_eq!(list(&s, "t1", &t)[0].grant_options, PRIV_SELECT);
    // 拒否: grantor 0・許可集合の外・部分集合違反。
    for (grantor, p, g) in [
        (0, PRIV_SELECT, 0),
        (1, PRIV_USAGE, 0),
        (1, PRIV_SELECT, PRIV_INSERT),
        (1, 0, PRIV_SELECT),
    ] {
        let r = write(&s, |tx| upsert_acl_in_txn(tx, "t1", &t, 5, grantor, p, g));
        assert!(
            matches!(r, Err(CatalogError::Invalid(_))),
            "{grantor} {p} {g}"
        );
    }
    // privileges 0 は行を消す。
    write(&s, |tx| upsert_acl_in_txn(tx, "t1", &t, 5, 1, 0, 0)).expect("remove");
    assert!(list(&s, "t1", &t).is_empty());
    assert!(!write(&s, |tx| remove_acl_in_txn(tx, "t1", &t, 5, 1)).expect("absent"));
}

#[test]
fn per_tenant_limits_are_enforced() {
    let (_g, s) = open("acl-limits");
    let txn = s.begin_write_txn().expect("begin");
    let t = tbl("tbl");
    upsert_acl_limited(&txn, "t1", &t, 1, 1, PRIV_SELECT, 0, 2).expect("1");
    upsert_acl_limited(&txn, "t1", &t, 2, 1, PRIV_SELECT, 0, 2).expect("2");
    let over = upsert_acl_limited(&txn, "t1", &t, 3, 1, PRIV_SELECT, 0, 2);
    assert!(matches!(over, Err(CatalogError::ViewLimitExceeded(m)) if m == ACL_LIMIT_MESSAGE));
    // 既存行の置き換えは件数を増やさないので通る。
    upsert_acl_limited(&txn, "t1", &t, 2, 1, PRIV_INSERT, 0, 2).expect("replace");
    // 別テナントには影響しない。
    upsert_acl_limited(&txn, "t2", &t, 3, 1, PRIV_SELECT, 0, 2).expect("other tenant");
    drop(txn);
}

#[test]
fn public_grantee_is_tenant_local() {
    let (_g, s) = open("acl-public");
    let t = tbl("tbl");
    grant(&s, "t1", &t, PUBLIC_GRANTEE_ID, PRIV_SELECT);
    assert_eq!(list(&s, "t1", &t).len(), 1);
    assert_eq!(list(&s, "t1", &t)[0].grantee_id, PUBLIC_GRANTEE_ID);
    assert!(list(&s, "t2", &t).is_empty());
}

#[test]
fn other_tenant_rows_unreachable_and_kinds_separate() {
    let (_g, s) = open("acl-boundary");
    grant(&s, "t1", &tbl("tbl"), 5, PRIV_SELECT);
    grant(&s, "t1", &col("tbl", "a"), 5, PRIV_SELECT);
    grant(&s, "t1", &PrivilegeObject::Schema(3), 5, PRIV_USAGE);
    assert!(list(&s, "t2", &tbl("tbl")).is_empty());
    assert_eq!(list(&s, "t1", &tbl("tbl")).len(), 1);
    assert_eq!(list(&s, "t1", &col("tbl", "a")).len(), 1);
    assert!(list(&s, "t1", &col("tbl", "b")).is_empty());
    assert_eq!(list(&s, "t1", &PrivilegeObject::Schema(3)).len(), 1);
    assert!(list(&s, "t1", &PrivilegeObject::Sequence(3)).is_empty());
}

#[test]
fn drop_table_purges_acl_for_all_tenants() {
    let (_g, s) = open("acl-drop-table");
    mk_table(&s, "tbl");
    for tenant in ["t1", "t2"] {
        grant(&s, tenant, &tbl("tbl"), 5, PRIV_SELECT);
        grant(&s, tenant, &col("tbl", "a"), 5, PRIV_SELECT);
    }
    grant(&s, "t1", &tbl("other"), 5, PRIV_SELECT);
    s.drop_table("tbl").expect("drop");
    mk_table(&s, "tbl");
    for tenant in ["t1", "t2"] {
        assert!(list(&s, tenant, &tbl("tbl")).is_empty());
        assert!(list(&s, tenant, &col("tbl", "a")).is_empty());
    }
    assert_eq!(list(&s, "t1", &tbl("other")).len(), 1);
}

#[test]
fn rename_table_moves_acl() {
    let (_g, s) = open("acl-rename-table");
    mk_table(&s, "old_t");
    grant(&s, "t1", &tbl("old_t"), 5, PRIV_SELECT);
    grant(&s, "t2", &col("old_t", "a"), 6, PRIV_UPDATE);
    // 改名先に残っていた孤児の行。
    grant(&s, "t1", &tbl("new_t"), 9, PRIV_DELETE);
    s.alter_table_rename("old_t", "new_t")
        .map_err(|e| format!("{e:?}"))
        .expect("rename");
    assert!(list(&s, "t1", &tbl("old_t")).is_empty());
    let moved = list(&s, "t1", &tbl("new_t"));
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].grantee_id, 5);
    assert_eq!(list(&s, "t2", &col("new_t", "a"))[0].grantee_id, 6);
}

#[test]
fn drop_column_purges_and_rename_column_moves_column_acl() {
    let (_g, s) = open("acl-columns");
    mk_table(&s, "tbl");
    grant(&s, "t1", &col("tbl", "a"), 5, PRIV_SELECT);
    grant(&s, "t2", &col("tbl", "a"), 6, PRIV_SELECT);
    grant(&s, "t1", &col("tbl", "b"), 7, PRIV_SELECT);
    // 改名先に残っていた孤児。
    grant(&s, "t1", &col("tbl", "c"), 9, PRIV_UPDATE);
    s.alter_table_rename_column("tbl", "a", "c")
        .map_err(|e| format!("{e:?}"))
        .expect("rename column");
    assert!(list(&s, "t1", &col("tbl", "a")).is_empty());
    let moved = list(&s, "t1", &col("tbl", "c"));
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].grantee_id, 5);
    assert_eq!(list(&s, "t2", &col("tbl", "c"))[0].grantee_id, 6);
    s.alter_table_drop_column("tbl", "c").expect("drop column");
    assert!(list(&s, "t1", &col("tbl", "c")).is_empty());
    assert!(list(&s, "t2", &col("tbl", "c")).is_empty());
    // 他の列は残る。
    assert_eq!(list(&s, "t1", &col("tbl", "b")).len(), 1);
    // 同名列の再追加へ旧付与が復活しない。
    s.alter_table_add_column("tbl", ColumnDef::new("c", ColumnType::Text, true))
        .expect("re-add");
    assert!(list(&s, "t1", &col("tbl", "c")).is_empty());
}

#[test]
fn drop_view_and_matview_purge_acl() {
    let (_g, s) = open("acl-views");
    s.create_table(&TableSchema::new(
        "base",
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    ))
    .expect("base");
    s.create_view("v", "base", "SELECT embedding FROM base")
        .expect("view");
    s.create_materialized_view(&MaterializedViewDef {
        name: "mv".to_string(),
        base_relations: vec!["base".to_string()],
        body_sql: "SELECT embedding FROM base".to_string(),
        column_aliases: Vec::new(),
        columns: vec![ColumnDef::new("label", ColumnType::Text, true)],
        populated: false,
    })
    .expect("matview");
    for name in ["v", "mv"] {
        grant(&s, "t1", &tbl(name), 5, PRIV_SELECT);
        grant(&s, "t2", &col(name, "label"), 5, PRIV_SELECT);
    }
    s.drop_view("v").expect("drop view");
    s.drop_materialized_view("mv").expect("drop matview");
    for name in ["v", "mv"] {
        assert!(list(&s, "t1", &tbl(name)).is_empty());
        assert!(list(&s, "t2", &col(name, "label")).is_empty());
    }
}

#[test]
fn id_keyed_objects_unaffected_by_relation_sweep() {
    let (_g, s) = open("acl-id-keyed");
    grant(&s, "t1", &PrivilegeObject::Schema(5), 5, PRIV_USAGE);
    grant(&s, "t1", &PrivilegeObject::Sequence(5), 5, PRIV_USAGE);
    grant(&s, "t1", &PrivilegeObject::Function(5), 5, PRIV_EXECUTE);
    write(&s, |t| {
        remove_relation_acl_in_txn(t, "5")?;
        rename_relation_acl_in_txn(t, "5", "6")?;
        remove_column_acl_in_txn(t, "5", "a")
    })
    .expect("sweep");
    assert_eq!(list(&s, "t1", &PrivilegeObject::Schema(5)).len(), 1);
    assert_eq!(list(&s, "t1", &PrivilegeObject::Sequence(5)).len(), 1);
    assert_eq!(list(&s, "t1", &PrivilegeObject::Function(5)).len(), 1);
}

#[test]
fn acl_survives_reopen() {
    let path = unique_db_path("acl-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let s = Storage::open(&path).expect("open");
        write(&s, |t| {
            upsert_acl_in_txn(t, "t1", &tbl("tbl"), 5, 1, PRIV_SELECT, PRIV_SELECT)
        })
        .expect("grant");
    }
    let s = Storage::open(&path).expect("reopen");
    let got = list(&s, "t1", &tbl("tbl"));
    assert_eq!(
        got,
        vec![AclEntry {
            grantee_id: 5,
            grantor_id: 1,
            privileges: PRIV_SELECT,
            grant_options: PRIV_SELECT,
        }]
    );
}
// ---- GRANT／REVOKE の依存解決（Issue #1965 レビュー指摘。表と列をまたぐ GRANT OPTION の伝播）----

mod apply_deps {
    use super::*;
    use crate::catalog::privilege::apply::{
        AclApplyOutcome, AclChangeError, AclItem, AclRequest, GranteeSpec,
    };
    use crate::catalog::role::{RoleAttributes, RoleDef};

    const T: &str = "acl_t";

    fn role(s: &Storage, name: &str) -> u64 {
        s.create_role(
            "tn",
            &RoleDef {
                id: 0,
                name: name.to_string(),
                attributes: RoleAttributes {
                    login: true,
                    inherit: true,
                    ..RoleAttributes::default()
                },
                connection_limit: -1,
                valid_until: None,
                scram_verifier: Vec::new(),
                legacy_phc: Vec::new(),
            },
        )
        .expect("role")
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        s: &Storage,
        grantor: u64,
        owner: bool,
        is_grant: bool,
        bits: u16,
        columns: &[&str],
        grantee: u64,
        go: bool,
        cascade: bool,
    ) -> std::result::Result<AclApplyOutcome, AclChangeError> {
        let items = [AclItem {
            bits,
            columns: columns.iter().map(|c| c.to_string()).collect(),
        }];
        let tables = [T.to_string()];
        let grantees = [GranteeSpec::Id(grantee)];
        s.apply_acl_changes(
            "tn",
            &AclRequest {
                is_grant,
                grantor_id: grantor,
                owner_equivalent: owner,
                tables: &tables,
                items: &items,
                grantees: &grantees,
                grant_option: go,
                cascade,
            },
        )
    }

    fn col_rows(s: &Storage, c: &str) -> Vec<AclEntry> {
        s.list_acl_for_object("tn", &col(T, c)).expect("list")
    }

    #[test]
    fn revoking_table_grant_option_restricts_or_cascades_to_column_grants() {
        for cascade in [false, true] {
            let (_g, s) = open("acl_dep_tbl_go");
            mk_table(&s, T);
            let owner = role(&s, "owner");
            let bob = role(&s, "bob");
            let carol = role(&s, "carol");
            run(&s, owner, true, true, PRIV_SELECT, &[], bob, true, false).expect("g1");
            run(
                &s,
                bob,
                false,
                true,
                PRIV_SELECT,
                &["a"],
                carol,
                false,
                false,
            )
            .expect("g2");
            let r = run(&s, owner, true, false, PRIV_SELECT, &[], bob, true, cascade);
            if cascade {
                r.expect("cascade");
                assert!(col_rows(&s, "a").iter().all(|e| e.grantee_id != carol));
            } else {
                assert!(matches!(r, Err(AclChangeError::DependentPrivileges)));
                assert_eq!(col_rows(&s, "a").len(), 1);
            }
        }
    }

    /// 表の GRANT OPTION を持たない非オーナーの表単位 REVOKE は、警告のみで ACL を変えず、
    /// 呼び出し元が付与した列権限も残す（Issue #1965 レビュー指摘）。
    #[test]
    fn table_revoke_without_table_grant_option_warns_and_keeps_column_grants() {
        let (_g, s) = open("acl_dep_tbl_rev_nogo");
        mk_table(&s, T);
        let owner = role(&s, "owner");
        let bob = role(&s, "bob");
        let carol = role(&s, "carol");
        run(&s, owner, true, true, PRIV_SELECT, &["a"], bob, true, false).expect("g1");
        run(
            &s,
            bob,
            false,
            true,
            PRIV_SELECT,
            &["a"],
            carol,
            false,
            false,
        )
        .expect("g2");
        let before = col_rows(&s, "a");
        let out =
            run(&s, bob, false, false, PRIV_SELECT, &[], carol, false, false).expect("revoke");
        assert_eq!(out.warning, Some(AclWarning::NoPrivileges));
        assert_eq!(out.rows_changed, 0);
        assert_eq!(col_rows(&s, "a"), before);
    }

    #[test]
    fn column_revoke_keeps_grant_when_table_grant_option_remains() {
        let (_g, s) = open("acl_dep_col_keep");
        mk_table(&s, T);
        let owner = role(&s, "owner");
        let bob = role(&s, "bob");
        let carol = role(&s, "carol");
        run(&s, owner, true, true, PRIV_SELECT, &[], bob, true, false).expect("g1");
        run(&s, owner, true, true, PRIV_SELECT, &["a"], bob, true, false).expect("g2");
        run(
            &s,
            bob,
            false,
            true,
            PRIV_SELECT,
            &["a"],
            carol,
            false,
            false,
        )
        .expect("g3");
        run(
            &s,
            owner,
            true,
            false,
            PRIV_SELECT,
            &["a"],
            bob,
            true,
            false,
        )
        .expect("restrict ok");
        assert!(col_rows(&s, "a").iter().any(|e| e.grantee_id == carol));
    }

    #[test]
    fn grant_option_cycle_is_judged_per_privilege_bit() {
        let (_g, s) = open("acl_dep_bits");
        mk_table(&s, T);
        let owner = role(&s, "owner");
        let alice = role(&s, "alice");
        let bob = role(&s, "bob");
        run(&s, owner, true, true, PRIV_SELECT, &[], alice, true, false).expect("1");
        run(&s, owner, true, true, PRIV_UPDATE, &[], bob, true, false).expect("2");
        run(&s, alice, false, true, PRIV_SELECT, &[], bob, true, false).expect("3");
        run(&s, bob, false, true, PRIV_UPDATE, &[], alice, true, false).expect("update ok");
        let r = run(&s, bob, false, true, PRIV_SELECT, &[], alice, true, false);
        assert!(matches!(r, Err(AclChangeError::InvalidGrant(_))));
    }
    #[test]
    fn column_grant_option_cycle_follows_table_level_path() {
        let (_g, s) = open("acl_dep_col_cycle");
        mk_table(&s, T);
        let owner = role(&s, "owner");
        let a = role(&s, "a");
        let b = role(&s, "b");
        run(&s, owner, true, true, PRIV_SELECT, &[], a, true, false).expect("1");
        run(&s, a, false, true, PRIV_SELECT, &[], b, true, false).expect("2");
        let r = run(&s, b, false, true, PRIV_SELECT, &["a"], a, true, false);
        assert!(matches!(r, Err(AclChangeError::InvalidGrant(_))));
    }
}
