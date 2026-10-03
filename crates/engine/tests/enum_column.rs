//! `ENUM` 列型（TABLE-14・TASK-198、Issue #890）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/data-model.md`
//! TABLE-14・TABLE-6・TABLE-7・`docs/spec/04-behavior/wire-protocol.md`
//! WIRE-13・`docs/spec/04-behavior/nosql.md` NOSQL-17。
//!
//! `tests/bytea_column.rs`（Issue #886）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。ENUM は名前付き型
//! （`Storage::create_enum_type`）を経由するため、`bytea_column.rs` とは異なり
//! テーブル作成前に型登録が必要になる。
//!
//! ファイル名は `boolean_column.rs`／`bytea_column.rs` と同じ命名規則
//! （`<type>_column.rs`）に揃えた。TASK-198 の spec 側成果物名
//! `composite_types.rs` は Issue #888（ARRAY）／#889（JSON）と共有される
//! 名前であり、並行実装との衝突を避けるためあえて使わない。

use engine::catalog::{
    CatalogError, ColumnDef, ColumnType, EnumTypeDef, TableSchema, MAX_ENUM_LABELS,
    MAX_ENUM_LABEL_LEN,
};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::Arc;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ENUM_TYPE: &str = "mood";

fn default_labels() -> Vec<String> {
    vec![
        "happy".to_string(),
        "sad".to_string(),
        "neutral".to_string(),
    ]
}

