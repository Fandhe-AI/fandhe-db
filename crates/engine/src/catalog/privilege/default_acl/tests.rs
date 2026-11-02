//! `catalog::privilege::default_acl` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2237）。
//!
//! 結果の ACL の保存・組み込みの既定へ戻ったときの削除・関数の全体エントリの印・作成時の合成と適用
//! （予約なし・OR REPLACE を除く適用は結合テストが固定する）・破損 blob の fail-closed・件数上限・
//! テナント分離・`DROP SCHEMA` と `DROP ROLE` のライフサイクルを Storage 水準で固定する。

use super::*;
use crate::catalog::privilege::{list_acl_for_object_in_txn, PRIV_INSERT, PRIV_SELECT, PRIV_USAGE};
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const T: &str = "dt";

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, s)
}

fn role(s: &Storage, tenant: &str, name: &str) -> u64 {
    s.create_role(
        tenant,
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
    tenant: &str,
    is_grant: bool,
    actor: u64,
    operator: bool,
    for_roles: &[&str],
    schemas: &[&str],
    kind: DefaultAclKind,
    privs: u16,
    grantees: &[GranteeSpec],
    go: bool,
) -> std::result::Result<usize, AclChangeError> {
    let for_roles: Vec<String> = for_roles.iter().map(|x| x.to_string()).collect();
    let schemas: Vec<String> = schemas.iter().map(|x| x.to_string()).collect();
    s.apply_default_acl_changes(
        tenant,
        &DefaultAclRequest {
            is_grant,
            actor_id: actor,
            operator,
            for_roles: &for_roles,
            schemas: &schemas,
            kind,
            privileges: privs,
            grantees,
            grant_option: go,
        },
    )
}

fn grant_to(
    s: &Storage,
    actor: u64,
    kind: DefaultAclKind,
    privs: u16,
    who: &str,
) -> std::result::Result<usize, AclChangeError> {
    run(
        s,
        T,
        true,
        actor,
        false,
        &[],
        &[],
        kind,
        privs,
        &[GranteeSpec::Name(who.to_string())],
        false,
    )
}

fn write_txn(s: &Storage, f: impl FnOnce(&redb::WriteTransaction)) {
    let txn = s.begin_write_txn().expect("begin");
    f(&txn);
    crate::recovery::commit_boundary::commit(txn).expect("commit");
}

#[test]
fn grant_stores_the_resulting_acl_and_revoke_returns_to_builtin() {
    let (_g, s) = open("dacl_roundtrip");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "bob").expect("grant");
    let rows = s.list_default_acl(T).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role_id, alice);
    assert_eq!(rows[0].schema_id, 0);
    assert_eq!(rows[0].kind, DefaultAclKind::Tables);
    assert_eq!(rows[0].entry.grantee_id, bob);
    assert_eq!(rows[0].entry.privileges, PRIV_SELECT);
    // 追加の付与は結果へ合算される。
    grant_to(&s, alice, DefaultAclKind::Tables, PRIV_INSERT, "bob").expect("grant");
    assert_eq!(
        s.list_default_acl(T).expect("list")[0].entry.privileges,
        PRIV_SELECT | PRIV_INSERT
    );
    // 全部取り消すと行が消える（組み込みの既定は空）。
    run(
        &s,
        T,
        false,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Tables,
        PRIV_SELECT | PRIV_INSERT,
        &[GranteeSpec::Name("bob".into())],
        false,
    )
    .expect("revoke");
    assert!(s.list_default_acl(T).expect("list").is_empty());
    // 何も変わらない文は 0 件（commit しない）。
    assert_eq!(
        run(
            &s,
            T,
            false,
            alice,
            false,
            &[],
            &[],
            DefaultAclKind::Tables,
            PRIV_SELECT,
            &[GranteeSpec::Name("bob".into())],
            false,
        )
        .expect("noop"),
        0
    );
}

