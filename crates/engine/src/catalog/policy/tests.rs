//! `catalog::policy` の単体テスト（RLS-13、Issue #2274）。
//!
//! 受入基準 (A) 定義・変更・削除が再オープン後も残る、(B) 他テナントの定義が見えない・消せない、と表の
//! ライフサイクル連動（孤児定義を作らない）を Storage 水準で固定する。式の束縛は `sql::policy_expr` の
//! 責務のため、ここでは束縛クロージャへ固定の [`PolicyExprs`] を注入する。

use super::*;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};

fn tmp_path(label: &str) -> (std::path::PathBuf, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    (path, guard)
}

fn setup(s: &Storage, table: &str) {
    s.create_table(&TableSchema::new(
        table,
        vec![
            ColumnDef::new("a", ColumnType::Text, true),
            ColumnDef::new("b", ColumnType::Text, true),
        ],
    ))
    .expect("table");
}

fn pdef(name: &str) -> PolicyDef {
    PolicyDef {
        id: 0,
        name: name.to_string(),
        permissive: true,
        command: PolicyCommand::All,
        roles: vec![PolicyRole::Public],
        using_sql: None,
        check_sql: None,
        referenced_columns: Vec::new(),
    }
}

fn exprs() -> PolicyExprs {
    PolicyExprs {
        using_sql: Some("a = 'x'".to_string()),
        check_sql: Some("b = 'y'".to_string()),
        referenced_columns: vec!["a".to_string(), "b".to_string()],
    }
}

fn create(s: &Storage, scope: &str, table: &str, def: &PolicyDef) -> Result<u64> {
    s.create_policy::<CatalogError, _>(scope, table, def, |_| Ok(exprs()))
}

fn full_def(name: &str, id: u64) -> PolicyDef {
    let e = exprs();
    PolicyDef {
        id,
        using_sql: e.using_sql,
        check_sql: e.check_sql,
        referenced_columns: e.referenced_columns,
        ..pdef(name)
    }
}

#[test]
fn blob_roundtrip_and_golden_bytes() {
    let mut d = full_def("pol", 7);
    d.permissive = false;
    d.command = PolicyCommand::Update;
    let bytes = encode_policy_def(&d).expect("encode");
    assert_eq!(decode_policy_def("pol", &bytes).expect("decode"), d);

    let mut plain = pdef("p");
    plain.id = 1;
    let golden: Vec<u8> = vec![
        1, // version
        1, 0, 0, 0, 0, 0, 0, 0, // id
        0, // owner（予約）
        1, // permissive
        0, // command = ALL
        1, 0, 0, 0, 0, // roles: 1 件・PUBLIC
        0, // using なし
        0, // check なし
        0, 0, 0, 0, // 参照列 0 件
    ];
    assert_eq!(encode_policy_def(&plain).expect("encode"), golden);
}

#[test]
fn corrupt_blobs_fail_closed_with_fixed_message() {
    let d = full_def("pol", 1);
    let good = encode_policy_def(&d).expect("encode");
    let mut bad_version = good.clone();
    bad_version[0] = 9;
    let mut bad_owner = good.clone();
    bad_owner[9] = 1;
    let mut bad_command = good.clone();
    bad_command[11] = 9;
    let mut trailing = good.clone();
    trailing.push(0);
    let truncated = good[..good.len() - 1].to_vec();
    for bytes in [
        bad_version,
        bad_owner,
        bad_command,
        trailing,
        truncated,
        Vec::new(),
    ] {
        match decode_policy_def("pol", &bytes) {
            Err(CatalogError::CorruptSchema(m)) => assert_eq!(m, "invalid policy catalog entry"),
            other => panic!("expected CorruptSchema, got {other:?}"),
        }
    }
    // 上限超過の件数は確保の前に拒否する。
    let mut huge = vec![1u8];
    huge.extend_from_slice(&1u64.to_le_bytes());
    huge.extend_from_slice(&[0, 1, 0]);
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode_policy_def("pol", &huge),
        Err(CatalogError::CorruptSchema(_))
    ));
    // `SELECT` ポリシーに WITH CHECK がある blob は形状不整合として拒否する。
    let mut select_with_check = d.clone();
    select_with_check.command = PolicyCommand::Select;
    assert!(encode_policy_def(&select_with_check).is_err());
}

