//! TABLE-42 (2) のポインタ。matview の定義 codec・文脈単位の実体ストア・名前空間・暫定ガードを
//! Storage 水準で検証する（SQL 表層の配線は後続 Issue #1664〜#1667）。
use super::*;
use crate::catalog::{ArrayType, DropBehavior, EnumTypeDef, IndexDef, IndexKind};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::TableHandle;

fn base_schema() -> TableSchema {
    TableSchema::new(
        "base",
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn def(name: &str) -> MaterializedViewDef {
    MaterializedViewDef {
        name: name.to_string(),
        base_relations: vec!["base".to_string()],
        body_sql: "SELECT embedding FROM base".to_string(),
        column_aliases: Vec::new(),
        columns: vec![
            ColumnDef::new("label", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
        populated: false,
    }
}

fn row(label: &str, n: i32) -> Vec<Value> {
    vec![Value::Text(label.to_string()), Value::Integer(n)]
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("ctx")
}

fn open(label: &str) -> (CleanupGuard, std::path::PathBuf, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage.create_table(&base_schema()).expect("base");
    (guard, path, storage)
}

fn table_names(s: &Storage) -> Vec<String> {
    let rt = s.db().begin_read().expect("read");
    rt.list_tables()
        .expect("list")
        .map(|t| t.name().to_string())
        .collect()
}

#[test]
fn def_codec_roundtrips_and_rejects_corruption() {
    let mut d = def("mv");
    d.column_aliases = vec!["a".to_string(), "b".to_string()];
    let bytes = encode_materialized_view_def(&d).expect("encode");
    assert_eq!(
        decode_materialized_view_def("mv", &bytes).expect("decode"),
        d
    );
    for cut in [0, 1, 5, bytes.len() - 1] {
        assert!(matches!(
            decode_materialized_view_def("mv", &bytes[..cut]),
            Err(CatalogError::CorruptSchema(_))
        ));
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(matches!(
        decode_materialized_view_def("mv", &trailing),
        Err(CatalogError::CorruptSchema(_))
    ));
    let mut huge = d.clone();
    huge.body_sql = "x".repeat(MAX_VIEW_BODY_BYTES + 1);
    assert!(matches!(
        encode_materialized_view_def(&huge),
        Err(CatalogError::ViewLimitExceeded(_))
    ));
}

#[test]
fn enum_and_enum_array_columns_are_rejected_on_save() {
    let enum_def = std::sync::Arc::new(EnumTypeDef {
        name: "mood".to_string(),
        labels: vec!["a".to_string(), "b".to_string()],
    });
    let arr = ArrayType::new_enum(enum_def.clone(), 4).expect("array");
    let arr2 = ArrayType::new_enum(enum_def.clone(), 4)
        .and_then(|a| a.with_ndims(2))
        .expect("array2");
    for ty in [
        ColumnType::Enum(enum_def),
        ColumnType::Array(arr),
        ColumnType::Array(arr2),
    ] {
        let mut d = def("mv");
        d.columns.push(ColumnDef::new("e", ty, true));
        assert!(matches!(
            encode_materialized_view_def(&d),
            Err(CatalogError::Invalid(_))
        ));
    }
}

#[test]
fn context_key_distinguishes_visibility_sets_and_tenants() {
    let full = ContextKey::from_context(&ctx("a"));
    let public_only = ContextKey::from_context(&PolicyContext::new("a").expect("ctx"));
    assert_ne!(full, public_only);
    assert_ne!(full, ContextKey::from_context(&ctx("b")));
    assert_eq!(full, ContextKey::from_context(&ctx("a")));
    // マルチバイトのテナント ID でも完全一致で区別される。
    assert_ne!(
        ContextKey::from_context(&ctx("あ")),
        ContextKey::from_context(&ctx("い"))
    );
}

/// 作成直後は未実体化で、定義は往復一致する。
#[test]
fn created_matview_is_unpopulated_for_every_context() {
    let (_g, _p, s) = open("mv-created");
    s.create_materialized_view(&def("mv")).expect("create");
    let got = s
        .materialized_view_definition("mv")
        .expect("def")
        .expect("some");
    assert!(!got.populated);
    assert_eq!(got.columns, def("mv").columns);
    assert_eq!(
        s.read_materialized_view("mv", &ctx("a")).expect("read"),
        None
    );
    assert!(matches!(
        s.read_materialized_view("nope", &ctx("a")),
        Err(CatalogError::ViewNotFound(_))
    ));
}

/// AC1 とテナント境界: 文脈ごとに自分の行だけを書いた順で読める。再オープン後も同じ。
#[test]
fn entities_are_isolated_per_context_and_persist() {
    let (_g, path, s) = open("mv-isolation");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("a");
    let b = ctx("b");
    s.replace_materialized_view_entities(
        "mv",
        &[
            (a.clone(), vec![row("a1", 1), row("a2", 2), row("a0", 0)]),
            (b.clone(), vec![row("b1", 9)]),
        ],
        10,
    )
    .expect("replace");
    let ea = s
        .read_materialized_view("mv", &a)
        .expect("read")
        .expect("a");
    assert_eq!(ea.rows, vec![row("a1", 1), row("a2", 2), row("a0", 0)]);
    let eb = s
        .read_materialized_view("mv", &b)
        .expect("read")
        .expect("b");
    assert_eq!(eb.rows, vec![row("b1", 9)]);
    // Public のみの同テナント文脈・実体のないテナントは未実体化（存在・件数を観測させない）。
    let public_only = PolicyContext::new("a").expect("ctx");
    assert_eq!(
        s.read_materialized_view("mv", &public_only).expect("r"),
        None
    );
    assert_eq!(s.read_materialized_view("mv", &ctx("c")).expect("r"), None);
    assert!(
        s.materialized_view_definition("mv")
            .expect("d")
            .expect("s")
            .populated
    );
    drop(s);
    let s = Storage::open(&path).expect("reopen");
    let ea = s
        .read_materialized_view("mv", &a)
        .expect("read")
        .expect("a");
    assert_eq!(ea.rows.len(), 3);
}

#[test]
fn replace_swaps_all_entities_and_clear_empties() {
    let (_g, _p, s) = open("mv-replace");
    s.create_materialized_view(&def("mv")).expect("create");
    let (a, b) = (ctx("a"), ctx("b"));
    s.replace_materialized_view_entities(
        "mv",
        &[
            (a.clone(), vec![row("x", 1)]),
            (b.clone(), vec![row("y", 2)]),
        ],
        10,
    )
    .expect("first");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("z", 3)])], 10)
        .expect("second");
    assert_eq!(
        s.read_materialized_view("mv", &a)
            .expect("r")
            .expect("a")
            .rows,
        vec![row("z", 3)]
    );
    assert_eq!(s.read_materialized_view("mv", &b).expect("r"), None);
    s.clear_materialized_view_entities("mv").expect("clear");
    assert_eq!(s.read_materialized_view("mv", &a).expect("r"), None);
    assert!(
        !s.materialized_view_definition("mv")
            .expect("d")
            .expect("s")
            .populated
    );
}

#[test]
fn failed_replace_keeps_old_entities() {
    let (_g, _p, s) = open("mv-atomic");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("a");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("old", 1)])], 2)
        .expect("first");
    let bad_type = vec![Value::Integer(1), Value::Integer(1)];
    let cases: Vec<Vec<(PolicyContext, Vec<Vec<Value>>)>> = vec![
        vec![(a.clone(), vec![row("1", 1), row("2", 2), row("3", 3)])],
        vec![(a.clone(), vec![row("ok", 1), bad_type])],
        vec![
            (a.clone(), vec![row("1", 1)]),
            (a.clone(), vec![row("2", 2)]),
        ],
    ];
    for entities in cases {
        assert!(s
            .replace_materialized_view_entities("mv", &entities, 2)
            .is_err());
        assert_eq!(
            s.read_materialized_view("mv", &a)
                .expect("r")
                .expect("a")
                .rows,
            vec![row("old", 1)]
        );
    }
    // 上限ちょうどは通り、合計が上限を超えても文脈ごとなら通る。
    s.replace_materialized_view_entities(
        "mv",
        &[
            (a.clone(), vec![row("1", 1), row("2", 2)]),
            (ctx("b"), vec![row("3", 3), row("4", 4)]),
        ],
        2,
    )
    .expect("per-context limit");
}