fn schema_with(def: Arc<EnumTypeDef>) -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("mood", ColumnType::Enum(def), true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("enum-column");
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(ENUM_TYPE, default_labels())
        .expect("create enum type");
    storage
        .create_table(&schema_with(def))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, mood_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, mood) VALUES ({id}, '[0.1,0.2]', '{lang}', {mood_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn select_mood(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT mood FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

// --- 受け入れ条件 1: 型 DDL の境界（個数・長さ・重複・組み込み型名衝突） -------

#[test]
fn create_enum_type_rejects_empty_and_duplicate_and_too_many_labels() {
    let path = unique_db_path("enum-ddl-labels");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        storage
            .create_enum_type("empty_labels", Vec::new())
            .unwrap_err(),
        CatalogError::Invalid(_)
    ));
    assert!(matches!(
        storage
            .create_enum_type("dup_labels", vec!["a".to_string(), "a".to_string()])
            .unwrap_err(),
        CatalogError::Invalid(_)
    ));
    let too_many: Vec<String> = (0..=MAX_ENUM_LABELS).map(|i| format!("l{i}")).collect();
    assert!(matches!(
        storage.create_enum_type("too_many", too_many).unwrap_err(),
        CatalogError::Invalid(_)
    ));
    // ちょうど上限（MAX_ENUM_LABELS）は受理される。
    let exactly_max: Vec<String> = (0..MAX_ENUM_LABELS).map(|i| format!("l{i}")).collect();
    storage
        .create_enum_type("exactly_max", exactly_max)
        .expect("exactly MAX_ENUM_LABELS labels should be accepted");
}

#[test]
fn create_enum_type_rejects_label_too_long_but_accepts_max_len() {
    let path = unique_db_path("enum-ddl-label-len");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    let too_long = "x".repeat(MAX_ENUM_LABEL_LEN + 1);
    assert!(matches!(
        storage
            .create_enum_type("too_long", vec![too_long])
            .unwrap_err(),
        CatalogError::Invalid(_)
    ));
    let exactly_max_len = "x".repeat(MAX_ENUM_LABEL_LEN);
    storage
        .create_enum_type("max_len", vec![exactly_max_len])
        .expect("exactly MAX_ENUM_LABEL_LEN byte label should be accepted");
}

#[test]
fn create_enum_type_rejects_control_char_and_empty_label() {
    let path = unique_db_path("enum-ddl-control");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    assert!(matches!(
        storage
            .create_enum_type("control", vec!["a\nb".to_string()])
            .unwrap_err(),
        CatalogError::Invalid(_)
    ));
    assert!(matches!(
        storage
            .create_enum_type("empty_label", vec![String::new()])
            .unwrap_err(),
        CatalogError::Invalid(_)
    ));
}

#[test]
fn create_enum_type_rejects_builtin_type_name_collision_case_insensitive() {
    let path = unique_db_path("enum-ddl-reserved");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");

    // "numeric" は main 側で既に予約済みの型名。"decimal"（NUMERIC の別名）は
    // 本 Issue（#885）で `RESERVED_TYPE_NAMES` へ追加したため、既存の型名衝突
    // 検査に追随させる。
    for reserved in [
        "text", "VECTOR", "Boolean", "bytea", "numeric", "DECIMAL", "UUID",
    ] {
        assert!(
            matches!(
                storage
                    .create_enum_type(reserved, vec!["a".to_string()])
                    .unwrap_err(),
                CatalogError::Invalid(_)
            ),
            "reserved name {reserved:?} should be rejected"
        );
    }
}

#[test]
fn create_enum_type_rejects_duplicate_type_name() {
    let path = unique_db_path("enum-ddl-dup-type");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_enum_type("mood", vec!["a".to_string()])
        .expect("first create should succeed");
    assert!(matches!(
        storage
            .create_enum_type("mood", vec!["b".to_string()])
            .unwrap_err(),
        CatalogError::TypeAlreadyExists(_)
    ));
}

#[test]
fn get_enum_type_of_unknown_name_is_type_not_found() {
    let path = unique_db_path("enum-ddl-not-found");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    assert!(matches!(
        storage.get_enum_type("nope").unwrap_err(),
        CatalogError::TypeNotFound(_)
    ));
}

// --- 受け入れ条件 5: DROP TYPE の依存列拒否 -----------------------------------

#[test]
fn drop_enum_type_rejects_when_dependent_column_exists_then_succeeds_after_drop_table() {
    let path = unique_db_path("enum-drop-type");
    let _guard = CleanupGuard(path.clone());
    // `EngineCore` を経由せず単一の `Storage` ハンドルで完結させる（redb は
    // 同一ファイルの二重オープンを許さないため、`EngineCore::from_storage`
    // へ渡した後は別ハンドルで再オープンできない。DDL 専用のこのテストでは
    // `EngineCore` 自体が不要）。
    let storage = Storage::open(&path).expect("open storage");
    let def = storage
        .create_enum_type(ENUM_TYPE, default_labels())
        .expect("create enum type");
    storage
        .create_table(&schema_with(def))
        .expect("create table");

    assert!(matches!(
        storage.drop_enum_type(ENUM_TYPE).unwrap_err(),
        CatalogError::DependentObjectsStillExist(_)
    ));
    storage.drop_table(TABLE).expect("drop table");
    storage
        .drop_enum_type(ENUM_TYPE)
        .expect("drop type should succeed once no dependents remain");
    assert!(matches!(
        storage.get_enum_type(ENUM_TYPE).unwrap_err(),
        CatalogError::TypeNotFound(_)
    ));
}

// --- 受け入れ条件 2: ALTER TYPE ... ADD VALUE（末尾追記のみ） ------------------

#[test]
fn alter_enum_type_add_value_allows_new_label_and_rejects_duplicate() {
    let path = unique_db_path("enum-alter-add-value");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    // 追記前は語彙外として拒否される（`EngineCore` はこのブロックの終わりで
    // drop し、redb のファイルハンドルを解放してから DDL 用に再オープンする。
    // redb は同一ファイルの二重オープンを許さない）。
    {
        let storage = Storage::open(&path).expect("open storage");
        let def = storage
            .create_enum_type(ENUM_TYPE, default_labels())
            .expect("create enum type");
        storage
            .create_table(&schema_with(def))
            .expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, "ja", "'excited'", 1),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22P02");
    }

    {
        let storage_handle = Storage::open(&path).expect("reopen for DDL");
        storage_handle
            .alter_enum_type_add_value(ENUM_TYPE, "excited".to_string())
            .expect("ADD VALUE should succeed");
        // 重複追記は拒否。
        assert!(storage_handle
            .alter_enum_type_add_value(ENUM_TYPE, "excited".to_string())
            .is_err());
    }

    // 追記後は `EngineCore` を再構築すれば受理されることを固定する
    // （`ALTER TYPE` 後の読み取りはテーブル世代整合キャッシュの再解決を経る）。
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "'excited'", 2),
    )
    .expect("insert with newly added label should succeed");
    assert_eq!(
        select_mood(&core, &alice, 2),
        Cell::Text("excited".to_string())
    );
}

// --- 往復: カタログ・行コーデック（reopen 越し） -------------------------------

#[test]
fn enum_column_roundtrips_through_storage_reopen() {
    let path = unique_db_path("enum-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let (core, _) = {
            let storage = Storage::open(&path).expect("open storage");
            let def = storage
                .create_enum_type(ENUM_TYPE, default_labels())
                .expect("create enum type");
            storage
                .create_table(&schema_with(def))
                .expect("create table");
            (
                EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
                (),
            )
        };
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'happy'", 1),
        )
        .expect("insert should succeed");
        // NULL 行（mood 未指定。nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-2-2'"
            ),
        )
        .expect("insert without mood should succeed");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        select_mood(&core, &alice, 1),
        Cell::Text("happy".to_string())
    );
    assert_eq!(select_mood(&core, &alice, 2), Cell::Null);
}

