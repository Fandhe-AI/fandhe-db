//! TABLE-42 (2)(5)(6) のポインタ。matview の定義 codec・文脈単位の実体ストア・名前空間・
//! 依存ガード・索引宣言・DROP を Storage 水準で検証する（SQL 表層の配線は `sql::ddl`・`core.rs`、
//! Issue #1664〜#1667）。
use super::*;
use crate::catalog::{ArrayType, CascadedObject, DropBehavior, EnumTypeDef, IndexDef, IndexKind};
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
    // CREATE OR REPLACE VIEW も matview 名とは衝突し、VIEWS_TABLE へ書かない（fail-closed）。
    assert!(matches!(
        s.create_or_replace_view("mv", "base", "SELECT embedding FROM base", &[], None),
        Err(CatalogError::WrongObjectKind(_))
    ));
    assert!(s.view_definition("mv").expect("view").is_none());
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
    // matview への索引は宣言できる（TABLE-42 (5)、Issue #1667）。
    s.create_index(&idx).expect("index on matview");
    assert!(matches!(
        s.create_index(&idx),
        Err(CatalogError::IndexAlreadyExists(_))
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
fn referenced_relations_restrict_blocks_drop_and_cascade_removes_dependents() {
    let (_g, _p, s) = open("mv-guard");
    s.create_materialized_view(&def("mv")).expect("create");
    // RESTRICT は参照元の表の削除を拒否する。改名は追随が入るまで拒否のまま。
    assert!(matches!(
        s.drop_tables(&["base".to_string()], false, DropBehavior::Restrict),
        Err(CatalogError::DependentViewsExist(_))
    ));
    assert!(s.alter_table_rename("base", "renamed").is_err());
    assert!(s.relation_name_exists("base").expect("e"));
    // ビューを参照元にした場合もビューの RESTRICT 削除を拒否する。
    s.create_view("v", "base", "SELECT embedding FROM base")
        .expect("view");
    let mut on_view = def("mv_v");
    on_view.base_relations = vec!["v".to_string()];
    s.create_materialized_view(&on_view)
        .expect("create on view");
    assert!(matches!(
        s.drop_view_with_behavior("v", DropBehavior::Restrict),
        Err(CatalogError::DependentViewsExist(_))
    ));
    // matview を参照する matview がある間は RESTRICT で DROP できない。
    let mut on_mv = def("mv_mv");
    on_mv.base_relations = vec!["mv".to_string()];
    s.create_materialized_view(&on_mv)
        .expect("create on matview");
    assert!(matches!(
        s.drop_materialized_view("mv"),
        Err(CatalogError::DependentViewsExist(_))
    ));
    // 拒否は副作用なし。
    for name in ["base", "v", "mv", "mv_v", "mv_mv"] {
        assert!(s.relation_name_exists(name).expect("exists"), "{name}");
    }

    // CASCADE: ビューの削除は、それを読む matview を連鎖削除する。
    let cascaded = s
        .drop_view_with_behavior("v", DropBehavior::Cascade)
        .expect("drop view cascade");
    assert_eq!(
        cascaded,
        vec![CascadedObject::MaterializedView {
            name: "mv_v".to_string()
        }]
    );
    assert!(!s.relation_name_exists("v").expect("v"));
    assert!(!s.relation_name_exists("mv_v").expect("mv_v"));
    // CASCADE: 表の削除は、直接・推移の matview（依存する側が先）を連鎖削除する。
    let report = s
        .drop_tables(&["base".to_string()], false, DropBehavior::Cascade)
        .expect("drop table cascade");
    assert_eq!(
        report.cascaded,
        vec![
            CascadedObject::MaterializedView {
                name: "mv_mv".to_string()
            },
            CascadedObject::MaterializedView {
                name: "mv".to_string()
            },
        ]
    );
    for name in ["base", "mv", "mv_mv"] {
        assert!(!s.relation_name_exists(name).expect("gone"), "{name}");
    }
}

#[test]
fn cascade_closure_drops_matviews_through_view_chains() {
    // 表 base -> ビュー v1 -> ビュー v2 -> matview mv_v2。
    let (_g, _p, s) = open("mv-cascade-closure");
    s.create_view("v1", "base", "SELECT embedding FROM base")
        .expect("v1");
    s.create_view("v2", "v1", "SELECT embedding FROM v1")
        .expect("v2");
    let mut on_v2 = def("mv_v2");
    on_v2.base_relations = vec!["v2".to_string()];
    s.create_materialized_view(&on_v2).expect("create matview");

    // RESTRICT は閉包ではなく直接の依存で拒否する（表 -> v1、ビュー v1 -> v2）。副作用なし。
    assert!(s
        .drop_tables(&["base".to_string()], false, DropBehavior::Restrict)
        .is_err());
    assert!(s
        .drop_view_with_behavior("v1", DropBehavior::Restrict)
        .is_err());
    assert!(s.relation_name_exists("mv_v2").expect("mv"));

    // ビュー -> ビュー -> matview 経路（DROP VIEW CASCADE）。matview はそれを読むビューより先に消える。
    let cascaded = s
        .drop_view_with_behavior("v1", DropBehavior::Cascade)
        .expect("cascade view");
    let pos = |o: &CascadedObject| cascaded.iter().position(|c| c == o).expect("present");
    assert!(
        pos(&CascadedObject::MaterializedView {
            name: "mv_v2".to_string()
        }) < pos(&CascadedObject::View {
            name: "v2".to_string()
        })
    );
    assert!(s.relation_name_exists("base").expect("base"));
    for name in ["v1", "v2", "mv_v2"] {
        assert!(!s.relation_name_exists(name).expect("gone"), "{name}");
    }
}

/// 定義と全文脈の実体を 1 つの write txn で作る（TABLE-42 (2)(3)・Issue #1664）。commit 前に
/// 失敗（行数超過・文脈キー重複・列に合わない行・名前衝突）すれば何も残らない。
fn create_with_entities(
    s: &Storage,
    d: &MaterializedViewDef,
    entities: Option<&[ContextEntity]>,
    max_rows: usize,
) -> Result<()> {
    let write_txn = s
        .begin_write_txn()
        .map_err(crate::catalog::convert_storage_error)?;
    create_materialized_view_in_txn(&write_txn, d, entities, max_rows)?;
    crate::storage::commit_write_txn(write_txn, true).map_err(crate::catalog::convert_storage_error)
}

#[test]
fn create_in_txn_writes_definition_and_all_entities_atomically() {
    let (_g, path, s) = open("mv-create-in-txn");
    let entities = vec![
        (ctx("a"), vec![row("a1", 1), row("a2", 2)]),
        (ctx("b"), vec![row("b1", 3)]),
    ];
    create_with_entities(&s, &def("mv"), Some(&entities), 10).expect("create");
    assert!(
        s.materialized_view_definition("mv")
            .unwrap()
            .unwrap()
            .populated
    );
    assert_eq!(
        s.read_materialized_view("mv", &ctx("a"))
            .unwrap()
            .unwrap()
            .rows,
        vec![row("a1", 1), row("a2", 2)]
    );
    assert_eq!(
        s.read_materialized_view("mv", &ctx("b"))
            .unwrap()
            .unwrap()
            .rows,
        vec![row("b1", 3)]
    );
    assert!(s.read_materialized_view("mv", &ctx("c")).unwrap().is_none());
    drop(s);
    let reopened = Storage::open(&path).expect("reopen");
    assert_eq!(
        reopened
            .read_materialized_view("mv", &ctx("a"))
            .unwrap()
            .unwrap()
            .rows
            .len(),
        2
    );
}

#[test]
fn create_in_txn_without_entities_is_unpopulated() {
    let (_g, _p, s) = open("mv-create-no-data");
    create_with_entities(&s, &def("mv"), None, 10).expect("create");
    assert!(
        !s.materialized_view_definition("mv")
            .unwrap()
            .unwrap()
            .populated
    );
    assert!(s.read_materialized_view("mv", &ctx("a")).unwrap().is_none());
}

#[test]
fn create_in_txn_failure_leaves_nothing() {
    let (_g, _p, s) = open("mv-create-fail");
    // 文脈ごとの行数超過（合計ではなく 1 文脈の件数で判定する）。
    let over = vec![(ctx("a"), vec![row("x", 1), row("y", 2), row("z", 3)])];
    assert!(matches!(
        create_with_entities(&s, &def("mv"), Some(&over), 2),
        Err(CatalogError::ViewLimitExceeded(_))
    ));
    let per_context_ok = vec![
        (ctx("a"), vec![row("x", 1), row("y", 2)]),
        (ctx("b"), vec![row("x", 1), row("y", 2)]),
    ];
    create_with_entities(&s, &def("mv_ok"), Some(&per_context_ok), 2).expect("per-context");
    // 文脈キーの重複。
    let dup = vec![(ctx("a"), vec![]), (ctx("a"), vec![])];
    assert!(matches!(
        create_with_entities(&s, &def("mv_dup"), Some(&dup), 10),
        Err(CatalogError::Invalid(_))
    ));
    // 列に合わない行。
    let bad = vec![(ctx("a"), vec![vec![Value::Integer(1)]])];
    assert!(matches!(
        create_with_entities(&s, &def("mv_bad"), Some(&bad), 10),
        Err(CatalogError::Invalid(_))
    ));
    // 失敗した 3 件は定義も実体も行表も残さない。
    for name in ["mv", "mv_dup", "mv_bad"] {
        assert!(
            s.materialized_view_definition(name).unwrap().is_none(),
            "{name}"
        );
    }
    let tables = table_names(&s);
    for name in ["mv", "mv_dup", "mv_bad"] {
        assert!(
            !tables.contains(&format!("matview_rows/{name}")),
            "{name} rows table leaked"
        );
    }
    // 名前衝突は何も書かない。
    assert!(matches!(
        create_with_entities(&s, &def("base"), None, 10),
        Err(CatalogError::TableAlreadyExists(_))
    ));
}

#[test]
fn read_in_works_over_a_write_txn_snapshot() {
    let (_g, _p, s) = open("mv-read-in-write");
    let entities = vec![(ctx("a"), vec![row("a1", 1)])];
    let write_txn = s.begin_write_txn().expect("write");
    create_materialized_view_in_txn(&write_txn, &def("mv"), Some(&entities), 10).expect("create");
    // 未 commit の実体を同じ txn から読める（確定済みスナップショットからは見えない）。
    let in_txn = read_materialized_view_in(&*write_txn, "mv", &ctx("a")).expect("read");
    assert_eq!(in_txn.unwrap().rows, vec![row("a1", 1)]);
    assert!(s.materialized_view_definition("mv").unwrap().is_none());
    drop(write_txn);
    assert!(s.materialized_view_definition("mv").unwrap().is_none());
}

/// 内部キー（`#` 始まり）は定義が無い扱い（`Ok(None)`＝未 populate と混同しない）。
#[test]
fn read_in_internal_key_is_view_not_found() {
    let (_g, _p, s) = open("mv-read-in-internal");
    let write_txn = s.begin_write_txn().expect("write");
    let r = read_materialized_view_in(&*write_txn, "#1.mv", &ctx("a"));
    assert!(matches!(r, Err(CatalogError::ViewNotFound(_))));
}

/// TABLE-42 (3)・Issue #1665: 逐次 REFRESH は commit すると旧実体を新しい文脈の実体で置き換える。
#[test]
fn streamed_refresh_replaces_all_contexts_on_commit() {
    let (_g, _p, s) = open("mv_refresh_commit");
    s.create_materialized_view(&def("mv")).expect("create");
    let (a, b) = (ctx("alice"), ctx("bob"));
    s.replace_materialized_view_entities(
        "mv",
        &[
            (a.clone(), vec![row("old", 1)]),
            (b.clone(), vec![row("old", 2)]),
        ],
        10,
    )
    .expect("populate");

    let txn = s.begin_write_txn().expect("txn");
    let d = begin_streamed_refresh_in_txn(&txn, "mv").expect("begin");
    let mut seen = Vec::new();
    write_context_entity_in_txn(&txn, &d, &a, &[row("new", 9)], &mut seen, 10).expect("write");
    finish_streamed_refresh_in_txn(&txn, &d, true).expect("finish");
    // 世代の bump は finish が bump_table_generation_in_txn で済ませている。
    crate::recovery::commit_boundary::commit(txn).expect("commit");

    let ea = s
        .read_materialized_view("mv", &a)
        .expect("read")
        .expect("a");
    assert_eq!(ea.rows.len(), 1);
    assert_eq!(ea.rows[0], row("new", 9));
    assert!(s.read_materialized_view("mv", &b).expect("read").is_none());
}

/// Issue #1665 指摘: 行 0 件の文脈でも符号化前提（テナント ID 長）を検査し、行ありの文脈と同じく
/// 失敗する（空の実体を populated として保存しない）。失敗時は何も書かず、検証失敗として分類される。
#[test]
fn write_context_entity_rejects_oversized_tenant_even_without_rows() {
    let (_g, _p, s) = open("mv_empty_long_tenant");
    s.create_materialized_view(&def("mv")).expect("create");
    let long = ctx(&"t".repeat(256));
    for rows in [Vec::new(), vec![row("x", 1)]] {
        let txn = s.begin_write_txn().expect("txn");
        let d = begin_streamed_refresh_in_txn(&txn, "mv").expect("begin");
        let mut seen = Vec::new();
        let err = write_context_entity_in_txn(&txn, &d, &long, &rows, &mut seen, 10)
            .expect_err("must fail");
        assert!(is_context_local_failure(&err));
        assert!(seen.is_empty());
    }
}

/// ストレージ起因のエラーは文脈固有の検証失敗として隠さない（部分書き込みを残したまま
/// commit させない）。
#[test]
fn storage_errors_are_not_context_local_failures() {
    assert!(!is_context_local_failure(&CatalogError::CorruptSchema(
        "x".to_string()
    )));
    assert!(!is_context_local_failure(
        &CatalogError::TableGenerationCounterOverflow
    ));
    assert!(is_context_local_failure(&CatalogError::Invalid(
        "x".to_string()
    )));
    assert!(is_context_local_failure(
        &CatalogError::UniqueConstraintViolation
    ));
}

/// 途中で txn を破棄すれば旧実体がすべて残る（失敗時に旧実体が残る受入基準）。
#[test]
fn streamed_refresh_dropped_txn_keeps_old_entities() {
    let (_g, _p, s) = open("mv_refresh_abort");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("alice");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("old", 1)])], 10)
        .expect("populate");
    {
        let txn = s.begin_write_txn().expect("txn");
        let d = begin_streamed_refresh_in_txn(&txn, "mv").expect("begin");
        let mut seen = Vec::new();
        write_context_entity_in_txn(&txn, &d, &a, &[row("new", 9)], &mut seen, 10).expect("write");
        finish_streamed_refresh_in_txn(&txn, &d, true).expect("finish");
        drop(txn);
    }
    let e = s
        .read_materialized_view("mv", &a)
        .expect("read")
        .expect("a");
    assert_eq!(e.rows[0], row("old", 1));
}

