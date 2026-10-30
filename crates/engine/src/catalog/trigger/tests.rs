//! `catalog::trigger` の単体テスト（TABLE-45、Issue #1722）。
//!
//! 受入基準 (A) 再起動後も定義が残る・(B) 他テナントの定義が見えない、と表・関数・SCHEMA の
//! ライフサイクル連動（孤児定義を作らない）を Storage 水準で固定する。`RETURNS trigger` 関数の保存形式
//! （#2228）が未整備のため、関数の判定は `create_trigger_inner` へ注入する。

use super::*;
use crate::catalog::function::{
    FunctionAttributes, FunctionDef, FunctionLanguage, FunctionParam, FunctionType,
};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};

fn tmp_path(label: &str) -> (std::path::PathBuf, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    (path, guard)
}

fn always_trigger(_: &FunctionDef) -> bool {
    true
}

fn func(key: &str) -> FunctionDef {
    FunctionDef {
        id: 1,
        function_key: key.to_string(),
        params: Vec::<FunctionParam>::new(),
        return_type: Some(FunctionType::from_column_type(&ColumnType::Integer).expect("ty")),
        language: FunctionLanguage::Sql,
        body_source: "SELECT 1".to_string(),
        attrs: FunctionAttributes::default(),
    }
}

fn setup(s: &Storage, scope: &str, table: &str, fkey: &str) {
    let _ = s.create_table(&TableSchema::new(
        table,
        vec![
            ColumnDef::new("a", ColumnType::Text, true),
            ColumnDef::new("b", ColumnType::Text, true),
        ],
    ));
    s.create_function(scope, &func(fkey)).expect("fn");
}

fn tdef(name: &str, fkey: &str) -> TriggerDef {
    TriggerDef {
        id: 0,
        name: name.to_string(),
        timing: TriggerTiming::Before,
        events: TriggerEvents::from_bits(TriggerEvents::INSERT | TriggerEvents::UPDATE)
            .expect("events"),
        level: TriggerLevel::Row,
        update_of: vec!["a".to_string()],
        function_key: fkey.to_string(),
        tg_args: vec!["x".to_string(), "1".to_string()],
        when_sql: None,
    }
}

fn create(s: &Storage, scope: &str, table: &str, def: &TriggerDef) -> Result<u64> {
    s.create_trigger_inner(
        scope,
        table,
        def,
        false,
        MAX_TRIGGERS_PER_TABLE,
        always_trigger,
    )
}

#[test]
fn blob_roundtrip_and_golden_bytes() {
    let mut d = tdef("trg", "f");
    d.id = 7;
    d.when_sql = Some("a > 1".to_string());
    let bytes = encode_trigger_def(&d).expect("encode");
    assert_eq!(decode_trigger_def("trg", &bytes).expect("decode"), d);

    let mut plain = tdef("t", "f");
    plain.id = 1;
    plain.update_of.clear();
    plain.tg_args.clear();
    plain.events =
        TriggerEvents::from_bits(TriggerEvents::INSERT | TriggerEvents::UPDATE).expect("events");
    let golden: Vec<u8> = vec![
        1, // version
        1, 0, 0, 0, 0, 0, 0, 0,      // id
        0,      // owner
        0,      // timing BEFORE
        0b0011, // events
        0,      // level ROW
        0, 0, 0, 0, // update_of
        1, 0, 0, 0, b'f', // function_key
        0, 0, 0, 0, // tg_args
        0, // when
    ];
    assert_eq!(encode_trigger_def(&plain).expect("encode"), golden);
}

