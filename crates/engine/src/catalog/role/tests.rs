//! `catalog::role` の単体テスト（SQL-39 のポインタ、Issue #1961）。
//!
//! 受入基準 (A) 表を持たない旧 DB を従来どおり開ける（空のカタログ）、(B) カタログが破損していても許可側へ
//! 倒さない（`Ok(空)`・`Ok(false)` ではなく固定文言の `CorruptSchema`）を Storage 水準で固定する。

use super::*;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::TableHandle;

fn open(label: &str) -> (CleanupGuard, std::path::PathBuf, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, path, s)
}

fn rdef(name: &str) -> RoleDef {
    RoleDef {
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
    }
}

/// 書き込み txn を 1 回張って `f` を実行し commit する。
fn write<T>(s: &Storage, f: impl FnOnce(&redb::WriteTransaction) -> Result<T>) -> Result<T> {
    let txn = s.begin_write_txn().expect("begin");
    let out = f(&txn)?;
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    Ok(out)
}

fn raw_put_role(s: &Storage, tenant: &str, name: &str, bytes: &[u8]) {
    write(s, |t| {
        t.open_table(ROLES_TABLE)?.insert((tenant, name), bytes)?;
        Ok(())
    })
    .expect("raw put");
}

fn is_fixed_corrupt<T: std::fmt::Debug>(r: Result<T>) -> bool {
    matches!(r, Err(CatalogError::CorruptSchema(m)) if m == "invalid role catalog entry")
}

fn table_names(path: &std::path::Path) -> Vec<String> {
    let db = redb::Database::open(path).expect("raw open");
    let read = db.begin_read().expect("read");
    read.list_tables()
        .expect("list")
        .map(|h| h.name().to_string())
        .collect()
}

const NEW_TABLES: [&str; 5] = [
    "roles",
    "role_id_counter",
    "role_memberships",
    "role_login_names",
    "acl",
];

#[test]
fn role_blob_roundtrip_and_golden_bytes() {
    let mut d = rdef("alice");
    d.id = 7;
    d.attributes.createdb = true;
    d.connection_limit = 5;
    d.valid_until = Some(-3);
    d.scram_verifier = b"dummy".to_vec();
    let bytes = encode_role_def(&d).expect("encode");
    assert_eq!(decode_role_def("alice", &bytes).expect("decode"), d);

    let mut plain = rdef("r");
    plain.id = 1;
    let golden: Vec<u8> = vec![
        1, // version
        1, 0, 0, 0, 0, 0, 0, 0, // id
        0b11, 0, // attrs: LOGIN | INHERIT
        0xff, 0xff, 0xff, 0xff, // connection_limit = -1
        0,    // valid_until なし
        0, 0, // scram 空
        0, 0, // legacy 空
    ];
    assert_eq!(encode_role_def(&plain).expect("encode"), golden);
}

#[test]
fn membership_and_login_blob_roundtrip() {
    let opts = MembershipOptions {
        admin: true,
        inherit: false,
        set: true,
    };
    let bytes = encode_membership(opts);
    assert_eq!(bytes, vec![1, 0b101]);
    assert_eq!(decode_membership(&bytes).expect("decode"), opts);

    let blob = encode_login_blob("t1", 9).expect("encode");
    assert_eq!(
        decode_login_blob(&blob).expect("decode"),
        ("t1".to_string(), 9)
    );
}