#[test]
fn grant_option_revoke_keeps_the_privilege() {
    let (_g, s) = open("dacl_go");
    let alice = role(&s, T, "alice");
    role(&s, T, "bob");
    run(
        &s,
        T,
        true,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Tables,
        PRIV_SELECT,
        &[GranteeSpec::Name("bob".into())],
        true,
    )
    .expect("grant with option");
    assert_eq!(
        s.list_default_acl(T).expect("l")[0].entry.grant_options,
        PRIV_SELECT
    );
    run(
        &s,
        T,
        false,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Tables,
        PRIV_SELECT,
        &[GranteeSpec::Name("bob".into())],
        true,
    )
    .expect("revoke option");
    let e = s.list_default_acl(T).expect("l")[0].entry;
    assert_eq!((e.privileges, e.grant_options), (PRIV_SELECT, 0));
}

#[test]
fn owner_as_grantee_is_not_stored() {
    let (_g, s) = open("dacl_owner_self");
    let alice = role(&s, T, "alice");
    assert_eq!(
        grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "alice").expect("g"),
        0
    );
    assert!(s.list_default_acl(T).expect("list").is_empty());
}

#[test]
fn functions_global_entry_starts_from_public_execute_and_marks() {
    let (_g, s) = open("dacl_fn_marker");
    let alice = role(&s, T, "alice");
    // PUBLIC の EXECUTE を取り消すと、組み込みの既定と異なる（空）ので印だけが残る。
    run(
        &s,
        T,
        false,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Functions,
        PRIV_EXECUTE,
        &[GranteeSpec::Public],
        false,
    )
    .expect("revoke public");
    assert!(s.list_default_acl(T).expect("l").is_empty());
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(init_marker_in_txn(&rt, T, alice).expect("marker"));
    // 作成時の合成: 空の結果（組み込みの既定と異なる）。
    assert_eq!(
        compose_in_txn(&rt, T, alice, 0, DefaultAclKind::Functions).expect("c"),
        Some(Vec::new())
    );
    drop(rt);
    // 付与し直すと組み込みの既定へ戻り、印も消える。
    run(
        &s,
        T,
        true,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Functions,
        PRIV_EXECUTE,
        &[GranteeSpec::Public],
        false,
    )
    .expect("grant public");
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(!init_marker_in_txn(&rt, T, alice).expect("marker"));
    assert_eq!(
        compose_in_txn(&rt, T, alice, 0, DefaultAclKind::Functions).expect("c"),
        None
    );
    assert!(s.list_default_acl(T).expect("l").is_empty());
}

#[test]
fn schema_specific_entries_merge_with_global_ones() {
    let (_g, s) = open("dacl_merge");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    let carol = role(&s, T, "carol");
    s.create_schema_owned(T, "sch", false, None)
        .expect("schema");
    let sid = s.schema_id(T, "sch").expect("id").expect("some");
    grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "bob").expect("global");
    run(
        &s,
        T,
        true,
        alice,
        false,
        &[],
        &["sch"],
        DefaultAclKind::Tables,
        PRIV_INSERT,
        &[
            GranteeSpec::Name("bob".into()),
            GranteeSpec::Name("carol".into()),
        ],
        false,
    )
    .expect("scoped");
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    // public（スコープ 0）の作成には全体エントリだけ。
    let g = compose_in_txn(&rt, T, alice, 0, DefaultAclKind::Tables)
        .expect("c")
        .expect("some");
    assert_eq!(g.len(), 1);
    assert_eq!((g[0].grantee_id, g[0].privileges), (bob, PRIV_SELECT));
    // SCHEMA 配下の作成には両方が合成される。
    let m = compose_in_txn(&rt, T, alice, sid, DefaultAclKind::Tables)
        .expect("c")
        .expect("some");
    assert_eq!(m.len(), 2);
    assert_eq!(
        (m[0].grantee_id, m[0].privileges),
        (bob, PRIV_SELECT | PRIV_INSERT)
    );
    assert_eq!((m[1].grantee_id, m[1].privileges), (carol, PRIV_INSERT));
    drop(rt);
    // DROP SCHEMA 相当の掃除はスキーマ別の行だけを消す。
    write_txn(&s, |t| {
        remove_schema_default_acl_in_txn(t, T, sid).expect("sweep")
    });
    let rows = s.list_default_acl(T).expect("l");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].schema_id, 0);
}