// --- 受け入れ条件 1: 語彙外ラベルの拒否（`22P02`）・非文字列リテラル（`22000`） --

#[test]
fn insert_rejects_label_not_in_vocabulary() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'furious'", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn insert_rejects_number_or_boolean_literal_for_enum_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "true", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 受け入れ条件 3: 等価述語・索引共有・集計 ----------------------------------

#[test]
fn where_predicate_equality_on_enum_column_matches_expected_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("insert 1");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(2, "ja", "'sad'", 2),
    )
    .expect("insert 2");

    let result = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE mood = 'happy' LIMIT 10"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].cells[0], Cell::Integer(1));
}

#[test]
fn where_predicate_with_unknown_label_on_enum_column_is_invalid_text_representation() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE mood = 'furious' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn like_predicate_on_enum_column_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE mood LIKE 'ha%' LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn sum_avg_min_max_on_enum_column_are_rejected_but_count_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("insert should succeed");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'seed-2-2'"
        ),
    )
    .expect("insert without mood should succeed");

    for func in ["SUM", "AVG", "MIN", "MAX"] {
        let err = core
            .execute_sql(&alice, &format!("SELECT {func}(mood) FROM {TABLE}"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42883", "func: {func}");
    }

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(mood) FROM {TABLE}"))
        .expect("COUNT(mood) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1), // id=1 のみ非 NULL
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

// --- UPDATE（単一行）・UPSERT・RETURNING の往復 --------------------------------

#[test]
fn update_single_row_set_enum_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!("UPDATE {TABLE} SET mood = 'sad' WHERE id = 1 USING OPERATION_ID 'op-set-mood'"),
    )
    .expect("UPDATE SET mood should succeed");
    assert_eq!(select_mood(&core, &alice, 1), Cell::Text("sad".to_string()));

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "UPDATE {TABLE} SET mood = 'furious' WHERE id = 1 USING OPERATION_ID 'op-set-mood-2'"
            ),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn upsert_do_update_set_excluded_enum_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            "INSERT INTO {TABLE} (id, embedding, lang, mood) VALUES (1, '[0.1,0.2]', 'ja', 'sad') \
             ON CONFLICT (id) DO UPDATE SET mood = EXCLUDED.mood \
             USING OPERATION_ID 'op-upsert-1'"
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    assert_eq!(select_mood(&core, &alice, 1), Cell::Text("sad".to_string()));
}

#[test]
fn insert_returning_enum_column_matches_select() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let result = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang, mood) VALUES (1, '[0.1,0.2]', 'ja', 'happy') \
                 RETURNING mood USING OPERATION_ID 'op-returning-1'"
            ),
        )
        .expect("insert with RETURNING should succeed");
    match result {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.result.rows.len(), 1);
            assert_eq!(o.result.rows[0].cells[0], Cell::Text("happy".to_string()));
        }
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
    assert_eq!(
        select_mood(&core, &alice, 1),
        Cell::Text("happy".to_string())
    );
}

// --- RLS 境界: 語彙外エラーの応答が他テナント行の有無で変わらない --------------

#[test]
fn invalid_text_representation_response_is_identical_regardless_of_other_tenant_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    // bob がまず行を持たない状態で拒否させる。
    let err_without_other_tenant = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "'furious'", 1),
        )
        .unwrap_err();

    // bob に行を作らせたうえで、同じ拒否を再現する。
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'happy'", 1),
    )
    .expect("bob's insert should succeed");
    let err_with_other_tenant = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "'furious'", 2),
        )
        .unwrap_err();

    assert_eq!(
        err_without_other_tenant.wire_code(),
        err_with_other_tenant.wire_code()
    );
    assert_eq!(
        err_without_other_tenant.client_message(),
        err_with_other_tenant.client_message()
    );
}

// --- row_codec の多層防御: Rust API から直接渡された語彙外 `Value::Enum` の拒否 -

#[test]
fn row_codec_encode_row_rejects_out_of_vocabulary_enum_value_directly() {
    use engine::row_codec::{encode_row, Value};
    use engine::storage::Visibility;

    let storage_def = {
        let path = unique_db_path("enum-row-codec-defense");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_enum_type(ENUM_TYPE, default_labels())
            .expect("create enum type")
    };
    let schema = schema_with(storage_def);

    // SQL 表層の束縛（`bind_enum_literal`）を経由せず、`row_codec::encode_row`
    // を直接呼んでも語彙外ラベルは拒否される（多層防御。Issue #890 D3）。
    let err = encode_row(
        &schema,
        "alice",
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("ja".to_string()),
            Value::Enum("furious".to_string()),
        ],
    )
    .unwrap_err();
    assert!(format!("{err}").contains("does not accept label"));

    // 語彙内のラベルは受理される。
    encode_row(
        &schema,
        "alice",
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("ja".to_string()),
            Value::Enum("happy".to_string()),
        ],
    )
    .expect("in-vocabulary label should encode successfully");
}