#[test]
fn streamed_refresh_unpopulate_and_missing_definition() {
    let (_g, _p, s) = open("mv_refresh_nodata");
    s.create_materialized_view(&def("mv")).expect("create");
    let a = ctx("alice");
    s.replace_materialized_view_entities("mv", &[(a.clone(), vec![row("old", 1)])], 10)
        .expect("populate");
    let txn = s.begin_write_txn().expect("txn");
    let d = begin_streamed_refresh_in_txn(&txn, "mv").expect("begin");
    finish_streamed_refresh_in_txn(&txn, &d, false).expect("finish");
    // 世代の bump は finish が bump_table_generation_in_txn で済ませている。
    crate::recovery::commit_boundary::commit(txn).expect("commit");
    assert!(s.read_materialized_view("mv", &a).expect("read").is_none());
    assert!(
        !s.materialized_view_definition("mv")
            .expect("def")
            .expect("some")
            .populated
    );

    let txn = s.begin_write_txn().expect("txn");
    assert!(matches!(
        begin_streamed_refresh_in_txn(&txn, "base"),
        Err(CatalogError::ViewNotFound(_))
    ));
    assert!(matches!(
        begin_streamed_refresh_in_txn(&txn, "nope"),
        Err(CatalogError::ViewNotFound(_))
    ));
}