#[test]
fn corrupt_blobs_fail_closed_with_fixed_message() {
    let mut d = rdef("alice");
    d.id = 3;
    d.scram_verifier = b"abc".to_vec();
    let good = encode_role_def(&d).expect("encode");
    let mut cases: Vec<Vec<u8>> = vec![Vec::new()];
    // 版の違い
    let mut v = good.clone();
    v[0] = 2;
    cases.push(v);
    // 予約ビット
    let mut v = good.clone();
    v[9] |= 0x10;
    cases.push(v);
    // id 0
    let mut v = good.clone();
    v[1..9].copy_from_slice(&0u64.to_le_bytes());
    cases.push(v);
    // connection_limit < -1
    let mut v = good.clone();
    v[11..15].copy_from_slice(&(-2i32).to_le_bytes());
    cases.push(v);
    // valid_until の未知タグ
    let mut v = good.clone();
    v[15] = 9;
    cases.push(v);
    // 切断
    cases.push(good[..good.len() - 1].to_vec());
    // 末尾の余り
    let mut v = good.clone();
    v.push(0);
    cases.push(v);
    // 検証子の長さが上限超過（確保の前に拒否される。実バイト列は用意しない）
    let mut v = good[..16].to_vec();
    v.extend_from_slice(
        &u16::try_from(MAX_ROLE_VERIFIER_BYTES + 1)
            .expect("u16")
            .to_le_bytes(),
    );
    cases.push(v);
    // 検証子に印字不可能な文字
    let mut v = good.clone();
    let n = v.len();
    v[n - 3] = 0x01;
    cases.push(v);
    for c in &cases {
        assert!(is_fixed_corrupt(decode_role_def("alice", c)), "{c:?}");
    }
    for bad in [vec![], vec![2, 0], vec![1], vec![1, 0x08], vec![1, 0, 0]] {
        assert!(is_fixed_corrupt(decode_membership(&bad)), "{bad:?}");
    }
    for bad in [
        vec![],
        vec![1, 0, 0],
        vec![1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    ] {
        assert!(is_fixed_corrupt(decode_login_blob(&bad)), "{bad:?}");
    }
}

#[test]
fn corrupt_row_in_store_returns_err_not_empty() {
    let (_g, _p, s) = open("role-corrupt-row");
    raw_put_role(&s, "t1", "alice", &[9, 9, 9]);
    assert!(is_fixed_corrupt(s.get_role("t1", "alice")));
    assert!(is_fixed_corrupt(s.list_roles("t1")));
    // 破損を「空」と取り違えない。
    assert!(is_fixed_corrupt(s.tenant_has_roles("t1")));
    // 他テナントは影響を受けず、空として答える。
    assert!(!s.tenant_has_roles("t2").expect("other tenant"));
    assert!(s.list_roles("t2").expect("other tenant").is_empty());
}

#[test]
fn login_map_inconsistency_fails_closed() {
    let (_g, _p, s) = open("role-login-map");
    let id = s.create_role("t1", &rdef("alice")).expect("role");
    let mut nologin = rdef("bob");
    nologin.attributes.login = false;
    let nologin_id = s.create_role("t1", &nologin).expect("role");
    let ident_ok = encode_login_identifier("q1", "alice").expect("ident");
    let ident_nologin = encode_login_identifier("q1", "bob").expect("ident");
    let ident_missing = encode_login_identifier("q1", "carol").expect("ident");
    let ident_wrong_id = encode_login_identifier("q2", "alice").expect("ident");
    write(&s, |t| {
        register_login_in_txn(t, &ident_ok, "t1", id)?;
        register_login_in_txn(t, &ident_nologin, "t1", nologin_id)?;
        register_login_in_txn(t, &ident_missing, "t1", 999)?;
        register_login_in_txn(t, &ident_wrong_id, "t1", nologin_id)?;
        Ok(())
    })
    .expect("register");
    assert_eq!(
        s.resolve_login(&ident_ok).expect("ok"),
        Some(("t1".to_string(), id))
    );
    for bad in [&ident_nologin, &ident_missing, &ident_wrong_id] {
        assert!(is_fixed_corrupt(s.resolve_login(bad)), "{bad}");
    }
    // 未登録は None。
    let unknown = encode_login_identifier("q9", "zed").expect("ident");
    assert_eq!(s.resolve_login(&unknown).expect("none"), None);
    // 重複登録は拒否。
    let dup = write(&s, |t| register_login_in_txn(t, &ident_ok, "t1", id));
    assert!(matches!(dup, Err(CatalogError::Invalid(m)) if m == LOGIN_ALREADY_EXISTS_MESSAGE));
}

#[test]
fn login_identifier_is_injective() {
    let a = encode_login_identifier("a", "b_c").expect("a");
    // 修飾子に `_` は使えない（英数字のみ）。ADR の衝突例の片方は符号化段で拒否される。
    assert!(encode_login_identifier("a_b", "c").is_err());
    let c = encode_login_identifier("ab", "c").expect("c");
    let d = encode_login_identifier("a", "bc").expect("d");
    assert_ne!(a, c);
    assert_ne!(c, d);
    for (q, r) in [("a", "b_c"), ("ab", "c"), ("a", "bc"), ("T1", "R")] {
        let ident = encode_login_identifier(q, r).expect("enc");
        assert_eq!(decode_login_identifier(&ident).expect("dec"), (q, r));
    }
    for bad in [
        "", ":", "a", "1:", "0:a", "01:ab", "2:a", "1:a", "1:ab-", "x:ab", "1a:b", "65:abc",
        "1:a1b", "-1:ab", "1:aé",
    ] {
        assert!(decode_login_identifier(bad).is_err(), "{bad:?}");
    }
    assert!(encode_login_identifier("", "r").is_err());
    assert!(encode_login_identifier("a-b", "r").is_err());
    assert!(encode_login_identifier("a", "1r").is_err());
}

#[test]
fn same_role_name_in_two_tenants_is_independent() {
    let (_g, _p, s) = open("role-two-tenants");
    let a = s.create_role("t1", &rdef("alice")).expect("t1");
    let b = s.create_role("t2", &rdef("alice")).expect("t2");
    assert_ne!(a, b);
    assert_eq!(s.get_role("t1", "alice").expect("get").expect("some").id, a);
    assert_eq!(s.get_role("t2", "alice").expect("get").expect("some").id, b);
    assert!(s.get_role("t3", "alice").expect("get").is_none());
    assert_eq!(s.list_roles("t1").expect("list").len(), 1);
    // 重複は同テナント内だけ。
    let dup = s.create_role("t1", &rdef("alice"));
    assert!(matches!(dup, Err(CatalogError::Invalid(m)) if m == ROLE_ALREADY_EXISTS_MESSAGE));
}

#[test]
fn other_tenant_rows_unreachable() {
    let (_g, _p, s) = open("role-tenant-boundary");
    let a = s.create_role("t1", &rdef("alice")).expect("a");
    let b = s.create_role("t1", &rdef("bob")).expect("b");
    write(&s, |t| {
        put_membership_in_txn(t, "t1", a, b, MembershipOptions::default())
    })
    .expect("member");
    let rt = s.db().begin_read().expect("read");
    assert_eq!(
        list_memberships_of_member_in_txn(&rt, "t1", a)
            .expect("t1")
            .len(),
        1
    );
    assert!(list_memberships_of_member_in_txn(&rt, "t2", a)
        .expect("t2")
        .is_empty());
    assert!(get_role_by_id_in_txn(&rt, "t2", a).expect("t2").is_none());
    assert!(get_role_by_id_in_txn(&rt, "t1", a).expect("t1").is_some());
}

#[test]
fn role_ids_are_monotonic_and_never_reused() {
    let (_g, _p, s) = open("role-ids");
    let first = s.create_role("t1", &rdef("alice")).expect("first");
    assert_eq!(first, 1);
    assert!(write(&s, |t| remove_role_in_txn(t, "t1", "alice")).expect("remove"));
    assert!(s.get_role("t1", "alice").expect("get").is_none());
    let second = s.create_role("t1", &rdef("alice")).expect("second");
    assert!(second > first);
    assert!(!write(&s, |t| remove_role_in_txn(t, "t1", "nobody")).expect("absent"));
}

/// カウンタ欠損 + 既存ロールありは ID 重複を避けるため固定文言の CorruptSchema で拒否する。
#[test]
fn missing_id_counter_with_existing_roles_is_rejected() {
    let (_g, _p, s) = open("role-ids-counter-missing");
    assert_eq!(s.create_role("t1", &rdef("alice")).expect("first"), 1);
    write(&s, |t| {
        t.open_table(ROLE_ID_COUNTER_TABLE)?
            .remove(ROLE_ID_COUNTER_KEY)?;
        Ok(())
    })
    .expect("drop counter");
    assert!(is_fixed_corrupt(s.create_role("t2", &rdef("bob"))));
    assert!(s.get_role("t2", "bob").expect("get").is_none());
}

/// 保存済みカウンタが 0・既存 ID 以下へ破損していれば、別ロールへ同じ ID を払い出さず拒否する。
#[test]
fn rolled_back_id_counter_is_rejected() {
    for bad in [0u64, 1] {
        let (_g, _p, s) = open("role-ids-counter-rolled-back");
        assert_eq!(s.create_role("t1", &rdef("alice")).expect("first"), 1);
        write(&s, |t| {
            t.open_table(ROLE_ID_COUNTER_TABLE)?
                .insert(ROLE_ID_COUNTER_KEY, bad)?;
            Ok(())
        })
        .expect("corrupt counter");
        assert!(is_fixed_corrupt(s.create_role("t2", &rdef("bob"))));
        assert!(s.get_role("t2", "bob").expect("get").is_none());
    }
}

#[test]
fn per_tenant_limits_are_enforced() {
    let (_g, _p, s) = open("role-limits");
    let txn = s.begin_write_txn().expect("begin");
    create_role_limited(&txn, "t1", &rdef("a"), 2).expect("a");
    create_role_limited(&txn, "t1", &rdef("b"), 2).expect("b");
    let over = create_role_limited(&txn, "t1", &rdef("c"), 2);
    assert!(matches!(over, Err(CatalogError::ViewLimitExceeded(m)) if m == ROLE_LIMIT_MESSAGE));
    // 別テナントには影響しない。
    create_role_limited(&txn, "t2", &rdef("c"), 2).expect("other tenant");
    drop(txn);
}

#[test]
fn self_membership_rejected() {
    let (_g, _p, s) = open("role-self-membership");
    let a = s.create_role("t1", &rdef("alice")).expect("a");
    let r = write(&s, |t| {
        put_membership_in_txn(t, "t1", a, a, MembershipOptions::default())
    });
    assert!(matches!(r, Err(CatalogError::Invalid(_))));
    let r = write(&s, |t| {
        put_membership_in_txn(t, "t1", 0, a, MembershipOptions::default())
    });
    assert!(matches!(r, Err(CatalogError::Invalid(_))));
}

#[test]
fn corrupt_membership_row_returns_err() {
    let (_g, _p, s) = open("role-corrupt-membership");
    write(&s, |t| {
        t.open_table(ROLE_MEMBERSHIPS_TABLE)?
            .insert(("t1", 1u64, 2u64), [1u8, 0x80].as_slice())?;
        Ok(())
    })
    .expect("raw");
    let rt = s.db().begin_read().expect("read");
    assert!(is_fixed_corrupt(list_memberships_of_member_in_txn(
        &rt, "t1", 1
    )));
}

#[test]
fn role_catalog_survives_reopen() {
    let (_g, path, s) = open("role-reopen");
    let mut d = rdef("alice");
    d.valid_until = Some(1_700_000_000_000_000);
    d.scram_verifier = b"dummy-verifier".to_vec();
    let a = s.create_role("t1", &d).expect("a");
    let b = s.create_role("t1", &rdef("bob")).expect("b");
    let ident = encode_login_identifier("q1", "alice").expect("ident");
    write(&s, |t| {
        put_membership_in_txn(
            t,
            "t1",
            a,
            b,
            MembershipOptions {
                admin: false,
                inherit: true,
                set: true,
            },
        )?;
        register_login_in_txn(t, &ident, "t1", a)
    })
    .expect("write");
    drop(s);
    let s = Storage::open(&path).expect("reopen");
    let got = s.get_role("t1", "alice").expect("get").expect("some");
    d.id = a;
    assert_eq!(got, d);
    assert!(s.tenant_has_roles("t1").expect("has"));
    assert_eq!(s.list_roles("t1").expect("list").len(), 2);
    assert_eq!(
        s.resolve_login(&ident).expect("login"),
        Some(("t1".to_string(), a))
    );
    let rt = s.db().begin_read().expect("read");
    let m = list_memberships_of_member_in_txn(&rt, "t1", a).expect("m");
    assert_eq!(m.len(), 1);
    assert!(m[0].1.inherit && m[0].1.set && !m[0].1.admin);
}

#[test]
fn fresh_db_never_creates_role_tables_without_writes() {
    let (_g, path, s) = open("role-lazy");
    assert!(s.get_role("t1", "a").expect("get").is_none());
    assert!(s.list_roles("t1").expect("list").is_empty());
    assert!(!s.tenant_has_roles("t1").expect("has"));
    let ident = encode_login_identifier("q1", "a").expect("ident");
    assert!(s.resolve_login(&ident).expect("login").is_none());
    s.create_table(&crate::catalog::TableSchema::new(
        "t",
        vec![crate::catalog::ColumnDef::new(
            "a",
            crate::catalog::ColumnType::Text,
            true,
        )],
    ))
    .expect("table");
    s.drop_table("t").expect("drop");
    drop(s);
    let names = table_names(&path);
    for n in NEW_TABLES {
        assert!(
            !names.iter().any(|x| x == n),
            "{n} must not exist: {names:?}"
        );
    }
}

/// 受入基準 (A): 表を持たない旧 DB を従来どおり開け、カタログは空として答え、遅延作成が保たれる。
#[test]
fn legacy_fixture_opens_with_empty_role_catalog() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/table23_legacy_pre_rowid.redb"
    );
    let path = unique_db_path("role-legacy-fixture");
    let _g = CleanupGuard(path.clone());
    std::fs::copy(fixture, &path).expect("copy fixture");
    let before = table_names(&path);
    for n in NEW_TABLES {
        assert!(!before.iter().any(|x| x == n), "{n} in legacy fixture");
    }
    let s = Storage::open(&path).expect("open legacy");
    assert!(s.get_role("t1", "alice").expect("get").is_none());
    assert!(s.list_roles("t1").expect("list").is_empty());
    assert!(!s.tenant_has_roles("t1").expect("has"));
    let ident = encode_login_identifier("q1", "alice").expect("ident");
    assert!(s.resolve_login(&ident).expect("login").is_none());
    let obj = crate::catalog::privilege::PrivilegeObject::Table("anything".to_string());
    assert!(s.list_acl_for_object("t1", &obj).expect("acl").is_empty());
    // ACL 表が無い状態でライフサイクルの掃除（DROP・RENAME）が通る。
    let names = s.list_tables().expect("tables");
    assert!(names.len() >= 2, "{names:?}");
    let first = names[0].clone();
    let second = names[1].clone();
    s.alter_table_rename(&first, "renamed_by_role_test")
        .map_err(|e| format!("{e:?}"))
        .expect("rename");
    s.drop_table(&second).expect("drop");
    drop(s);
    let reopened = Storage::open(&path).expect("reopen");
    drop(reopened);
    let after = table_names(&path);
    for n in NEW_TABLES {
        assert!(
            !after.iter().any(|x| x == n),
            "{n} created lazily: {after:?}"
        );
    }
}
// ---- Issue #1963: ALTER ROLE ／ RENAME ／ SET ROLE の到達判定 ----