// --- row_codec の多層防御: decode 時の語彙外ラベル拒否（PR #1015 レビュー指摘・
// codex-review P1。encode 時点で有効だったラベルでも、decode 時に渡すスキーマの
// `EnumTypeDef` に含まれなければ拒否する。テーブル定義の型解決結果が decode 時点
// で変わりうる状況〔ここでは同一プロセス内で別インスタンスの `EnumTypeDef` を
// 意図的に渡すことで模す〕でも、語彙外ラベルが `Value::Enum`／`ScalarRef::Enum`
// として下流へ流出しないことを固定する） -------------------------------------

/// `EnumTypeDef` はフィールド private のためテスト側で直接組み立てられない。
/// `Storage::create_enum_type` を経由して構築する（同名・別語彙の 2 定義を
/// 独立した一時 DB からそれぞれ得ることで、テーブル内で同一型名の重複登録を
/// 禁止する既存契約と衝突せずに用意する）。
fn enum_def_via_storage(label_prefix: &str, labels: Vec<String>) -> Arc<EnumTypeDef> {
    let path = unique_db_path(&format!("enum-row-codec-defense-{label_prefix}"));
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_enum_type(ENUM_TYPE, labels)
        .expect("create enum type")
}

#[test]
fn row_codec_decode_row_rejects_label_absent_from_the_schemas_current_vocabulary() {
    use engine::row_codec::{decode_row, encode_row, Value};
    use engine::storage::Visibility;

    // encode 時点の語彙は "furious" を含む（正当に encode できる）。
    let encode_def =
        enum_def_via_storage("encode", vec!["happy".to_string(), "furious".to_string()]);
    let encode_schema = schema_with(encode_def);
    let buf = encode_row(
        &encode_schema,
        "alice",
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("ja".to_string()),
            Value::Enum("furious".to_string()),
        ],
    )
    .expect("encode with in-vocabulary label must succeed");

    // decode 時に渡すスキーマの現行語彙には "furious" が無い。破損行・
    // decode 時点で解決される語彙が encode 時点と食い違うケースを模す。
    let decode_def = enum_def_via_storage("decode", default_labels());
    let decode_schema = schema_with(decode_def);
    let err = decode_row(&decode_schema, &buf).unwrap_err();
    assert!(
        format!("{err}").contains("is not a valid label"),
        "decode must reject out-of-vocabulary enum label: {err}"
    );
}

#[test]
fn row_codec_scan_scalar_columns_masked_rejects_out_of_vocabulary_enum_label_even_when_unwanted() {
    use engine::row_codec::{encode_scalar_columns, scan_scalar_columns_masked, Value};

    let encode_def = enum_def_via_storage(
        "encode-masked",
        vec!["happy".to_string(), "furious".to_string()],
    );
    let encode_schema = schema_with(encode_def);
    // `scan_scalar_columns_masked` はスカラー専用バッファ（`encode_scalar_columns`）
    // を前提とする（`encode_row` の VECTOR 列込みフルバッファとは形が異なる。
    // 既存テスト `scan_scalar_columns_masked_mask_length_mismatch_is_rejected`
    // と同じ準備方法）。
    let buf = encode_scalar_columns(
        &encode_schema,
        &[
            Value::Vector(vec![0.1, 0.2]),
            Value::Text("ja".to_string()),
            Value::Enum("furious".to_string()),
        ],
    )
    .expect("encode with in-vocabulary label must succeed");

    let decode_def = enum_def_via_storage("decode-masked", default_labels());
    let decode_schema = schema_with(decode_def);

    // ENUM 列（インデックス 2）をマスクで非要求にしても、構造検証（UTF-8・
    // 語彙照合）は要求・非要求を問わず一律に行われる（Issue #350 の必要列限定
    // デコードが安全性検証を弱めないことの固定。Text 列の UTF-8 検証と同じ方針）。
    let mask = vec![false, false, false];
    let err = scan_scalar_columns_masked(&decode_schema, &buf, Some(&mask)).unwrap_err();
    assert!(
        format!("{err}").contains("is not a valid label"),
        "masked-unwanted decode must still reject out-of-vocabulary enum label: {err}"
    );
}
