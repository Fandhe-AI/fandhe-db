//! `JSON`／`JSONB` 列型（TABLE-14・TASK-198、Issue #889）の結合テスト。ポインタ:
//! `docs/spec/05-tasks.md` TASK-198・`docs/spec/04-behavior/data-model.md`
//! TABLE-14・TABLE-6・TABLE-7・`docs/spec/04-behavior/nosql.md` NOSQL-8・
//! NOSQL-17。
//!
//! `tests/bytea_column.rs`（Issue #886）と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`／
//! `execute_sql_in_session` を production 経路として検証）。`JSON` 列は入力
//! テキスト保持・`JSONB` 列は正規化を固定し、不正 JSON・数値/真偽値リテラル・
//! `WHERE` の等価・`IN`・`IS [NOT] NULL`（Issue #1193。値としての等価は UNIQUE 制約と
//! 共通の `canonical_equality_text`）・集計／式評価への拒否経路・RLS 境界・
//! `operation_id` 再送判定
//! （`JSON` は非正規空白により内容不一致 `22023` になりうる非対称を含む）を
//! 固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("doc", ColumnType::Json, true),
            ColumnDef::new("docb", ColumnType::Jsonb, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("json-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, lang: &str, doc_literal: &str, docb_literal: &str, seq: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding, lang, doc, docb) VALUES ({id}, '[0.1,0.2]', '{lang}', {doc_literal}, {docb_literal}) \
         USING OPERATION_ID 'seed-{id}-{seq}'"
    )
}

fn select_doc(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT doc FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

fn select_docb(core: &EngineCore, ctx: &PolicyContext, id: u64) -> Cell {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT docb FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
        .expect("select should succeed");
    assert_eq!(result.rows.len(), 1);
    result.rows[0].cells[0].clone()
}

// --- 受け入れ条件 1: カタログ・行コーデックでの往復 ----------------------------

#[test]
fn json_column_preserves_input_text_jsonb_normalizes_across_reopen() {
    let path = unique_db_path("json-column-roundtrip");
    let _guard = CleanupGuard(path.clone());
    let alice = ctx_for("alice");

    {
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        // `doc`（JSON）には非正規の空白を含む入力を、`docb`（JSONB）には
        // キー順が辞書順でない入力を与える。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", r#"'{"b": 2, "a": 1}'"#, r#"'{"b":2,"a":1}'"#, 1),
        )
        .expect("insert should succeed");
        // JSON `null` は SQL NULL とは別物であり有効な値として受理される。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(2, "ja", "'null'", "'null'", 2),
        )
        .expect("insert of JSON null literal should succeed");
        // 列自体を未指定（SQL NULL、nullable のため許容される）。
        core.execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-3-3'"
            ),
        )
        .expect("insert without doc/docb should succeed");
    }

    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    // JSON 列は入力テキスト（空白・キー順）をそのまま保持する。
    assert_eq!(
        select_doc(&core, &alice, 1),
        Cell::Json(r#"{"b": 2, "a": 1}"#.to_string())
    );
    // JSONB 列はキー文字列の昇順・空白なしへ正規化される。
    assert_eq!(
        select_docb(&core, &alice, 1),
        Cell::Json(r#"{"a":1,"b":2}"#.to_string())
    );
    // JSON の `null` は SQL NULL と区別される。
    assert_eq!(select_doc(&core, &alice, 2), Cell::Json("null".to_string()));
    assert_eq!(
        select_docb(&core, &alice, 2),
        Cell::Json("null".to_string())
    );
    // 列自体を未指定にした場合は SQL NULL。
    assert_eq!(select_doc(&core, &alice, 3), Cell::Null);
    assert_eq!(select_docb(&core, &alice, 3), Cell::Null);
}

// --- 受け入れ条件 4: 不正 JSON・数値/真偽値リテラルの拒否 ----------------------

#[test]
fn insert_rejects_malformed_or_non_rfc8259_json() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    for (label, literal) in [
        ("not json at all", "'not json'"),
        ("duplicate key", r#"'{"a":1,"a":2}'"#),
        ("leading zero", "'01'"),
        ("trailing comma", "'[1,]'"),
        ("unterminated object", r#"'{"a":1'"#),
    ] {
        let err = core
            .execute_sql_in_session(
                &alice,
                &mut SessionState::default(),
                &insert_sql(1, "ja", literal, "'{}'", 1),
            )
            .unwrap_err();
        assert_eq!(err.wire_code(), "22P02", "case: {label}");
    }
}

#[test]
fn insert_rejects_excess_nesting_depth() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    // MAX_JSON_DEPTH(16) を超えるネスト。
    let mut deep = String::new();
    for _ in 0..20 {
        deep.push('[');
    }
    for _ in 0..20 {
        deep.push(']');
    }
    let literal = format!("'{deep}'");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", &literal, "'{}'", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22P02");
}