#[test]
fn drop_removes_everything_and_recreate_starts_empty() {
    let (_g, _p, s) = open("mv-drop");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("a");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("x", 1)])], 10)
        .expect("replace");
    assert!(table_names(&s).iter().any(|n| n == "matview_rows/mv"));
    s.drop_materialized_view("mv").expect("drop");
    assert!(s.materialized_view_definition("mv").expect("d").is_none());
    assert!(!table_names(&s).iter().any(|n| n == "matview_rows/mv"));
    s.create_materialized_view(&def("mv")).expect("recreate");
    assert_eq!(s.read_materialized_view("mv", &a).expect("r"), None);
    assert!(matches!(
        s.drop_materialized_view("none"),
        Err(CatalogError::ViewNotFound(_))
    ));
}

#[test]
fn corrupted_entities_fail_closed() {
    let (_g, _p, s) = open("mv-corrupt");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("a");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("x", 1)])], 10)
        .expect("replace");
    let key = ContextKey::from_context(&a);
    // メタの row_count を実際の行数と食い違わせる。
    let txn = s.begin_write_txn().expect("w");
    {
        let mut meta = txn.open_table(MATVIEW_ENTITY_META_TABLE).expect("meta");
        meta.insert(("mv", key.as_bytes()), encode_meta(5).as_slice())
            .expect("ins");
    }
    bump_table_generation_in_txn(&txn, "mv").expect("bump");
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    assert!(matches!(
        s.read_materialized_view("mv", &a),
        Err(CatalogError::CorruptSchema(_))
    ));
    // 行ヘッダのテナントが別テナント。
    let schema = TableSchema::new("mv".to_string(), def("mv").columns);
    let foreign =
        row_codec::encode_row(&schema, "b", Visibility::Private, &row("x", 1)).expect("enc");
    let txn = s.begin_write_txn().expect("w");
    {
        let mut meta = txn.open_table(MATVIEW_ENTITY_META_TABLE).expect("meta");
        meta.insert(("mv", key.as_bytes()), encode_meta(1).as_slice())
            .expect("ins");
        let name = matview_rows_table_name("mv");
        let rows_def: MatviewRowsDef = TableDefinition::new(&name);
        let mut rows = txn.open_table(rows_def).expect("rows");
        rows.insert((key.as_bytes(), 0u64), foreign.as_slice())
            .expect("ins");
    }
    bump_table_generation_in_txn(&txn, "mv").expect("bump");
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    assert!(matches!(
        s.read_materialized_view("mv", &a),
        Err(CatalogError::CorruptSchema(_))
    ));
}