#[test]
fn shape_validation_rejects_invalid_definitions() {
    let mut d = pdef("p");
    d.name = String::new();
    assert!(encode_policy_def(&d).is_err());
    let mut d = pdef("p");
    d.command = PolicyCommand::Insert;
    d.using_sql = Some("a = 'x'".to_string());
    assert!(encode_policy_def(&d).is_err());
    let mut d = pdef("p");
    d.using_sql = Some("x".repeat(MAX_POLICY_EXPR_BYTES + 1));
    assert!(encode_policy_def(&d).is_err());
    let mut d = pdef("p");
    d.referenced_columns = vec!["a".to_string(), "a".to_string()];
    assert!(encode_policy_def(&d).is_err());
}

/// 受入 (A): 作成・変更・削除の結果が再オープン後も残る。
#[test]
fn create_alter_drop_survive_reopen() {
    let (path, _g) = tmp_path("pol_reopen");
    {
        let s = Storage::open(&path).expect("open");
        setup(&s, "docs");
        assert_eq!(create(&s, "t1", "docs", &pdef("one")).expect("create"), 1);
        create(&s, "t1", "docs", &pdef("two")).expect("create");
        create(&s, "t1", "docs", &pdef("three")).expect("create");
        s.rename_policy("t1", "docs", "one", "uno").expect("rename");
        s.alter_policy::<CatalogError, _>("t1", "docs", "two", |_, cur| {
            Ok(PolicyDef {
                using_sql: Some("b = 'z'".to_string()),
                referenced_columns: vec!["b".to_string()],
                check_sql: None,
                ..cur.clone()
            })
        })
        .expect("alter");
        assert!(s.drop_policy("t1", "docs", "three").expect("drop"));
    }
    let s = Storage::open(&path).expect("reopen");
    let list = s.list_policies("t1", "docs").expect("list");
    let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["two", "uno"]);
    let two = &list[0];
    assert_eq!(two.id, 2);
    assert_eq!(two.using_sql.as_deref(), Some("b = 'z'"));
    assert_eq!(two.check_sql, None);
    // 改名後も内部 ID を引き継ぐ。
    assert_eq!(list[1].id, 1);
    assert_eq!(list[1].using_sql.as_deref(), Some("a = 'x'"));
}

/// 受入 (B): 他テナントの定義は list／alter／rename／drop のいずれでも見えない・触れない。
#[test]
fn other_tenants_cannot_see_or_touch_definitions() {
    let (path, _g) = tmp_path("pol_tenant");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    create(&s, "t1", "docs", &pdef("p")).expect("create");
    assert!(s.list_policies("t2", "docs").expect("list").is_empty());
    assert!(!s.drop_policy("t2", "docs", "p").expect("drop"));
    match s.rename_policy("t2", "docs", "p", "q") {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, POLICY_NOT_FOUND_MESSAGE),
        other => panic!("{other:?}"),
    }
    match s.alter_policy::<CatalogError, _>("t2", "docs", "p", |_, cur| Ok(cur.clone())) {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, POLICY_NOT_FOUND_MESSAGE),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.list_policies("t1", "docs").expect("list").len(), 1);
    // 別テナントは同じ表に同名のポリシーを作れる（存在を漏らす 42710 にならない）。
    create(&s, "t2", "docs", &pdef("p")).expect("t2 create");
    assert!(s.drop_policy("t1", "docs", "p").expect("drop t1"));
    assert_eq!(s.list_policies("t2", "docs").expect("list").len(), 1);
}

#[test]
fn create_judgement_order_and_errors() {
    let (path, _g) = tmp_path("pol_errors");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    // 表なし。
    assert!(matches!(
        create(&s, "t1", "nope", &pdef("p")),
        Err(CatalogError::TableNotFound(_))
    ));
    // 束縛の失敗はそのまま返り、何も作られない。
    let r = s.create_policy::<CatalogError, _>("t1", "docs", &pdef("p"), |_| {
        Err(CatalogError::ColumnNotFound("zzz".to_string()))
    });
    assert!(matches!(r, Err(CatalogError::ColumnNotFound(_))));
    assert!(s.list_policies("t1", "docs").expect("list").is_empty());
    // 同名の既存。
    create(&s, "t1", "docs", &pdef("p")).expect("create");
    match create(&s, "t1", "docs", &pdef("p")) {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, POLICY_ALREADY_EXISTS_MESSAGE),
        other => panic!("{other:?}"),
    }
    // 改名先の重複。
    create(&s, "t1", "docs", &pdef("q")).expect("create q");
    match s.rename_policy("t1", "docs", "p", "q") {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, POLICY_ALREADY_EXISTS_MESSAGE),
        other => panic!("{other:?}"),
    }
    // 不在。
    match s.rename_policy("t1", "docs", "nope", "r") {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, POLICY_NOT_FOUND_MESSAGE),
        other => panic!("{other:?}"),
    }
}