fn put_member(s: &Storage, tenant: &str, member: u64, granted: u64, set: bool) {
    write(s, |t| {
        put_membership_in_txn(
            t,
            tenant,
            member,
            granted,
            MembershipOptions {
                admin: false,
                inherit: true,
                set,
            },
        )
    })
    .expect("put membership");
}

#[test]
fn rename_keeps_id_and_moves_key() {
    let (_g, _p, s) = open("role-rename");
    let id = s.create_role("t1", &rdef("old")).expect("create");
    s.create_role("t2", &rdef("other")).expect("create t2");
    s.rename_role("t1", "old", "new", &[]).expect("rename");
    assert!(s.get_role("t1", "old").expect("get").is_none());
    let moved = s.get_role("t1", "new").expect("get").expect("exists");
    assert_eq!(moved.id, id);
    assert_eq!(moved.name, "new");
    assert!(s.get_role("t2", "other").expect("get").is_some());
}

#[test]
fn rename_rejections() {
    let (_g, _p, s) = open("role-rename-rej");
    let a = s.create_role("t1", &rdef("a")).expect("a");
    s.create_role("t1", &rdef("b")).expect("b");
    s.create_role("t2", &rdef("only_t2")).expect("t2");
    let dup = s.rename_role("t1", "a", "b", &[]);
    assert!(matches!(dup, Err(CatalogError::Invalid(m)) if m == ROLE_ALREADY_EXISTS_MESSAGE));
    let missing = s.rename_role("t1", "nope", "c", &[]);
    assert!(matches!(missing, Err(CatalogError::Invalid(m)) if m == ROLE_NOT_FOUND_MESSAGE));
    let cross = s.rename_role("t1", "only_t2", "c", &[]);
    assert!(matches!(cross, Err(CatalogError::Invalid(m)) if m == ROLE_NOT_FOUND_MESSAGE));
    let prot = s.rename_role("t1", "a", "c", &[a]);
    assert!(matches!(prot, Err(CatalogError::Invalid(m)) if m == ROLE_IN_SESSION_USE_MESSAGE));
    assert!(s.get_role("t1", "a").expect("get").is_some());
}