#[test]
fn creation_application_writes_acl_rows_with_owner_as_grantor() {
    let (_g, s) = open("dacl_apply");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "bob").expect("g");
    grant_to(&s, alice, DefaultAclKind::Sequences, PRIV_USAGE, "bob").expect("g");
    grant_to(&s, alice, DefaultAclKind::Functions, PRIV_EXECUTE, "bob").expect("g");
    write_txn(&s, |t| {
        apply_default_acl_to_relation_in_txn(t, T, alice, "newt").expect("rel");
        apply_default_acl_to_sequence_in_txn(t, T, alice, "newt_seq", 7).expect("seq");
        apply_default_acl_to_function_in_txn(t, T, alice, "newf", 9).expect("fn");
    });
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    let rel =
        list_acl_for_object_in_txn(&rt, T, &PrivilegeObject::Table("newt".into())).expect("l");
    assert_eq!(rel.len(), 1);
    assert_eq!(
        (rel[0].grantee_id, rel[0].grantor_id, rel[0].privileges),
        (bob, alice, PRIV_SELECT)
    );
    let seq = list_acl_for_object_in_txn(&rt, T, &PrivilegeObject::Sequence(7)).expect("l");
    assert_eq!((seq[0].grantee_id, seq[0].privileges), (bob, PRIV_USAGE));
    // 関数: 行と印が一緒に書かれる（PUBLIC EXECUTE の既定は初回の変更で展開済み）。
    let f = crate::catalog::privilege::function_privileges_in_txn(&rt, T, 9, &[bob]).expect("fp");
    assert_eq!(f, PRIV_EXECUTE);
    let other =
        crate::catalog::privilege::function_privileges_in_txn(&rt, T, 9, &[999]).expect("fp");
    assert_eq!(other, PRIV_EXECUTE); // PUBLIC の EXECUTE も残る（組み込みの既定 ∪ bob）。
                                     // 別ロールの作成には作用しない。
    drop(rt);
    write_txn(&s, |t| {
        apply_default_acl_to_relation_in_txn(t, T, bob, "bobt").expect("rel");
    });
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(
        list_acl_for_object_in_txn(&rt, T, &PrivilegeObject::Table("bobt".into()))
            .expect("l")
            .is_empty()
    );
}

#[test]
fn function_default_without_any_change_writes_nothing() {
    let (_g, s) = open("dacl_fn_none");
    let alice = role(&s, T, "alice");
    write_txn(&s, |t| {
        apply_default_acl_to_function_in_txn(t, T, alice, "f", 3).expect("fn");
    });
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    // 印も行も無い（暗黙の既定のまま。「印の無い行」の不整合を作らない）。
    assert_eq!(
        crate::catalog::privilege::function_privileges_in_txn(&rt, T, 3, &[1]).expect("fp"),
        PRIV_EXECUTE
    );
}

#[test]
fn revoke_public_execute_default_creates_function_with_marker_and_no_rows() {
    let (_g, s) = open("dacl_fn_revoked");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    run(
        &s,
        T,
        false,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Functions,
        PRIV_EXECUTE,
        &[GranteeSpec::Public],
        false,
    )
    .expect("revoke");
    write_txn(&s, |t| {
        apply_default_acl_to_function_in_txn(t, T, alice, "f", 3).expect("fn");
    });
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert_eq!(
        crate::catalog::privilege::function_privileges_in_txn(&rt, T, 3, &[bob]).expect("fp"),
        0
    );
}