#[test]
fn corrupt_blobs_fail_closed_with_fixed_message() {
    let mut d = tdef("trg", "f");
    d.id = 1;
    let good = encode_trigger_def(&d).expect("encode");
    let mut bad_version = good.clone();
    bad_version[0] = 9;
    let mut bad_events = good.clone();
    bad_events[11] = 0b1_0000;
    let mut trailing = good.clone();
    trailing.push(0);
    let truncated = good[..good.len() - 1].to_vec();
    for bytes in [bad_version, bad_events, trailing, truncated, Vec::new()] {
        match decode_trigger_def("trg", &bytes) {
            Err(CatalogError::CorruptSchema(m)) => {
                assert_eq!(m, "invalid trigger catalog entry");
            }
            other => panic!("expected CorruptSchema, got {other:?}"),
        }
    }
    // 上限超過の件数は確保の前に拒否する。
    let mut huge_list = vec![1u8];
    huge_list.extend_from_slice(&1u64.to_le_bytes());
    huge_list.extend_from_slice(&[0, 0, 1, 0]);
    huge_list.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        decode_trigger_def("trg", &huge_list),
        Err(CatalogError::CorruptSchema(_))
    ));
}

#[test]
fn shape_validation_rejects_invalid_definitions() {
    let mut d = tdef("trg", "f");
    d.level = TriggerLevel::Row;
    d.events = TriggerEvents::from_bits(TriggerEvents::TRUNCATE).expect("e");
    d.update_of.clear();
    assert!(matches!(
        encode_trigger_def(&d),
        Err(CatalogError::Invalid(_))
    ));
    let mut d = tdef("trg", "f");
    d.events = TriggerEvents::from_bits(TriggerEvents::INSERT).expect("e");
    assert!(matches!(
        encode_trigger_def(&d),
        Err(CatalogError::Invalid(_))
    ));
    let mut d = tdef("trg", "f");
    d.update_of = vec!["a".to_string(), "a".to_string()];
    assert!(matches!(
        encode_trigger_def(&d),
        Err(CatalogError::Invalid(_))
    ));
    let mut d = tdef("trg", "f");
    d.tg_args = vec!["x".to_string(); MAX_TRIGGER_ARGS + 1];
    assert!(matches!(
        encode_trigger_def(&d),
        Err(CatalogError::Invalid(_))
    ));
    assert!(TriggerEvents::from_bits(0).is_err());
    assert!(TriggerEvents::from_bits(0b1_0000).is_err());
}

/// 受入 (A): 再オープン後も定義が残る。
#[test]
fn definitions_survive_reopen() {
    let (path, _g) = tmp_path("trg_reopen");
    {
        let s = Storage::open(&path).expect("open");
        setup(&s, "t1", "docs", "f");
        let id = create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
        assert_eq!(id, 1);
    }
    let s = Storage::open(&path).expect("reopen");
    let list = s.list_triggers("t1", "docs").expect("list");
    assert_eq!(list.len(), 1);
    let mut want = tdef("trg", "f");
    want.id = 1;
    assert_eq!(list[0], want);
}

/// 受入 (B): 他テナントの定義は list／drop のいずれでも見えない。
#[test]
fn other_tenants_cannot_see_or_drop_definitions() {
    let (path, _g) = tmp_path("trg_tenant");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    s.create_function("t2", &func("f")).expect("fn2");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    assert!(s.list_triggers("t2", "docs").expect("list").is_empty());
    assert!(!s.drop_trigger("t2", "docs", "trg").expect("drop"));
    assert_eq!(s.list_triggers("t1", "docs").expect("list").len(), 1);
    // 別テナントは同じ表に同名のトリガを作れる（存在を漏らす 42710 にならない）。
    create(&s, "t2", "docs", &tdef("trg", "f")).expect("t2 create");
    assert!(s.drop_trigger("t1", "docs", "trg").expect("drop t1"));
    assert_eq!(s.list_triggers("t2", "docs").expect("list").len(), 1);
}