#[test]
fn alter_role_keeps_unspecified_fields() {
    let (_g, _p, s) = open("role-alter");
    let mut d = rdef("a");
    d.scram_verifier = b"SCRAM-SHA-256$x".to_vec();
    d.legacy_phc = b"legacy".to_vec();
    d.connection_limit = 3;
    let id = s.create_role("t1", &d).expect("create");
    s.alter_role("t1", "a", |r| {
        r.attributes.login = false;
        r.attributes.createdb = true;
        r.id = 999;
        r.name = "zzz".to_string();
    })
    .expect("alter");
    let got = s.get_role("t1", "a").expect("get").expect("exists");
    assert_eq!(got.id, id);
    assert_eq!(got.name, "a");
    assert!(!got.attributes.login && got.attributes.createdb && got.attributes.inherit);
    assert_eq!(got.scram_verifier, b"SCRAM-SHA-256$x".to_vec());
    assert_eq!(got.legacy_phc, b"legacy".to_vec());
    assert_eq!(got.connection_limit, 3);
    let missing = s.alter_role("t1", "nope", |_| {});
    assert!(matches!(missing, Err(CatalogError::Invalid(m)) if m == ROLE_NOT_FOUND_MESSAGE));
    let bad = s.alter_role("t1", "a", |r| r.scram_verifier = vec![0x01]);
    assert!(matches!(bad, Err(CatalogError::Invalid(_))));
}