#[test]
fn for_role_requires_set_path_unless_operator() {
    let (_g, s) = open("dacl_for_role");
    let alice = role(&s, T, "alice");
    role(&s, T, "bob");
    let call = |operator: bool| {
        run(
            &s,
            T,
            true,
            alice,
            operator,
            &["bob"],
            &[],
            DefaultAclKind::Tables,
            PRIV_SELECT,
            &[GranteeSpec::Public],
            false,
        )
    };
    assert!(matches!(call(false), Err(AclChangeError::PermissionDenied)));
    assert!(s.list_default_acl(T).expect("l").is_empty());
    assert_eq!(call(true).expect("operator"), 1);
}

#[test]
fn missing_and_cross_tenant_names_are_indistinguishable() {
    let (_g, s) = open("dacl_tenant");
    let alice = role(&s, T, "alice");
    role(&s, "other", "mallory");
    s.create_schema_owned("other", "theirs", false, None)
        .expect("schema");
    // 他テナントにだけ在るロール・SCHEMA は、不在と同じ分類になる。
    assert!(matches!(
        grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "mallory"),
        Err(AclChangeError::GranteeNotFound(n)) if n == "mallory"
    ));
    assert!(matches!(
        grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "ghost"),
        Err(AclChangeError::GranteeNotFound(n)) if n == "ghost"
    ));
    let sch = |name: &str| {
        run(
            &s,
            T,
            true,
            alice,
            false,
            &[],
            &[name],
            DefaultAclKind::Tables,
            PRIV_SELECT,
            &[GranteeSpec::Public],
            false,
        )
    };
    assert!(matches!(
        sch("theirs"),
        Err(AclChangeError::SchemaNotFound(_))
    ));
    assert!(matches!(
        sch("nope"),
        Err(AclChangeError::SchemaNotFound(_))
    ));
    assert!(matches!(
        run(
            &s,
            T,
            true,
            alice,
            true,
            &["mallory"],
            &[],
            DefaultAclKind::Tables,
            PRIV_SELECT,
            &[GranteeSpec::Public],
            false
        ),
        Err(AclChangeError::GranteeNotFound(_))
    ));
    // 失敗した文は何も保存しない。他テナントの既定権限は引けない。
    assert!(s.list_default_acl(T).expect("l").is_empty());
    assert!(s.list_default_acl("other").expect("l").is_empty());
}

#[test]
fn one_missing_grantee_stores_nothing() {
    let (_g, s) = open("dacl_atomic");
    let alice = role(&s, T, "alice");
    role(&s, T, "bob");
    let r = run(
        &s,
        T,
        true,
        alice,
        false,
        &[],
        &[],
        DefaultAclKind::Tables,
        PRIV_SELECT,
        &[
            GranteeSpec::Name("bob".into()),
            GranteeSpec::Name("ghost".into()),
        ],
        false,
    );
    assert!(matches!(r, Err(AclChangeError::GranteeNotFound(_))));
    assert!(s.list_default_acl(T).expect("l").is_empty());
}

#[test]
fn drop_role_dependency_covers_target_and_grantee() {
    let (_g, s) = open("dacl_dep");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    let carol = role(&s, T, "carol");
    grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "bob").expect("g");
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(default_acl_references_role_in_txn(&rt, T, alice).expect("d"));
    assert!(default_acl_references_role_in_txn(&rt, T, bob).expect("d"));
    assert!(!default_acl_references_role_in_txn(&rt, T, carol).expect("d"));
    // PUBLIC・他テナントは参照に数えない。
    assert!(!default_acl_references_role_in_txn(&rt, T, 0).expect("d"));
    assert!(!default_acl_references_role_in_txn(&rt, "other", alice).expect("d"));
    drop(rt);
    // 関数の全体エントリが空でも印だけで対象ロールの参照になる。
    run(
        &s,
        T,
        false,
        carol,
        false,
        &[],
        &[],
        DefaultAclKind::Functions,
        PRIV_EXECUTE,
        &[GranteeSpec::Public],
        false,
    )
    .expect("revoke");
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(default_acl_references_role_in_txn(&rt, T, carol).expect("d"));
}