fn scalar_index(name: &str, table: &str, cols: &[&str]) -> IndexDef {
    IndexDef::new(
        name.to_string(),
        table.to_string(),
        IndexKind::Scalar,
        cols.iter().map(|c| c.to_string()).collect(),
    )
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|n| n.to_string()).collect()
}

#[test]
fn matview_index_declaration_rules() {
    let (_g, _p, s) = open("mv-index-rules");
    s.create_materialized_view(&def("mv")).expect("create");
    let gen_before = s.table_generation("mv").expect("gen");
    s.create_index(&scalar_index("ix", "mv", &["label", "n"]))
        .expect("scalar index");
    assert!(s.table_generation("mv").expect("gen") > gen_before);
    // 名前衝突（索引・表・matview）は IndexAlreadyExists。
    for taken in ["ix", "base", "mv"] {
        assert!(matches!(
            s.create_index(&scalar_index(taken, "mv", &["label"])),
            Err(CatalogError::IndexAlreadyExists(_))
        ));
    }
    // matview に暗黙の id 列は無い。
    assert!(matches!(
        s.create_index(&scalar_index("ix_id", "mv", &["id"])),
        Err(CatalogError::ColumnNotFound(_))
    ));
    assert!(matches!(
        s.create_index(&scalar_index("ix_nope", "mv", &["nope"])),
        Err(CatalogError::ColumnNotFound(_))
    ));
    // HNSW は対象外（TABLE-42 (6)）。
    let hnsw = IndexDef::new(
        "ix_h".to_string(),
        "mv".to_string(),
        IndexKind::Hnsw,
        vec!["label".to_string()],
    );
    assert!(matches!(
        s.create_index(&hnsw),
        Err(CatalogError::IndexKindMismatch(_))
    ));
    // 照合を持てない型への COLLATE。
    let collated = scalar_index("ix_c", "mv", &["n"])
        .with_column_collations(vec![Some(crate::collation::Collation::C)]);
    assert!(matches!(
        s.create_index(&collated),
        Err(CatalogError::IndexCollationNotSupported(_))
    ));
    let listed: Vec<String> = s
        .list_indexes()
        .expect("list")
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(listed, vec!["ix".to_string()]);
    // 索引は DROP INDEX で外せる。
    s.drop_index("ix").expect("drop index");
    assert!(s.list_indexes().expect("list").is_empty());
}