#[test]
fn can_set_direct_indirect_and_broken_paths() {
    let (_g, _p, s) = open("role-can-set");
    let a = s.create_role("t1", &rdef("a")).expect("a");
    let b = s.create_role("t1", &rdef("b")).expect("b");
    let c = s.create_role("t1", &rdef("c")).expect("c");
    let d = s.create_role("t1", &rdef("d")).expect("d");
    let can = |from: u64, to: u64| {
        let rt = s.db().begin_read().expect("read");
        role_can_set_in_txn(&rt, "t1", from, to).expect("walk")
    };
    // 自分自身は常に可。メンバーシップが無ければ不可。
    assert!(can(a, a));
    assert!(!can(a, b));
    // 直接（SET 真）。
    put_member(&s, "t1", a, b, true);
    assert!(can(a, b));
    assert!(!can(b, a));
    // 間接（全辺が SET 真）。
    put_member(&s, "t1", b, c, true);
    assert!(can(a, c));
    // 経路中に SET 偽の辺があれば不可。
    put_member(&s, "t1", c, d, false);
    assert!(!can(a, d));
    // SET 偽だけの付与は不可。
    put_member(&s, "t1", d, a, false);
    assert!(!can(d, a));
}

#[test]
fn can_set_terminates_on_cycles_and_ignores_other_tenants() {
    let (_g, _p, s) = open("role-can-set-cycle");
    let a = s.create_role("t1", &rdef("a")).expect("a");
    let b = s.create_role("t1", &rdef("b")).expect("b");
    let c = s.create_role("t1", &rdef("c")).expect("c");
    put_member(&s, "t1", a, b, true);
    put_member(&s, "t1", b, a, true);
    let rt = s.db().begin_read().expect("read");
    assert!(!role_can_set_in_txn(&rt, "t1", a, c).expect("walk"));
    drop(rt);
    // 他テナントに同じ ID のメンバーシップがあっても辿らない。
    put_member(&s, "t2", a, c, true);
    let rt = s.db().begin_read().expect("read");
    assert!(!role_can_set_in_txn(&rt, "t1", a, c).expect("walk"));
}