#[test]
fn per_table_limit_is_enforced_and_ids_are_not_reused() {
    let (path, _g) = tmp_path("pol_limit");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    let mk = |name: &str| {
        s.create_policy_inner::<CatalogError, _>("t1", "docs", &pdef(name), |_| Ok(exprs()), 2)
    };
    assert_eq!(mk("a1").expect("1"), 1);
    assert_eq!(mk("a2").expect("2"), 2);
    assert!(matches!(mk("a3"), Err(CatalogError::ViewLimitExceeded(_))));
    // 他テナントは別枠。
    assert!(s
        .create_policy_inner::<CatalogError, _>("t2", "docs", &pdef("a1"), |_| Ok(exprs()), 2)
        .is_ok());
    assert!(s.drop_policy("t1", "docs", "a1").expect("drop"));
    // 削除しても ID は再利用されない。
    assert_eq!(mk("a4").expect("4"), 4);
}

#[test]
fn drop_policy_contract() {
    let (path, _g) = tmp_path("pol_drop");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    assert!(matches!(
        s.drop_policy("t1", "nope", "p"),
        Err(CatalogError::TableNotFound(_))
    ));
    assert!(!s.drop_policy("t1", "docs", "p").expect("absent"));
    create(&s, "t1", "docs", &pdef("p")).expect("create");
    assert!(s.drop_policy("t1", "docs", "p").expect("present"));
    assert!(!s.drop_policy("t1", "docs", "p").expect("again"));
}

#[test]
fn drop_table_removes_policies_of_all_tenants() {
    let (path, _g) = tmp_path("pol_droptable");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    create(&s, "t1", "docs", &pdef("p")).expect("c1");
    create(&s, "t2", "docs", &pdef("p")).expect("c2");
    s.drop_table("docs").expect("drop table");
    setup(&s, "docs");
    assert!(s.list_policies("t1", "docs").expect("l").is_empty());
    assert!(s.list_policies("t2", "docs").expect("l").is_empty());
}

#[test]
fn rename_table_moves_policies_of_all_tenants() {
    let (path, _g) = tmp_path("pol_rename");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    create(&s, "t1", "docs", &pdef("p")).expect("c1");
    create(&s, "t2", "docs", &pdef("p")).expect("c2");
    s.alter_table_rename("docs", "docs2").expect("rename");
    assert!(s.list_policies("t1", "docs").expect("l").is_empty());
    assert_eq!(s.list_policies("t1", "docs2").expect("l").len(), 1);
    assert_eq!(s.list_policies("t2", "docs2").expect("l").len(), 1);
}

#[test]
fn column_ddl_is_guarded_by_policy_references() {
    let (path, _g) = tmp_path("pol_column");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    create(&s, "t1", "docs", &pdef("p")).expect("create");
    assert!(matches!(
        s.alter_table_drop_column("docs", "a"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    assert!(s.alter_table_rename_column("docs", "a", "c").is_err());
    assert!(s
        .alter_table_alter_column_type("docs", "a", &ColumnType::Integer)
        .is_err());
}

#[test]
fn unreferenced_column_ddl_still_works() {
    let (path, _g) = tmp_path("pol_column_free");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    s.create_policy::<CatalogError, _>("t1", "docs", &pdef("p"), |_| {
        Ok(PolicyExprs {
            using_sql: Some("a = 'x'".to_string()),
            check_sql: None,
            referenced_columns: vec!["a".to_string()],
        })
    })
    .expect("create");
    s.alter_table_drop_column("docs", "b").expect("drop b");
}

#[test]
fn lazy_table_creation_leaves_existing_layout_untouched() {
    use redb::TableHandle;
    let (path, _g) = tmp_path("pol_lazy");
    let s = Storage::open(&path).expect("open");
    setup(&s, "docs");
    s.alter_table_rename("docs", "docs2").expect("rename");
    s.drop_table("docs2").expect("drop");
    assert!(s.list_policies("t1", "docs").expect("list").is_empty());
    let rt = s.db().begin_read().expect("read");
    let has_policy_table = rt
        .list_tables()
        .expect("list")
        .any(|h| h.name() == "policies" || h.name() == "policy_id_counter");
    assert!(!has_policy_table);
}