#[test]
fn typed_row_insert_rejects_excess_container_item_count() {
    // MAX_JSON_CONTAINER_ITEMS（65,536）を超える配列要素数は SQL 表層の
    // 字句解析入力長上限（1 MiB）に先に抵触するため、単体テスト
    // （`json.rs::parse_json_rejects_...` 系）に加え、SQL テキストを経由
    // しない Rust API（`tenant::insert_typed_row`）でも `JSON` 列としての
    // 拒否を固定する。
    let path = unique_db_path("json-column-typed-insert-container-items");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-container-items").expect("valid operation_id");

    let mut items = String::new();
    for i in 0..65_537u32 {
        if i > 0 {
            items.push(',');
        }
        items.push('0');
    }
    let too_many_items = format!("[{items}]");

    let err = engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Json(too_many_items),
            engine::row_codec::Value::Null,
        ],
        &op_id,
    )
    .unwrap_err();
    let _ = err; // encode チョークポイントが拒否したことのみを固定する。
}

// SQL 表層は字句解析の入力長上限（`sql::lexer::MAX_INPUT_LEN` = 1 MiB）が
// `MAX_JSON_FIELD_LEN`（4 MiB）より小さいため、SQL リテラル経由では JSON
// フィールド長上限そのものへは構造的に到達できない（1 MiB 超の SQL テキストは
// 常に字句解析段で `42601` になる）。総バイト長の検証が
// [`engine::json::validate_json_column_text`]／[`engine::json::canonicalize_jsonb_text`]
// のパース**前**で働くこと自体は `crates/engine/src/json.rs` の単体テストで
// 固定済み。ここでは、SQL テキストを経由しない Rust API
// （`tenant::insert_typed_row`）が束縛層の検証をバイパスした未検証入力を
// encode チョークポイントで確実に拒否する（API 誤用時は fail-closed に
// 内部エラー〔`XX000`〕として扱われる。ユーザー向け分類〔`54000`〕は
// 束縛層〔`sql::parser::bind_json_literal`〕が先に判定する契約）ことを固定する。
#[test]
fn typed_row_insert_rejects_total_byte_length_over_limit_before_parsing() {
    let path = unique_db_path("json-column-typed-insert-too-large");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-too-large").expect("valid operation_id");

    // MAX_JSON_FIELD_LEN（4 MiB）+ 1 バイトの、構文としては不正な JSON
    // （閉じ括弧を欠く）テキスト。
    let huge = format!("[{}", "1".repeat(4 * 1024 * 1024));
    assert!(huge.len() > 4 * 1024 * 1024);

    let err = engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Json(huge),
            engine::row_codec::Value::Null,
        ],
        &op_id,
    )
    .unwrap_err();
    // encode チョークポイントが拒否したことのみを固定する（分類は API 誤用時の
    // 内部エラー。上記コメント参照）。
    let _ = err;
}

#[test]
fn insert_rejects_number_or_boolean_literal_for_json_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "1", "'{}'", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");

    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &insert_sql(1, "ja", "true", "'{}'", 1),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- 受け入れ条件 5: WHERE 述語・集計・式評価・パス演算子への露出は拒否 -------