#[test]
fn resolve_settable_role_distinguishes_missing_denied_allowed() {
    let (_g, _p, s) = open("role-resolve-set");
    let a = s.create_role("t1", &rdef("a")).expect("a");
    let b = s.create_role("t1", &rdef("b")).expect("b");
    s.create_role("t2", &rdef("t2only")).expect("t2");
    put_member(&s, "t1", a, b, true);
    assert_eq!(
        s.resolve_settable_role("t1", a, "b").expect("r"),
        SetRoleResolution::Allowed(b)
    );
    assert_eq!(
        s.resolve_settable_role("t1", b, "a").expect("r"),
        SetRoleResolution::Denied
    );
    assert_eq!(
        s.resolve_settable_role("t1", a, "t2only").expect("r"),
        SetRoleResolution::NotFound
    );
    assert_eq!(
        s.resolve_settable_role("t1", a, "nope").expect("r"),
        SetRoleResolution::NotFound
    );
}

#[test]
fn can_set_fails_closed_on_corrupt_membership() {
    let (_g, _p, s) = open("role-can-set-corrupt");
    let a = s.create_role("t1", &rdef("a")).expect("a");
    let b = s.create_role("t1", &rdef("b")).expect("b");
    write(&s, |t| {
        t.open_table(ROLE_MEMBERSHIPS_TABLE)?
            .insert(("t1", a, b), &[0xFFu8][..])?;
        Ok(())
    })
    .expect("raw");
    let rt = s.db().begin_read().expect("read");
    assert!(role_can_set_in_txn(&rt, "t1", a, b).is_err());
}