#[test]
fn corrupt_rows_fail_closed() {
    let (_g, s) = open("dacl_corrupt");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    for (kind, bytes) in [
        (KIND_TABLE, vec![2u8, 1, 0, 0, 0]),    // 未知の版
        (KIND_TABLE, vec![1u8, 0, 0, 0, 0]),    // 権限 0
        (KIND_TABLE, vec![1u8, 0, 1, 0, 0]),    // 許可集合の外（USAGE）
        (KIND_TABLE, vec![1u8, 1, 0, 2, 0]),    // GRANT OPTION が部分集合でない
        (KIND_TABLE, vec![1u8, 1, 0, 0]),       // 切断
        (KIND_TABLE, vec![1u8, 1, 0, 0, 0, 0]), // 末尾の余り
        (9u8, vec![1u8, 1, 0, 0, 0]),           // 未知の種別
    ] {
        write_txn(&s, |t| {
            let mut table = t.open_table(DEFAULT_ACL_TABLE).expect("t");
            table
                .insert((T, alice, 0u64, kind, bob), bytes.as_slice())
                .expect("ins");
        });
        let r = s.list_default_acl(T);
        assert!(
            matches!(&r, Err(CatalogError::CorruptSchema(m)) if m == "invalid privilege catalog entry"),
            "{bytes:?}: {r:?}"
        );
        let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
        assert!(compose_in_txn(&rt, T, alice, 0, DefaultAclKind::Tables).is_err() || kind == 9);
        drop(rt);
        write_txn(&s, |t| {
            let mut table = t.open_table(DEFAULT_ACL_TABLE).expect("t");
            table.remove((T, alice, 0u64, kind, bob)).expect("rm");
        });
    }
    // 所有者自身を付与先とする行も破損。
    write_txn(&s, |t| {
        let mut table = t.open_table(DEFAULT_ACL_TABLE).expect("t");
        table
            .insert(
                (T, alice, 0u64, KIND_TABLE, alice),
                [1u8, 1, 0, 0, 0].as_slice(),
            )
            .expect("ins");
    });
    assert!(s.list_default_acl(T).is_err());
}

#[test]
fn function_rows_without_marker_are_corrupt() {
    let (_g, s) = open("dacl_fn_nomark");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    write_txn(&s, |t| {
        let mut table = t.open_table(DEFAULT_ACL_TABLE).expect("t");
        table
            .insert(
                (T, alice, 0u64, KIND_FUNCTION, bob),
                [1u8, 0, 4, 0, 0].as_slice(),
            )
            .expect("ins");
    });
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert!(compose_in_txn(&rt, T, alice, 0, DefaultAclKind::Functions).is_err());
}

#[test]
fn tenant_row_count_is_capped() {
    let (_g, s) = open("dacl_cap");
    let alice = role(&s, T, "alice");
    let bob = role(&s, T, "bob");
    // 上限ちょうどまで直接積み、もう 1 件の付与が `LimitExceeded` で拒否され何も書かれないことを確かめる。
    write_txn(&s, |t| {
        let mut table = t.open_table(DEFAULT_ACL_TABLE).expect("t");
        for i in 0..MAX_DEFAULT_ACL_ENTRIES_PER_TENANT as u64 {
            // 付与先 ID は実在しなくても構造上は書ける（上限判定だけを見る）。
            table
                .insert(
                    (T, alice, 0u64, KIND_SEQUENCE, 1_000 + i),
                    [1u8, 0, 1, 0, 0].as_slice(),
                )
                .expect("ins");
        }
    });
    let r = grant_to(&s, alice, DefaultAclKind::Tables, PRIV_SELECT, "bob");
    assert!(matches!(r, Err(AclChangeError::LimitExceeded)), "{r:?}");
    let rt = redb::ReadableDatabase::begin_read(s.db()).expect("rt");
    assert_eq!(
        load_entries(&rt, T, alice, 0, DefaultAclKind::Tables)
            .expect("l")
            .len(),
        0
    );
    let _ = bob;
}