#[test]
fn create_judgement_order_and_errors() {
    let (path, _g) = tmp_path("trg_errors");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    // 表なし。
    assert!(matches!(
        create(&s, "t1", "nope", &tdef("trg", "f")),
        Err(CatalogError::TableNotFound(_))
    ));
    // UPDATE OF の未知の列。
    let mut d = tdef("trg", "f");
    d.update_of = vec!["zzz".to_string()];
    assert!(matches!(
        create(&s, "t1", "docs", &d),
        Err(CatalogError::ColumnNotFound(_))
    ));
    // 関数なし（他テナントにしか無い関数も同じ）。
    s.create_function("t2", &func("g")).expect("g");
    match create(&s, "t1", "docs", &tdef("trg", "g")) {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, TRIGGER_FUNCTION_NOT_FOUND_MESSAGE),
        other => panic!("{other:?}"),
    }
    // 関数はあるが RETURNS trigger ではない（現状は常に）。
    match s.create_trigger("t1", "docs", &tdef("trg", "f"), false) {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE),
        other => panic!("{other:?}"),
    }
    // 同名の既存。
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    match create(&s, "t1", "docs", &tdef("trg", "f")) {
        Err(CatalogError::Invalid(m)) => assert_eq!(m, TRIGGER_ALREADY_EXISTS_MESSAGE),
        other => panic!("{other:?}"),
    }
}

#[test]
fn or_replace_is_atomic_and_keeps_id() {
    let (path, _g) = tmp_path("trg_replace");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    let id = create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    let mut d = tdef("trg", "f");
    d.timing = TriggerTiming::After;
    d.tg_args.clear();
    let id2 = s
        .create_trigger_inner(
            "t1",
            "docs",
            &d,
            true,
            MAX_TRIGGERS_PER_TABLE,
            always_trigger,
        )
        .expect("replace");
    assert_eq!(id, id2);
    let list = s.list_triggers("t1", "docs").expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].timing, TriggerTiming::After);
    assert!(list[0].tg_args.is_empty());
}

#[test]
fn per_table_limit_is_enforced_and_ids_are_not_reused() {
    let (path, _g) = tmp_path("trg_limit");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    for i in 0..2 {
        s.create_trigger_inner(
            "t1",
            "docs",
            &tdef(&format!("t{i}"), "f"),
            false,
            2,
            always_trigger,
        )
        .expect("create");
    }
    assert!(matches!(
        s.create_trigger_inner("t1", "docs", &tdef("t9", "f"), false, 2, always_trigger),
        Err(CatalogError::ViewLimitExceeded(_))
    ));
    assert!(s.drop_trigger("t1", "docs", "t0").expect("drop"));
    let id = s
        .create_trigger_inner("t1", "docs", &tdef("t3", "f"), false, 2, always_trigger)
        .expect("create");
    assert_eq!(id, 3);
}

#[test]
fn drop_trigger_contract() {
    let (path, _g) = tmp_path("trg_drop");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    assert!(matches!(
        s.drop_trigger("t1", "nope", "trg"),
        Err(CatalogError::TableNotFound(_))
    ));
    assert!(!s.drop_trigger("t1", "docs", "trg").expect("absent"));
}

/// 表の DROP は全テナント分を消し、同名の表を再作成しても復活しない。
#[test]
fn drop_table_removes_triggers_of_all_tenants() {
    let (path, _g) = tmp_path("trg_droptable");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    s.create_function("t2", &func("f")).expect("fn2");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("c1");
    create(&s, "t2", "docs", &tdef("trg", "f")).expect("c2");
    s.drop_table("docs").expect("drop table");
    setup_table_only(&s, "docs");
    assert!(s.list_triggers("t1", "docs").expect("l").is_empty());
    assert!(s.list_triggers("t2", "docs").expect("l").is_empty());
}

fn setup_table_only(s: &Storage, table: &str) {
    s.create_table(&TableSchema::new(
        table,
        vec![ColumnDef::new("a", ColumnType::Text, true)],
    ))
    .expect("table");
}

#[test]
fn rename_table_moves_triggers_of_all_tenants() {
    let (path, _g) = tmp_path("trg_rename");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    s.create_function("t2", &func("f")).expect("fn2");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("c1");
    create(&s, "t2", "docs", &tdef("trg", "f")).expect("c2");
    s.alter_table_rename("docs", "docs2").expect("rename");
    assert!(s.list_triggers("t1", "docs").expect("l").is_empty());
    assert_eq!(s.list_triggers("t1", "docs2").expect("l").len(), 1);
    assert_eq!(s.list_triggers("t2", "docs2").expect("l").len(), 1);
}