#[test]
fn read_and_definition_do_not_create_tables() {
    let (_g, _p, s) = open("mv-no-create");
    // write txn の存在検査は redb の仕様で空表を作りうるため、読み取り API の前後差で見る。
    let before = table_names(&s);
    assert!(s.materialized_view_definition("mv").expect("d").is_none());
    assert!(matches!(
        s.read_materialized_view("mv", &ctx("a")),
        Err(CatalogError::ViewNotFound(_))
    ));
    assert_eq!(table_names(&s), before);
}

#[test]
fn namespace_is_shared_with_tables_views_and_indexes() {
    let (_g, _p, s) = open("mv-namespace");
    s.create_materialized_view(&def("mv")).expect("create");
    assert!(s.relation_name_exists("mv").expect("exists"));
    // matview 名で表・ビュー・改名先を作ると 42P07 系。
    let t = TableSchema::new(
        "mv",
        vec![ColumnDef::new("e", ColumnType::Vector(2), false)],
    );
    assert!(matches!(
        s.create_table(&t),
        Err(CatalogError::TableAlreadyExists(_))
    ));
    assert!(matches!(
        s.create_view("mv", "base", "SELECT embedding FROM base"),
        Err(CatalogError::TableAlreadyExists(_))
    ));
    s.create_table(&TableSchema::new(
        "other",
        vec![ColumnDef::new("e", ColumnType::Vector(2), false)],
    ))
    .expect("other");
    assert!(s.alter_table_rename("other", "mv").is_err());
    assert!(s.relation_name_exists("other").expect("e"));
    // 既存の名前で matview を作ると 42P07、参照先なしは TableNotFound。
    assert!(matches!(
        s.create_materialized_view(&def("base")),
        Err(CatalogError::TableAlreadyExists(_))
    ));
    assert!(matches!(
        s.create_materialized_view(&def("mv")),
        Err(CatalogError::TableAlreadyExists(_))
    ));
    let mut missing = def("mv2");
    missing.base_relations = vec!["ghost".to_string()];
    assert!(matches!(
        s.create_materialized_view(&missing),
        Err(CatalogError::TableNotFound(_))
    ));
    // 種別不一致（42809）。
    assert!(matches!(
        s.drop_tables(&["mv".to_string()], false, DropBehavior::Restrict),
        Err(CatalogError::WrongObjectKind(_))
    ));
    assert!(matches!(
        s.drop_view("mv"),
        Err(CatalogError::WrongObjectKind(_))
    ));
    assert!(matches!(
        s.drop_index("mv"),
        Err(CatalogError::WrongObjectKind(_))
    ));
    let idx = IndexDef::new(
        "ix".to_string(),
        "mv".to_string(),
        IndexKind::Scalar,
        vec!["label".to_string()],
    );
    assert!(matches!(
        s.create_index(&idx),
        Err(CatalogError::WrongObjectKind(_))
    ));
    let idx2 = IndexDef::new(
        "mv".to_string(),
        "base".to_string(),
        IndexKind::Scalar,
        vec!["embedding".to_string()],
    );
    assert!(matches!(
        s.create_index(&idx2),
        Err(CatalogError::IndexAlreadyExists(_))
    ));
}