#[test]
fn unique_index_is_enforced_per_context() {
    let (_g, _p, s) = open("mv-index-unique");
    s.create_materialized_view(&def("mv")).expect("create");
    // 文脈 a は label 重複なし、文脈 b は a と同じキーを 1 件ずつ持つ（文脈内では一意）。
    s.replace_materialized_view_entities(
        "mv",
        &[
            (ctx("a"), vec![row("k", 1), row("j", 2)]),
            (ctx("b"), vec![row("k", 3)]),
        ],
        10,
    )
    .expect("replace");
    let unique = scalar_index("ux", "mv", &["label"]).with_unique(true);
    s.create_index(&unique)
        .expect("文脈をまたぐ同値は重複ではない");
    // NULL は distinct。
    let nulls = vec![
        vec![Value::Null, Value::Integer(1)],
        vec![Value::Null, Value::Integer(2)],
    ];
    s.replace_materialized_view_entities("mv", &[(ctx("a"), nulls)], 10)
        .expect("NULL は重複しない");
    // 書き込みプリミティブは文脈内の重複を拒否し、旧実体を残す。
    let err = s
        .replace_materialized_view_entities(
            "mv",
            &[(ctx("a"), vec![row("dup", 1), row("dup", 2)])],
            10,
        )
        .expect_err("duplicate");
    assert!(matches!(err, CatalogError::UniqueConstraintViolation));
    assert_eq!(
        s.read_materialized_view("mv", &ctx("a"))
            .unwrap()
            .unwrap()
            .rows
            .len(),
        2
    );
    // 逐次書き込みプリミティブも同じ。
    let write_txn = s.begin_write_txn().expect("txn");
    let mut seen = Vec::new();
    let d = def("mv");
    let res = write_context_entity_in_txn(
        &write_txn,
        &d,
        &ctx("z"),
        &[row("x", 1), row("x", 2)],
        &mut seen,
        10,
    );
    assert!(matches!(res, Err(CatalogError::UniqueConstraintViolation)));
    drop(write_txn);

    // 既存実体に重複があれば UNIQUE 索引の作成は拒否され、宣言は残らない。
    s.drop_index("ux").expect("drop");
    s.replace_materialized_view_entities(
        "mv",
        &[
            (ctx("a"), vec![row("k", 1), row("k", 2)]),
            (ctx("b"), vec![row("q", 3)]),
        ],
        10,
    )
    .expect("replace");
    let err = s
        .create_index(&scalar_index("ux2", "mv", &["label"]).with_unique(true))
        .expect_err("duplicate in context a");
    assert!(matches!(err, CatalogError::UniqueConstraintViolation));
    assert!(s.list_indexes().expect("list").is_empty());
}