fn select_ids(core: &EngineCore, ctx: &PolicyContext, predicate: &str) -> Vec<u64> {
    let sql = format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 100");
    let mut ids: Vec<u64> = core
        .execute_sql(ctx, &sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    ids.sort_unstable();
    ids
}

fn seed_json_rows(core: &EngineCore, ctx: &PolicyContext) {
    // (id, doc, docb)。id=4 は doc・docb とも NULL。
    let rows: [(u64, Option<&str>, Option<&str>); 4] = [
        (
            1,
            Some(r#"'{"b": 1, "a": [1, 2]}'"#),
            Some(r#"'{"b":1,"a":[1,2]}'"#),
        ),
        (2, Some("'[]'"), Some("'{}'")),
        (3, Some(r#"'{"n": 1.0}'"#), Some(r#"'{"n":1e0}'"#)),
        (4, None, None),
    ];
    for (id, doc, docb) in rows {
        let sql = match (doc, docb) {
            (Some(d), Some(b)) => insert_sql(id, "ja", d, b, id),
            _ => format!(
                "INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[0.1,0.2]', 'ja') \
                 USING OPERATION_ID 'seed-{id}-{id}'"
            ),
        };
        core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
            .expect("seed insert");
    }
}

#[test]
fn where_equality_in_and_is_null_on_json_columns_use_value_equality() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    seed_json_rows(&core, &alice);

    // 空白・キー順・数値表記（1／1.0／1e0）の違いは同値。JSON・JSONB のどちらでも。
    assert_eq!(
        select_ids(&core, &alice, r#"doc = '{"a":[1,2],"b":1.0}'"#),
        vec![1]
    );
    assert_eq!(
        select_ids(&core, &alice, r#"docb = '{ "a" : [1, 2], "b": 1 }'"#),
        vec![1]
    );
    assert_eq!(select_ids(&core, &alice, r#"doc = '{"n":1}'"#), vec![3]);
    assert_eq!(select_ids(&core, &alice, r#"docb = '{"n":1.00}'"#), vec![3]);
    assert_eq!(select_ids(&core, &alice, "doc = '[]'"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "docb = '{}'"), vec![2]);
    assert_eq!(
        select_ids(&core, &alice, r#"docb = '{"a":[2,1],"b":1}'"#),
        Vec::<u64>::new()
    );
    assert_eq!(
        select_ids(&core, &alice, "doc IN ('[]', '{\"n\":1}')"),
        vec![2, 3]
    );
    assert_eq!(select_ids(&core, &alice, "docb IN ('{}', '[]')"), vec![2]);
    assert_eq!(select_ids(&core, &alice, "doc IS NULL"), vec![4]);
    assert_eq!(select_ids(&core, &alice, "docb IS NOT NULL"), vec![1, 2, 3]);
    // 三値論理: NULL 列は NOT でも一致しない。
    assert_eq!(select_ids(&core, &alice, "NOT doc = '[]'"), vec![1, 3]);
    // 索引対応述語との複合でも再評価される。
    assert_eq!(
        select_ids(&core, &alice, "lang = 'ja' AND doc = '[]'"),
        vec![2]
    );
    assert_eq!(
        select_ids(&core, &alice, "lang = 'zz' AND doc = '[]'"),
        Vec::<u64>::new()
    );
}

#[test]
fn where_json_literal_errors_use_json_column_classes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let attempt = |predicate: &str| {
        core.execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE {predicate} LIMIT 10"),
        )
        .unwrap_err()
        .wire_code()
    };
    assert_eq!(attempt("doc = '{not json'"), "22P02");
    // 重複キー・深さ超過も INSERT と同じ検証。
    assert_eq!(attempt(r#"docb = '{"a":1,"a":2}'"#), "22P02");
    let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
    assert_eq!(attempt(&format!("doc = '{deep}'")), "22P02");
    // 範囲比較・LIKE・BETWEEN は JSON 列に対して従来どおり 22000。
    assert_eq!(attempt("doc LIKE 'x%'"), "22000");
    assert_eq!(attempt("doc > '{}'"), "22000");
    assert_eq!(attempt("doc BETWEEN '{}' AND '[]'"), "22000");
}

#[test]
fn where_json_equality_does_not_cross_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");
    seed_json_rows(&core, &alice);
    assert_eq!(select_ids(&core, &alice, "doc = '[]'"), vec![2]);
    assert!(select_ids(&core, &bob, "doc = '[]'").is_empty());
    assert!(select_ids(&core, &bob, "doc IS NOT NULL").is_empty());
}

#[test]
fn json_path_operator_is_rejected_as_unsupported_syntax() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", r#"'{"a":1}'"#, "'{}'", 1),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(&alice, &format!("SELECT doc -> 'a' FROM {TABLE} LIMIT 10"))
        .unwrap_err();
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn json_column_cannot_be_used_in_an_expression() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", r#"'{"a":1}'"#, "'{}'", 1),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT vec_norm(doc) FROM {TABLE} LIMIT 10"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn sum_avg_min_max_on_json_column_are_rejected_but_count_is_accepted() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'{}'", "'{}'", 1),
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
    .expect("insert without doc should succeed");

    for func in ["SUM", "AVG", "MIN", "MAX"] {
        let err = core
            .execute_sql(&alice, &format!("SELECT {func}(doc) FROM {TABLE}"))
            .unwrap_err();
        assert_eq!(err.wire_code(), "42883", "func: {func}");
    }

    let result = core
        .execute_sql(&alice, &format!("SELECT COUNT(doc) FROM {TABLE}"))
        .expect("COUNT(doc) should succeed");
    match &result.rows[0].cells[0] {
        Cell::Integer(v) => assert_eq!(*v, 1), // id=1 のみ非 NULL
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

#[test]
fn group_by_on_json_column_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'{}'", "'{}'", 1),
    )
    .expect("insert should succeed");

    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT doc, COUNT(*) FROM {TABLE} GROUP BY doc"),
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

// --- UPDATE（単一行）・UPSERT・RETURNING の往復 --------------------------------

#[test]
fn update_single_row_set_json_columns() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'{}'", "'{}'", 1),
    )
    .expect("insert should succeed");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            r#"UPDATE {TABLE} SET doc = '{{"x":1}}', docb = '{{"y":2}}' WHERE id = 1 USING OPERATION_ID 'op-set-doc'"#
        ),
    )
    .expect("UPDATE SET doc/docb should succeed");
    assert_eq!(
        select_doc(&core, &alice, 1),
        Cell::Json(r#"{"x":1}"#.to_string())
    );
    assert_eq!(
        select_docb(&core, &alice, 1),
        Cell::Json(r#"{"y":2}"#.to_string())
    );
}

#[test]
fn upsert_do_update_set_excluded_json_column() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", "'{}'", "'{}'", 1),
    )
    .expect("initial insert");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &format!(
            r#"INSERT INTO {TABLE} (id, embedding, lang, doc, docb) VALUES (1, '[0.1,0.2]', 'ja', '{{"z":9}}', '{{}}')
             ON CONFLICT (id) DO UPDATE SET doc = EXCLUDED.doc
             USING OPERATION_ID 'op-upsert-1'"#
        ),
    )
    .expect("upsert DO UPDATE should succeed");
    assert_eq!(
        select_doc(&core, &alice, 1),
        Cell::Json(r#"{"z":9}"#.to_string())
    );
}

#[test]
fn insert_returning_json_column_matches_select() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let result = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                r#"INSERT INTO {TABLE} (id, embedding, lang, doc, docb) VALUES (1, '[0.1,0.2]', 'ja', '{{"a":1}}', '{{"a":1}}')
                 RETURNING doc USING OPERATION_ID 'op-insert-returning'"#
            ),
        )
        .expect("INSERT RETURNING should succeed");
    match result {
        SqlOutcome::Returning(o) => {
            assert_eq!(o.result.rows.len(), 1);
            assert_eq!(
                o.result.rows[0].cells[0],
                Cell::Json(r#"{"a":1}"#.to_string())
            );
        }
        other => panic!("expected SqlOutcome::Returning, got {other:?}"),
    }
    assert_eq!(
        select_doc(&core, &alice, 1),
        Cell::Json(r#"{"a":1}"#.to_string())
    );
}

// --- RLS: 2 テナントの混在で他テナント行が混入しない・エラー応答が存在情報を漏らさない ---

#[test]
fn rls_isolates_json_rows_across_tenants() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let bob = ctx_for("bob");

    core.execute_sql_in_session(
        &alice,
        &mut SessionState::default(),
        &insert_sql(1, "ja", r#"'{"owner":"alice"}'"#, "'{}'", 1),
    )
    .expect("alice insert");
    core.execute_sql_in_session(
        &bob,
        &mut SessionState::default(),
        &insert_sql(2, "ja", r#"'{"owner":"bob"}'"#, "'{}'", 2),
    )
    .expect("bob insert");

    let alice_result = core
        .execute_sql(&alice, &format!("SELECT id, doc FROM {TABLE} LIMIT 100"))
        .expect("alice select should succeed");
    assert_eq!(alice_result.rows.len(), 1);
    assert_eq!(alice_result.rows[0].id, 1);

    // 0 件 UPDATE（他テナント所有行）の応答は行の有無・値を漏らさない
    // （RLS-9・RLS-10）。
    let outcome = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &format!(
                r#"UPDATE {TABLE} SET doc = '{{}}' WHERE id = 2 USING OPERATION_ID 'op-cross'"#
            ),
        )
        .expect("cross-tenant UPDATE should succeed with 0 rows");
    match outcome {
        SqlOutcome::Update(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Update, got {other:?}"),
    }
    // bob 側の値は変更されていない。
    assert_eq!(
        select_doc(&core, &bob, 2),
        Cell::Json(r#"{"owner":"bob"}"#.to_string())
    );
}

// --- content_hash: operation_id 再送判定 --------------------------------------

#[test]
fn resend_same_operation_id_with_same_jsonb_value_is_23505_even_with_differing_whitespace() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", "'{}'", r#"'{"a":1,"b":2}'"#, 1).replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // JSONB は正規化されるため、空白・キー順が異なっていても同一内容として
    // 23505 になる（JSON 型との非対称。D4 参照）。
    let same_content_reordered =
        insert_sql(1, "ja", "'{}'", r#"'{ "b": 2, "a": 1 }'"#, 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(
            &alice,
            &mut SessionState::default(),
            &same_content_reordered,
        )
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // 内容が異なる再送は 22023。
    let differing =
        insert_sql(1, "ja", "'{}'", r#"'{"a":1,"b":3}'"#, 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn resend_same_operation_id_with_same_json_text_is_23505_but_differing_whitespace_is_22023() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let alice = ctx_for("alice");
    let sql = insert_sql(1, "ja", r#"'{"a":1}'"#, "'{}'", 1).replace("seed-1-1", "op-resend");
    core.execute_sql_in_session(&alice, &mut SessionState::default(), &sql)
        .expect("first insert should succeed");

    // 同一テキストの再送は 23505。
    let same_text = insert_sql(1, "ja", r#"'{"a":1}'"#, "'{}'", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &same_text)
        .unwrap_err();
    assert_eq!(err.wire_code(), "23505");

    // JSON 型は入力テキストをそのまま保持するため、内容として等価でも
    // テキスト表現が異なれば（空白の有無）内容不一致 22023 になる
    // （JSONB との意図的な非対称。D4 参照）。
    let differing_whitespace =
        insert_sql(1, "ja", r#"'{ "a": 1 }'"#, "'{}'", 1).replace("seed-1-1", "op-resend");
    let err = core
        .execute_sql_in_session(&alice, &mut SessionState::default(), &differing_whitespace)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22023");
}

// --- Rust API 直接投入（`RowInput`／`tenant::insert_typed_row` 経路）での往復 ---

#[test]
fn typed_row_insert_roundtrips_json_value() {
    let path = unique_db_path("json-column-typed-insert");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Json(r#"{"k":"v"}"#.to_string()),
            engine::row_codec::Value::Json(r#"{"k":"v"}"#.to_string()),
        ],
        &op_id,
    )
    .expect("typed insert should succeed");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        select_doc(&core, &alice, 1),
        Cell::Json(r#"{"k":"v"}"#.to_string())
    );
    assert_eq!(
        select_docb(&core, &alice, 1),
        Cell::Json(r#"{"k":"v"}"#.to_string())
    );
}

// --- Rust API 直接投入での不正 JSON 拒否（encode チョークポイントの多層防御） ---

#[test]
fn typed_row_insert_rejects_invalid_json_via_encode_chokepoint() {
    let path = unique_db_path("json-column-typed-insert-invalid");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let alice = ctx_for("alice");
    let op_id = OperationId::parse("typed-op-invalid").expect("valid operation_id");
    let err = engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &alice,
        1,
        Visibility::Public,
        &[
            engine::row_codec::Value::Vector(vec![0.1, 0.2]),
            engine::row_codec::Value::Text("ja".to_string()),
            engine::row_codec::Value::Json("not json".to_string()),
            engine::row_codec::Value::Null,
        ],
        &op_id,
    )
    .unwrap_err();
    // API 誤用（未検証 JSON の直接投入）は fail-closed に拒否される。
    let _ = err;
}