#[test]
fn referenced_relations_cannot_be_dropped_or_renamed() {
    let (_g, _p, s) = open("mv-guard");
    s.create_materialized_view(&def("mv")).expect("create");
    for behavior in [DropBehavior::Restrict, DropBehavior::Cascade] {
        assert!(matches!(
            s.drop_tables(&["base".to_string()], false, behavior),
            Err(CatalogError::DependentViewsExist(_))
        ));
    }
    assert!(s.alter_table_rename("base", "renamed").is_err());
    assert!(s.relation_name_exists("base").expect("e"));
    // ビューを参照元にした場合もビューの DROP を拒否する。
    s.create_view("v", "base", "SELECT embedding FROM base")
        .expect("view");
    let mut on_view = def("mv_v");
    on_view.base_relations = vec!["v".to_string()];
    s.create_materialized_view(&on_view)
        .expect("create on view");
    for behavior in [DropBehavior::Restrict, DropBehavior::Cascade] {
        assert!(matches!(
            s.drop_view_with_behavior("v", behavior),
            Err(CatalogError::DependentViewsExist(_))
        ));
    }
    // matview を参照する matview がある間は DROP できない。
    let mut on_mv = def("mv_mv");
    on_mv.base_relations = vec!["mv".to_string()];
    s.create_materialized_view(&on_mv)
        .expect("create on matview");
    assert!(matches!(
        s.drop_materialized_view("mv"),
        Err(CatalogError::DependentViewsExist(_))
    ));
}

#[test]
fn cascade_closure_views_referenced_by_matview_are_rejected() {
    // 表 base -> ビュー v1 -> ビュー v2 -> matview mv_v2。
    // 指定対象（base／v1）は matview の直接の参照元ではないが、CASCADE の閉包に含まれる
    // v2 を削除すると mv_v2 の参照先が欠落するため、削除前に fail-closed で拒否する。
    let (_g, _p, s) = open("mv-cascade-closure");
    s.create_view("v1", "base", "SELECT embedding FROM base")
        .expect("v1");
    s.create_view("v2", "v1", "SELECT embedding FROM v1")
        .expect("v2");
    let mut on_v2 = def("mv_v2");
    on_v2.base_relations = vec!["v2".to_string()];
    s.create_materialized_view(&on_v2).expect("create matview");

    // 表 -> ビュー -> matview 経路（DROP TABLE CASCADE）。
    assert!(matches!(
        s.drop_tables(&["base".to_string()], false, DropBehavior::Cascade),
        Err(CatalogError::DependentViewsExist(_))
    ));
    // ビュー -> ビュー -> matview 経路（DROP VIEW CASCADE）。
    assert!(matches!(
        s.drop_view_with_behavior("v1", DropBehavior::Cascade),
        Err(CatalogError::DependentViewsExist(_))
    ));
    // 拒否は副作用なし: 表・ビュー・matview がすべて残る。
    assert!(s.relation_name_exists("base").expect("base"));
    assert!(s.relation_name_exists("v1").expect("v1"));
    assert!(s.relation_name_exists("v2").expect("v2"));
    assert!(s.relation_name_exists("mv_v2").expect("mv"));
}