#[test]
fn drop_materialized_views_resolution_atomicity_and_cleanup() {
    let (_g, path, s) = open("mv-drop-many");
    s.create_materialized_view(&def("mv1")).expect("mv1");
    s.create_materialized_view(&def("mv2")).expect("mv2");
    s.replace_materialized_view_entities("mv1", &[(ctx("a"), vec![row("a1", 1)])], 10)
        .expect("fill");
    s.create_index(&scalar_index("ux", "mv1", &["label"]).with_unique(true))
        .expect("index");
    s.create_view("v", "base", "SELECT embedding FROM base")
        .expect("view");

    // 1 件でも失敗すれば何も消えない（不在・種別違い）。
    assert!(matches!(
        s.drop_materialized_views(&names(&["mv1", "ghost"]), false, DropBehavior::Restrict),
        Err(CatalogError::ViewNotFound(_))
    ));
    for bad in ["base", "v", "ux"] {
        for if_exists in [false, true] {
            assert!(matches!(
                s.drop_materialized_views(&names(&["mv1", bad]), if_exists, DropBehavior::Restrict),
                Err(CatalogError::WrongObjectKind(_))
            ));
        }
    }
    assert!(s.relation_name_exists("mv1").expect("mv1"));
    // IF EXISTS は不在だけを skip し、全件 skip は副作用なし。
    let gen = s.table_generation("mv2").expect("gen");
    let report = s
        .drop_materialized_views(&names(&["ghost"]), true, DropBehavior::Restrict)
        .expect("skip");
    assert_eq!((report.dropped, report.skipped.len()), (0, 1));
    assert_eq!(s.table_generation("mv2").expect("gen"), gen);
    // 重複指定は除去され、複数を 1 回で消せる。
    let report = s
        .drop_materialized_views(
            &names(&["mv1", "ghost", "mv2", "mv1"]),
            true,
            DropBehavior::Restrict,
        )
        .expect("drop many");
    assert_eq!(report.dropped, 2);
    assert_eq!(report.skipped, vec!["ghost".to_string()]);
    assert!(s.list_indexes().expect("list").is_empty());
    assert!(s.materialized_view_definition("mv1").unwrap().is_none());
    assert!(!table_names(&s)
        .iter()
        .any(|t| t.contains("matview_rows/mv1")));
    // 同名で作り直すと実体も索引宣言も引き継がない。
    s.create_materialized_view(&def("mv1")).expect("recreate");
    assert!(s
        .read_materialized_view("mv1", &ctx("a"))
        .unwrap()
        .is_none());
    s.create_index(&scalar_index("ux", "mv1", &["label"]).with_unique(true))
        .expect("索引名を再利用できる");
    drop(s);
    // 再オープン後も削除状態が保たれる。
    let s = Storage::open(&path).expect("reopen");
    assert!(s.materialized_view_definition("mv2").unwrap().is_none());
}

#[test]
fn drop_materialized_views_restrict_ignores_dependents_in_the_same_statement() {
    let (_g, _p, s) = open("mv-drop-same-stmt");
    s.create_materialized_view(&def("mv")).expect("mv");
    let mut on_mv = def("mv_mv");
    on_mv.base_relations = vec!["mv".to_string()];
    s.create_materialized_view(&on_mv).expect("mv_mv");
    assert!(matches!(
        s.drop_materialized_views(&names(&["mv"]), false, DropBehavior::Restrict),
        Err(CatalogError::DependentViewsExist(_))
    ));
    // 並べた名前どうしの依存は依存に数えない。
    let report = s
        .drop_materialized_views(&names(&["mv", "mv_mv"]), false, DropBehavior::Restrict)
        .expect("both");
    assert_eq!(report.dropped, 2);
    assert!(report.cascaded.is_empty());
}