#[test]
fn drop_function_is_blocked_by_dependent_trigger() {
    let (path, _g) = tmp_path("trg_dropfn");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    assert!(matches!(
        s.drop_function_by_name("t1", "f", false),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    assert!(s.drop_trigger("t1", "docs", "trg").expect("drop"));
    assert!(s
        .drop_function_by_name("t1", "f", false)
        .expect("drop fn")
        .is_some());
}

/// SQL-66 (3)・Issue #1917: `CASCADE` は同じ scope の依存トリガだけを関数と同じ txn で消し、
/// 他テナントのトリガ・無関係な関数のトリガは残す。再オープン後も削除が残る。
#[test]
fn drop_function_cascade_removes_only_dependent_triggers_of_same_scope() {
    let (path, _g) = tmp_path("trg_dropfn_cascade");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    s.create_function("t1", &func("g")).expect("g");
    s.create_function("t2", &func("f")).expect("other tenant f");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    create(&s, "t1", "docs", &tdef("keep", "g")).expect("create g");
    create(&s, "t2", "docs", &tdef("other", "f")).expect("create other tenant");
    // RESTRICT（cascade = false）は 2BP01 のまま、何も消えない。
    assert!(matches!(
        s.drop_function_by_name("t1", "f", false),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    assert_eq!(s.list_triggers("t1", "docs").expect("list").len(), 2);
    let before =
        crate::catalog::table_generation_in_txn(&s.db().begin_read().expect("read"), "docs")
            .expect("gen");
    assert!(s
        .drop_function_by_name("t1", "f", true)
        .expect("cascade")
        .is_some());
    let names: Vec<String> = s
        .list_triggers("t1", "docs")
        .expect("list")
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["keep".to_string()]);
    assert_eq!(s.list_triggers("t2", "docs").expect("list").len(), 1);
    let after =
        crate::catalog::table_generation_in_txn(&s.db().begin_read().expect("read"), "docs")
            .expect("gen");
    assert!(after > before, "table generation must advance");
    drop(s);
    let s = Storage::open(&path).expect("reopen");
    assert_eq!(s.list_triggers("t1", "docs").expect("list").len(), 1);
    assert!(s.get_function("t1", "f", "").expect("get").is_none());
}

#[test]
fn column_ddl_is_guarded_by_update_of_references() {
    let (path, _g) = tmp_path("trg_column");
    let s = Storage::open(&path).expect("open");
    setup(&s, "t1", "docs", "f");
    create(&s, "t1", "docs", &tdef("trg", "f")).expect("create");
    assert!(matches!(
        s.alter_table_drop_column("docs", "a"),
        Err(CatalogError::DependentObjectsStillExist(_))
    ));
    assert!(s.alter_table_rename_column("docs", "a", "c").is_err());
    assert!(s
        .alter_table_alter_column_type("docs", "a", &ColumnType::Integer)
        .is_err());
    // 参照されない列は従来どおり操作できる。
    s.alter_table_drop_column("docs", "b").expect("drop b");
}

#[test]
fn lazy_table_creation_leaves_existing_layout_untouched() {
    use redb::TableHandle;
    let (path, _g) = tmp_path("trg_lazy");
    let s = Storage::open(&path).expect("open");
    setup_table_only(&s, "docs");
    s.alter_table_rename("docs", "docs2").expect("rename");
    s.drop_table("docs2").expect("drop");
    assert!(s.list_triggers("t1", "docs").expect("list").is_empty());
    let rt = s.db().begin_read().expect("read");
    let has_trigger_table = rt
        .list_tables()
        .expect("list")
        .any(|h| h.name() == "triggers" || h.name() == "trigger_id_counter");
    assert!(!has_trigger_table);
}
